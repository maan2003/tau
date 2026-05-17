//! Persistent-WebSocket transport for the Codex Responses API.
//!
//! The provider owns a small pool of these connections, keyed by
//! `(base_url, account_id, session_id)`, so the connection-local
//! `previous_response_id` cache stays warm across turns of the same
//! conversation. The pool itself lives in [`super::pool`]; this
//! module handles a single connection's lifecycle and one-turn
//! streaming.
//!
//! Wire shape:
//! - Upgrade `wss://{base_url}/codex/responses` (same path as the HTTP+SSE
//!   endpoint, just `wss://`) with `Authorization`, `chatgpt-account-id`, and
//!   the dated `OpenAI-Beta: responses_websockets=2026-02-06` header.
//! - Send one client text frame per turn: a `{ "type": "response.create", ...
//!   }` envelope produced by [`super::build_ws_envelope`].
//! - Read server text frames as one decoded `response.*` event each and hand
//!   them to [`super::apply_event`]. Same event shape as SSE (the WS guide is
//!   explicit on this).
//! - On `response.completed`/`response.done` the connection stays open and idle
//!   for the next turn.
//!
//! Threading model: each connection has two tokio tasks behind the
//! scenes — a reader looping on `stream.next()` and a writer
//! draining an outbound channel + driving a periodic client-side
//! ping. The pings keep the upstream's keepalive timer happy
//! (default 25 s; the live Codex server reaps with a 1011
//! "keepalive ping timeout" close when no client pong has been seen
//! recently). The sync [`WsConn`] type holds the channel handles —
//! `run_turn` is sync, owned by the provider's main loop, and just
//! marshals envelopes to the writer task and pulls events back from
//! the reader.

use std::time::{Duration, Instant};

use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::AbortHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite};

use super::{
    ResponsesConfig, apply_event, build_ws_envelope, build_ws_prewarm_envelope,
    ws_chain_fingerprint,
};
use crate::common::{LlmError, PreviousResponse, PromptPayload, StreamState};
use crate::responses::ws_runtime;

/// Beta-feature header value the OpenAI WebSocket endpoint expects.
/// Dated by the server; will need a bump when OpenAI rolls a new
/// release. Pinned here as a single `const` so that bump is a
/// one-line change.
pub(crate) const OPENAI_BETA_WS: &str = "responses_websockets=2026-02-06";

/// How often the writer task sends an unsolicited client `Ping`.
///
/// The live Codex server reaps idle connections with a 1011
/// "keepalive ping timeout" close when its own ping cycle goes
/// pong-less. The empirical window between turn N completing and a
/// reap-triggered drop on turn N+1 is ~90 s (one ping interval plus
/// two missed-pong slots). 25 s keeps us comfortably inside that
/// budget *and* under common LB / NAT idle timeouts (60 s default
/// on AWS ALB, nginx, etc.) which would otherwise hang up the TCP
/// connection mid-idle.
///
/// Doubles as a flush trigger: `tungstenite` queues an outgoing
/// `Pong` whenever the reader half processes a server `Ping`, but
/// the queued bytes only leave the wire on the next sink write.
/// Periodic client pings ensure pongs don't sit buffered for a full
/// turn boundary.
const KEEPALIVE_PING_INTERVAL: Duration = Duration::from_secs(25);

type SharedStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Sink = SplitSink<SharedStream, Message>;
type Stream = SplitStream<SharedStream>;

/// Commands sent from sync land into a connection's writer task.
enum WsCommand {
    /// Send a text frame on the wire — used for `response.create`
    /// turn envelopes.
    SendText(String),
}

/// Events surfaced from the reader task to sync land.
///
/// The reader pre-parses text frames as JSON before forwarding so
/// the sync caller doesn't do JSON work on the runtime side, and so
/// unparseable frames can be quietly dropped at the source (same
/// behavior as the SSE line-level resync).
enum InboundEvent {
    /// One parsed `response.*` event.
    Event(serde_json::Value),
    /// Server sent a `Close` frame (or the stream ended cleanly
    /// without one). The string is the close-frame reason for
    /// logging.
    Closed(String),
    /// Transport / protocol error mid-stream.
    Error(String),
}

