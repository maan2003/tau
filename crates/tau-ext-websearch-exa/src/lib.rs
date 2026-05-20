//! Web-search extension backed by Exa's keyless free-tier MCP at
//! <https://mcp.exa.ai/mcp>.
//!
//! Registers a single `web_search` tool. On each `ToolInvoke` the
//! extension proxies the call to Exa's hosted `web_search_exa` tool
//! over its Streamable-HTTP MCP transport (HTTP POST returning a
//! `text/event-stream` body with one `message` SSE event), unwraps the
//! JSON-RPC envelope, and returns the model-friendly text blob Exa
//! produces.
//!
//! Free tier: ~1000 requests/month per IP, no API key required. We
//! intentionally keep this minimal — a richer extension covering
//! `web_fetch_exa`, API-key support, etc. can layer on later.

use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use tau_proto::{
    Ack, CborValue, ConfigError, Event, Frame, FrameReader, FrameWriter, LogEventId, Message,
    ToolDisplay, ToolDisplayStats, ToolDisplayStatus, ToolError, ToolExecutionMode, ToolInvoke,
    ToolResult, ToolSpec,
};

/// `tracing` target for events emitted from this extension.
pub const LOG_TARGET: &str = "websearch-exa";

/// Tool name the extension registers with the harness. The harness's
/// `ToolName` newtype only accepts `[a-zA-Z0-9_]+`, so we use an
/// underscore between the two halves.
pub const TOOL_NAME: &str = "websearch_exa";

/// Tool name advertised to models.
pub const MODEL_VISIBLE_TOOL_NAME: &str = "web_search";

/// Default Exa MCP endpoint. Override via the extension's
/// `config.endpoint` field if you want to point at a self-hosted MCP
/// server or attach an `?exaApiKey=…` query parameter for the paid
/// tier.
pub const DEFAULT_ENDPOINT: &str = "https://mcp.exa.ai/mcp";

/// Name of the upstream tool we forward to.
const REMOTE_TOOL: &str = "web_search_exa";

/// MCP protocol version we declare. Exa accepts our `tools/call`
/// requests directly without an `initialize` handshake, so this header
/// just helps version-pin behavior on the server side.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Default `numResults` we forward to Exa when the agent omits it.
/// Five is enough to triage most queries without burning the monthly
/// quota too fast.
const DEFAULT_NUM_RESULTS: u32 = 5;

/// Maximum `numResults` the agent can request. Mirrors Exa's own
/// upper bound on the `web_search_exa` schema.
const MAX_NUM_RESULTS: u32 = 100;

/// Per-request timeout. Exa search latency is in the high hundreds
/// of milliseconds; the long ceiling is in case a slow page extraction
/// is in play, but we'd rather fail fast than block the agent turn.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

/// Maximum number of HTTP searches in flight at once. Each invocation
/// spawns a worker thread that holds a `REQUEST_TIMEOUT`-bounded
/// request open against Exa; without a cap, a bursty agent could
/// stack arbitrarily many native threads. 8 is well above the
/// realistic concurrency for a single agent turn and well below any
/// rate limit a free-tier IP would face.
const MAX_IN_FLIGHT: usize = 8;

/// Hard cap on the bytes of an Exa error response body that we paste
/// into a `tool.error` message. Bounds memory and log noise if the
/// upstream returns a large HTML page on failure.
const ERROR_BODY_MAX_BYTES: usize = 64 * 1024;

pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_with_searcher(reader, writer, Arc::new(HttpSearcher::default()))
}

/// Performs one Exa search. Abstracted so tests can stub the network
/// call without spinning up a real HTTP server.
pub trait Searcher: Send + Sync + 'static {
    fn search(&self, query: &str, num_results: u32) -> Result<String, String>;

    /// Apply a runtime endpoint update from a harness `Configure`.
    /// Default no-op for stubs that don't talk to a network.
    fn set_endpoint(&self, _endpoint: String) {}
}

/// Extension-side config carried in `Message::Configure.config`.
///
/// All fields optional with `#[serde(default)]` so an empty or
/// missing config object falls back to compiled-in defaults.
/// `deny_unknown_fields` surfaces typos in `harness.yaml` as
/// actionable `ConfigError`s instead of silently ignoring them.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExtConfig {
    /// Override the Exa MCP endpoint. Use this to attach an
    /// `?exaApiKey=…` query parameter for the paid tier, or point at
    /// a self-hosted `exa-mcp-server`.
    endpoint: Option<String>,
}

