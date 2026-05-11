//! Internal harness event type and the per-connection reader/writer threads
//! that funnel decoded protocol events into the central event loop.

use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use tau_core::{ConnectionSendError, ConnectionSink};
use tau_proto::{Disconnect, Frame, FrameReader, FrameWriter, Message};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Internal event type — all reader threads feed this into one channel.
pub(crate) enum HarnessEvent {
    /// Decoded frame from any connection (extension or client).
    FromConnection {
        connection_id: tau_proto::ConnectionId,
        frame: Box<Frame>,
    },
    /// A connection's reader hit EOF or decode error.
    Disconnected {
        connection_id: tau_proto::ConnectionId,
    },
    /// Socket listener accepted a new client.
    NewClient(UnixStream),
}

/// Connection sink — sends to the per-connection writer channel.
pub(crate) struct ChannelSink {
    pub(crate) tx: Sender<Frame>,
}

impl ConnectionSink for ChannelSink {
    fn send(&mut self, routed: tau_core::RoutedFrame) -> Result<(), ConnectionSendError> {
        self.tx
            .send(routed.frame)
            .map_err(|_| ConnectionSendError::new("writer closed"))
    }
}

/// Reader thread — one per connection, sends to the shared harness channel.
pub(crate) fn spawn_reader_thread(
    connection_id: tau_proto::ConnectionId,
    stream: impl io::Read + Send + 'static,
    tx: Sender<HarnessEvent>,
) {
    thread::spawn(move || {
        let mut reader = FrameReader::new(BufReader::new(stream));
        loop {
            match reader.read_frame() {
                Ok(Some(frame)) => {
                    if tx
                        .send(HarnessEvent::FromConnection {
                            connection_id: connection_id.clone(),
                            frame: Box::new(frame),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) | Err(_) => {
                    let _ = tx.send(HarnessEvent::Disconnected {
                        connection_id: connection_id.clone(),
                    });
                    return;
                }
            }
        }
    });
}

/// What the writer thread should do when its channel closes.
pub(crate) enum WriterShutdown {
    /// Just close the stream (socket clients, in-process peers).
    CloseStream,
    /// Supervised child: send disconnect, close stdin, wait/signal.
    KillChild(Child),
}

/// Writer thread — one per connection, drains channel and writes to stream.
pub(crate) fn spawn_writer_thread(
    writer: impl Write + Send + 'static,
    shutdown: WriterShutdown,
) -> Sender<Frame> {
    let (tx, rx) = mpsc::channel::<Frame>();
    thread::spawn(move || {
        let mut w = FrameWriter::new(BufWriter::new(writer));

        // Drain frames until the channel closes.
        while let Ok(frame) = rx.recv() {
            if w.write_frame(&frame).is_err() {
                return;
            }
            if w.flush().is_err() {
                return;
            }
        }

        // Channel closed — run shutdown sequence.
        match shutdown {
            WriterShutdown::CloseStream => {
                // Drop the writer → closes the stream.
            }
            WriterShutdown::KillChild(child) => {
                // Best-effort disconnect message.
                let _ = w.write_frame(&Frame::Message(Message::Disconnect(Disconnect {
                    reason: Some("shutdown".to_owned()),
                })));
                let _ = w.flush();
                // Drop the writer → closes stdin → extension sees EOF.
                drop(w);

                wait_with_grace(child, SHUTDOWN_GRACE);
            }
        }
    });
    tx
}

/// Block until `child` exits, or escalate to `SIGKILL` after `grace`.
///
/// The wait happens on a helper thread so the caller can time it out via a
/// channel rather than polling `try_wait`. On timeout we signal the child
/// by PID; the helper thread's `wait()` then reaps it.
fn wait_with_grace(mut child: Child, grace: Duration) {
    let pid = child.id();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let waiter = thread::spawn(move || {
        let _ = child.wait();
        let _ = done_tx.send(());
    });
    if done_rx.recv_timeout(grace).is_err() {
        // SAFETY: signaling a process by PID. The PID cannot be recycled until
        // the helper thread's `wait()` reaps the child, which has not happened
        // yet (we just timed out waiting for it).
        #[allow(unsafe_code)]
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        let _ = done_rx.recv();
    }
    let _ = waiter.join();
}
