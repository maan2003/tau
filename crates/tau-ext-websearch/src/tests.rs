use std::io::{BufRead, BufReader as IoBufReader, Read as _};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::thread;

use tau_proto::ToolStarted;

use super::*;

/// Test-side wrapper around [`FrameReader`] that exposes an `Event`-flavoured
/// API (peels `LogEvent`, drops other messages).
struct EventReader<R> {
    inner: FrameReader<R>,
}

impl<R: std::io::Read> EventReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner: FrameReader::new(inner),
        }
    }

    fn read_event(&mut self) -> Result<Option<Event>, tau_proto::DecodeError> {
        loop {
            match self.inner.read_frame()? {
                None => return Ok(None),
                Some(frame) => match frame.peel_log().1 {
                    Frame::Event(event) => return Ok(Some(event)),
                    Frame::Message(_) => continue,
                },
            }
        }
    }
}

/// Test-side wrapper around [`FrameWriter`] that accepts `Event` directly.
struct EventWriter<W> {
    inner: FrameWriter<W>,
}

impl<W: std::io::Write> EventWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner: FrameWriter::new(inner),
        }
    }

    fn write_event(&mut self, event: &Event) -> Result<(), tau_proto::EncodeError> {
        self.inner.write_frame(&Frame::Event(event.clone()))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct StubSearcher {
    calls: Mutex<Vec<(String, u32)>>,
    response: Mutex<Result<String, String>>,
}

impl StubSearcher {
    fn ok(text: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            response: Mutex::new(Ok(text.into())),
        })
    }

    fn err(message: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            response: Mutex::new(Err(message.into())),
        })
    }
}

impl Searcher for StubSearcher {
    fn search(&self, query: &str, num_results: u32) -> Result<String, String> {
        self.calls
            .lock()
            .expect("lock")
            .push((query.to_owned(), num_results));
        self.response.lock().expect("lock").clone()
    }
}

struct StubParallelClient {
    calls: Mutex<Vec<(String, serde_json::Value)>>,
    response: Mutex<Result<String, String>>,
}

impl StubParallelClient {
    fn ok(text: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            response: Mutex::new(Ok(text.into())),
        })
    }
}

impl ParallelClient for StubParallelClient {
    fn call(&self, remote_tool: &str, arguments: serde_json::Value) -> Result<String, String> {
        self.calls
            .lock()
            .expect("lock")
            .push((remote_tool.to_owned(), arguments));
        self.response.lock().expect("lock").clone()
    }
}

fn spawn_extension(
    searcher: Arc<dyn Searcher>,
    parallel_client: Arc<dyn ParallelClient>,
) -> (
    EventReader<BufReader<UnixStream>>,
    EventWriter<BufWriter<UnixStream>>,
) {
    let (ext_stream, harness_stream) = UnixStream::pair().expect("pair");
    let reader_stream = ext_stream.try_clone().expect("clone");
    thread::spawn(move || {
        run_with_clients(reader_stream, ext_stream, searcher, parallel_client).expect("run");
    });
    (
        EventReader::new(BufReader::new(
            harness_stream.try_clone().expect("harness clone"),
        )),
        EventWriter::new(BufWriter::new(harness_stream)),
    )
}

fn spawn_with_searcher(
    searcher: Arc<dyn Searcher>,
) -> (
    EventReader<BufReader<UnixStream>>,
    EventWriter<BufWriter<UnixStream>>,
) {
    spawn_extension(searcher, StubParallelClient::ok("unused"))
}

fn drain_startup(reader: &mut EventReader<BufReader<UnixStream>>) -> Vec<ToolSpec> {
    // Startup registers Exa plus Parallel search/fetch. Parallel tools are
    // disabled by default so roles can opt into them without duplicating the
    // model-visible `web_search` provided by Exa.
    let mut tools = Vec::new();
    while tools.len() < 3 {
        let event = reader.read_event().expect("read").expect("register");
        let Event::ToolRegister(register) = event else {
            panic!("expected ToolRegister, got {event:?}");
        };
        tools.push(register.tool);
    }
    tools
}