fn run_with_searcher<R, W>(
    reader: R,
    writer: W,
    searcher: Arc<dyn Searcher>,
) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write + Send + 'static,
{
    let mut reader = FrameReader::new(BufReader::new(reader));
    let mut writer = FrameWriter::new(BufWriter::new(writer));

    tau_extension::Handshake::tool("tau-ext-websearch-exa")
        .subscribe([tau_proto::EventName::TOOL_INVOKE])
        .register_tool(tool_spec())
        .ready_message("websearch-exa ready")
        .run(&mut writer)?;

    let (tx, rx) = mpsc::channel::<Frame>();
    let sem = Arc::new(Semaphore::new(MAX_IN_FLIGHT));

    let writer_handle = std::thread::spawn(move || -> Result<(), Box<dyn Error + Send>> {
        for frame in rx {
            writer
                .write_frame(&frame)
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
            writer
                .flush()
                .map_err(|e| -> Box<dyn Error + Send> { Box::new(e) })?;
        }
        Ok(())
    });

    while let Some(frame) = reader.read_frame()? {
        let (log_id, inner) = frame.peel_log();
        match inner {
            Frame::Message(Message::Configure(msg)) => {
                match tau_extension::parse_config::<ExtConfig>(&msg.config) {
                    Ok(cfg) => {
                        if let Some(endpoint) = cfg.endpoint {
                            tracing::info!(
                                target: LOG_TARGET,
                                endpoint = %endpoint,
                                "applying endpoint override",
                            );
                            searcher.set_endpoint(endpoint);
                        }
                    }
                    Err(message) => {
                        tracing::warn!(target: LOG_TARGET, error = %message, "rejecting config");
                        let _ = tx.send(Frame::Message(Message::ConfigError(ConfigError {
                            message,
                        })));
                    }
                }
            }
            Frame::Event(Event::ToolInvoke(invoke)) => {
                // Acquire before spawning so the in-flight cap bounds
                // native thread count, not just concurrent execution.
                let permit = sem.acquire();
                let tx = tx.clone();
                let searcher = Arc::clone(&searcher);
                std::thread::spawn(move || {
                    let _permit = permit;
                    dispatch_tool_invoke(invoke, searcher.as_ref(), &tx);
                });
            }
            Frame::Message(Message::Disconnect(_)) => break,
            _ => {}
        }
        if let Some(id) = log_id {
            ack_log_event(id, &tx);
        }
    }

    drop(tx);
    writer_handle
        .join()
        .map_err(|e| -> Box<dyn Error> { format!("writer thread panicked: {e:?}").into() })?
        .map_err(|e| -> Box<dyn Error> { e })?;
    Ok(())
}

fn ack_log_event(id: LogEventId, tx: &mpsc::Sender<Frame>) {
    let _ = tx.send(Frame::Message(Message::Ack(Ack { up_to: id })));
}

fn tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(TOOL_NAME),
        model_visible_name: Some(tau_proto::ToolName::new(MODEL_VISIBLE_TOOL_NAME)),
        description: Some(
            "Search the web via Exa's free-tier hosted MCP. Returns clean, ready-to-use \
             text content (titles, URLs, highlights) from top-ranked pages. Works best with a \
             natural-language description of the *ideal page* rather than a keyword query — \
             e.g. \"blog post comparing React and Vue performance\" beats \"React vs Vue\". \
             Use category:people / category:company prefixes to scope results to LinkedIn-style \
             profiles or company pages."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language description of the ideal page. May start with `category:people` or `category:company` to focus the result set."
                },
                "num_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_NUM_RESULTS,
                    "description": format!("Number of results to return (default: {DEFAULT_NUM_RESULTS}, max: {MAX_NUM_RESULTS}).")
                }
            },
            "required": ["query"]
        })),
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: None,
    }
}

