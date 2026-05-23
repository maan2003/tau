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
//! snake_case names, distinct from `Event`'s `{"event": "tool.started",
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
///
/// Selectors describe event interest, not replay intent. UI socket
/// clients currently receive selected late-join replay from the
/// harness, while extension subscriptions are live-only. This payload
/// has no past-event opt-in field.
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
/// `config: { … }` value was for that extension in `harness.yaml`,
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
/// a `harness.yaml` parse error so the user can see why their
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
pub struct LogEventId(u64);

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

/// Wall-clock timestamp as microseconds since the UNIX epoch.
///
/// Stamped onto persisted session events and the JSONL debug log so
/// offline inspection can compute inter-event gaps, RPM bursts, and
/// correlations with provider-side cache misses. `u64` µs covers
/// ~584,000 years past 1970, so saturation is not a concern in
/// practice — callers still saturate on bogus clocks rather than
/// panic, keeping the persistence path infallible. A zero value
/// marks records written before this field existed
/// (`#[serde(default)]` on the carrying struct).
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct UnixMicros(u64);

impl UnixMicros {
    #[must_use]
    pub fn new(v: u64) -> Self {
        Self(v)
    }

    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// Reads the current wall clock and returns a `UnixMicros`.
    /// Saturates on bogus clocks (pre-1970 or post-2554) instead of
    /// panicking, so callers on the durable-write path can stay
    /// infallible.
    #[must_use]
    pub fn now() -> Self {
        let micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Self(micros)
    }
}

impl std::fmt::Display for UnixMicros {
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
///
/// `recorded_at` is stamped by the harness at the moment the event
/// is appended to the in-memory event log. Subscribers receive the
/// same value the persisted record carries, so offline timing
/// analyses agree with what live consumers saw. Older peers send
/// records without the field; they deserialize as `UnixMicros(0)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogEvent {
    pub id: LogEventId,
    #[serde(default)]
    pub recorded_at: UnixMicros,
    pub event: Box<Event>,
}

/// Extension/client request to emit one event with harness-owned
/// delivery metadata.
///
/// The inner `event` is the fact that subscribers see. `transient`
/// controls whether the harness writes it to durable per-session
/// event history; it is not part of the emitted fact itself.
///
/// `Emit` is strictly for emitting fresh events. Interceptor replies
/// — including the optionally-mutated event — go through
/// [`InterceptReply`], not `Emit`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Emit {
    pub event: Box<Event>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub transient: bool,
}

/// Directed harness → interceptor message carrying an event emission that has
/// not reached the event log yet. The interceptor must reply with an
/// [`InterceptReply`]; until it does, the harness suspends draining of any
/// further publishes that would themselves be subject to interception.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterceptRequest {
    pub event: Box<Event>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub transient: bool,
}

/// What an interceptor wants the harness to do with the event it was given.
///
/// `Pass(None)` republishes the original event unchanged (the common
/// no-op case). `Pass(Some(event))` substitutes a possibly-mutated
/// version that flows on through any remaining interceptors and then to
/// subscribers. `Drop` discards the event entirely — but the harness
/// may override `Drop` for events the publisher marked `must_pass`,
/// `tracing::warn!`-ing and falling back to the original.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum InterceptAction {
    Pass(Option<Box<Event>>),
    Drop,
}

/// Interceptor → harness response to an [`InterceptRequest`]. Exactly
/// one reply per request; out-of-order or duplicate replies are a
/// programming error and the harness logs + falls back to the original
/// event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterceptReply {
    pub action: InterceptAction,
}

/// Request a materialized full `session.prompt_created` payload by id.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GetSessionPromptCreated {
    pub request_id: String,
    pub session_prompt_id: crate::SessionPromptId,
}

/// Response to [`GetSessionPromptCreated`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptCreatedResult {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<crate::SessionPromptCreated>,
}

/// Request that the harness render the effective system prompt for one role.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GetRenderedSystemPrompt {
    /// Request correlation id echoed by [`RenderedSystemPromptResult`].
    pub request_id: String,
    /// Role name whose resolved prompt should be rendered.
    pub role: String,
}

/// Response to [`GetRenderedSystemPrompt`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RenderedSystemPromptResult {
    /// Request correlation id copied from the request.
    pub request_id: String,
    /// Rendered prompt when the role exists and template rendering succeeds.
    /// Exactly one of `prompt` and `error` should be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Human-readable failure when the role is unknown or rendering fails.
    /// Exactly one of `prompt` and `error` should be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
    InterceptRequest(InterceptRequest),
    InterceptReply(InterceptReply),
    GetSessionPromptCreated(GetSessionPromptCreated),
    SessionPromptCreatedResult(Box<SessionPromptCreatedResult>),
    GetRenderedSystemPrompt(GetRenderedSystemPrompt),
    RenderedSystemPromptResult(Box<RenderedSystemPromptResult>),
    LogEvent(LogEvent),
    Ack(Ack),
}
