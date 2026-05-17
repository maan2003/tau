//! [`DebugEventLog`]: append-only JSONL log of every harness event for
//! offline inspection.

use std::path::{Path, PathBuf};

const DEBUG_STRING_COMPACT_THRESHOLD: usize = 100;
const DEBUG_STRING_COMPACT_EDGE_BYTES: usize = 20;

use tau_proto::{ConnectionId, Event, UnixMicros};

use crate::error::HarnessError;
use crate::event::HarnessEvent;

/// Append-only JSON event log for debugging.
pub(crate) struct DebugEventLog {
    path: PathBuf,
    file: std::fs::File,
}

impl DebugEventLog {
    pub(crate) fn open(dir: &Path) -> Result<Self, HarnessError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("events.jsonl");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self { path, file })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn log_harness_event(&mut self, harness_event: &HarnessEvent) {
        // Stamped on every line — including incoming-frame and
        // lifecycle entries that aren't event-log emissions — so an
        // offline reader can compute inter-event gaps and bursts
        // across the entire harness, not just the durable subset.
        let recorded_at = UnixMicros::now().get();
        let entry = match harness_event {
            HarnessEvent::FromConnection {
                connection_id,
                frame,
            } => {
                let name = match frame.as_ref() {
                    tau_proto::Frame::Event(event) => {
                        if event.defaults_to_transient() {
                            return;
                        }
                        event.name().to_string()
                    }
                    tau_proto::Frame::Message(_) => "<message>".to_owned(),
                };
                let mut frame_json = serde_json::to_value(frame).unwrap_or_default();
                compact_debug_json_strings(&mut frame_json);
                serde_json::json!({
                    "type": "from_connection",
                    "recorded_at_micros": recorded_at,
                    "source": connection_id,
                    "event_name": name,
                    "event": frame_json,
                })
            }
            HarnessEvent::Disconnected { connection_id } => {
                serde_json::json!({
                    "type": "disconnected",
                    "recorded_at_micros": recorded_at,
                    "source": connection_id,
                })
            }
            HarnessEvent::NewClient(_) => {
                serde_json::json!({
                    "type": "new_client",
                    "recorded_at_micros": recorded_at,
                })
            }
        };
        self.write_entry(&entry);
    }

    /// Logs an event the harness committed (broadcast onto the bus).
    /// Captures the *enriched* payload — for `AgentResponseFinished`
    /// that's the harness-built `token_usage` with model and running
    /// session stats, which the inbound `from_connection` line could
    /// not carry. Together with `log_harness_event`, an offline reader
    /// can correlate the raw agent emit against the enriched committed
    /// copy.
    pub(crate) fn log_published_event(
        &mut self,
        source: Option<&ConnectionId>,
        event: &Event,
        recorded_at: UnixMicros,
    ) {
        let mut event_json = serde_json::to_value(event).unwrap_or_default();
        compact_debug_json_strings(&mut event_json);
        let entry = serde_json::json!({
            "type": "published",
            "recorded_at_micros": recorded_at.get(),
            "source": source,
            "event_name": event.name(),
            "event": event_json,
        });
        self.write_entry(&entry);
    }

    fn write_entry(&mut self, entry: &serde_json::Value) {
        use std::io::Write;
        let _ = serde_json::to_writer(&mut self.file, entry);
        let _ = self.file.write_all(b"\n");
        let _ = self.file.flush();
    }
}