/// One live WS connection to a Responses endpoint, as seen from the
/// provider's sync main loop.
///
/// The actual socket and `tungstenite` state machine live in two
/// tokio tasks (reader + writer) spawned at [`Self::connect`] time.
/// This struct just holds the channel ends and the abort handles —
/// `run_turn` is a thin sync wrapper that pushes the envelope onto
/// the outbound channel and pulls events off the inbound one.
pub(crate) struct WsConn {
    outbound_tx: UnboundedSender<WsCommand>,
    inbound_rx: UnboundedReceiver<InboundEvent>,
    /// Aborted on [`Drop`] so a `WsConn` falling out of scope cleanly
    /// tears down its background tasks. Cooperative cancellation via
    /// channel close would also work but adds latency on the path
    /// where we already know we want both tasks gone (the
    /// `is_recoverable_ws_error` retry path drops the conn
    /// immediately).
    reader_abort: AbortHandle,
    writer_abort: AbortHandle,
    /// Wall-clock time of the upgrade. Used by the pool to retire
    /// connections before the server's 60-minute hard cap fires
    /// mid-turn.
    pub opened_at: Instant,
    /// Bearer token the upgrade was authenticated with. The pool
    /// compares against the current resolved token on checkout — a
    /// mismatch means OAuth refreshed and this socket's auth is
    /// stale, so it gets dropped and reopened.
    pub bearer: String,
    /// Response id returned by a non-generating prewarm on this
    /// socket. The next matching real turn can send only the input
    /// delta with `previous_response_id` pointing here, which is how
    /// Codex's own client makes request prewarm useful.
    prewarm_anchor: Option<PrewarmAnchor>,
}

struct PrewarmAnchor {
    response_id: String,
    item_count: usize,
    context_items: Vec<tau_proto::ContextItem>,
    fingerprint: String,
}

impl PrewarmAnchor {
    fn matches(
        &self,
        config: &ResponsesConfig,
        request: &PromptPayload<'_>,
    ) -> Result<bool, LlmError> {
        if request.context_items.len() < self.context_items.len() {
            return Ok(false);
        }
        if request.context_items[..self.context_items.len()] != self.context_items {
            return Ok(false);
        }
        Ok(self.fingerprint == ws_chain_fingerprint(config, request)?)
    }
}

impl WsConn {
    /// Open a fresh connection and perform the WS upgrade. Spawns
    /// the reader and writer tasks on the shared runtime so the
    /// connection is immediately ready to serve a turn — and
    /// already auto-pongs any server-initiated `Ping` even before
    /// the first `run_turn` call.
    ///
    /// Errors:
    /// - `LlmError::HttpStatus(426, _)` — server rejected the upgrade (sticky
    ///   fallback to HTTP+SSE).
    /// - `LlmError::HttpStatus(0, "stream error: ...")` — transient transport
    ///   hiccup, retryable.
    /// - Other 4xx — surface as-is.
    pub fn connect(config: &ResponsesConfig) -> Result<Self, LlmError> {
        let request = build_request(config)?;
        let bearer = config.api_key.clone();
        let runtime = ws_runtime::handle();
        let (ws, _response) = runtime
            .block_on(async { tokio_tungstenite::connect_async(request).await })
            .map_err(map_ws_connect_error)?;

        let (sink, stream) = ws.split();
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        // Both tasks share the inbound channel: the reader's
        // primary job is feeding events, but the writer also
        // surfaces send-side failures there so `run_turn` never
        // hangs on `blocking_recv` after a half-open socket
        // (write fails, read still pending). The writer signals
        // first when writes break; the reader's eventual close
        // event just stacks behind it in the buffer.
        let reader_abort = runtime
            .spawn(read_loop(stream, inbound_tx.clone()))
            .abort_handle();
        let writer_abort = runtime
            .spawn(write_loop(
                sink,
                outbound_rx,
                inbound_tx,
                KEEPALIVE_PING_INTERVAL,
            ))
            .abort_handle();

        Ok(Self {
            outbound_tx,
            inbound_rx,
            reader_abort,
            writer_abort,
            opened_at: Instant::now(),
            bearer,
            prewarm_anchor: None,
        })
    }

    /// Send one `response.create` envelope and stream events back
    /// until `response.completed` / `response.done`. Returns the
    /// accumulated [`StreamState`]; leaves the socket open for the
    /// next turn.
    ///
    /// Mid-stream WS close or IO error surfaces as a retryable
    /// `LlmError` (code 0, body prefixed with `"stream error:"`) so
    /// the provider's outer retry loop reopens on the next attempt.
    pub fn run_turn(
        &mut self,
        config: &ResponsesConfig,
        session_prompt_id: &str,
        request: &PromptPayload<'_>,
        on_update: &mut impl FnMut(&str, Option<&str>),
    ) -> Result<StreamState, LlmError> {
        let envelope = {
            let owned_previous;
            let chained_request;
            let request = if request.previous_response.is_none() {
                match self.prewarm_anchor.as_ref() {
                    Some(anchor) if anchor.matches(config, request)? => {
                        owned_previous = PreviousResponse {
                            id: &anchor.response_id,
                            next_item_index: anchor.item_count,
                            transport: Some(tau_proto::AgentBackendTransport::Websocket),
                        };
                        chained_request = PromptPayload {
                            previous_response: Some(owned_previous),
                            system_prompt: request.system_prompt,
                            context_items: request.context_items,
                            tools: request.tools,
                            params: request.params,
                            tool_choice: request.tool_choice,
                            originator: request.originator,
                            session_id: request.session_id,
                            share_user_cache_key: request.share_user_cache_key,
                        };
                        &chained_request
                    }
                    _ => request,
                }
            } else {
                request
            };
            super::maybe_debug_write_provider_request(
                session_prompt_id,
                config,
                request,
                tau_proto::AgentBackendTransport::Websocket,
            );
            build_ws_envelope(config, request)
        };
        let state = self.run_envelope(envelope, on_update)?;
        self.prewarm_anchor = None;
        Ok(state)
    }

