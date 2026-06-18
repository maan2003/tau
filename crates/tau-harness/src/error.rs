//! [`HarnessError`]: the unified error type returned by every fallible
//! harness operation. Aggregates I/O, protocol, routing, and timeout
//! failures behind one `From`-rich enum.

use std::{fmt, io};

use tau_core::{AgentStoreError, PolicyStoreError, RouteError, ToolRouteError};
use tau_proto::DecodeError;
use tau_socket::SocketTransportError;

/// Errors returned by the harness.
#[derive(Debug)]
pub enum HarnessError {
    Io(io::Error),
    ProtocolDecode(DecodeError),
    ProtocolEncode(tau_proto::EncodeError),
    PolicyStore(PolicyStoreError),
    AgentStore(AgentStoreError),
    SocketTransport(SocketTransportError),
    Route(RouteError),
    ToolRoute(ToolRouteError),
    StartupTimeout,
    ResponseTimeout,
    ThreadJoin(String),
    Participant(String),
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::ProtocolDecode(source) => write!(f, "protocol decode error: {source}"),
            Self::ProtocolEncode(source) => write!(f, "protocol encode error: {source}"),
            Self::PolicyStore(source) => write!(f, "policy store error: {source}"),
            Self::AgentStore(source) => write!(f, "agent store error: {source}"),
            Self::SocketTransport(source) => write!(f, "socket transport error: {source}"),
            Self::Route(source) => write!(f, "routing error: {source}"),
            Self::ToolRoute(source) => write!(f, "tool routing error: {source}"),
            Self::StartupTimeout => f.write_str("timed out waiting for extensions to start"),
            Self::ResponseTimeout => f.write_str("timed out waiting for agent response"),
            Self::ThreadJoin(name) => write!(f, "failed to join {name} thread cleanly"),
            Self::Participant(message) => write!(f, "participant error: {message}"),
        }
    }
}

impl std::error::Error for HarnessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::ProtocolDecode(source) => Some(source),
            Self::ProtocolEncode(source) => Some(source),
            Self::PolicyStore(source) => Some(source),
            Self::AgentStore(source) => Some(source),
            Self::SocketTransport(source) => Some(source),
            Self::Route(source) => Some(source),
            Self::ToolRoute(source) => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for HarnessError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}
impl From<DecodeError> for HarnessError {
    fn from(source: DecodeError) -> Self {
        Self::ProtocolDecode(source)
    }
}
impl From<PolicyStoreError> for HarnessError {
    fn from(source: PolicyStoreError) -> Self {
        Self::PolicyStore(source)
    }
}
impl From<AgentStoreError> for HarnessError {
    fn from(source: AgentStoreError) -> Self {
        Self::AgentStore(source)
    }
}
impl From<SocketTransportError> for HarnessError {
    fn from(source: SocketTransportError) -> Self {
        Self::SocketTransport(source)
    }
}
impl From<RouteError> for HarnessError {
    fn from(source: RouteError) -> Self {
        Self::Route(source)
    }
}
impl From<ToolRouteError> for HarnessError {
    fn from(source: ToolRouteError) -> Self {
        Self::ToolRoute(source)
    }
}
