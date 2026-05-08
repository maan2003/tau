//! Shared protocol types and CBOR stream codec helpers.
//!
//! The wire format is a sequence of self-delimiting CBOR items. Each
//! item is a [`Frame`] — an untagged enum that's either:
//!
//! - a [`Message`]: control-plane point-to-point traffic, encoded as
//!   `{"message": "<flat_name>", "payload": {...}}`, or
//! - an [`Event`]: bus-broadcast facts, encoded as `{"event":
//!   "<category>.<call>", "payload": {...}}`.
//!
//! The codec helpers in this crate work with any [`std::io::Read`] or
//! [`std::io::Write`], so the same protocol layer can be reused for
//! stdio, Unix sockets, tests, or in-memory transports.

mod diff;
mod events;
mod frame;
mod messages;

use std::io::{Cursor, Read, Write};

pub use ciborium::value::Value as CborValue;
pub use diff::{DiffHunk, DiffLine, DiffSegment, DiffSummary};
pub use events::*;
pub use frame::Frame;
pub use messages::*;

/// First protocol version implemented by this crate.
pub const PROTOCOL_VERSION: u32 = 1;

macro_rules! string_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
            pub fn into_string(self) -> String { self.0 }
            pub fn is_empty(&self) -> bool { self.0.is_empty() }
        }

        impl std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str { &self.0 }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self { Self(s) }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.to_owned()) }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool { self.0 == other }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool { self.0 == *other }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool { self.0 == *other }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str { &self.0 }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str { &self.0 }
        }
    };
}

string_newtype!(/// Session identifier.
    SessionId);
// ToolName is defined manually below with validation.
string_newtype!(/// Tool call identifier.
    ToolCallId);
string_newtype!(/// Connection identifier.
    ConnectionId);
string_newtype!(/// Unique identifier for one prompt within a session.
    SessionPromptId);
string_newtype!(/// Extension name.
    ExtensionName);
string_newtype!(/// Qualified model identifier (e.g. `"openai/gpt-4o"`).
    ModelId);
string_newtype!(/// Provider name (e.g. `"openai"`, `"anthropic"`).
    ProviderName);
string_newtype!(/// Skill name (e.g. `"jujutsu"`, `"preview-site"`).
    SkillName);
string_newtype!(/// Identifier correlating a user-initiated `!`/`!!` shell
    /// command's lifecycle events (progress, finished).
    ShellCommandId);

// ---------------------------------------------------------------------------
// ToolName (validated newtype)
// ---------------------------------------------------------------------------

/// Tool name: must be non-empty and contain only ASCII alphanumerics or
/// underscores (`[a-zA-Z0-9_]+`).
#[derive(Clone, Debug, Eq, PartialEq, Hash, serde::Serialize, Default)]
#[serde(transparent)]
pub struct ToolName(String);