fn dispatch_tool_invoke(invoke: ToolInvoke, searcher: &dyn Searcher, tx: &mpsc::Sender<Frame>) {
    if invoke.tool_name.as_str() != TOOL_NAME {
        let _ = tx.send(Frame::Event(Event::ToolError(ToolError {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            tool_type: tau_proto::ToolType::Function,
            display: Some(exa_error_display("unknown tool")),
            message: "unknown tool".to_owned(),
            details: None,
            originator: tau_proto::PromptOriginator::User,
        })));
        return;
    }
    let event = match parse_args(&invoke.arguments) {
        Ok((query, num_results)) => match searcher.search(&query, num_results) {
            Ok(text) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    query = %query,
                    num_results,
                    response_len = text.len(),
                    "exa search returned",
                );
                let display = exa_ok_display(&text);
                Event::ToolResult(ToolResult {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                    result: CborValue::Text(text),
                    display: Some(display),
                    originator: tau_proto::PromptOriginator::User,
                })
            }
            Err(message) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    query = %query,
                    error = %message,
                    "exa search failed",
                );
                Event::ToolError(ToolError {
                    call_id: invoke.call_id,
                    tool_name: invoke.tool_name,
                    tool_type: tau_proto::ToolType::Function,
                    display: Some(exa_error_display(&message)),
                    message,
                    details: Some(invoke.arguments),
                    originator: tau_proto::PromptOriginator::User,
                })
            }
        },
        Err(message) => Event::ToolError(ToolError {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            tool_type: tau_proto::ToolType::Function,
            display: Some(exa_error_display(&message)),
            message,
            details: Some(invoke.arguments),
            originator: tau_proto::PromptOriginator::User,
        }),
    };
    let _ = tx.send(Frame::Event(event));
}

fn exa_ok_display(response: &str) -> ToolDisplay {
    // Count Title:/URL: lines for the result tally. Either half may be
    // missing (formatter quirks), so use the higher count to match the
    // previous CLI behaviour.
    let titles = response
        .lines()
        .filter(|line| line.starts_with("Title:"))
        .count();
    let urls = response
        .lines()
        .filter(|line| line.starts_with("URL:"))
        .count();
    let results = titles.max(urls);
    let has_response = !response.is_empty();
    ToolDisplay {
        args: String::new(),
        stats: ToolDisplayStats {
            matches: (0 < results).then_some(results as u64),
            lines: has_response.then_some(response.lines().count() as u64),
            bytes: has_response.then_some(response.len() as u64),
        },
        status: ToolDisplayStatus::Success,
        status_text: "ok".to_owned(),
        ..Default::default()
    }
}

fn exa_error_display(message: &str) -> ToolDisplay {
    let first = message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let status_text = first.to_owned();
    ToolDisplay {
        args: String::new(),
        status: ToolDisplayStatus::Error,
        status_text,
        ..Default::default()
    }
}

fn parse_args(arguments: &CborValue) -> Result<(String, u32), String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut query: Option<String> = None;
    let mut num_results: Option<u32> = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "query" => match v {
                CborValue::Text(text) => query = Some(text.clone()),
                _ => return Err("`query` must be a string".to_owned()),
            },
            "num_results" => num_results = Some(parse_num_results(v)?),
            // Forward-compat: ignore unknown keys instead of erroring.
            _ => {}
        }
    }
    let query = query.ok_or_else(|| "missing string argument: query".to_owned())?;
    if query.trim().is_empty() {
        return Err("`query` must not be empty".to_owned());
    }
    Ok((query, num_results.unwrap_or(DEFAULT_NUM_RESULTS)))
}

fn parse_num_results(value: &CborValue) -> Result<u32, String> {
    // Some providers serialize integer-valued tool arguments as JSON
    // floats (`5.0`); accept those if they're integer-valued.
    let raw: i128 = match value {
        CborValue::Integer(n) => (*n).into(),
        CborValue::Float(f) => {
            if !f.is_finite() || f.fract() != 0.0 {
                return Err("`num_results` must be an integer".to_owned());
            }
            *f as i128
        }
        _ => return Err("`num_results` must be an integer".to_owned()),
    };
    if raw < 1 {
        return Err("`num_results` must be >= 1".to_owned());
    }
    if raw > i128::from(MAX_NUM_RESULTS) {
        return Err(format!("`num_results` must be <= {MAX_NUM_RESULTS}"));
    }
    Ok(raw as u32)
}

// ---------------------------------------------------------------------------
// Default Searcher: HTTP → Exa MCP
// ---------------------------------------------------------------------------

struct HttpSearcher {
    endpoint: Mutex<String>,
    agent: ureq::Agent,
}

impl Default for HttpSearcher {
    fn default() -> Self {
        Self::new(DEFAULT_ENDPOINT.to_owned())
    }
}

impl HttpSearcher {
    fn new(endpoint: String) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();
        Self {
            endpoint: Mutex::new(endpoint),
            agent,
        }
    }
}