#[test]
fn registers_exa_by_default_and_parallel_tools_disabled() {
    let searcher = StubSearcher::ok("unused");
    let parallel = StubParallelClient::ok("unused");
    let (mut reader, _writer) = spawn_extension(searcher, parallel);

    let tools = drain_startup(&mut reader);
    assert_eq!(tools[0].name.as_str(), EXA_TOOL_NAME);
    assert_eq!(
        tools[0]
            .model_visible_name
            .as_ref()
            .map(|name| name.as_str()),
        Some(MODEL_VISIBLE_SEARCH_TOOL_NAME)
    );
    assert_eq!(
        tools[0]
            .parameters
            .as_ref()
            .and_then(|parameters| parameters.get("additionalProperties")),
        Some(&serde_json::Value::Bool(false))
    );
    assert!(tools[0].enabled_by_default);

    assert_eq!(tools[1].name.as_str(), PARALLEL_SEARCH_TOOL_NAME);
    assert_eq!(
        tools[1]
            .model_visible_name
            .as_ref()
            .map(|name| name.as_str()),
        Some(MODEL_VISIBLE_SEARCH_TOOL_NAME)
    );
    assert!(!tools[1].enabled_by_default);

    assert_eq!(tools[2].name.as_str(), PARALLEL_FETCH_TOOL_NAME);
    assert_eq!(
        tools[2]
            .model_visible_name
            .as_ref()
            .map(|name| name.as_str()),
        Some(MODEL_VISIBLE_FETCH_TOOL_NAME)
    );
    assert!(!tools[2].enabled_by_default);
}

#[test]
fn forwards_query_and_num_results_to_exa_searcher_and_returns_text() {
    let searcher = StubSearcher::ok("Title: hi\nURL: https://x\n");
    let (mut reader, mut writer) = spawn_with_searcher(searcher.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(EXA_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Text("rust async runtime comparison".to_owned()),
                ),
                (
                    CborValue::Text("num_results".to_owned()),
                    CborValue::Integer(3.into()),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolResult(result) = event else {
        panic!("expected ToolResult, got {event:?}");
    };
    assert_eq!(result.call_id.as_str(), "call-1");
    assert_eq!(result.tool_name.as_str(), EXA_TOOL_NAME);
    let CborValue::Text(text) = result.result else {
        panic!("expected Text result");
    };
    assert!(text.contains("Title: hi"));
    let display = result.display.expect("display");
    assert!(display.info_chips.is_empty());
    assert_eq!(display.stats.matches, Some(1));
    assert_eq!(display.stats.lines, Some(2));
    assert_eq!(display.stats.bytes, Some(25));

    let calls = searcher.calls.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "rust async runtime comparison");
    assert_eq!(calls[0].1, 3);
}

#[test]
fn defaults_num_results_when_omitted() {
    let searcher = StubSearcher::ok("ok");
    let (mut reader, mut writer) = spawn_with_searcher(searcher.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-2".into(),
            tool_name: tau_proto::ToolName::new(EXA_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("query".to_owned()),
                CborValue::Text("hello world".to_owned()),
            )]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    assert!(matches!(event, Event::ToolResult(_)));
    assert_eq!(
        searcher.calls.lock().expect("lock")[0].1,
        DEFAULT_NUM_RESULTS,
    );
}

#[test]
fn missing_query_returns_tool_error() {
    let searcher = StubSearcher::ok("unused");
    let (mut reader, mut writer) = spawn_with_searcher(searcher);
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-3".into(),
            tool_name: tau_proto::ToolName::new(EXA_TOOL_NAME),
            arguments: CborValue::Map(Vec::new()),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        panic!("expected ToolError, got {event:?}");
    };
    assert!(err.message.contains("query"), "message: {}", err.message);
}

