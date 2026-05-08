use std::thread;
use std::time::Duration;

use tau_proto::{ClientKind, Frame, Hello, Message, PROTOCOL_VERSION};
use tempfile::TempDir;

use super::*;

#[test]
fn later_attached_client_can_exchange_protocol_events_over_unix_socket() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let socket_path = tempdir.path().join("tau.sock");
    let listener = SocketListener::bind(&socket_path).expect("listener should bind");

    let client_thread = thread::spawn({
        let socket_path = socket_path.clone();
        move || {
            let mut client = SocketPeer::connect(socket_path).expect("client should connect");
            client
                .send(&Frame::Message(Message::Hello(Hello {
                    protocol_version: PROTOCOL_VERSION,
                    client_name: "client".into(),
                    client_kind: ClientKind::Ui,
                })))
                .expect("client hello should send");
            client
                .recv_timeout(Duration::from_secs(1))
                .expect("client should read response")
                .expect("response should arrive")
        }
    });

    let mut server = listener.accept().expect("server should accept client");
    let hello = server
        .recv_timeout(Duration::from_secs(1))
        .expect("server should read hello")
        .expect("hello should arrive");
    assert_eq!(
        hello,
        Frame::Message(Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "client".into(),
            client_kind: ClientKind::Ui,
        }))
    );
    server
        .send(&Frame::Message(Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "server".into(),
            client_kind: ClientKind::Core,
        })))
        .expect("server hello should send");

    let response = client_thread.join().expect("client thread should finish");
    assert_eq!(
        response,
        Frame::Message(Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "server".into(),
            client_kind: ClientKind::Core,
        }))
    );
}