impl Searcher for HttpSearcher {
    fn search(&self, query: &str, num_results: u32) -> Result<String, String> {
        let endpoint = self
            .endpoint
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": REMOTE_TOOL,
                "arguments": {
                    "query": query,
                    "numResults": num_results,
                },
            },
        });
        let response = self
            .agent
            .post(&endpoint)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
            .send_string(&body.to_string())
            .map_err(|e| match e {
                ureq::Error::Status(code, resp) => {
                    let body = read_capped(resp.into_reader());
                    format!("exa MCP returned HTTP {code}: {body}")
                }
                ureq::Error::Transport(err) => format!("exa MCP transport error: {err}"),
            })?;
        let payload = response
            .into_string()
            .map_err(|e| format!("reading exa MCP response: {e}"))?;
        decode_mcp_text_result(&payload)
    }

    fn set_endpoint(&self, endpoint: String) {
        *self.endpoint.lock().unwrap_or_else(|e| e.into_inner()) = endpoint;
    }
}

/// Read up to [`ERROR_BODY_MAX_BYTES`] from `reader` into a `String`,
/// appending a marker if the body was truncated. Used to surface
/// upstream error responses without unbounded memory or log noise.
fn read_capped(reader: impl std::io::Read) -> String {
    let mut buf = Vec::new();
    let _ = reader
        .take(ERROR_BODY_MAX_BYTES as u64 + 1)
        .read_to_end(&mut buf);
    let truncated = buf.len() > ERROR_BODY_MAX_BYTES;
    if truncated {
        buf.truncate(ERROR_BODY_MAX_BYTES);
    }
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        s.push_str("… (truncated)");
    }
    s
}

/// Pull the `result.content[*].text` blob out of an MCP JSON-RPC
/// response, accepting either a bare JSON body or a `text/event-stream`
/// body whose `data:` line(s) carry the JSON. Concatenates multiple
/// text content parts with blank lines so the agent sees one prose
/// blob.
fn decode_mcp_text_result(payload: &str) -> Result<String, String> {
    let json = parse_sse_or_json(payload)?;
    if let Some(error) = json.get("error") {
        let message = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("exa MCP returned a JSON-RPC error");
        return Err(message.to_owned());
    }
    let content = json
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| "exa MCP response missing `result.content`".to_owned())?;
    let mut chunks = Vec::new();
    for part in content {
        if part.get("type").and_then(|v| v.as_str()) != Some("text") {
            continue;
        }
        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
            chunks.push(text.to_owned());
        }
    }
    if chunks.is_empty() {
        return Err("exa MCP response had no text content".to_owned());
    }
    Ok(chunks.join("\n\n"))
}

/// Parse Exa's response, which may be `application/json` or
/// `text/event-stream` (the MCP Streamable-HTTP transport). For SSE we
/// only look at `data:` lines — Exa sends a single `event: message`
/// frame, but we tolerate multiple frames by taking the first
/// well-formed one (terminated by a blank line). If the stream ends
/// without a trailing blank line, the accumulated buffer is parsed as
/// the final frame.
fn parse_sse_or_json(payload: &str) -> Result<serde_json::Value, String> {
    let trimmed = payload.trim_start();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed)
            .map_err(|e| format!("invalid JSON from exa MCP: {e}"));
    }
    let mut buf = String::new();
    for line in payload.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            buf.push_str(rest.trim_start());
            buf.push('\n');
        } else if line.is_empty() && !buf.is_empty() {
            return serde_json::from_str(buf.trim())
                .map_err(|e| format!("invalid JSON from exa MCP SSE frame: {e}"));
        }
    }
    if buf.is_empty() {
        return Err("exa MCP returned no SSE data frames".to_owned());
    }
    serde_json::from_str(buf.trim())
        .map_err(|e| format!("invalid JSON from exa MCP SSE frame: {e}"))
}

// ---------------------------------------------------------------------------
// Counting semaphore with owned permits (private).
// ---------------------------------------------------------------------------

struct Semaphore {
    state: Mutex<usize>,
    cond: Condvar,
}

struct OwnedPermit(Arc<Semaphore>);

impl Semaphore {
    fn new(permits: usize) -> Self {
        Self {
            state: Mutex::new(permits),
            cond: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> OwnedPermit {
        let mut count = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while *count == 0 {
            count = self.cond.wait(count).unwrap_or_else(|e| e.into_inner());
        }
        *count -= 1;
        OwnedPermit(Arc::clone(self))
    }
}

impl Drop for OwnedPermit {
    fn drop(&mut self) {
        let mut count = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        *count += 1;
        self.0.cond.notify_one();
    }
}

#[cfg(test)]
mod tests;
