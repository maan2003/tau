//! Test suite for the harness. Split by concern around major harness behaviors
//! such as interception, replay, dispatch, lifecycle, and tool handling.
//!
//! The shared helpers and imports live here so each submodule can
//! pull them in with `use super::*;`.

use rand::SeedableRng as _;
use rand::rngs::StdRng;

use super::extension_data::{
    append_extension_data_file, atomic_replace_extension_data_file, checked_extension_data_path,
    create_extension_data_file, delete_extension_data_file, list_extension_data_entries,
    rename_extension_data_file, sanitize_extension_data_path,
};
use super::{
    HARNESS_CONNECTION_ID, Harness, mint_available_agent_id_for_role_with, prune_old_agent_dirs,
    prune_old_debug_dirs,
};

fn deterministic_agent_id_rng() -> StdRng {
    StdRng::seed_from_u64(0)
}

fn mint_agent_id_for_role(role: &str) -> String {
    mint_available_agent_id_for_role_with(
        role,
        role,
        "{{random_alphanumeric 6}}",
        |_| false,
        &mut deterministic_agent_id_rng(),
        |_, _| {},
    )
}

/// Active test harness fixture that keeps its temporary state directory alive.
struct HarnessFixture {
    /// Temporary root containing config, state, and agent store files.
    _temp_dir: TempDir,
    /// Harness under test.
    harness: Harness,
}

/// In-memory bus sink that records every routed frame for assertions.
struct RecordingSink {
    /// Shared frame buffer populated by [`ConnectionSink::send`].
    frames: Arc<Mutex<Vec<RoutedFrame>>>,
}

impl ConnectionSink for RecordingSink {
    fn send(&mut self, frame: RoutedFrame) -> Result<(), ConnectionSendError> {
        self.frames
            .lock()
            .expect("recorded frames lock")
            .push(frame);
        Ok(())
    }
}

fn echo_runner(reader: UnixStream, writer: UnixStream) -> Result<(), String> {
    super::run_echo_provider(reader, writer).map_err(|error| error.to_string())
}

fn harness_fixture() -> HarnessFixture {
    let temp_dir = TempDir::new().expect("tempdir");
    let state_dir = temp_dir.path().join("state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(temp_dir.path().join("config")),
        state_dir: Some(temp_dir.path().join("runtime")),
    };
    let harness =
        Harness::new_with_provider(&state_dir, dirs, echo_runner, Vec::new()).expect("harness");
    HarnessFixture {
        _temp_dir: temp_dir,
        harness,
    }
}

fn write_events_jsonl_with_mtime(dir: &Path, modified: SystemTime) {
    std::fs::create_dir_all(dir).expect("debug dir");
    let events_path = dir.join("events.jsonl");
    std::fs::write(&events_path, "{}\n").expect("events jsonl");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(events_path)
        .expect("events jsonl file");
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .expect("set mtime");
}

fn write_agent_events_cbor_with_mtime(dir: &Path, modified: SystemTime) {
    std::fs::create_dir_all(dir).expect("agent dir");
    let events_path = dir.join("events.cbor");
    std::fs::write(&events_path, b"").expect("events cbor");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(events_path)
        .expect("events cbor file");
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .expect("set mtime");
}

#[test]
fn agent_pruning_removes_old_agent_directories_by_events_mtime() {
    let temp_dir = TempDir::new().expect("tempdir");
    let state_dir = temp_dir.path().join("state");
    let agents_dir = state_dir.join("agents");
    let old_dir = agents_dir.join("old-agent");
    let fresh_dir = agents_dir.join("fresh-agent");
    let no_events_dir = agents_dir.join("no-events-agent");
    let invalid_dir = agents_dir.join("not an agent");

    write_agent_events_cbor_with_mtime(&old_dir, UNIX_EPOCH);
    std::fs::write(old_dir.join("meta.json"), "{}").expect("old meta");
    write_agent_events_cbor_with_mtime(&fresh_dir, SystemTime::now());
    std::fs::create_dir_all(&no_events_dir).expect("no-events dir");
    write_agent_events_cbor_with_mtime(&invalid_dir, UNIX_EPOCH);

    prune_old_agent_dirs(&state_dir, Some(Duration::from_secs(24 * 60 * 60)))
        .expect("prune agent dirs");

    assert!(!old_dir.exists(), "stale agent dir should be pruned");
    assert!(fresh_dir.exists(), "fresh agent dir should be kept");
    assert!(
        no_events_dir.exists(),
        "agent dirs without events.cbor should not be age-pruned"
    );
    assert!(
        invalid_dir.exists(),
        "invalid agent ids should not be pruned by retention cleanup"
    );
}

#[test]
fn debug_pruning_removes_old_run_directories_by_events_mtime() {
    let temp_dir = TempDir::new().expect("tempdir");
    let state_dir = temp_dir.path().join("state");
    let debug_root = crate::extension::debug_root(&state_dir);
    let old_dir = debug_root.join("oldrun");
    let current_dir = debug_root.join("current");
    let fresh_dir = debug_root.join("fresh");
    let no_events_dir = debug_root.join("no-events");

    write_events_jsonl_with_mtime(&old_dir, UNIX_EPOCH);
    std::fs::write(old_dir.join("nested-artifact"), "remove me").expect("nested artifact");
    write_events_jsonl_with_mtime(&current_dir, UNIX_EPOCH);
    write_events_jsonl_with_mtime(&fresh_dir, SystemTime::now());
    std::fs::create_dir_all(&no_events_dir).expect("no-events dir");

    prune_old_debug_dirs(
        &state_dir,
        Some(Duration::from_secs(24 * 60 * 60)),
        &current_dir,
    )
    .expect("prune debug dirs");

    assert!(!old_dir.exists(), "stale debug dir should be pruned");
    assert!(current_dir.exists(), "current debug dir should be kept");
    assert!(fresh_dir.exists(), "fresh debug dir should be kept");
    assert!(
        no_events_dir.exists(),
        "dirs without events.jsonl should not be age-pruned"
    );
}

fn seed_started_agent(harness: &mut Harness, agent_id: &tau_proto::AgentId) {
    harness
        .agent_store
        .append_agent_event(
            agent_id.as_str(),
            Some(HARNESS_CONNECTION_ID.into()),
            Event::AgentStarted(tau_proto::AgentStarted {
                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "test".to_owned(),
                display_name: None,
                metadata: Vec::new(),
            }),
        )
        .expect("seed agent.started");
}

/// Ensures the load-boundary event is a metadata-free membership snapshot.
///
/// Durable metadata is reconstructed from replayed `agent.started` and
/// `agent.metadata_*` events before this boundary; `agent.loaded` itself only
/// announces runtime membership.
#[test]
fn agent_loaded_event_is_metadata_free_membership_snapshot() {
    let mut fixture = harness_fixture();
    let harness = &mut fixture.harness;
    let agent_id = tau_proto::AgentId::parse("agent-loaded-membership").expect("agent id");

    seed_started_agent(harness, &agent_id);
    let Event::AgentLoaded(loaded) = harness.agent_loaded_event(&agent_id) else {
        panic!("expected agent.loaded");
    };

    assert_eq!(loaded.agent_id, agent_id);
}