    /// Send one non-generating prewarm envelope and wait for the
    /// provider to finish accepting it. No Tau-visible response
    /// events are emitted by this layer's caller.
    pub fn run_prewarm(
        &mut self,
        config: &ResponsesConfig,
        request: &PromptPayload<'_>,
    ) -> Result<StreamState, LlmError> {
        let envelope = build_ws_prewarm_envelope(config, request);
        let fingerprint = ws_chain_fingerprint(config, request)?;
        let state = self.run_envelope(envelope, &mut |_, _| {})?;
        self.prewarm_anchor = state.response_id.as_ref().map(|response_id| PrewarmAnchor {
            response_id: response_id.clone(),
            item_count: request.context_items.len(),
            context_items: request.context_items.to_vec(),
            fingerprint,
        });
        Ok(state)
    }

    fn run_envelope(
        &mut self,
        envelope: super::WsResponseCreate,
        on_update: &mut impl FnMut(&str, Option<&str>),
    ) -> Result<StreamState, LlmError> {
        let text = serde_json::to_string(&envelope).map_err(LlmError::Json)?;
        self.outbound_tx
            .send(WsCommand::SendText(text))
            .map_err(|_| LlmError::HttpStatus(0, "stream error: ws writer task gone".to_owned()))?;

        let mut state = StreamState::new();
        loop {
            let Some(event) = self.inbound_rx.blocking_recv() else {
                return Err(LlmError::HttpStatus(
                    0,
                    "stream error: ws reader task gone".to_owned(),
                ));
            };
            match event {
                InboundEvent::Event(value) => {
                    if apply_event(&mut state, &value, on_update)? {
                        break;
                    }
                }
                InboundEvent::Closed(reason) => {
                    return Err(LlmError::HttpStatus(
                        0,
                        format!("stream error: ws closed mid-stream ({reason})"),
                    ));
                }
                InboundEvent::Error(msg) => {
                    return Err(LlmError::HttpStatus(0, format!("stream error: {msg}")));
                }
            }
        }
        Ok(state)
    }
}

impl Drop for WsConn {
    fn drop(&mut self) {
        // Stops the two background tasks at the next await point —
        // the runtime then closes the underlying socket as a side
        // effect of dropping its owned `WebSocketStream` halves.
        self.reader_abort.abort();
        self.writer_abort.abort();
    }
}

/// Build the client `Request` for the WS upgrade — URL + bearer +
/// Codex-specific headers.
fn build_request(config: &ResponsesConfig) -> Result<Request, LlmError> {
    let url = build_ws_url(&config.base_url)?;
    let mut request: Request = url
        .as_str()
        .into_client_request()
        .map_err(|e| LlmError::HttpStatus(0, format!("ws request build: {e}")))?;
    set_header(
        request.headers_mut(),
        "Authorization",
        &format!("Bearer {}", config.api_key),
    )?;
    set_header(request.headers_mut(), "OpenAI-Beta", OPENAI_BETA_WS)?;
    if let Some(account_id) = config.account_id.as_deref() {
        set_header(request.headers_mut(), "chatgpt-account-id", account_id)?;
    }
    Ok(request)
}

/// Reader task. Pumps server frames into the inbound channel until
/// the stream ends, the channel receiver is dropped (WsConn went
/// away), or the task is aborted on Drop. Auto-pongs are handled
/// transparently inside `tungstenite`'s state machine — they're
/// buffered on the sink half and flushed by the writer task's next
/// send (the periodic ping in the steady state).
async fn read_loop(mut stream: Stream, tx: UnboundedSender<InboundEvent>) {
    while let Some(item) = stream.next().await {
        let (event, terminal) = match item {
            Ok(Message::Text(text)) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(value) => (InboundEvent::Event(value), false),
                // Unparseable frames are skipped on the SSE path too
                // (line-level resync). Mirror it here.
                Err(_) => continue,
            },
            Ok(Message::Close(frame)) => {
                let reason = frame
                    .as_ref()
                    .map(|f| format!("code={} reason={}", f.code, f.reason))
                    .unwrap_or_else(|| "no close frame".to_owned());
                tracing::info!(
                    target: crate::LOG_TARGET,
                    %reason,
                    "ws server sent close frame — connection will be reopened on next turn",
                );
                (InboundEvent::Closed(reason), true)
            }
            Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {
                // Codex never sends binary. Ping/Pong are protocol
                // control frames — tungstenite surfaces them after
                // auto-handling, no caller action needed.
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    target: crate::LOG_TARGET,
                    error = %e,
                    "ws read failed — connection will be reopened on next turn",
                );
                (InboundEvent::Error(format!("{e}")), true)
            }
        };
        if tx.send(event).is_err() {
            // Receiver dropped — WsConn went away mid-stream. We're
            // done.
            return;
        }
        if terminal {
            return;
        }
    }
    // Stream ended without a close frame (clean EOF). Surface it as
    // a `Closed` so the next `run_turn` call returns a retryable
    // error rather than hanging on `blocking_recv`.
    let _ = tx.send(InboundEvent::Closed("stream ended".to_owned()));
}

