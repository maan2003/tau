use std::io::Cursor;

use tau_proto::{Event, Frame, FrameReader, FrameWriter, Message, ToolInvoke};

use super::*;

fn invoke_restart() -> Frame {
    Frame::Event(Event::ToolInvoke(ToolInvoke {
        call_id: "call-1".into(),
        tool_name: RESTART_TEST_DUMMY_TOOL_NAME.into(),
        arguments: tau_proto::CborValue::Map(Vec::new()),
    }))
}

#[test]
fn restart_tool_can_return_error() {
    let mut input = Vec::new();
    let mut writer = FrameWriter::new(&mut input);
    writer.write_frame(&invoke_restart()).expect("write invoke");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    let mut rng = StdRng::seed_from_u64(1);
    run_with_rng(Cursor::new(input), &mut output, &mut rng).expect("run");

    let mut reader = FrameReader::new(Cursor::new(output));
    let hello = reader
        .read_frame()
        .expect("read")
        .expect("hello should exist");
    assert!(matches!(hello, Frame::Message(Message::Hello(_))));
    let subscribe = reader
        .read_frame()
        .expect("read")
        .expect("subscribe should exist");
    assert!(matches!(subscribe, Frame::Message(Message::Subscribe(_))));
    let register = reader
        .read_frame()
        .expect("read")
        .expect("register should exist");
    assert!(matches!(register, Frame::Event(Event::ToolRegister(_))));
    let ready = reader
        .read_frame()
        .expect("read")
        .expect("ready should exist");
    assert!(matches!(ready, Frame::Message(Message::Ready(_))));
    let error = reader
        .read_frame()
        .expect("read")
        .expect("error should exist");
    let Frame::Event(Event::ToolError(error)) = error else {
        panic!("expected tool error");
    };
    assert_eq!(error.message, "restarting failed");
    assert!(reader.read_frame().expect("read eof").is_none());
}

#[test]
fn restart_tool_can_exit_without_reply() {
    let mut input = Vec::new();
    let mut writer = FrameWriter::new(&mut input);
    writer.write_frame(&invoke_restart()).expect("write invoke");
    writer.flush().expect("flush");

    let mut output = Vec::new();
    let mut rng = StdRng::seed_from_u64(2);
    run_with_rng(Cursor::new(input), &mut output, &mut rng).expect("run");

    let mut reader = FrameReader::new(Cursor::new(output));
    let mut frames = Vec::new();
    while let Some(frame) = reader.read_frame().expect("read") {
        frames.push(frame);
    }
    assert_eq!(frames.len(), 4);
    assert!(matches!(frames[0], Frame::Message(Message::Hello(_))));
    assert!(matches!(frames[1], Frame::Message(Message::Subscribe(_))));
    assert!(matches!(frames[2], Frame::Event(Event::ToolRegister(_))));
    assert!(matches!(frames[3], Frame::Message(Message::Ready(_))));
}