/// Ensures explicit `agent.load` brackets replayed history with load lifecycle
/// events.
///
/// `agent.loading` announces that an existing durable agent is being caught up;
/// replayed durable transcript facts follow; metadata-free `agent.loaded` marks
/// the catch-up-complete boundary.
#[test]
fn agent_load_brackets_replayed_history_with_loading_and_loaded() {
    let mut fixture = harness_fixture();
    let harness = &mut fixture.harness;
    let agent_id = tau_proto::AgentId::parse("agent-load-ordering").expect("agent id");
    seed_started_agent(harness, &agent_id);
    harness
        .agent_store
        .append_agent_event(
            agent_id.as_str(),
            Some(HARNESS_CONNECTION_ID.into()),
            Event::AgentPromptSubmitted(tau_proto::AgentPromptSubmitted {
                agent_id: agent_id.clone(),
                text: "persisted prompt".to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                display_name: None,
                ctx_id: None,
            }),
        )
        .expect("seed transcript prompt");

    let frames = Arc::new(Mutex::new(Vec::new()));
    let connection_id = harness.bus.connect(Connection::new(
        ConnectionMetadata {
            id: "agent-load-observer".into(),
            name: "agent-load-observer".to_owned(),
            kind: ClientKind::Tool,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(RecordingSink {
            frames: Arc::clone(&frames),
        }),
    ));
    harness
        .bus
        .set_subscriptions(
            &connection_id,
            vec![EventSelector::Prefix("agent.".to_owned())],
        )
        .expect("subscribe observer");

    harness
        .handle_agent_load(tau_proto::AgentLoad {
            agent_id: agent_id.clone(),
        })
        .expect("load agent");

    let frames = frames.lock().expect("recorded frames");
    let loading_index = frames
        .iter()
        .position(|routed| {
            matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if !delivery.replay
                        && matches!(delivery.event.as_ref(), Event::AgentLoading(loading) if loading.agent_id == agent_id)
            )
        })
        .expect("live agent.loading delivery");
    let replay_index = frames
        .iter()
        .position(|routed| {
            matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if delivery.replay
                        && matches!(
                            delivery.event.as_ref(),
                            Event::AgentPromptSubmitted(prompt)
                                if prompt.agent_id == agent_id && prompt.text == "persisted prompt"
                        )
            )
        })
        .expect("replayed transcript delivery");
    let loaded_index = frames
        .iter()
        .position(|routed| {
            matches!(
                &routed.frame,
                HarnessOutputMessage::Deliver(delivery)
                    if !delivery.replay
                        && matches!(delivery.event.as_ref(), Event::AgentLoaded(loaded) if loaded.agent_id == agent_id)
            )
        })
        .expect("live agent.loaded delivery");

    assert!(
        loading_index < replay_index,
        "agent.loading must arrive before replayed history"
    );
    assert!(
        replay_index < loaded_index,
        "agent.loaded must arrive after replayed history"
    );
    assert!(
        harness.pending_agent_loading_history_replays.is_empty(),
        "load replay continuation should be consumed after commit"
    );
}

use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tau_agent_inspect::{format_agent_entry, policy_lines};
use tau_core::{
    AgentEntry, AgentStore, AgentTree, Connection, ConnectionMetadata, ConnectionOrigin,
    ConnectionSendError, ConnectionSink, RoutedFrame,
};
use tau_proto::{
    AgentPromptCreated, AgentPromptId, AgentPromptQueued, AgentPromptRecalled, CborValue,
    ClientKind, ContentPart, ContextItem, ContextRole, Disconnect, Event, EventDelivery,
    EventSelector, HarnessInputMessage, HarnessInputWriter, HarnessOutputMessage,
    HarnessOutputReader, Intercept, InterceptAction, InterceptReply, InterceptionPriority,
    MessageItem, NodeId, ProviderResponseFinished, ProviderResponseUpdated, StartAgentRequest,
    Subscribe, ToolCallId, ToolCallItem, ToolName, ToolResult, ToolResultItem, ToolResultStatus,
    ToolSpec, UiPromptDraft, UiPromptSubmitted,
};
use tempfile::TempDir;

use crate::AgentId;
use crate::agent::{AgentTurnState, PendingPrompt};
use crate::daemon::{
    ServeOptions, bind_listener, get_daemon_rendered_system_prompt,
    get_daemon_rendered_tool_definitions, run_daemon_with_echo, run_embedded_message_with_echo,
    send_daemon_message, send_daemon_message_with_trace,
};
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill, DiscoveredSkillSource};
use crate::error::HarnessError;
use crate::event::HarnessEvent;
use crate::harness::{AgentState, AgentToolCall};
use crate::model::{
    clamp_effort, efforts_for_model, load_roles, role_infos, select_model_for_role,
    selected_params_for_role, thinking_summaries_for_model, verbosities_for_model,
};
use crate::turn::TurnState;

impl Harness {
    fn handle_provider_response_finished(
        &mut self,
        response: ProviderResponseFinished,
    ) -> Result<(), HarnessError> {
        self.handle_provider_response_finished_from(Some("provider"), response)
    }

    fn extension_connection_id(&self, name: &str) -> Option<tau_proto::ConnectionId> {
        self.bus
            .connections()
            .into_iter()
            .find(|metadata| metadata.name == name)
            .map(|metadata| metadata.id)
    }

    fn build_system_prompt_for_role(&self, role: &str) -> String {
        self.build_system_prompt_for_role_preview(role)
    }

