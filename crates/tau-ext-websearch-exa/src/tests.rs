use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::thread;

use tau_proto::ToolInvoke;

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

fn spawn_extension(
    searcher: Arc<dyn Searcher>,
) -> (
    EventReader<BufReader<UnixStream>>,
    EventWriter<BufWriter<UnixStream>>,
) {
    let (ext_stream, harness_stream) = UnixStream::pair().expect("pair");
    let reader_stream = ext_stream.try_clone().expect("clone");
    thread::spawn(move || {
        run_with_searcher(reader_stream, ext_stream, searcher).expect("run");
    });
    (
        EventReader::new(BufReader::new(
            harness_stream.try_clone().expect("harness clone"),
        )),
        EventWriter::new(BufWriter::new(harness_stream)),
    )
}

fn drain_startup(reader: &mut EventReader<BufReader<UnixStream>>) {
    // The hello/subscribe/ready messages are filtered out by the
    // test-side `EventReader` wrapper; only the tool register survives.
    let event = reader.read_event().expect("read").expect("register");
    let Event::ToolRegister(register) = event else {
        panic!("expected ToolRegister, got {event:?}");
    };
    assert_eq!(register.tool.name.as_str(), TOOL_NAME);
    assert_eq!(
        register
            .tool
            .model_visible_name
            .as_ref()
            .map(|name| name.as_str()),
        Some(MODEL_VISIBLE_TOOL_NAME)
    );
}

#[test]
fn forwards_query_and_num_results_to_searcher_and_returns_text() {
    let searcher = StubSearcher::ok("Title: hi\nURL: https://x\n");
    let (mut reader, mut writer) = spawn_extension(searcher.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new(TOOL_NAME),
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
    assert_eq!(result.tool_name.as_str(), TOOL_NAME);
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
    let (mut reader, mut writer) = spawn_extension(searcher.clone());
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-2".into(),
            tool_name: tau_proto::ToolName::new(TOOL_NAME),
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
    let (mut reader, mut writer) = spawn_extension(searcher);
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-3".into(),
            tool_name: tau_proto::ToolName::new(TOOL_NAME),
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
    let (mut reader, mut writer) = spawn_extension(searcher);
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-4".into(),
            tool_name: tau_proto::ToolName::new(TOOL_NAME),
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
    let (mut reader, mut writer) = spawn_extension(searcher);
    drain_startup(&mut reader);

    writer
        .write_event(&Event::ToolInvoke(ToolInvoke {
            call_id: "call-5".into(),
            tool_name: tau_proto::ToolName::new(TOOL_NAME),
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

// ---- Wire decoding ----

#[test]
fn decodes_sse_message_frame() {
    let body = "event: message\n\
                data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n\
                \n";
    let text = decode_mcp_text_result(body).expect("decode");
    assert_eq!(text, "hello");
}

#[test]
fn concatenates_multiple_text_content_parts() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"first"},{"type":"text","text":"second"}]}}"#;
    let text = decode_mcp_text_result(body).expect("decode");
    assert_eq!(text, "first\n\nsecond");
}

#[test]
fn surfaces_jsonrpc_error_message() {
    let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad params"}}"#;
    let err = decode_mcp_text_result(body).expect_err("should fail");
    assert!(err.contains("bad params"), "err: {err}");
}

#[test]
fn fails_when_response_has_no_text_content() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"image","data":""}]}}"#;
    let err = decode_mcp_text_result(body).expect_err("should fail");
    assert!(err.contains("no text content"), "err: {err}");
}

#[test]
fn first_wellformed_sse_frame_wins() {
    // Two complete `message` frames, blank-line-terminated. The
    // documented contract is "take the first well-formed one".
    let body = "event: message\n\
                data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"first\"}]}}\n\
                \n\
                event: message\n\
                data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"second\"}]}}\n\
                \n";
    let text = decode_mcp_text_result(body).expect("decode");
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