/// Writer task. Drains outbound commands and emits periodic client
/// pings to keep the upstream's keepalive timer happy. Exits when
/// the command channel is closed (WsConn was dropped) or when the
/// sink errors (server hung up mid-write); on the latter, signals
/// the failure through `inbound_tx` so a sync `run_turn` blocked
/// on `blocking_recv` wakes immediately rather than waiting on the
/// reader to independently notice the close (which it might miss
/// entirely on a half-open socket).
async fn write_loop(
    mut sink: Sink,
    mut rx: UnboundedReceiver<WsCommand>,
    inbound_tx: UnboundedSender<InboundEvent>,
    ping_interval: Duration,
) {
    let mut ticker = tokio::time::interval(ping_interval);
    // First tick fires immediately by default — skip it. Pinging
    // right after a freshly-completed upgrade burns RTT for no
    // benefit; the upstream's timer just reset.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(WsCommand::SendText(text)) => {
                    if let Err(e) = sink.send(Message::Text(text.into())).await {
                        let _ = inbound_tx.send(InboundEvent::Error(format!("ws send failed: {e}")));
                        return;
                    }
                }
                // Command channel closed — WsConn was dropped.
                // Close the sink gracefully so the server gets a
                // proper close frame instead of a torn TCP socket.
                // No `inbound_tx` signal: the receiver was dropped
                // alongside the sender (both live on `WsConn`).
                None => {
                    let _ = sink.close().await;
                    return;
                }
            },
            _ = ticker.tick() => {
                match sink.send(Message::Ping(Vec::new().into())).await {
                    Ok(()) => {
                        // Pings are 25 s apart — info isn't spammy at
                        // that cadence, and a session log that suddenly
                        // *stops* showing them is the clearest signal
                        // that the writer task is stuck (and that the
                        // upstream's reap timer is therefore counting
                        // down toward a 1011 close). When we're confident
                        // the keepalive path is solid, demote to debug.
                        tracing::info!(
                            target: crate::LOG_TARGET,
                            "ws keepalive ping sent",
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: crate::LOG_TARGET,
                            error = %e,
                            "ws keepalive ping failed — writer task exiting, next turn will reopen",
                        );
                        let _ = inbound_tx.send(InboundEvent::Error(format!("ws keepalive failed: {e}")));
                        return;
                    }
                }
            }
        }
    }
}

/// Map the configured HTTP base URL to a `ws://` / `wss://` URL
/// pointing at the WebSocket endpoint. The Codex backend lives at
/// the same path as the HTTP+SSE endpoint (`/codex/responses`) — the
/// only delta is the scheme.
fn build_ws_url(base_url: &str) -> Result<String, LlmError> {
    let base = base_url.trim_end_matches('/');
    let rest = if let Some(rest) = base.strip_prefix("https://") {
        return Ok(format!("wss://{rest}/codex/responses"));
    } else if let Some(rest) = base.strip_prefix("http://") {
        rest
    } else {
        return Err(LlmError::HttpStatus(
            0,
            format!("ws scheme unsupported in base_url: {base_url}"),
        ));
    };
    Ok(format!("ws://{rest}/codex/responses"))
}

fn set_header(
    headers: &mut tungstenite::http::HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), LlmError> {
    let header_value = value
        .parse()
        .map_err(|e| LlmError::HttpStatus(0, format!("ws header {name}: {e}")))?;
    headers.insert(name, header_value);
    Ok(())
}

fn map_ws_connect_error(e: tungstenite::Error) -> LlmError {
    if let tungstenite::Error::Http(response) = &e {
        let code = response.status().as_u16();
        let body = response
            .body()
            .as_ref()
            .and_then(|b| std::str::from_utf8(b).ok())
            .map(str::to_owned)
            .unwrap_or_default();
        return LlmError::HttpStatus(code, body);
    }
    // Network / TLS / protocol — treat as retryable transport.
    LlmError::HttpStatus(0, format!("stream error: ws connect: {e}"))
}