    fn gather_prompt_fragments(&self) -> Vec<tau_proto::PromptFragment> {
        let (fragments, tool_fragments) =
            self.gather_sourced_prompt_fragment_groups(&self.selected_role);
        fragments
            .into_iter()
            .map(|sourced| sourced.fragment)
            .chain(tool_fragments.into_iter().map(|sourced| sourced.fragment))
            .collect()
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
enum TestProtocolItem {
    Event(Event),
    Message(TestMessage),
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
enum TestMessage {
    Hello(tau_proto::Hello),
    Subscribe(Subscribe),
    Intercept(Intercept),
    Ready(tau_proto::Ready),
    Disconnect(Disconnect),
    ConfigError(tau_proto::ConfigError),
    Emit(tau_proto::Emit),
    InterceptReply(InterceptReply),
    Configure(tau_proto::Configure),
    InterceptRequest(tau_proto::InterceptRequest),
    LiveDelivery(EventDelivery),
    AgentPromptCreatedResult(Box<tau_proto::AgentPromptCreatedResult>),
    RenderedSystemPromptResult(Box<tau_proto::RenderedSystemPromptResult>),
    RenderedPromptResult(Box<tau_proto::RenderedPromptResult>),
    RenderedToolDefinitionsResult(Box<tau_proto::RenderedToolDefinitionsResult>),
    ExtensionDataResult(Box<tau_proto::ExtensionDataResult>),
}

impl TestProtocolItem {
    fn into_input_message(self) -> HarnessInputMessage {
        match self {
            Self::Event(event) => HarnessInputMessage::emit(event),
            Self::Message(message) => message.into_input_message(),
        }
    }

    fn from_output_message(message: HarnessOutputMessage) -> Self {
        match message {
            HarnessOutputMessage::Configure(message) => {
                Self::Message(TestMessage::Configure(message))
            }
            HarnessOutputMessage::Disconnect(message) => {
                Self::Message(TestMessage::Disconnect(message))
            }
            HarnessOutputMessage::Deliver(delivery) => {
                if !delivery.replay && delivery.recorded_at.is_some() {
                    Self::Message(TestMessage::LiveDelivery(delivery))
                } else {
                    Self::Event(delivery.into_event())
                }
            }
            HarnessOutputMessage::InterceptRequest(message) => {
                Self::Message(TestMessage::InterceptRequest(message))
            }
            HarnessOutputMessage::AgentPromptCreatedResult(message) => {
                Self::Message(TestMessage::AgentPromptCreatedResult(message))
            }
            HarnessOutputMessage::RenderedSystemPromptResult(message) => {
                Self::Message(TestMessage::RenderedSystemPromptResult(message))
            }
            HarnessOutputMessage::RenderedPromptResult(message) => {
                Self::Message(TestMessage::RenderedPromptResult(message))
            }
            HarnessOutputMessage::RenderedToolDefinitionsResult(message) => {
                Self::Message(TestMessage::RenderedToolDefinitionsResult(message))
            }
            HarnessOutputMessage::ExtensionDataResult(message) => {
                Self::Message(TestMessage::ExtensionDataResult(message))
            }
        }
    }

    fn into_event_frame(self) -> Self {
        match self {
            Self::Message(TestMessage::LiveDelivery(delivery)) => {
                Self::Event(delivery.into_event())
            }
            other => other,
        }
    }
}

impl From<TestMessage> for HarnessInputMessage {
    fn from(message: TestMessage) -> Self {
        message.into_input_message()
    }
}

impl TestMessage {
    fn into_input_message(self) -> HarnessInputMessage {
        match self {
            Self::Hello(message) => HarnessInputMessage::Hello(message),
            Self::Subscribe(message) => HarnessInputMessage::Subscribe(message),
            Self::Intercept(message) => HarnessInputMessage::Intercept(message),
            Self::Ready(message) => HarnessInputMessage::Ready(message),
            Self::Disconnect(message) => HarnessInputMessage::Disconnect(message),
            Self::ConfigError(message) => HarnessInputMessage::ConfigError(message),
            Self::Emit(message) => HarnessInputMessage::Emit(message),
            Self::InterceptReply(message) => HarnessInputMessage::InterceptReply(message),
            Self::Configure(_)
            | Self::InterceptRequest(_)
            | Self::LiveDelivery(_)
            | Self::AgentPromptCreatedResult(_)
            | Self::RenderedSystemPromptResult(_)
            | Self::RenderedPromptResult(_)
            | Self::RenderedToolDefinitionsResult(_)
            | Self::ExtensionDataResult(_) => {
                panic!("test frame shim cannot send harness-output message as input")
            }
        }
    }
}

struct TestOutputReader<R> {
    inner: HarnessOutputReader<R>,
}

impl<R> TestOutputReader<R>
where
    R: Read,
{
    fn new(inner: R) -> Self {
        Self {
            inner: HarnessOutputReader::new(inner),
        }
    }

    fn read_frame(&mut self) -> Result<Option<TestProtocolItem>, tau_proto::DecodeError> {
        self.inner
            .read_message()
            .map(|message| message.map(TestProtocolItem::from_output_message))
    }
}

struct TestInputWriter<W> {
    inner: HarnessInputWriter<W>,
}

impl<W> TestInputWriter<W>
where
    W: Write,
{
    fn new(inner: W) -> Self {
        Self {
            inner: HarnessInputWriter::new(inner),
        }
    }

    fn write_frame(&mut self, frame: &TestProtocolItem) -> Result<(), tau_proto::EncodeError> {
        self.inner
            .write_message(&frame.clone().into_input_message())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

trait HarnessTestProtocolExt {
    fn handle_extension_event(
        &mut self,
        source_id: &str,
        frame: TestProtocolItem,
    ) -> Result<(), HarnessError>;

    fn handle_client_event(
        &mut self,
        client_id: &str,
        frame: TestProtocolItem,
    ) -> Result<bool, HarnessError>;
}

impl HarnessTestProtocolExt for Harness {
    fn handle_extension_event(
        &mut self,
        source_id: &str,
        frame: TestProtocolItem,
    ) -> Result<(), HarnessError> {
        self.handle_extension_message(source_id, frame.into_input_message())
    }

    fn handle_client_event(
        &mut self,
        client_id: &str,
        frame: TestProtocolItem,
    ) -> Result<bool, HarnessError> {
        self.handle_client_message(client_id, frame.into_input_message())
    }
}

fn assert_agent_id_chars(agent_id: &str) {
    assert!(!agent_id.is_empty());
    assert!(agent_id.len() <= tau_proto::AGENT_ID_MAX_LEN);
    assert!(
        agent_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    );
}

fn assert_role_hex_agent_id(agent_id: &str, _role: &str) {
    assert_agent_id_chars(agent_id);
}

fn test_discovered_skill(
    source_id: &str,
    description: &str,
    modified_secs: u64,
) -> DiscoveredSkill {
    DiscoveredSkill {
        source_id: source_id.into(),
        description: description.to_owned(),
        source: DiscoveredSkillSource::File(PathBuf::from(format!("/tmp/{description}.md"))),
        add_to_prompt: false,
        user_invocable: true,
        disable_model_invocation: false,
        modified: Some(std::time::UNIX_EPOCH + Duration::from_secs(modified_secs)),
    }
}

fn write_skill_file(dir: &Path, name: &str, description: &str, mtime: Option<u64>) -> PathBuf {
    let path = dir.join(format!("{description}.md"));
    std::fs::write(
        &path,
        format!("---\nname: {name}\ndescription: {description}\n---\n"),
    )
    .expect("write skill file");
    if let Some(mtime) = mtime {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open skill file");
        let modified = std::time::UNIX_EPOCH + Duration::from_secs(mtime);
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .expect("set skill mtime");
    }
    path
}

/// Ensures build timestamps used for built-in skill freshness parse to exact
/// instants and reject malformed inputs before collision comparison.
#[test]
fn build_last_modified_parser_validates_packaged_format() {
    assert_eq!(
        super::parse_build_last_modified("1970-01-01 00:00"),
        Some(std::time::UNIX_EPOCH)
    );
    assert_eq!(
        super::parse_build_last_modified("2024-06-12 09:30"),
        Some(std::time::UNIX_EPOCH + Duration::from_secs(1_718_184_600))
    );
    assert!(super::parse_build_last_modified("2024/06/12 09:30").is_none());
    assert!(super::parse_build_last_modified("2024-1x-12 09:30").is_none());
    assert!(super::parse_build_last_modified("2024-06-aa 09:30").is_none());
    assert!(super::parse_build_last_modified("2024-13-12 09:30").is_none());
    assert!(super::parse_build_last_modified("2024-06-12 24:00").is_none());
}

/// Ensures skill candidate selection chooses the newest timestamp and keeps the
/// earlier candidate when timestamps tie.
#[test]
fn selected_skill_candidate_prefers_newest_with_stable_tie_break() {
    let first = test_discovered_skill("first", "first", 100);
    let newer = test_discovered_skill("newer", "newer", 200);
    let same_as_first = test_discovered_skill("same", "same", 100);

    let candidates = [first.clone(), newer];
    let selected = super::selected_skill_candidate(&candidates).expect("selected newest");
    assert_eq!(selected.description, "newer");

    let candidates = [first, same_as_first];
    let selected = super::selected_skill_candidate(&candidates).expect("selected tie");
    assert_eq!(selected.description, "first");
}

/// Ensures the harness keeps fallback candidates so disconnecting the newest
/// skill provider restores the next-best skill instead of losing the name.
#[test]
fn skill_winner_disconnect_restores_next_best_candidate() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let name = tau_proto::SkillName::new("same-skill");
    let older = test_discovered_skill("old-ext", "older", 100);
    let newer = test_discovered_skill("new-ext", "newer", 200);

    h.discovered_skill_candidates
        .insert(name.clone(), vec![older, newer]);
    h.recompute_discovered_skill_winner(&name);
    assert_eq!(h.discovered_skills[&name].description, "newer");

    h.remove_discovered_context("new-ext");
    assert_eq!(h.discovered_skills[&name].description, "older");

    h.shutdown().expect("shutdown");
}

/// Ensures cross-source skill collisions emit useful trace diagnostics for both
/// replacement by newer mtimes and ignoring equal/unavailable timestamps
/// without becoming mandatory visible notices.
#[test]
fn skill_collision_diagnostics_describe_replaced_and_ignored_candidates() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let old_path = write_skill_file(tmp.path(), "collision-skill", "old", Some(100));
    let new_path = write_skill_file(tmp.path(), "collision-skill", "new", Some(200));
    let tie_path = write_skill_file(tmp.path(), "collision-skill", "tie", Some(200));

    h.record_discovered_skill(
        "old-ext",
        &tau_proto::ExtSkillAvailable {
            name: "collision-skill".into(),
            description: "old".to_owned(),
            file_path: old_path,
            add_to_prompt: false,
            user_invocable: true,
            disable_model_invocation: false,
            argument_hint: None,
        },
    );
    h.record_discovered_skill(
        "new-ext",
        &tau_proto::ExtSkillAvailable {
            name: "collision-skill".into(),
            description: "new".to_owned(),
            file_path: new_path,
            add_to_prompt: false,
            user_invocable: true,
            disable_model_invocation: false,
            argument_hint: None,
        },
    );
    h.record_discovered_skill(
        "tie-ext",
        &tau_proto::ExtSkillAvailable {
            name: "collision-skill".into(),
            description: "tie".to_owned(),
            file_path: tie_path,
            add_to_prompt: false,
            user_invocable: true,
            disable_model_invocation: false,
            argument_hint: None,
        },
    );

    let infos = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::HarnessNotice(info) => Some(info),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(infos.iter().any(|info| {
        info.kind == tau_proto::notice_kind::SKILL_COLLISION
            && info.level == tau_proto::NoticeLevel::Trace
            && !info.always_show
            && info.message.contains("skill collision: collision-skill")
            && info.message.contains("replaces")
            && info.message.contains("newer modified time")
    }));
    assert!(infos.iter().any(|info| {
        info.kind == tau_proto::notice_kind::SKILL_COLLISION
            && info.level == tau_proto::NoticeLevel::Trace
            && !info.always_show
            && info.message.contains("skill collision: collision-skill")
            && info.message.contains("ignored")
            && info.message.contains("same or unavailable modified time")
    }));

    h.shutdown().expect("shutdown");
}

/// Ensures user slash invocation expands to a Pi-style prompt block and appends
/// opaque arguments without making disabled-model skills unavailable to users.
#[test]
fn user_skill_command_expands_prompt_block() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let path = tmp.path().join("manual.md");
    std::fs::write(
        &path,
        "---\nname: manual\ndescription: Manual\ndisable-model-invocation: true\n---\nUse manual steps.\n",
    )
    .expect("write skill");
    h.record_discovered_skill(
        "ext",
        &tau_proto::ExtSkillAvailable {
            name: "manual".into(),
            description: "Manual".to_owned(),
            file_path: path.clone(),
            add_to_prompt: true,
            user_invocable: true,
            disable_model_invocation: true,
            argument_hint: Some("[topic]".to_owned()),
        },
    );

    let expanded = h
        .expand_user_skill_command("/skill manual do this")
        .expect("expanded");
    assert!(expanded.contains("<skill name=\"manual\" location="));
    assert!(expanded.contains("References are relative to"));
    assert!(expanded.contains("Use manual steps."));
    assert!(expanded.ends_with("</skill>\n\ndo this"));
}

/// Ensures `disable-model-invocation` is treated as manual-only rather than
/// unreachable, even if an extension also sends `user-invocable: false`.
#[test]
fn disable_model_invocation_implies_user_invocable() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let path = tmp.path().join("manual-only.md");
    std::fs::write(
        &path,
        "---\nname: manual-only\ndescription: Manual only\n---\nManual only body.\n",
    )
    .expect("write skill");
    h.record_discovered_skill(
        "ext",
        &tau_proto::ExtSkillAvailable {
            name: "manual-only".into(),
            description: "Manual only".to_owned(),
            file_path: path,
            add_to_prompt: true,
            user_invocable: false,
            disable_model_invocation: true,
            argument_hint: None,
        },
    );

    assert!(h.discovered_skills["manual-only"].user_invocable);
    assert!(h.discovered_skills["manual-only"].disable_model_invocation);
    assert!(
        h.expand_user_skill_command("/skill manual-only")
            .expect("expanded")
            .contains("Manual only body.")
    );
}

/// Ensures the harness normalizes manual-only skill policy before publishing
/// availability events that UI clients use for `/skill` completion.
#[test]
fn published_skill_event_normalizes_disable_model_invocation() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let path = tmp.path().join("manual-event.md");
    std::fs::write(
        &path,
        "---\nname: manual-event\ndescription: Manual event\n---\nManual event body.\n",
    )
    .expect("write skill");

