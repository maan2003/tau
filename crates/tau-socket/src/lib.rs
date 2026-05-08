//! Unix socket listener and transport adapters.
//!
//! This crate exposes a small transport-agnostic socket peer that reuses the
//! same self-delimiting CBOR event codec as stdio transports.

use std::io::{self, BufReader, BufWriter};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;
use std::{fmt, fs, thread};

use tau_proto::{DecodeError, Frame, FrameReader, FrameWriter};

/// Errors returned by the Unix socket transport.
#[derive(Debug)]
pub enum SocketTransportError {
    CreateParentDirectory { path: PathBuf, source: io::Error },
    RemoveStaleSocket { path: PathBuf, source: io::Error },
    Bind { path: PathBuf, source: io::Error },
    Accept(io::Error),
    Connect { path: PathBuf, source: io::Error },
    Clone(io::Error),
    Encode(tau_proto::EncodeError),
    Flush(io::Error),
    Decode(DecodeError),
}

impl fmt::Display for SocketTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateParentDirectory { path, source } => write!(
                f,
                "failed to create socket parent directory {}: {source}",
                path.display()
            ),
            Self::RemoveStaleSocket { path, source } => write!(
                f,
                "failed to remove stale socket {}: {source}",
                path.display()
            ),
            Self::Bind { path, source } => {
                write!(f, "failed to bind Unix socket {}: {source}", path.display())
            }
            Self::Accept(source) => write!(f, "failed to accept Unix socket client: {source}"),
            Self::Connect { path, source } => {
                write!(
                    f,
                    "failed to connect to Unix socket {}: {source}",
                    path.display()
                )
            }
            Self::Clone(source) => write!(f, "failed to clone Unix socket stream: {source}"),
            Self::Encode(source) => write!(f, "failed to encode socket event: {source}"),
            Self::Flush(source) => write!(f, "failed to flush socket stream: {source}"),
            Self::Decode(source) => write!(f, "failed to decode socket event: {source}"),
        }
    }
}

impl std::error::Error for SocketTransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CreateParentDirectory { source, .. } => Some(source),
            Self::RemoveStaleSocket { source, .. } => Some(source),
            Self::Bind { source, .. } => Some(source),
            Self::Accept(source) => Some(source),
            Self::Connect { source, .. } => Some(source),
            Self::Clone(source) => Some(source),
            Self::Encode(source) => Some(source),
            Self::Flush(source) => Some(source),
            Self::Decode(source) => Some(source),
        }
    }
}

/// Unix socket listener for later-attached protocol clients.
pub struct SocketListener {
    path: PathBuf,
    listener: UnixListener,
}

impl SocketListener {
    /// Binds a Unix socket listener at the given path.
    pub fn bind(path: impl Into<PathBuf>) -> Result<Self, SocketTransportError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| {
                SocketTransportError::CreateParentDirectory {
                    path: parent.to_path_buf(),
                    source,
                }
            })?;
        }
        if path.exists() {
            fs::remove_file(&path).map_err(|source| SocketTransportError::RemoveStaleSocket {
                path: path.clone(),
                source,
            })?;
        }

        let listener = UnixListener::bind(&path).map_err(|source| SocketTransportError::Bind {
            path: path.clone(),
            source,
        })?;
        Ok(Self { path, listener })
    }

    /// Returns the filesystem path of the listener socket.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accepts one attached client.
    pub fn accept(&self) -> Result<SocketPeer, SocketTransportError> {
        let (stream, _) = self
            .listener
            .accept()
            .map_err(SocketTransportError::Accept)?;
        SocketPeer::new(stream)
    }
}

impl Drop for SocketListener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// One connected Unix socket peer speaking the protocol.
pub struct SocketPeer {
    writer: FrameWriter<BufWriter<UnixStream>>,
    reader_frames: Receiver<Result<Frame, DecodeError>>,
}

impl SocketPeer {
    /// Connects to an existing Unix socket listener.
    pub fn connect(path: impl Into<PathBuf>) -> Result<Self, SocketTransportError> {
        let path = path.into();
        let stream = UnixStream::connect(&path)
            .map_err(|source| SocketTransportError::Connect { path, source })?;
        Self::new(stream)
    }

    fn new(stream: UnixStream) -> Result<Self, SocketTransportError> {
        let writer_stream = stream.try_clone().map_err(SocketTransportError::Clone)?;
        let reader_frames = spawn_reader(stream);
        Ok(Self {
            writer: FrameWriter::new(BufWriter::new(writer_stream)),
            reader_frames,
        })
    }

    /// Sends one protocol frame over the Unix socket.
    pub fn send(&mut self, frame: &Frame) -> Result<(), SocketTransportError> {
        self.writer
            .write_frame(frame)
            .map_err(SocketTransportError::Encode)?;
        self.writer.flush().map_err(SocketTransportError::Flush)
    }

    /// Reads one protocol frame, or returns `Ok(None)` on timeout or clean EOF.
    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<Frame>, SocketTransportError> {
        match self.reader_frames.recv_timeout(timeout) {
            Ok(Ok(frame)) => Ok(Some(frame)),
            Ok(Err(error)) if is_unexpected_eof(&error) => Ok(None),
            Ok(Err(error)) => Err(SocketTransportError::Decode(error)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Ok(None),
        }
    }
}

fn spawn_reader(stream: UnixStream) -> Receiver<Result<Frame, DecodeError>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = FrameReader::new(BufReader::new(stream));
        loop {
            match reader.read_frame() {
                Ok(Some(frame)) => {
                    if sender.send(Ok(frame)).is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            }
        }
    });
    receiver
}

fn is_unexpected_eof(error: &DecodeError) -> bool {
    match error {
        DecodeError::Io(source) => source.kind() == io::ErrorKind::UnexpectedEof,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