#[test]
fn searcher_error_surfaces_as_tool_error() {
    let searcher = StubSearcher::err("upstream timed out");
    let (mut reader, mut writer) = spawn_with_searcher(searcher);
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-4".into(),
            tool_name: tau_proto::ToolName::new(EXA_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("query".to_owned()),
                CborValue::Text("anything".to_owned()),
            )]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        panic!("expected ToolError, got {event:?}");
    };
    assert_eq!(err.message, "upstream timed out");
}

#[test]
fn rejects_num_results_out_of_range() {
    let searcher = StubSearcher::ok("unused");
    let (mut reader, mut writer) = spawn_with_searcher(searcher);
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-5".into(),
            tool_name: tau_proto::ToolName::new(EXA_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Text("anything".to_owned()),
                ),
                (
                    CborValue::Text("num_results".to_owned()),
                    CborValue::Integer(0.into()),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        panic!("expected ToolError, got {event:?}");
    };
    assert!(err.message.contains(">= 1"), "message: {}", err.message);
}

#[test]
fn forwards_parallel_search_to_web_search_and_returns_text() {
    let searcher = StubSearcher::ok("unused");
    let parallel = StubParallelClient::ok("search result");
    let (mut reader, mut writer) = spawn_extension(searcher, parallel.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-6".into(),
            tool_name: tau_proto::ToolName::new(PARALLEL_SEARCH_TOOL_NAME),
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Text("latest rust release".to_owned()),
                ),
                (
                    CborValue::Text("max_results".to_owned()),
                    CborValue::Integer(3.into()),
                ),
            ]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolResult(result) = event else {
        panic!("expected ToolResult, got {event:?}");
    };
    assert_eq!(result.call_id.as_str(), "call-6");
    assert_eq!(result.tool_name.as_str(), PARALLEL_SEARCH_TOOL_NAME);
    assert_eq!(result.result, CborValue::Text("search result".to_owned()));

    let calls = parallel.calls.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, PARALLEL_REMOTE_SEARCH_TOOL);
    assert_eq!(calls[0].1["query"], "latest rust release");
    assert_eq!(calls[0].1["max_results"], 3);
}

#[test]
fn forwards_parallel_fetch_to_web_fetch() {
    let searcher = StubSearcher::ok("unused");
    let parallel = StubParallelClient::ok("page text");
    let (mut reader, mut writer) = spawn_extension(searcher, parallel.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-7".into(),
            tool_name: tau_proto::ToolName::new(PARALLEL_FETCH_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Text("url".to_owned()),
                CborValue::Text("https://example.com".to_owned()),
            )]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    assert!(matches!(event, Event::ToolResult(_)));

    let calls = parallel.calls.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, PARALLEL_REMOTE_FETCH_TOOL);
    assert_eq!(calls[0].1["url"], "https://example.com");
}

#[test]
fn parallel_non_string_argument_keys_are_rejected_before_forwarding() {
    let searcher = StubSearcher::ok("unused");
    let parallel = StubParallelClient::ok("unused");
    let (mut reader, mut writer) = spawn_extension(searcher, parallel.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolStarted(ToolStarted {
            call_id: "call-8".into(),
            tool_name: tau_proto::ToolName::new(PARALLEL_SEARCH_TOOL_NAME),
            arguments: CborValue::Map(vec![(
                CborValue::Integer(1.into()),
                CborValue::Text("anything".to_owned()),
            )]),
            originator: tau_proto::PromptOriginator::User,
        }))
        .expect("write");
    writer.flush().expect("flush");

    let event = reader.read_event().expect("read").expect("event");
    let Event::ToolError(err) = event else {
        panic!("expected ToolError, got {event:?}");
    };
    assert!(
        err.message.contains("keys must be strings"),
        "message: {}",
        err.message
    );
    assert!(parallel.calls.lock().expect("lock").is_empty());
}

// ---- Wire decoding ----