    h.publish_extension_skill_available(
        "ext",
        tau_proto::ExtSkillAvailable {
            name: "manual-event".into(),
            description: "Manual event".to_owned(),
            file_path: path,
            add_to_prompt: false,
            user_invocable: false,
            disable_model_invocation: true,
            argument_hint: Some("<task>".to_owned()),
        },
    );

    let published = event_log_events(&h)
        .into_iter()
        .find_map(|event| match event {
            Event::ExtSkillAvailable(skill) if skill.name.as_str() == "manual-event" => Some(skill),
            _ => None,
        })
        .expect("published skill event");
    assert!(published.user_invocable);
    assert!(published.disable_model_invocation);
    assert_eq!(published.argument_hint.as_deref(), Some("<task>"));
}

/// Ensures non-user-invocable skill commands are rejected before any prompt is
/// submitted to the model path.
#[test]
fn user_skill_command_rejects_non_user_invocable_skill() {
    let tmp = TempDir::new().expect("tempdir");
    let mut h = echo_harness(tmp.path()).expect("harness");
    let path = tmp.path().join("hidden.md");
    std::fs::write(
        &path,
        "---\nname: hidden\ndescription: Hidden\n---\nHidden body.\n",
    )
    .expect("write skill");
    h.record_discovered_skill(
        "ext",
        &tau_proto::ExtSkillAvailable {
            name: "hidden".into(),
            description: "Hidden".to_owned(),
            file_path: path,
            add_to_prompt: false,
            user_invocable: false,
            disable_model_invocation: false,
            argument_hint: None,
        },
    );

    assert!(h.expand_user_skill_command("/skill hidden").is_none());
    let infos = event_log_events(&h)
        .into_iter()
        .filter_map(|event| match event {
            Event::HarnessNotice(info) => Some(info.message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        infos
            .iter()
            .any(|message| message.contains("not user-invocable"))
    );
}

#[test]
fn extension_data_paths_reject_escape_components() {
    assert!(sanitize_extension_data_path("notes/file.txt", false).is_ok());
    assert!(sanitize_extension_data_path("", true).is_ok());
    assert_eq!(
        sanitize_extension_data_path("", false)
            .expect_err("empty file path")
            .kind,
        tau_proto::ExtensionDataErrorKind::InvalidPath
    );
    assert_eq!(
        sanitize_extension_data_path("../secret", false)
            .expect_err("parent escape")
            .kind,
        tau_proto::ExtensionDataErrorKind::InvalidPath
    );
    assert!(sanitize_extension_data_path("notes/../secret", false).is_err());
    assert!(sanitize_extension_data_path("/tmp/secret", false).is_err());
    assert!(sanitize_extension_data_path("./secret", false).is_err());
}

#[test]
fn extension_data_list_skips_symlinks_and_returns_relative_entries() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(root.join("nested")).expect("mkdir");
    std::fs::write(root.join("file.txt"), b"abc").expect("write file");
    #[cfg(unix)]
    std::os::unix::fs::symlink("/tmp", root.join("outside")).expect("symlink");

    let entries = list_extension_data_entries(&root, &root).expect("list entries");
    assert!(
        entries.iter().any(|entry| entry.path.as_str() == "file.txt"
            && !entry.is_dir
            && entry.len == Some(3))
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.path.as_str() == "nested" && entry.is_dir)
    );
    assert!(!entries.iter().any(|entry| entry.path.as_str() == "outside"));
}

