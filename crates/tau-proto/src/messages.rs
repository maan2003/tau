//! Control-plane point-to-point messages.
//!
//! `Message` is the sibling of [`crate::Event`]: where `Event` carries
//! bus facts (broadcast to subscribers, dotted `category.call` names),
//! `Message` carries directed control-plane traffic — handshake,
//! subscription registration, configuration, the at-least-once
//! `LogEvent`/`Ack` envelope, etc. Messages are not subscribable;
//! they're sent point-to-point between the harness and one specific
//! peer.
//!
//! Wire form: `{"message": "hello", "payload": {...}}` — flat, lower
//! snake_case names, distinct from `Event`'s `{"event": "tool.invoke",
//! ...}` shape so the [`crate::Frame`] envelope can disambiguate by
//! discriminator.

use serde::{Deserialize, Serialize};

use crate::{CborValue, ClientKind, Event, EventSelector, ExtensionName, InterceptionPriority};

// ---------------------------------------------------------------------------
// Lifecycle messages
// ---------------------------------------------------------------------------

/// Announcement sent by a participant after connecting.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub client_name: ExtensionName,
    pub client_kind: ClientKind,
}

/// Subscription request describing which events a participant wants.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Subscribe {
    pub selectors: Vec<EventSelector>,
}

/// Interception request describing which event emissions a participant wants
/// to handle before they reach the event log and regular subscribers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Intercept {
    pub selectors: Vec<EventSelector>,
    pub priority: InterceptionPriority,
}

/// Readiness notification emitted after startup or handshake.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Ready {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Disconnect notification with an optional human-readable reason.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Disconnect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Configuration handed to an extension at startup. Sent
/// point-to-point from the harness to the extension immediately
/// after the harness sees the extension's
/// [`Hello`](crate::Hello). Carries whatever the
/// `config: { … }` value was for that extension in `harness.json5`,
/// or [`CborValue::Null`] / an empty map when no config was
/// provided.
///
/// `Eq` is not derivable because the underlying CBOR value can
/// contain floats; `PartialEq` is enough for tests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Configure {
    pub config: CborValue,
}

/// Reported by an extension when its
/// [`Configure`](Configure) value is malformed (or
/// otherwise unusable). The harness surfaces the message just like
/// a `harness.json5` parse error so the user can see why their
/// per-extension config was rejected.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConfigError {
    pub message: String,
}

// ---------------------------------------------------------------------------
// Wire transport — at-least-once delivery for event-log entries
// ---------------------------------------------------------------------------

/// Monotonic id assigned by the harness when an event is appended to its
/// event log. Receivers acknowledge processing by returning the same id
/// in [`Ack::up_to`].
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct LogEventId(pub u64);

impl LogEventId {
    #[must_use]
    pub fn new(v: u64) -> Self {
        Self(v)
    }

    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for LogEventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A bus event delivered through the harness's event log. Receivers
/// must process the inner event and then send an [`Ack`] referencing
/// `id` (or any later id, since acks are cumulative).
///
/// `event` is boxed because the inner value is the (potentially
/// large) bus fact. It is never another `LogEvent` or `Ack` — only
/// "real" payload events (e.g., `SessionStarted`, `ExtensionReady`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogEvent {
    pub id: LogEventId,
    pub event: Box<Event>,
}

/// Extension/client request to emit one event with harness-owned
/// delivery metadata.
///
/// The inner `event` is the fact that subscribers see. `transient`
/// controls whether the harness writes it to durable per-session
/// event history; it is not part of the emitted fact itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Emit {
    pub event: Box<Event>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub transient: bool,
    /// Redelivery cursor. `None` starts interception from the beginning.
    /// When set by an interceptor, the harness resumes after that priority
    /// and the sending component at that priority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interception: Option<InterceptionPriority>,
}

/// Directed harness → interceptor message carrying an event emission that has
/// not reached the event log yet.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Intercepted {
    pub event: Box<Event>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub transient: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interception: Option<InterceptionPriority>,
}

/// Receiver → sender acknowledgement that all log events with id
/// `<= up_to` have been processed. Cumulative — newer acks supersede
/// older ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    pub up_to: LogEventId,
}

// ---------------------------------------------------------------------------
// Top-level message envelope
// ---------------------------------------------------------------------------

/// Point-to-point control-plane message envelope used on the wire.
///
/// Wire form is `{"message": "<flat_name>", "payload": {...}}`. Names
/// are flat (no dot, snake_case) to make the discriminator trivially
/// distinguishable from [`Event`]'s dotted `category.call` form — the
/// outer [`crate::Frame`] envelope relies on this distinction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "message", content = "payload", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    Subscribe(Subscribe),
    Intercept(Intercept),
    Ready(Ready),
    Disconnect(Disconnect),
    Configure(Configure),
    ConfigError(ConfigError),
    Emit(Emit),
    Intercepted(Intercepted),
    LogEvent(LogEvent),
    Ack(Ack),
}