fn compact_debug_json_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            *s = compact_debug_string(s);
        }
        serde_json::Value::Array(values) => {
            for value in values {
                compact_debug_json_strings(value);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values_mut() {
                compact_debug_json_strings(value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn compact_debug_string(s: &str) -> String {
    if s.len() <= DEBUG_STRING_COMPACT_THRESHOLD {
        return s.to_owned();
    }

    let mut prefix_end = DEBUG_STRING_COMPACT_EDGE_BYTES;
    while !s.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }

    let mut suffix_start = s.len() - DEBUG_STRING_COMPACT_EDGE_BYTES;
    while suffix_start < s.len() && !s.is_char_boundary(suffix_start) {
        suffix_start += 1;
    }

    format!(
        "{}┄total {}┄{}",
        &s[..prefix_end],
        s.len(),
        &s[suffix_start..]
    )
}

#[cfg(test)]
mod tests {
    use tau_proto::{
        AgentResponseFinished, AgentResponseUpdated, AgentTokenUsage, Frame, ModelId,
        PromptOriginator, SessionPromptId,
    };

    use super::*;
    use crate::event::HarnessEvent;

    fn read_lines(path: &Path) -> Vec<serde_json::Value> {
        let raw = std::fs::read_to_string(path).expect("read events.jsonl");
        raw.lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("parse line"))
            .collect()
    }

    #[test]
    fn published_line_preserves_enriched_token_usage() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut log = DebugEventLog::open(td.path()).expect("open");
        let model: ModelId = "openai/gpt-5".parse().expect("model id");
        let event = Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: SessionPromptId::from("sp-0"),
            output_items: Vec::new(),
            stop_reason: tau_proto::AgentStopReason::EndTurn,
            originator: PromptOriginator::User,
            usage: Some(AgentTokenUsage {
                model: Some(model),
                prompt_sent_tokens: 1000,
                prompt_cached_tokens: 800,
                response_received_tokens: 42,
                stats: tau_proto::TokenUsageStats::default(),
            }),
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        });
        log.log_published_event(
            Some(&ConnectionId::from("conn-1")),
            &event,
            UnixMicros::now(),
        );

        let lines = read_lines(log.path());
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert_eq!(line["type"], "published");
        assert_eq!(line["event_name"], "provider.response_finished");
        assert_eq!(line["source"], "conn-1");
        let usage = &line["event"]["payload"]["usage"];
        assert_eq!(usage["prompt_sent_tokens"], 1000);
        assert_eq!(usage["prompt_cached_tokens"], 800);
        assert_eq!(usage["response_received_tokens"], 42);
        assert_eq!(usage["model"], "openai/gpt-5");
    }

    #[test]
    fn published_line_compacts_long_strings() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut log = DebugEventLog::open(td.path()).expect("open");
        let event = Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: SessionPromptId::from("sp-0"),
            text: "x".repeat(101),
            thinking: Some(format!("{}{}{}", "α".repeat(30), "middle", "ω".repeat(30))),
            originator: PromptOriginator::User,
        });

        log.log_published_event(None, &event, UnixMicros::now());

        let lines = read_lines(log.path());
        assert_eq!(lines.len(), 1);
        let payload = &lines[0]["event"]["payload"];
        assert_eq!(
            payload["text"],
            "xxxxxxxxxxxxxxxxxxxx┄total 101┄xxxxxxxxxxxxxxxxxxxx"
        );
        assert_eq!(payload["thinking"], "αααααααααα┄total 126┄ωωωωωωωωωω");
    }

    #[test]
    fn compact_debug_string_keeps_short_strings() {
        assert_eq!(compact_debug_string(&"x".repeat(100)), "x".repeat(100));
    }

    #[test]
    fn transient_from_connection_events_are_not_logged_twice() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut log = DebugEventLog::open(td.path()).expect("open");
        let event = Event::AgentResponseUpdated(AgentResponseUpdated {
            session_prompt_id: SessionPromptId::from("sp-0"),
            text: "partial".to_owned(),
            thinking: None,
            originator: PromptOriginator::User,
        });

        log.log_harness_event(&HarnessEvent::FromConnection {
            connection_id: ConnectionId::from("conn-1"),
            frame: Box::new(Frame::Event(event)),
        });

        let lines = read_lines(log.path());
        assert!(
            lines.is_empty(),
            "transient streaming events are logged on publish; the raw inbound copy is redundant"
        );
    }
}