#[test]
fn extension_data_checked_path_rejects_symlink_leaf_and_ancestor() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).expect("mkdir root");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/tmp", root.join("leaf")).expect("leaf symlink");
        std::os::unix::fs::symlink("/tmp", root.join("parent")).expect("parent symlink");
        assert!(checked_extension_data_path(&root, Path::new("leaf"), false).is_err());
        assert!(checked_extension_data_path(&root, Path::new("parent/file"), true).is_err());
    }
}
#[test]
fn extension_data_checked_path_rejects_symlink_root() {
    let tmp = TempDir::new().expect("tempdir");
    let real = tmp.path().join("real");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&real).expect("mkdir real");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755))
            .expect("chmod real");
        std::os::unix::fs::symlink(&real, &root).expect("root symlink");
        assert!(checked_extension_data_path(&root, Path::new("file"), true).is_err());
        assert!(checked_extension_data_path(&root, Path::new(""), true).is_err());
        let real_mode = std::fs::metadata(&real)
            .expect("real metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(real_mode, 0o755);
    }
}
#[test]
fn extension_data_file_helpers_create_append_replace_delete_private_files() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("root");
    let file = root.join("nested/file.txt");

    create_extension_data_file(&file, b"first").expect("create file");
    assert_eq!(std::fs::read(&file).expect("read created"), b"first");
    let duplicate = create_extension_data_file(&file, b"second").expect_err("duplicate");
    assert_eq!(duplicate.kind(), std::io::ErrorKind::AlreadyExists);

    append_extension_data_file(&file, b"\nappended").expect("append file");
    assert_eq!(
        std::fs::read(&file).expect("read appended"),
        b"first\nappended"
    );

    atomic_replace_extension_data_file(&file, b"replaced").expect("replace file");
    assert_eq!(std::fs::read(&file).expect("read replaced"), b"replaced");
    let renamed = root.join("nested/renamed.txt");
    rename_extension_data_file(&file, &renamed).expect("rename file");
    assert!(!file.exists());
    assert_eq!(std::fs::read(&renamed).expect("read renamed"), b"replaced");

    delete_extension_data_file(&renamed).expect("delete file");
    assert!(!renamed.exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let dir_mode = std::fs::metadata(root.join("nested"))
            .expect("nested metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        create_extension_data_file(&file, b"private").expect("recreate file");
        let file_mode = std::fs::metadata(&file)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
    }
}
#[test]
fn minted_agent_ids_use_default_random_alphanumeric_template() {
    let agent_id = mint_agent_id_for_role("engineer");

    assert_eq!(agent_id.len(), 6);
    assert_agent_id_chars(&agent_id);
}

#[test]
fn minted_agent_ids_use_deterministic_test_rng_sequence() {
    // Harness tests install a fixed RNG seed. The sequence should be stable
    // across harnesses while still advancing between agent creations.
    let mint_pair = || {
        let tmp = TempDir::new().expect("tempdir");
        let mut h = echo_harness(tmp.path()).expect("harness");
        let role = h.selected_role.clone();
        let first = h.create_durable_user_agent(&role);
        let second = h.create_durable_user_agent(&role);
        (first.to_string(), second.to_string())
    };

    let first_run = mint_pair();
    let second_run = mint_pair();

    assert_eq!(first_run, second_run);
    assert_ne!(first_run.0, first_run.1);
}

#[test]
fn minting_agent_ids_renders_configured_template() {
    let mut warnings = Vec::new();
    let agent_id = super::mint_available_agent_id_for_role_with(
        "engineer",
        "engineer",
        "{{role}}-{{random_alphanumeric 6}}",
        |_| false,
        &mut deterministic_agent_id_rng(),
        |kind, warning| warnings.push((kind, warning)),
    );

    assert!(agent_id.starts_with("engineer-"));
    assert_eq!(agent_id.len(), "engineer-".len() + 6);
    assert_agent_id_chars(&agent_id);
    assert!(warnings.is_empty());
}

#[test]
fn minting_agent_ids_renders_role_group_in_configured_template() {
    // Agent ID templates can include the navigation role group so related
    // roles share an ID prefix while still retaining the exact role name.
    let mut warnings = Vec::new();
    let agent_id = super::mint_available_agent_id_for_role_with(
        "staff-engineer",
        "engineer",
        "{{role_group}}-{{role}}-{{random_alphanumeric 4}}",
        |_| false,
        &mut deterministic_agent_id_rng(),
        |kind, warning| warnings.push((kind, warning)),
    );

    assert!(agent_id.starts_with("engineer-staff-engineer-"));
    assert_eq!(agent_id.len(), "engineer-staff-engineer-".len() + 4);
    assert_agent_id_chars(&agent_id);
    assert!(warnings.is_empty());
}

#[test]
fn minting_agent_ids_reject_display_name_only_template_fields() {
    // Agent ID templates must stay strict and only expose ID-safe context.
    // Display-name-only fields would otherwise silently render as empty strings.
    let mut warnings = Vec::new();
    let agent_id = super::mint_available_agent_id_for_role_with(
        "engineer",
        "engineer",
        "{{role}}-{{task_name}}",
        |_| false,
        &mut deterministic_agent_id_rng(),
        |kind, warning| warnings.push((kind, warning)),
    );

    assert_eq!(agent_id.len(), 6);
    assert_agent_id_chars(&agent_id);
    assert!(matches!(
        warnings.as_slice(),
        [(
            super::AgentIdTemplateKind::Configured,
            super::AgentIdMintWarning::RenderFailed { .. }
        )]
    ));
}

#[test]
fn agent_template_uses_role_when_task_name_is_absent() {
    let mut rng = deterministic_agent_id_rng();
    let rendered = super::render_agent_template(
        "{{#if task_name_present}}{{role}}: {{task_name}}{{else}}{{role}}{{/if}}",
        "staff-engineer",
        "engineer",
        "engineer-Ab12",
        None,
        0,
        &mut rng,
    )
    .expect("render");

    assert_eq!(rendered, "staff-engineer");
}

#[test]
fn agent_template_renders_display_name_context() {
    let mut rng = deterministic_agent_id_rng();
    let rendered = super::render_agent_template(
        "{{role_group}}/{{role}}/{{agent_id}}/{{task_name}}/{{task_name_present}}/{{random_alphanumeric 4}}",
        "staff-engineer",
        "engineer",
        "engineer-Ab12",
        Some("review fix"),
        0,
        &mut rng,
    )
    .expect("render");

    assert!(rendered.starts_with("engineer/staff-engineer/engineer-Ab12/review fix/true/"));
    assert_eq!(
        rendered.len(),
        "engineer/staff-engineer/engineer-Ab12/review fix/true/".len() + 4
    );
}