#[test]
fn decodes_sse_message_frame() {
    let body = "event: message\n\
                data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n\
                \n";
    let text = decode_mcp_text_result(body, "exa").expect("decode");
    assert_eq!(text, "hello");
}

#[test]
fn concatenates_multiple_text_content_parts() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"first"},{"type":"text","text":"second"}]}}"#;
    let text = decode_mcp_text_result(body, "exa").expect("decode");
    assert_eq!(text, "first\n\nsecond");
}

#[test]
fn surfaces_jsonrpc_error_message() {
    let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad params"}}"#;
    let err = decode_mcp_text_result(body, "exa").expect_err("should fail");
    assert!(err.contains("bad params"), "err: {err}");
}

#[test]
fn fails_when_response_has_no_text_content() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"image","data":""}]}}"#;
    let err = decode_mcp_text_result(body, "exa").expect_err("should fail");
    assert!(err.contains("no text content"), "err: {err}");
}

#[test]
fn first_wellformed_sse_frame_wins() {
    // Two complete `message` frames, blank-line-terminated. The documented
    // contract is "take the first well-formed one".
    let body = "event: message\n\
                data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"first\"}]}}\n\
                \n\
                event: message\n\
                data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"second\"}]}}\n\
                \n";
    let text = decode_mcp_text_result(body, "parallel").expect("decode");
    assert_eq!(text, "first");
}

#[test]
fn parse_num_results_accepts_integer_valued_float() {
    let v = parse_num_results(&CborValue::Float(3.0)).expect("ok");
    assert_eq!(v, 3);
}

#[test]
fn parse_num_results_rejects_non_integer_float() {
    let err = parse_num_results(&CborValue::Float(3.5)).expect_err("should fail");
    assert!(err.contains("integer"), "err: {err}");
}

#[test]
fn parallel_config_rejects_api_key_field() {
    // Parallel runs against the unauthenticated Search MCP endpoint; keeping
    // `deny_unknown_fields` catches stale configs that try to pass credentials.
    let err = serde_json::from_value::<ExtConfig>(serde_json::json!({
        "api_key": "secret"
    }))
    .expect_err("api_key is not a supported Parallel config field");
    assert!(err.to_string().contains("api_key"), "err: {err}");
}

#[test]
fn parallel_http_client_posts_tools_call_without_authorization_header() {
    // Regression coverage for the Parallel.ai integration: the first-party
    // extension intentionally uses the default unauthenticated MCP endpoint and
    // must not invent API-key config or send Authorization headers.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = IoBufReader::new(stream.try_clone().expect("clone"));
        let mut headers = Vec::new();
        let mut content_len = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read line");
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                content_len = value.trim().parse().expect("content length");
            }
            headers.push(line);
        }
        let mut body = vec![0; content_len];
        reader.read_exact(&mut body).expect("body");
        let response_body =
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"ok"}]}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        std::io::Write::write_all(&mut &stream, response.as_bytes()).expect("write");
        (headers, String::from_utf8(body).expect("utf8"))
    });

    let client = HttpParallelClient::new(endpoint);
    let text = client
        .call(
            PARALLEL_REMOTE_SEARCH_TOOL,
            serde_json::json!({ "query": "rust" }),
        )
        .expect("call");
    assert_eq!(text, "ok");

    let (headers, body) = server.join().expect("join");
    assert!(
        !headers
            .iter()
            .any(|h| h.to_ascii_lowercase().starts_with("authorization:")),
        "headers: {headers:?}"
    );
    assert!(
        headers
            .iter()
            .any(|h| h.eq_ignore_ascii_case("MCP-Protocol-Version: 2025-06-18\r\n")),
        "headers: {headers:?}"
    );
    let body: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(body["method"], "tools/call");
    assert_eq!(body["params"]["name"], PARALLEL_REMOTE_SEARCH_TOOL);
    assert_eq!(body["params"]["arguments"]["query"], "rust");
}