impl ToolName {
    /// Create a new `ToolName`, panicking if the name is invalid.
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        assert!(Self::is_valid(&s), "invalid tool name: {s:?}");
        Self(s)
    }

    /// Try to create a `ToolName`, returning `None` if invalid.
    pub fn try_new(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        Self::is_valid(&s).then_some(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn into_string(self) -> String {
        self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn is_valid(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }
}

impl std::ops::Deref for ToolName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ToolName {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for ToolName {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl PartialEq<str> for ToolName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ToolName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for ToolName {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl std::borrow::Borrow<str> for ToolName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ToolName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for ToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if Self::is_valid(&s) {
            Ok(Self(s))
        } else {
            Err(serde::de::Error::custom(format!(
                "invalid tool name: {s:?}"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// ToolNameMaybe (LLM-boundary tool name)
// ---------------------------------------------------------------------------

/// Tool name at the LLM boundary: either a validated [`ToolName`] or
/// the raw string the model produced.
///
/// LLM output is untrusted: models hallucinate, stream partial tokens,
/// and occasionally emit empty or structurally wrong tool names.
/// `ToolNameMaybe` preserves those values through deserialization
/// instead of rejecting the whole event so a single bad tool call
/// doesn't kill a batch of good ones. Consumers match on the enum
/// before dispatching, which makes it syntactically impossible to
/// accidentally `.into()`-panic a raw model string into a `ToolName`.
///
/// The wire encoding is a transparent string — same bytes on the wire
/// as a plain `String` field, so this can be introduced without
/// breaking format compatibility.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum ToolNameMaybe {
    Valid(ToolName),
    Invalid(String),
}

impl ToolNameMaybe {
    /// The underlying string, whether or not it was validated.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Valid(name) => name.as_str(),
            Self::Invalid(raw) => raw.as_str(),
        }
    }

    /// Classify an arbitrary string into Valid or Invalid.
    pub fn from_raw(s: impl Into<String>) -> Self {
        let s = s.into();
        match ToolName::try_new(s.clone()) {
            Some(name) => Self::Valid(name),
            None => Self::Invalid(s),
        }
    }
}

impl From<String> for ToolNameMaybe {
    fn from(s: String) -> Self {
        Self::from_raw(s)
    }
}

impl From<&str> for ToolNameMaybe {
    fn from(s: &str) -> Self {
        Self::from_raw(s)
    }
}

impl From<ToolName> for ToolNameMaybe {
    fn from(name: ToolName) -> Self {
        Self::Valid(name)
    }
}

impl std::fmt::Display for ToolNameMaybe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for ToolNameMaybe {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Transparent: emit the inner string unchanged for both
        // variants. Round-tripping Invalid through deserialize will
        // produce Invalid again, round-tripping Valid will re-validate.
        self.as_str().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for ToolNameMaybe {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_raw(s))
    }
}

/// Unique identifier for one extension instance (monotonic counter).
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ExtensionInstanceId(u64);

impl ExtensionInstanceId {
    pub fn new(v: u64) -> Self {
        Self(v)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for ExtensionInstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for ExtensionInstanceId {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

/// CBOR serialization error used by [`encode_frame`] and [`FrameWriter`].
pub type EncodeError = ciborium::ser::Error<std::io::Error>;

/// CBOR deserialization error used by [`decode_frame`] and [`FrameReader`].
pub type DecodeError = ciborium::de::Error<std::io::Error>;

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

/// Encodes one frame as a self-delimiting CBOR item.
pub fn encode_frame<W>(writer: W, frame: &Frame) -> Result<(), EncodeError>
where
    W: Write,
{
    ciborium::into_writer(frame, writer)
}

/// Decodes one frame from a self-delimiting CBOR item.
pub fn decode_frame<R>(reader: R) -> Result<Frame, DecodeError>
where
    R: Read,
{
    ciborium::from_reader(reader)
}

/// Encodes one frame into an owned byte buffer.
pub fn encode_frame_to_vec(frame: &Frame) -> Result<Vec<u8>, EncodeError> {
    let mut bytes = Vec::new();
    encode_frame(&mut bytes, frame)?;
    Ok(bytes)
}

/// Decodes one frame from a byte slice.
pub fn decode_frame_from_slice(bytes: &[u8]) -> Result<Frame, DecodeError> {
    decode_frame(Cursor::new(bytes))
}

/// Convert a `serde_json::Value` into a [`CborValue`].
///
/// Numbers are preserved as integers when possible, otherwise as
/// floats. Anything that doesn't round-trip cleanly (e.g. an
/// out-of-range number that's neither `i64` nor `f64`) becomes
/// [`CborValue::Null`]. JSON's full type system is a strict subset
/// of CBOR, so the conversion is otherwise lossless.
#[must_use]
pub fn json_to_cbor(v: &serde_json::Value) -> CborValue {
    match v {
        serde_json::Value::Null => CborValue::Null,
        serde_json::Value::Bool(b) => CborValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                CborValue::Integer(i.into())
            } else if let Some(f) = n.as_f64() {
                CborValue::Float(f)
            } else {
                CborValue::Null
            }
        }
        serde_json::Value::String(s) => CborValue::Text(s.clone()),
        serde_json::Value::Array(arr) => CborValue::Array(arr.iter().map(json_to_cbor).collect()),
        serde_json::Value::Object(map) => CborValue::Map(
            map.iter()
                .map(|(k, v)| (CborValue::Text(k.clone()), json_to_cbor(v)))
                .collect(),
        ),
    }
}

/// Stateful writer for a stream of protocol frames.
#[derive(Debug)]
pub struct FrameWriter<W> {
    inner: W,
}

impl<W> FrameWriter<W> {
    /// Wraps an arbitrary writer.
    #[must_use]
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Returns the wrapped writer.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W> FrameWriter<W>
where
    W: Write,
{
    /// Writes one protocol frame to the stream.
    pub fn write_frame(&mut self, frame: &Frame) -> Result<(), EncodeError> {
        encode_frame(&mut self.inner, frame)
    }

    /// Flushes the wrapped writer.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Stateful reader for a stream of protocol frames.
#[derive(Debug)]
pub struct FrameReader<R> {
    inner: R,
}

impl<R> FrameReader<R> {
    /// Wraps an arbitrary reader.
    #[must_use]
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Returns the wrapped reader.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R> FrameReader<R>
where
    R: Read,
{
    /// Reads one protocol frame from the stream.
    ///
    /// Returns `Ok(None)` on clean end-of-stream (EOF at a message
    /// boundary). Returns `Err` only for actual corruption or
    /// truncated data.
    pub fn read_frame(&mut self) -> Result<Option<Frame>, DecodeError> {
        let mut first = [0_u8; 1];
        match self.inner.read_exact(&mut first) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(DecodeError::Io(e)),
        }
        let chained = Cursor::new(first).chain(&mut self.inner);
        ciborium::from_reader(chained).map(Some)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