#[test]
fn minting_agent_ids_falls_back_immediately_on_invalid_rendered_id() {
    // Invalid configured output must not be retried; it falls back to the safe
    // default template and reports a warning the harness can surface to users.
    let mut warnings = Vec::new();
    let agent_id = super::mint_available_agent_id_for_role_with(
        "engineer",
        "engineer",
        "bad/id",
        |_| false,
        &mut deterministic_agent_id_rng(),
        |kind, warning| warnings.push((kind, warning)),
    );

    assert_eq!(agent_id.len(), 6);
    assert_agent_id_chars(&agent_id);
    assert!(matches!(
        warnings.as_slice(),
        [(
            super::AgentIdTemplateKind::Configured,
            super::AgentIdMintWarning::InvalidRendered { .. }
        )]
    ));
}

#[test]
fn minting_agent_ids_falls_back_after_configured_template_collisions() {
    // A configured template that keeps producing a reserved id should not loop
    // forever. After the configured attempt budget, minting falls back to the
    // default random template.
    let mut warnings = Vec::new();
    let agent_id = super::mint_available_agent_id_for_role_with(
        "engineer",
        "engineer",
        "taken",
        |agent_id| agent_id == "taken",
        &mut deterministic_agent_id_rng(),
        |kind, warning| warnings.push((kind, warning)),
    );

    assert_ne!(agent_id, "taken");
    assert_agent_id_chars(&agent_id);
    assert!(warnings.iter().any(|(kind, warning)| matches!(
        (kind, warning),
        (
            super::AgentIdTemplateKind::Configured,
            super::AgentIdMintWarning::CollisionsExceeded { attempts }
        ) if *attempts == super::AGENT_ID_TEMPLATE_COLLISION_ATTEMPTS
    )));
}

#[test]
fn minting_agent_ids_skips_persisted_agent_dirs() {
    // A rendered id already present on disk must stay reserved even when the
    // lazy store has not loaded that agent tree into memory yet.
    let td = TempDir::new().expect("tempdir");
    let agents_dir = td.path().join("agents");
    let store = AgentStore::open_lazy(agents_dir.clone()).expect("agent store");
    let reserved_dir = agents_dir.join("engineer_0");
    std::fs::create_dir_all(&reserved_dir).expect("agent dir");
    std::fs::write(reserved_dir.join("meta.json"), "{}").expect("agent meta");

    let mut warnings = Vec::new();
    let agent_id = super::mint_available_agent_id_for_role_with(
        "engineer",
        "engineer",
        "engineer_0",
        |agent_id| store.agent_exists(agent_id),
        &mut deterministic_agent_id_rng(),
        |kind, warning| warnings.push((kind, warning)),
    );

    assert_ne!(agent_id, "engineer_0");
    assert_agent_id_chars(&agent_id);
    assert!(warnings.iter().any(|(kind, warning)| matches!(
        (kind, warning),
        (
            super::AgentIdTemplateKind::Configured,
            super::AgentIdMintWarning::CollisionsExceeded { .. }
        )
    )));
}

/// Ensures the rendered config self-knowledge page includes the generated
/// default config blocks and current runtime path shape.
#[test]
fn render_self_knowledge_config_content_inserts_config_defaults() {
    let rendered = crate::harness::render_self_knowledge_config_content();

    assert!(!rendered.contains("{harness_config}"));
    assert!(!rendered.contains("{ui_config}"));
    assert!(rendered.contains("${XDG_RUNTIME_DIR}/tau/harnesses/"));
    assert!(rendered.contains("agent_retention_days: 60"));
    assert!(rendered.contains("show_thinking: true"));
    assert!(rendered.contains("{{role_group}}-{{random_alphanumeric 4}}"));
    assert!(rendered.contains("{{role_group}}: {{task_name}}"));
    assert!(
        rendered
            .contains("{{#if task_name_present}}{{role}}: {{task_name}}{{else}}{{role}}{{/if}}")
    );
}

#[test]
fn render_self_knowledge_pim_content_inserts_config_defaults() {
    let rendered = crate::harness::render_self_knowledge_pim_content();

    assert!(!rendered.contains("{pim_config}"));
    assert!(rendered.contains("std-pim:"));
    assert!(rendered.contains("calendar:"));
}

fn agent_tree_for_conversation<'a>(h: &'a Harness, cid: &AgentId) -> &'a AgentTree {
    let agent_id = h
        .agents
        .get(cid)
        .and_then(|conv| conv.agent_id.as_deref())
        .expect("conversation has agent id");
    h.agent_store.agent(agent_id).expect("agent tree")
}

fn ensure_test_user_agent(h: &mut Harness) -> AgentId {
    let cid = h
        .agents
        .iter()
        .find_map(|(cid, conv)| conv.originator.is_user().then_some(cid.clone()))
        .unwrap_or_else(|| {
            let role = h.selected_role.clone();
            h.create_durable_user_agent(&role)
        });
    // Most harness unit tests use this helper to focus on tool/provider state,
    // not extension-provided prompt context. Treat the synthetic agent as if
    // registered context providers have already acknowledged it; tests that
    // exercise context readiness drive `agent.loaded` explicitly.
    if let Some(agent_id) = h
        .agents
        .get(&cid)
        .and_then(|conv| conv.agent_id.as_deref())
        .map(crate::parse_agent_id)
    {
        h.pending_agent_context_ready.remove(&agent_id);
    }
    cid
}

fn test_user_agent(h: &Harness) -> AgentId {
    h.agents
        .iter()
        .find_map(|(cid, conv)| conv.originator.is_user().then_some(cid.clone()))
        .expect("test should create a user agent first")
}

fn durable_agent_id_for_conversation(h: &Harness, cid: &AgentId) -> tau_proto::AgentId {
    crate::parse_agent_id(
        h.agents
            .get(cid)
            .and_then(|conv| conv.agent_id.clone())
            .expect("conversation has durable agent id"),
    )
}

fn default_agent_tree(h: &Harness) -> &AgentTree {
    let cid = test_user_agent(h);
    agent_tree_for_conversation(h, &cid)
}

fn agent_branch_for_conversation<'a>(h: &'a Harness, cid: &AgentId) -> Vec<&'a AgentEntry> {
    let head = h.agents.get(cid).and_then(|conv| conv.head);
    agent_tree_for_conversation(h, cid).branch_from(head)
}

fn default_agent_branch(h: &Harness) -> Vec<&AgentEntry> {
    let cid = test_user_agent(h);
    agent_branch_for_conversation(h, &cid)
}

fn default_agent_node(h: &Harness, id: NodeId) -> &tau_core::AgentNode {
    default_agent_tree(h).node(id).expect("agent node")
}

fn event_log_events(h: &Harness) -> Vec<Event> {
    let mut events = Vec::new();
    let mut seq = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq.next();
        events.push(entry.event);
    }
    events
}

fn loaded_agent_events(h: &Harness) -> Vec<Event> {
    let loaded_agents: Vec<_> = h.agent_routes.keys().cloned().collect();
    loaded_agents
        .into_iter()
        .filter_map(|agent_id| h.agent_store.agent_events(agent_id.as_str()).ok())
        .flatten()
        .map(|entry| entry.event)
        .collect()
}

fn persisted_agent_branch(state_dir: &Path) -> Vec<AgentEntry> {
    persisted_agent_branches(state_dir)
        .into_iter()
        .next()
        .expect("loaded agent")
}

fn persisted_agent_branches(state_dir: &Path) -> Vec<Vec<AgentEntry>> {
    let mut agent_store = AgentStore::open(state_dir.join("agents")).expect("agent store");
    std::fs::read_dir(state_dir.join("agents"))
        .expect("agents dir")
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .map(|agent_id| {
            let tree = agent_store
                .load_agent(agent_id.as_str())
                .expect("load agent")
                .expect("agent tree");
            tree.current_branch().into_iter().cloned().collect()
        })
        .collect()
}

/// Test-only helper that appends a user message through the harness's normal
/// agent-transcript publish path without driving a provider turn.
fn append_user_message_via_event(h: &mut Harness, text: &str) {
    let cid = ensure_test_user_agent(h);
    h.publish_pending_prompt_for_agent(&cid, PendingPrompt::user(text.to_owned()))
        .expect("append user message");
}

fn send_prompt_to_test_agent(h: &mut Harness) -> AgentPromptId {
    let cid = test_user_agent(h);
    h.send_prompt_to_agent_for(&cid).expect("agent prompt id")
}

fn read_agent_prompt_created(h: &Harness, spid: &AgentPromptId) -> Option<AgentPromptCreated> {
    let mut cursor = crate::event_log::EventLogSeq::new(0);
    while let Some(entry) = h.event_log.get_next_from(cursor) {
        cursor = entry.seq.next();
        if let Event::AgentPromptCreated(prompt) = entry.event
            && &prompt.agent_prompt_id == spid
        {
            return Some(prompt);
        }
    }
    None
}

fn load_test_agent(h: &mut Harness, agent_id: &str) {
    let agent_id = tau_proto::AgentId::parse(agent_id).expect("agent id");
    h.agent_store
        .append_agent_event(
            agent_id.as_str(),
            Some(HARNESS_CONNECTION_ID.into()),
            Event::AgentStarted(tau_proto::AgentStarted {
                agent_id: agent_id.clone(),
                parent_agent: None,
                role: "senior-engineer".to_owned(),
                display_name: None,
                metadata: Vec::new(),
            }),
        )
        .expect("seed agent.started");
    h.handle_agent_load(tau_proto::AgentLoad { agent_id })
        .expect("load test agent");
}

fn echo_harness(state_dir: impl Into<PathBuf>) -> Result<Harness, HarnessError> {
    echo_harness_for(state_dir)
}

fn echo_harness_for(state_dir: impl Into<PathBuf>) -> Result<Harness, HarnessError> {
    let state_dir = state_dir.into();
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(state_dir.join("config")),
        state_dir: Some(state_dir.join("runtime")),
    };
    echo_harness_with_dirs(state_dir, dirs)
}

fn echo_harness_with_dirs(
    state_dir: impl Into<PathBuf>,
    dirs: tau_config::settings::TauDirs,
) -> Result<Harness, HarnessError> {
    fn shell_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        tau_ext_shell::run(r, w).map_err(|e| e.to_string())
    }
    let mut h = Harness::new_with_provider(
        state_dir,
        dirs,
        echo_runner,
        vec![crate::harness::InProcessTool {
            name: "shell",
            runner: shell_runner,
        }],
    )?;
    h.agent_id_rng = deterministic_agent_id_rng();
    h.enable_echo_tool_for_tests();
    // not let its startup context-provider registration defer unrelated prompt
    // dispatch assertions; readiness-specific tests register providers directly.
    h.agent_context_providers.clear();
    h.pending_agent_context_ready.clear();
    Ok(h)
}

fn quiet_provider_harness(state_dir: impl Into<PathBuf>) -> Result<Harness, HarnessError> {
    fn quiet_provider_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        fn inner(r: UnixStream, w: UnixStream) -> Result<(), Box<dyn std::error::Error>> {
            let mut reader = TestOutputReader::new(BufReader::new(r));
            let mut writer = TestInputWriter::new(BufWriter::new(w));

            writer.write_frame(&TestProtocolItem::Message(TestMessage::Hello(
                tau_proto::Hello {
                    protocol_version: tau_proto::PROTOCOL_VERSION,
                    client_name: "tau-quiet-provider".into(),
                    client_kind: tau_proto::ClientKind::Provider,
                },
            )))?;
            writer.write_frame(&TestProtocolItem::Event(Event::ProviderModelsUpdated(
                tau_proto::ProviderModelsUpdated {
                    models: vec![tau_proto::ProviderModelInfo {
                        id: "test/model".into(),
                        display_name: Some("Test".to_owned()),
                        tags: Vec::new(),
                        default_affinity: 0,
                        context_window: 1_000,
                        efforts: vec![tau_proto::Effort::Medium],
                        verbosities: vec![tau_proto::Verbosity::Medium],
                        thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
                        supports_compaction: true,
                    }],
                },
            )))?;
            writer.write_frame(&TestProtocolItem::Message(TestMessage::Ready(
                tau_proto::Ready {
                    message: Some("quiet provider ready".to_owned()),
                },
            )))?;
            writer.flush()?;

            while let Some(frame) = reader.read_frame()? {
                let frame = frame.into_event_frame();
                if matches!(frame, TestProtocolItem::Message(TestMessage::Disconnect(_))) {
                    return Ok(());
                }
            }
            Ok(())
        }

        inner(r, w).map_err(|e| e.to_string())
    }

    let state_dir = state_dir.into();
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(state_dir.join("config")),
        state_dir: Some(state_dir.join("runtime")),
    };
    let mut h = Harness::new_with_provider(state_dir, dirs, quiet_provider_runner, Vec::new())?;
    h.agent_id_rng = deterministic_agent_id_rng();
    Ok(h)
}

struct TestSink {
    events: Arc<Mutex<Vec<RoutedFrame>>>,
}

impl ConnectionSink for TestSink {
    fn send(&mut self, event: RoutedFrame) -> Result<(), ConnectionSendError> {
        self.events.lock().expect("sink mutex").push(event);
        Ok(())
    }
}

fn connect_test_client(
    h: &mut Harness,
    name: &str,
    kind: tau_proto::ClientKind,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    h.bus.connect(Connection::new(
        ConnectionMetadata {
            id: name.into(),
            name: name.to_owned(),
            kind,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(TestSink {
            events: Arc::clone(&events),
        }),
    ));
    events
}

fn connect_test_tool(h: &mut Harness, name: &str) -> Arc<Mutex<Vec<RoutedFrame>>> {
    connect_test_client(h, name, tau_proto::ClientKind::Tool)
}

/// Pre-seed the per-conversation `AgentThinking` state for tests that
/// bypass `dispatch_prompt_for_agent` and call response handlers
/// directly.
fn seed_agent_thinking(h: &mut Harness, cid: &crate::AgentId, spid: &str) {
    // Tests that bypass prompt dispatch still need the same loaded-agent and
    // runtime-membership side effects that a real dispatch would establish.
    let agent_id = h
        .ensure_agent_id_for_agent(cid)
        .expect("conversation agent id");
    let conv = h.agents.get(cid).expect("conversation present");
    let role = h.role_name_for_agent(conv).to_owned();
    let model = h
        .model_for_agent_role(conv)
        .or_else(|| h.selected_model.clone());
    let tool_specs = h.gather_effective_tool_specs_for_role_model(&role, model.as_ref());
    h.prompt_tool_specs.insert(spid.into(), tool_specs);
    let conv = h.agents.get_mut(cid).expect("conversation present");
    if let Some(next_index) = spid
        .rsplit_once('-')
        .and_then(|(_, index)| index.parse::<u64>().ok())
        .map(|index| index.saturating_add(1))
    {
        conv.next_prompt_index = conv.next_prompt_index.max(next_index);
    }
    conv.turn_state = AgentTurnState::AgentThinking {
        agent_prompt_id: spid.into(),
    };
    h.agent_routes.insert(agent_id.clone(), cid.clone());
    h.agent_states.insert(agent_id, AgentState::Active);
    if let Some(model) = model {
        h.prompt_models.insert(spid.into(), model);
    }
}

/// Pre-seed the per-conversation `ToolsRunning` state for tests that
/// bypass the agent-response path and call tool handlers directly.
fn seed_tools_running(h: &mut Harness, cid: &crate::AgentId, remaining: Vec<ToolCallId>) {
    h.agents
        .get_mut(cid)
        .expect("conversation present")
        .turn_state = AgentTurnState::ToolsRunning {
        remaining_calls: remaining,
    };
}

/// Seed the transcript and turn state as if the assistant had just
/// emitted one or more tool calls for this conversation.
fn seed_assistant_tool_round(h: &mut Harness, cid: &crate::AgentId, calls: &[(&str, &str)]) {
    let agent_id = h
        .agents
        .get(cid)
        .and_then(|conv| conv.agent_id.clone())
        .unwrap_or_else(|| "main".to_owned());
    h.publish_for_agent(
        cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            agent_prompt_id: "sp-seeded-tools".into(),
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: calls
                .iter()
                .map(|(call_id, tool_name)| {
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: (*call_id).into(),
                        name: ToolName::new(*tool_name),
                        tool_type: tau_proto::ToolType::Function,
                        arguments: CborValue::Map(Vec::new()),
                    })
                })
                .collect(),
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            error: None,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    seed_tools_running(
        h,
        cid,
        calls.iter().map(|(call_id, _)| (*call_id).into()).collect(),
    );
}

/// Pumps the harness event loop until the named tool call's result
/// or error is received and handled. Panics on timeout.
fn drive_harness_until_call_completes(h: &mut Harness, target_call_id: &str) {
    let started = Instant::now();
    loop {
        if started.elapsed() >= Duration::from_secs(3) {
            panic!("timed out waiting for {target_call_id} to complete");
        }
        let event =
            h.rx.recv_timeout(Duration::from_secs(1))
                .expect("tool result should arrive");
        match event {
            HarnessEvent::FromConnection {
                connection_id,
                message,
            } => {
                let is_target = match message.as_ref() {
                    HarnessInputMessage::Emit(emit) => match emit.event.as_ref() {
                        Event::ToolResult(r) => r.call_id.as_str() == target_call_id,
                        Event::ToolError(e) => e.call_id.as_str() == target_call_id,
                        _ => false,
                    },
                    _ => false,
                };
                h.handle_extension_message(&connection_id, *message)
                    .expect("handle");
                if is_target {
                    return;
                }
            }
            HarnessEvent::Disconnected { connection_id } => {
                h.handle_disconnect(&connection_id);
            }
            HarnessEvent::NewClient(_) => {}
            HarnessEvent::Command(command) => h.handle_harness_command(command).expect("handle"),
        }
    }
}

fn drive_harness_until_tool_turn_empty(h: &mut Harness) {
    let started = Instant::now();
    loop {
        if h.tool_turn.is_empty() {
            return;
        }
        if started.elapsed() >= Duration::from_secs(3) {
            panic!("timed out waiting for tool turn to empty");
        }
        let event =
            h.rx.recv_timeout(Duration::from_secs(1))
                .expect("tool result should arrive");
        match event {
            HarnessEvent::FromConnection {
                connection_id,
                message,
            } => h
                .handle_extension_message(&connection_id, *message)
                .expect("handle"),
            HarnessEvent::Disconnected { connection_id } => {
                h.handle_disconnect(&connection_id);
            }
            HarnessEvent::NewClient(_) => {}
            HarnessEvent::Command(command) => h.handle_harness_command(command).expect("handle"),
        }
    }
}

/// Find the conversation id of the outer side conversation (the one
/// whose originator is the delegate extension's first query). Used by
/// the cross-conversation regression test above to disambiguate
/// nested-vs-outer side prompt ids.
fn outer_side_cid_str(h: &Harness) -> &str {
    h.agents
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. }
                    if query_id == "q-outer"
            )
            .then_some(cid.as_str())
        })
        .unwrap_or("")
}

/// Subscribe a fresh test sink to `tool.delegate_progress` events and
/// hand back its accumulator.
fn collect_event_sink(h: &mut Harness) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let events = connect_test_tool(h, "test-delegate-progress-sink");
    h.bus
        .set_subscriptions(
            "test-delegate-progress-sink",
            vec![tau_proto::EventSelector::Exact(
                tau_proto::EventName::TOOL_DELEGATE_PROGRESS,
            )],
        )
        .expect("subscribe");
    events
}

/// Peel a routed message to its bus-event payload. Returns `None` for
/// non-event output messages (configure, intercept request, …).
fn peel_inner_event(message: &HarnessOutputMessage) -> Option<&Event> {
    message.delivered_event()
}

fn pop_delegate_progress(
    sink: &Arc<Mutex<Vec<RoutedFrame>>>,
    call_id: &str,
) -> Option<tau_proto::DelegateProgress> {
    let mut events = sink.lock().expect("sink");
    let pos = events.iter().position(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::ToolDelegateProgress(p)) if p.call_id.as_str() == call_id
        )
    })?;
    let removed = events.remove(pos);
    match removed.frame.into_delivered_event() {
        Some(Event::ToolDelegateProgress(p)) => Some(p),
        _ => unreachable!(),
    }
}

fn drain_delegate_progress(
    sink: &Arc<Mutex<Vec<RoutedFrame>>>,
    call_id: &str,
) -> Vec<tau_proto::DelegateProgress> {
    let mut events = sink.lock().expect("sink");
    let mut out = Vec::new();
    events.retain(|routed| match peel_inner_event(&routed.frame) {
        Some(Event::ToolDelegateProgress(p)) if p.call_id.as_str() == call_id => {
            out.push(p.clone());
            false
        }
        _ => true,
    });
    out
}

fn read_raw_prompt_created(h: &Harness, spid: &AgentPromptId) -> AgentPromptCreated {
    let mut cursor = crate::event_log::EventLogSeq::new(0);
    loop {
        let entry = h
            .event_log
            .get_next_from(cursor)
            .expect("prompt event in log");
        cursor = entry.seq.next();
        match entry.event {
            Event::AgentPromptCreated(prompt) if &prompt.agent_prompt_id == spid => {
                return prompt;
            }
            _ => {}
        }
    }
}

fn read_nth_prompt_created(h: &Harness, index: usize) -> AgentPromptCreated {
    let mut cursor = crate::event_log::EventLogSeq::new(0);
    let mut seen = 0;
    loop {
        let entry = h
            .event_log
            .get_next_from(cursor)
            .expect("prompt event in log");
        cursor = entry.seq.next();
        if let Event::AgentPromptCreated(prompt) = entry.event {
            if seen == index {
                return read_agent_prompt_created(h, &prompt.agent_prompt_id)
                    .expect("materialized prompt event");
            }
            seen += 1;
        }
    }
}

fn read_prompt_created(h: &Harness, spid: &AgentPromptId) -> AgentPromptCreated {
    read_agent_prompt_created(h, spid).expect("materialized prompt event")
}

fn intercepted_payload(events: &Arc<Mutex<Vec<RoutedFrame>>>) -> Event {
    let events = events.lock().expect("events mutex");
    let intercepted = events
        .iter()
        .find_map(|routed| match &routed.frame {
            HarnessOutputMessage::InterceptRequest(req) => Some(req),
            _ => None,
        })
        .expect("intercept request delivered");
    (*intercepted.event).clone()
}

fn draft_event(text: &str) -> Event {
    Event::UiPromptDraft(UiPromptDraft {
        text: text.to_owned(),
    })
}

mod action;
mod dedup;
mod dispatch;
mod format;
mod interception;
mod lifecycle;
mod mode;
mod model;
mod replay;
