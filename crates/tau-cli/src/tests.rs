use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use tau_cli_term::TermHandle;
use tau_cli_term_raw::{Color, Term};
use tau_proto::{
    AgentMessage, CborValue, ContentPart, ContextItem, ContextRole, Effort, Event,
    ExtAgentsMdAvailable, ExtensionReady, HarnessContextUsageChanged, HarnessRoleInfo,
    HarnessRoleSelected, HarnessRolesAvailable, MessageItem, ProviderResponseFinished,
    ProviderResponseUpdated, ProviderStopReason, ServiceTier, SessionPromptCreated,
    SessionPromptQueued, SessionPromptSteered, SessionPromptTerminated,
    SessionPromptTerminationReason, SessionStartReason, SessionStarted, ThinkingSummary,
    ToolBackgroundResult, ToolCallItem, ToolCancelled, ToolError, ToolResult, UiPromptSubmitted,
    UiRoleUpdateAction, Verbosity,
};

use super::chat::{
    DraftSlot, invalidate_pending_draft, is_local_slash_command, should_send_draft_snapshot,
};
use super::event_renderer::EventRenderer;
use super::tool_render::{
    CompactionStatus, ToolStatus, build_delegate_completion_display, build_osc1337_set_user_var,
    cache_hit_percent, format_turn_stats_line, render_action_error_block,
    render_action_output_block, render_compaction_block, render_delegate_display,
    render_shell_block, render_tool_block, render_tool_display, render_turn_stats_block,
    streaming_block, synthesize_fallback_display,
};

#[test]
fn dev_print_prompt_parses_role_flag() {
    // `tau dev print-prompt -r <role>` is a diagnostic command, so keep
    // the clap shape pinned even though it is hidden from normal help.
    let cli = super::cli::Cli::parse_from(["tau", "dev", "print-prompt", "-r", "engineer"]);
    match cli.command {
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::PrintPrompt { role },
        }) => assert_eq!(role, "engineer"),
        _ => panic!("unexpected command"),
    }
}

#[test]
fn dev_print_tools_parses_role_flag() {
    // `tau dev print-tools -r <role>` mirrors print-prompt, but prints the
    // role-filtered tool definitions advertised to the provider.
    let cli = super::cli::Cli::parse_from(["tau", "dev", "print-tools", "-r", "engineer"]);
    match cli.command {
        Some(super::cli::Command::Dev {
            command: super::cli::DevCommand::PrintTools { role },
        }) => assert_eq!(role, "engineer"),
        _ => panic!("unexpected command"),
    }
}

#[test]
fn startup_role_flag_is_parsed_for_default_run() {
    let cli = super::cli::Cli::parse_from(["tau", "--role", "manager"]);

    assert_eq!(cli.run.role.as_deref(), Some("manager"));
}

#[test]
fn role_cli_flags_accept_repeated_and_mixed_options() {
    let cli = super::cli::Cli::parse_from([
        "tau",
        "--disable-roles-all",
        "--enable-role",
        "manager",
        "--disable-role",
        "senior-engineer",
        "--disable-roles-all",
    ]);

    assert_eq!(cli.role_overrides.disable_roles_all, 2);
    assert_eq!(cli.role_overrides.enable_role, vec!["manager"]);
    assert_eq!(cli.role_overrides.disable_role, vec!["senior-engineer"]);
}

#[test]
fn extension_cli_flags_accept_repeated_and_mixed_options() {
    let cli = super::cli::Cli::parse_from([
        "tau",
        "--enable-extensions-all",
        "--disable-extension",
        "core-shell",
        "--enable-extension",
        "std-websearch",
        "--disable-extensions-all",
    ]);

    assert_eq!(cli.extension_overrides.enable_extensions_all, 1);
    assert_eq!(cli.extension_overrides.disable_extensions_all, 1);
    assert_eq!(
        cli.extension_overrides.enable_extension,
        vec!["std-websearch"]
    );
    assert_eq!(
        cli.extension_overrides.disable_extension,
        vec!["core-shell"]
    );
}

#[test]
fn role_cli_overrides_preserve_argument_order() {
    let overrides = super::parse_role_cli_overrides([
        "tau",
        "--disable-role",
        "manager",
        "--disable-roles-all",
        "--enable-role=manager",
        "--enable-role",
        "senior-engineer",
    ]);

    assert_eq!(
        overrides,
        vec![
            tau_config::settings::RoleCliOverride::Disable("manager".to_owned()),
            tau_config::settings::RoleCliOverride::DisableAll,
            tau_config::settings::RoleCliOverride::Enable("manager".to_owned()),
            tau_config::settings::RoleCliOverride::Enable("senior-engineer".to_owned()),
        ]
    );
}

#[test]
fn extension_cli_overrides_preserve_argument_order() {
    let overrides = super::parse_extension_cli_overrides([
        "tau",
        "--disable-extension",
        "core-shell",
        "--enable-extensions-all",
        "--disable-extensions-all",
        "--enable-extension=std-websearch",
    ]);

    assert_eq!(
        overrides,
        vec![
            tau_config::settings::ExtensionCliOverride::Disable("core-shell".to_owned()),
            tau_config::settings::ExtensionCliOverride::EnableAll,
            tau_config::settings::ExtensionCliOverride::DisableAll,
            tau_config::settings::ExtensionCliOverride::Enable("std-websearch".to_owned()),
        ]
    );
}

#[test]
fn local_slash_commands_are_identified_for_history_rendering() {
    assert!(is_local_slash_command("/model engineer"));
    assert!(is_local_slash_command("/set show-tools compact"));
    assert!(is_local_slash_command("/quit"));
    assert!(!is_local_slash_command("/unknown please answer"));
    assert!(!is_local_slash_command("hello /model engineer"));
}

/// Writer that feeds bytes into a vt100::Parser. Bytes are
/// buffered per-write and flushed atomically to the parser on
/// flush(), so the test thread never sees a partial render.
#[derive(Clone)]
struct VtWriter {
    parser: Arc<Mutex<vt100::Parser>>,
}

impl VtWriter {
    fn new(parser: vt100::Parser) -> Self {
        Self {
            parser: Arc::new(Mutex::new(parser)),
        }
    }

    fn screen_text(&self, w: u16) -> Vec<String> {
        self.parser
            .lock()
            .expect("vt")
            .screen()
            .rows(0, w)
            .collect()
    }

    fn screen_contains(&self, w: u16, needle: &str) -> bool {
        self.screen_text(w).iter().any(|r| r.contains(needle))
    }
}

impl std::io::Write for VtWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Process bytes directly into the parser. The mutex
        // ensures the test thread sees a consistent state.
        self.parser.lock().expect("vt").process(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn setup(w: u16, h: u16) -> (Term, TermHandle, VtWriter) {
    let vt = VtWriter::new(vt100::Parser::new(h, w, 100));
    let (term, handle, _input) = Term::new_virtual(
        w as usize,
        h as usize,
        "> ",
        Box::new(vt.clone()),
        tau_cli_term::CursorShape::Bar,
    );
    (term, handle, vt)
}

fn sync(handle: &TermHandle) {
    handle.redraw_sync();
}

fn agent_message(sender_id: &str, recipient_id: &str, message: &str) -> AgentMessage {
    AgentMessage {
        session_id: "s1".into(),
        sender_id: sender_id.to_owned(),
        recipient_id: recipient_id.to_owned(),
        message: message.to_owned(),
    }
}

fn visible_lines(vt: &VtWriter, w: u16) -> Vec<String> {
    vt.screen_text(w)
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect()
}

fn eventually_screen_contains(vt: &VtWriter, w: u16, needle: &str) -> bool {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if vt.screen_contains(w, needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn assistant_message_item(text: impl Into<String>) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
    })
}

fn session_prompt_created(session_prompt_id: &str, session_id: &str) -> SessionPromptCreated {
    SessionPromptCreated {
        session_prompt_id: session_prompt_id.into(),
        session_id: session_id.into(),
        system_prompt: String::new(),
        context_items: Vec::new(),
        tools: Vec::new(),
        tools_ref: None,
        model: None,
        model_params: tau_proto::ModelParams::default(),
        tool_choice: Default::default(),
        originator: tau_proto::PromptOriginator::User,
        share_user_cache_key: false,
        ctx_id: None,
        previous_response_candidate: None,
    }
}

fn tool_started(call_id: &str, tool_name: &str, arguments: CborValue) -> Event {
    Event::ToolStarted(tau_proto::ToolStarted {
        call_id: call_id.into(),
        tool_name: tau_proto::ToolName::new(tool_name),
        arguments,
        originator: tau_proto::PromptOriginator::User,
    })
}

fn finished_response(
    session_prompt_id: &str,
    output_items: Vec<ContextItem>,
) -> ProviderResponseFinished {
    let stop_reason = if output_items
        .iter()
        .any(|item| matches!(item, ContextItem::ToolCall(_)))
    {
        ProviderStopReason::ToolCalls
    } else {
        ProviderStopReason::EndTurn
    };
    ProviderResponseFinished {
        session_prompt_id: session_prompt_id.into(),
        output_items,
        stop_reason,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

#[test]
fn stale_draft_snapshot_is_dropped_after_submit_epoch_bump() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::locked(mtx);
        slot.pending = Some((
            slot.epoch,
            tau_proto::UiPromptDraft {
                session_id: "s1".into(),
                text: "old".into(),
            },
        ));
    }

    let (epoch, _draft) = {
        let (mtx, _cv) = &handle;
        super::locked(mtx).pending.take().expect("pending draft")
    };
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::locked(mtx);
        slot.epoch = slot.epoch.wrapping_add(1);
        slot.pending = None;
    }

    assert!(!should_send_draft_snapshot(&handle, epoch));
}

/// Role-update parsing must keep explicit `off` distinct from clearing a field;
/// otherwise `/role <role> effort off` and `/role <role> thinking-summary off`
/// would accidentally reset the selected role instead of storing the user's
/// requested off state. `reset` is the only textual way to clear a setting.
#[test]
fn role_setting_updates_are_typed_and_reset_aware() {
    use super::chat::parse_role_setting_update;

    assert_eq!(
        parse_role_setting_update("effort", "off").expect("effort off"),
        UiRoleUpdateAction::SetEffort {
            effort: Some(Effort::Off),
        }
    );
    assert_eq!(
        parse_role_setting_update("effort", "reset").expect("effort reset"),
        UiRoleUpdateAction::SetEffort { effort: None }
    );
    assert_eq!(
        parse_role_setting_update("thinking-summary", "off").expect("summary off"),
        UiRoleUpdateAction::SetThinkingSummary {
            thinking_summary: Some(ThinkingSummary::Off),
        }
    );
    assert!(parse_role_setting_update("service-tier", "off").is_err());
    assert_eq!(
        parse_role_setting_update("service-tier", "reset").expect("tier reset"),
        UiRoleUpdateAction::SetServiceTier { service_tier: None }
    );
    assert_eq!(
        parse_role_setting_update("service-tier", "fast").expect("tier fast"),
        UiRoleUpdateAction::SetServiceTier {
            service_tier: Some(ServiceTier::Fast),
        }
    );
    assert_eq!(
        parse_role_setting_update("compaction-threshold", "85").expect("threshold 85"),
        UiRoleUpdateAction::SetCompactionThreshold {
            compaction_threshold: Some(85),
        }
    );
    assert_eq!(
        parse_role_setting_update("compaction-threshold", "reset").expect("threshold reset"),
        UiRoleUpdateAction::SetCompactionThreshold {
            compaction_threshold: None,
        }
    );
    assert!(parse_role_setting_update("compaction-threshold", "101").is_err());
    assert_eq!(
        parse_role_setting_update("enable-tools", "web_search,grep").expect("enable tools"),
        UiRoleUpdateAction::SetEnableTools {
            enable_tools: vec![
                tau_proto::ToolName::new("web_search"),
                tau_proto::ToolName::new("grep"),
            ],
        }
    );
    assert_eq!(
        parse_role_setting_update("enable-tools", "reset").expect("reset enable tools"),
        UiRoleUpdateAction::SetEnableTools {
            enable_tools: Vec::new(),
        }
    );
}

#[test]
fn action_submission_invalidates_pending_draft_like_prompt_submission() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());
    {
        let (mtx, _cv) = &handle;
        let mut slot = super::locked(mtx);
        slot.pending = Some((
            slot.epoch,
            tau_proto::UiPromptDraft {
                session_id: "s1".into(),
                text: "/email list".into(),
            },
        ));
    }

    invalidate_pending_draft(&handle);

    let (mtx, _cv) = &handle;
    let slot = super::locked(mtx);
    assert_eq!(slot.epoch, 1);
    assert!(slot.pending.is_none());
}

#[test]
fn current_draft_snapshot_is_sent_when_epoch_matches() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());

    assert!(should_send_draft_snapshot(&handle, 0));
}

#[test]
fn draft_snapshot_is_dropped_after_shutdown() {
    let handle = (Mutex::new(DraftSlot::default()), std::sync::Condvar::new());
    {
        let (mtx, _cv) = &handle;
        super::locked(mtx).done = true;
    }

    assert!(!should_send_draft_snapshot(&handle, 0));
}

/// `AgentMessage` events are normal history entries, not active blocks. They
/// must render for every sender/recipient pair and scroll away as history
/// grows.
#[test]
fn agent_messages_render_all_recipients_as_history() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::AgentMessage(agent_message(
        "manager_11111111",
        "engineer_22222222",
        "hello worker",
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "Message from manager_11111111 to engineer_22222222:"));
    assert!(vt.screen_contains(80, "hello worker"));

    for idx in 0..20 {
        renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: format!("scroll filler {idx}"),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }));
    }
    sync(&handle);
    assert!(!vt.screen_contains(80, "Message from manager_11111111 to engineer_22222222:"));
}

#[test]
fn show_messages_none_leaves_no_visible_message_output() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    let before = visible_lines(&vt, 80);

    renderer.apply_setting("show-messages", "none");
    renderer.handle(&Event::AgentMessage(agent_message(
        "agent-a",
        "agent-b",
        "secret hidden body",
    )));
    sync(&handle);

    assert_eq!(visible_lines(&vt, 80), before);
    assert!(!vt.screen_contains(80, "Message from"));
    assert!(!vt.screen_contains(80, "secret hidden body"));
}

#[test]
fn show_messages_summary_modes_do_not_show_body() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.apply_setting("show-messages", "all-summary");
    renderer.handle(&Event::AgentMessage(agent_message(
        "agent-a",
        "agent-b",
        "secret summarized body",
    )));
    sync(&handle);

    assert!(vt.screen_contains(80, "Message from agent-a to agent-b"));
    assert!(!vt.screen_contains(80, "secret summarized body"));
}

#[test]
fn show_messages_toggle_retroactively_hides_and_shows_history() {
    let (_term, handle, vt) = setup(80, 8);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.apply_setting("show-messages", "none");
    renderer.handle(&Event::AgentMessage(agent_message(
        "agent-a",
        "agent-b",
        "retro body",
    )));
    sync(&handle);
    assert!(!vt.screen_contains(80, "Message from agent-a to agent-b"));
    assert!(!vt.screen_contains(80, "retro body"));

    renderer.apply_setting("show-messages", "all-full");
    sync(&handle);
    assert!(vt.screen_contains(80, "Message from agent-a to agent-b:"));
    assert!(vt.screen_contains(80, "retro body"));

    renderer.apply_setting("show-messages", "none");
    sync(&handle);
    assert!(!vt.screen_contains(80, "Message from agent-a to agent-b"));
    assert!(!vt.screen_contains(80, "retro body"));
}

#[test]
fn new_session_clears_session_ui_state() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "old prompt".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![
            assistant_message_item("old response"),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/lib.rs".into()),
                )]),
            }),
        ],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Map(vec![
            (
                CborValue::Text("path".into()),
                CborValue::Text("src/lib.rs".into()),
            ),
            (
                CborValue::Text("content".into()),
                CborValue::Text("fn main() {}\n".into()),
            ),
        ]),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolDisplay {
            args: "src/lib.rs".into(),
            status: tau_proto::ToolDisplayStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "old prompt"));
    assert!(vt.screen_contains(80, "old response"));
    assert!(vt.screen_contains(80, "read src/lib.rs"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "old prompt"));
    assert!(!vt.screen_contains(80, "old response"));
    assert!(!vt.screen_contains(80, "read src/lib.rs"));
    assert!(!vt.screen_contains(80, "no role selected"));
}

#[test]
fn new_session_replays_startup_context_and_kept_extensions() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::ExtAgentsMdAvailable(ExtAgentsMdAvailable {
        file_path: std::path::PathBuf::from("/tmp/AGENTS.md"),
        content: "# Test\n".into(),
    }));
    renderer.handle(&Event::ExtensionReady(ExtensionReady {
        instance_id: 1.into(),
        extension_name: "core-shell".into(),
        pid: Some(123),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "tau"));
    assert!(vt.screen_contains(80, "extension core-shell kept"));
}

#[test]
fn new_session_preserves_role_status() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(100_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "+engineer"));

    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer"));
    assert!(vt.screen_contains(80, "@s2"));
    assert!(!vt.screen_contains(80, "no role selected"));
}

#[test]
fn model_status_uses_symbol_prefixed_chips() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams {
            verbosity: Verbosity::High,
            ..Default::default()
        },
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "tau-agent-test".into(),
        reason: SessionStartReason::New,
    }));
    renderer.handle(&Event::HarnessContextUsageChanged(
        HarnessContextUsageChanged {
            input_tokens: Some(12_000),
            cached_tokens: None,
            percent_used: Some(6),
        },
    ));
    sync(&handle);

    let status_row = vt
        .screen_text(80)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row");
    assert!(status_row.starts_with("+engineer ~high @tau-agent-test"));
    assert!(status_row.ends_with("#12k/200k"));
    assert!(!vt.screen_contains(80, "=test/model"));
    assert!(!vt.screen_contains(80, "v=high"));
    assert!(!vt.screen_contains(80, "ctx:"));
}

#[test]
fn model_status_shows_main_tool_usage_before_context() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    renderer.handle(&Event::HarnessContextUsageChanged(
        HarnessContextUsageChanged {
            input_tokens: Some(12_000),
            cached_tokens: None,
            percent_used: Some(6),
        },
    ));

    // Regression coverage for the bottom status bar: main-agent tool
    // usage should mirror delegate progress chips (`%complete/total`)
    // and should render immediately before the context chip, while
    // side-conversation tool calls stay rolled up under their delegate.
    renderer.handle(&Event::ProviderResponseFinished(ProviderResponseFinished {
        session_prompt_id: "side-sp".into(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "side-call".into(),
            name: tau_proto::ToolName::new("grep"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: ProviderStopReason::ToolCalls,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q1".to_owned(),
        },
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after side response");
    assert!(status_row.ends_with("#12k/200k"));
    assert!(!status_row.contains('%'));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "main-sp",
        vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-2".into(),
                name: tau_proto::ToolName::new("grep"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
        ],
    )));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after main response");
    assert!(status_row.ends_with("%0/2 #12k/200k"));

    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "side-call".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("side result".into()),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q1".to_owned(),
        },
    }));
    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("main result".into()),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after tool result");
    assert!(status_row.ends_with("%1/2 #12k/200k"));

    // Regression coverage for turn visibility: once an extension/sub-agent
    // prompt becomes active, the main-agent tool chip must disappear instead
    // of showing stale progress from the previous main turn. Context remains
    // visible because it is model/session state, not per-turn tool usage.
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q2".to_owned(),
        },
        ..session_prompt_created("side-sp-2", "s1")
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after side prompt starts");
    assert!(status_row.ends_with("#12k/200k"));
    assert!(!status_row.contains('%'));

    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "call-2".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("main result".into()),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after second main tool result during side turn");
    assert!(status_row.ends_with("#12k/200k"));
    assert!(!status_row.contains('%'));

    // Tool completions that arrive while a side conversation is active update
    // counters silently. The chip becomes visible again only when a main/user
    // lifecycle event shows the main agent has control again.
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "main-follow-up-sp",
        "s1",
    )));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after main prompt resumes");
    assert!(status_row.ends_with("%2/2 #12k/200k"));

    // The main agent's final no-tool response ends the tool-using turn and
    // hides the chip while preserving context stats.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "main-final-sp",
        vec![assistant_message_item("done")],
    )));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after final main response");
    assert!(status_row.ends_with("#12k/200k"));
    assert!(!status_row.contains('%'));

    // Starting a new user task in the same session also keeps the chip hidden
    // until the main agent requests tools for that task.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "next task".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after next prompt");
    assert!(status_row.ends_with("#12k/200k"));
    assert!(!status_row.contains('%'));
}

#[test]
fn agent_in_progress_ignores_completed_replayed_prompt_history() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "old prompt".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));

    // Late subscribers can replay historical UI submit and provider-finished
    // events without replaying the old SessionPromptCreated. That sequence is
    // already complete, so it must not leave Ctrl-D permanently guarded.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "old-sp",
        vec![assistant_message_item("old answer")],
    )));

    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn prompt_termination_clears_live_response_and_activity() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-stale", "s1",
    )));
    sync(&handle);
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(vt.screen_contains(80, "…"));

    // Regression: if the harness discards a stale provider response, it now
    // publishes this terminal lifecycle fact instead of leaving the UI's live
    // response block and Ctrl-D guard stuck forever.
    renderer.handle(&Event::SessionPromptTerminated(SessionPromptTerminated {
        session_id: "s1".into(),
        session_prompt_id: "sp-stale".into(),
        reason: SessionPromptTerminationReason::Stale,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(!vt.screen_contains(80, "…"));
}

#[test]
fn agent_in_progress_clears_when_tool_is_cancelled() {
    let (_term, handle, _vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle,
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp1", "s1",
    )));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp1",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
    )));
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));

    // ToolCancelled is a terminal tool event just like ToolResult/ToolError.
    // The Ctrl-D guard must clear it, otherwise a cancelled tool leaves the
    // session looking busy forever after the harness has stopped the tool.
    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
    }));

    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn delegate_side_conversation_keeps_parent_tool_status_visible() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        baseline_params: None,
        model_params: tau_proto::ModelParams::default(),
    }));
    renderer.handle(&Event::HarnessContextUsageChanged(
        HarnessContextUsageChanged {
            input_tokens: Some(12_000),
            cached_tokens: None,
            percent_used: Some(6),
        },
    ));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "main-sp",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "delegate-call".into(),
            name: tau_proto::ToolName::new("delegate"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
    )));
    renderer.handle(&tool_started(
        "delegate-call",
        "delegate",
        CborValue::Map(Vec::new()),
    ));

    // A running parent `delegate` call is the visible main-agent work while
    // the sub-agent side conversation is active. Regression coverage: the
    // side prompt lifecycle must not hide `%0/1` from the status bar, because
    // otherwise users lose the only bottom-bar indication that delegation is
    // still in progress.
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q1".to_owned(),
        },
        ..session_prompt_created("side-sp", "s1")
    }));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "side-sp".into(),
        text: "working".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q1".to_owned(),
        },
    }));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row during delegate side conversation");
    assert!(status_row.ends_with("%0/1 #12k/200k"));

    // Once the delegated side conversation reports its own tool progress,
    // the status bar should prefer that live `%complete/total` chip over the
    // parent aggregate `%0/1`, and the progress event should repaint it.
    renderer.handle(&Event::ToolDelegateProgress(tau_proto::DelegateProgress {
        call_id: "delegate-call".into(),
        task_name: "probe".into(),
        role: Some("engineer".to_owned()),
        execution_mode: Some(tau_proto::ToolExecutionMode::Update),
        ctx_percent: None,
        ctx_input_tokens: None,
        ctx_window: None,
        tools_in_flight: 2,
        tools_total: 3,
        display: Some(tau_proto::ToolDisplay {
            args: "[probe]".into(),
            progress_counters: vec![tau_proto::ProgressCounter {
                label: Some("tools".into()),
                unit: tau_proto::ProgressUnit::Count,
                complete: Some(1),
                total: Some(3),
            }],
            status: tau_proto::ToolDisplayStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
            ..Default::default()
        }),
    }));
    assert!(
        eventually_screen_contains(&vt, 100, "%1/3 #12k/200k"),
        "delegate progress should repaint the status bar with sub-agent tool progress: {:?}",
        vt.screen_text(100)
    );
    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("#12k/200k"))
        .expect("status row after delegate progress");
    assert!(status_row.contains("+engineer"));
    assert!(status_row.ends_with("%1/3 #12k/200k"));

    renderer.handle(&Event::ToolCancelled(ToolCancelled {
        call_id: "delegate-call".into(),
        tool_name: tau_proto::ToolName::new("delegate"),
        tool_type: tau_proto::ToolType::Function,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        originator: tau_proto::PromptOriginator::Extension {
            name: "core-subagents".into(),
            query_id: "q2".to_owned(),
        },
        ..session_prompt_created("later-side-sp", "s1")
    }));
    sync(&handle);

    let status_row = vt
        .screen_text(100)
        .into_iter()
        .find(|row| row.contains("+engineer"))
        .expect("status row after delegate cancellation");
    assert!(status_row.ends_with("#12k/200k"));
    assert!(!status_row.contains('%'));
}

#[test]
fn role_default_knobs_are_hidden_and_overrides_follow_role() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::HarnessRolesAvailable(HarnessRolesAvailable {
        roles: vec![HarnessRoleInfo {
            name: "engineer".to_owned(),
            description: "model=test/model, effort=medium, verbosity=medium, thinking-summary=auto"
                .to_owned(),
            role_description: None,
        }],
        groups: Vec::new(),
    }));
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        model_params: tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::Medium,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        },
        baseline_params: Some(tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::Medium,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        }),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s2".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer @s2"));
    assert!(!vt.screen_contains(80, "^medium"));
    assert!(!vt.screen_contains(80, "~medium"));

    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: Some(200_000),
        role: "engineer".into(),
        model_params: tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::High,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        },
        baseline_params: Some(tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::Medium,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        }),
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer ~high @s2"));
}

#[test]
fn role_state_overrides_are_compared_to_role_baseline() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // HarnessRolesAvailable describes the current role including
    // persisted state overrides. The status bar must use the role/provider
    // baseline from HarnessRoleSelected instead.
    renderer.handle(&Event::HarnessRolesAvailable(HarnessRolesAvailable {
        roles: vec![HarnessRoleInfo {
            name: "engineer".to_owned(),
            description: "model=test/model, effort=low, verbosity=high, thinking-summary=auto"
                .to_owned(),
            role_description: None,
        }],
        groups: Vec::new(),
    }));
    renderer.handle(&Event::HarnessRoleSelected(HarnessRoleSelected {
        model: Some("test/model".into()),
        context_window: None,
        role: "engineer".into(),
        model_params: tau_proto::ModelParams {
            effort: tau_proto::Effort::Low,
            verbosity: Verbosity::High,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: None,
        },
        baseline_params: Some(tau_proto::ModelParams {
            effort: tau_proto::Effort::Medium,
            verbosity: Verbosity::Medium,
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            service_tier: Some(tau_proto::ServiceTier::Fast),
        }),
    }));
    renderer.handle(&Event::SessionStarted(SessionStarted {
        session_id: "s3".into(),
        reason: SessionStartReason::New,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "+engineer ^low ~high !off @s3"));
}

#[test]
fn single_prompt_response_cycle() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // User submits prompt.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hello".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "> hello"));

    // Harness creates session prompt.
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-0", "s1",
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "…"));

    // Agent streams response.
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "Hi there!".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hi there!"));

    // Agent finishes.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("Hi there! How can I help?")],
    )));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "Hi there! How can I help?"),
        "final response should be visible, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn thinking_renders_as_separate_block_above_response() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        model_params: tau_proto::ModelParams {
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            ..Default::default()
        },
        ..session_prompt_created("sp-0", "s1")
    }));
    sync(&handle);

    // Thinking arrives before the response text. Both should be
    // visible simultaneously, with thinking above response.
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: String::new(),
        thinking: Some("planning the answer".into()),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "planning the answer"),
        "thinking block should be live: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "actual answer".into(),
        thinking: Some("planning the answer".into()),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "actual answer"));
    assert!(vt.screen_contains(80, "planning the answer"));

    // Order matters even during live streaming: thinking should
    // render ABOVE the response, not below it.
    let live = vt.screen_text(80);
    let live_thinking = live
        .iter()
        .position(|l| l.contains("planning the answer"))
        .unwrap_or_else(|| panic!("live thinking missing: {live:?}"));
    let live_response = live
        .iter()
        .position(|l| l.contains("actual answer"))
        .unwrap_or_else(|| panic!("live response missing: {live:?}"));
    assert!(
        live_thinking < live_response,
        "live thinking should render above live response (thinking @ {live_thinking}, response @ {live_response}); lines: {live:?}",
    );

    // On finish both stick in history.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("actual answer")],
    )));
    sync(&handle);
    // Thinking should appear above the response in the history.
    let lines = vt.screen_text(80);
    let thinking_row = lines
        .iter()
        .position(|l| l.contains("planning the answer"))
        .unwrap_or_else(|| panic!("thinking should remain in history: {lines:?}"));
    let response_row = lines
        .iter()
        .position(|l| l.contains("actual answer"))
        .unwrap_or_else(|| panic!("response should remain in history: {lines:?}"));
    assert!(
        thinking_row < response_row,
        "thinking should render above response (thinking @ {thinking_row}, response @ {response_row}); lines: {lines:?}",
    );
}

#[test]
fn set_show_thinking_round_trip_restores_history() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        model_params: tau_proto::ModelParams {
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            ..Default::default()
        },
        ..session_prompt_created("sp-0", "s1")
    }));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "the_response".into(),
        thinking: Some("the_thinking_text".into()),
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("the_response")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "the_thinking_text"));
    assert!(vt.screen_contains(80, "the_response"));

    // Off — thinking content disappears, no placeholder, no
    // blank row left behind: the response should be on the same
    // row as the (now-empty) thinking block sat before. We assert
    // this indirectly by counting non-blank lines.
    let lines_before = vt
        .screen_text(80)
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .count();
    renderer.apply_setting("show-thinking", "false");
    sync(&handle);
    assert!(!vt.screen_contains(80, "the_thinking_text"));
    assert!(!vt.screen_contains(80, "thinking hidden"));
    assert!(vt.screen_contains(80, "the_response"));
    let lines_after = vt
        .screen_text(80)
        .into_iter()
        .filter(|l| !l.trim().is_empty())
        .count();
    // Hiding the one thinking block should remove exactly one
    // visible line of content from the screen.
    assert_eq!(lines_after + 1, lines_before);

    // Back on — original thinking text returns in its original
    // position above the response.
    renderer.apply_setting("show-thinking", "true");
    sync(&handle);
    let lines = vt.screen_text(80);
    let thinking_row = lines
        .iter()
        .position(|l| l.contains("the_thinking_text"))
        .unwrap_or_else(|| panic!("thinking should reappear: {lines:?}"));
    let response_row = lines
        .iter()
        .position(|l| l.contains("the_response"))
        .unwrap_or_else(|| panic!("response should still be visible: {lines:?}"));
    assert!(thinking_row < response_row);
}

#[test]
fn thinking_created_while_off_stays_invisible_after_toggle_on() {
    // Blocks that arrive while `show_thinking == false` are
    // never rendered and never tracked, so toggling back on
    // doesn't suddenly resurrect them. Only blocks that were
    // visible at some point round-trip through `set_block`.
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    renderer.apply_setting("show-thinking", "false");

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
        model_params: tau_proto::ModelParams {
            thinking_summary: tau_proto::ThinkingSummary::Auto,
            ..Default::default()
        },
        ..session_prompt_created("sp-0", "s1")
    }));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("answer")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "answer"));
    assert!(!vt.screen_contains(80, "hidden reasoning"));

    renderer.apply_setting("show-thinking", "true");
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "hidden reasoning"),
        "blocks created while off should not appear after toggle on"
    );
}

#[test]
fn no_thinking_block_when_summary_absent() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "hello".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("hello")],
    )));
    sync(&handle);
    // Just make sure we didn't crash and the response is visible.
    assert!(vt.screen_contains(80, "hello"));
}

#[test]
fn queued_prompt_renders_after_first_completes() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // First prompt.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "first".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-0", "s1",
    )));

    // Regression: the production busy-submit path immediately publishes
    // only `SessionPromptQueued`; there may be no preceding local
    // `UiPromptSubmitted` echo for the renderer to replace. The queued
    // event itself must make the user's prompt visible.
    renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
        session_id: "s1".into(),
        text: "second".into(),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "second (queued)"),
        "queued indicator should show, got: {:?}",
        vt.screen_text(80)
    );

    // First finishes.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("response one")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "response one"));

    // Second dispatched — "(queued)" should be removed.
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-1", "s1",
    )));
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "(queued)"),
        "queued indicator should be gone after dispatch, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "> second"),
        "dispatched prompt should show normally, got: {:?}",
        vt.screen_text(80)
    );
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("second"))
            .count(),
        1,
        "queued prompt should be promoted instead of duplicated, got: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-1".into(),
        text: "response two".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "response two"),
        "second response should stream, got: {:?}",
        vt.screen_text(80)
    );

    // Second finishes.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-1",
        vec![assistant_message_item("response two complete")],
    )));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "response two complete"),
        "final second response should show, got: {:?}",
        vt.screen_text(80)
    );
    // First response should still be visible.
    assert!(
        vt.screen_contains(80, "response one"),
        "first response should still show, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn queued_prompt_then_late_ui_submit_advances_without_duplicate() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // Regression: some paths can observe the durable queued event before
    // a local UI echo. The late echo must not add a second copy, and the
    // later prompt creation must promote the queued marker to one normal
    // transcript item.
    renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
        session_id: "s1".into(),
        text: "late echo".into(),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "late echo".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "late echo (queued)"));
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("late echo"))
            .count(),
        1,
        "late UI echo should not duplicate queued prompt, got: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-queued",
        "s1",
    )));
    sync(&handle);
    assert!(!vt.screen_contains(80, "late echo (queued)"));
    assert!(vt.screen_contains(80, "> late echo"));
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("late echo"))
            .count(),
        1,
        "created queued prompt should be promoted once, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn queued_prompt_steered_promotes_without_duplicate() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // Regression: steering folds a queued prompt into the in-flight turn
    // immediately, without a later `SessionPromptCreated`. The queued
    // marker should therefore be promoted in place to one normal user
    // prompt instead of lingering or duplicating.
    renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
        session_id: "s1".into(),
        text: "folded queued prompt".into(),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "folded queued prompt (queued)"),
        "queued marker should show before steering, got: {:?}",
        vt.screen_text(80)
    );

    renderer.handle(&Event::SessionPromptSteered(SessionPromptSteered {
        session_id: "s1".into(),
        text: "folded queued prompt".into(),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);
    assert!(
        !vt.screen_contains(80, "folded queued prompt (queued)"),
        "queued marker should be gone after steering, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "> folded queued prompt"),
        "steered prompt should show normally, got: {:?}",
        vt.screen_text(80)
    );
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("folded queued prompt"))
            .count(),
        1,
        "steered queued prompt should be promoted instead of duplicated, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn internal_prompt_events_are_hidden() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // Background tool completion prompts are delivered to the model as
    // prompt-like events, but they are internal control text and must not show
    // up in the user's transcript or queued prompt area.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "[tau-internal] Tool call `bg` is complete.".into(),
        message_class: tau_proto::PromptMessageClass::Internal,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
        session_id: "s1".into(),
        text: "[tau-internal] Tool call `queued` is complete.".into(),
        message_class: tau_proto::PromptMessageClass::Internal,
    }));
    renderer.handle(&Event::SessionPromptSteered(SessionPromptSteered {
        session_id: "s1".into(),
        text: "[tau-internal] Tool call `steered` is complete.".into(),
        message_class: tau_proto::PromptMessageClass::Internal,
    }));
    sync(&handle);

    assert!(!vt.screen_contains(80, "Tool call"));
    assert!(
        vt.screen_text(80)
            .iter()
            .all(|row| !row.contains("Tool call"))
    );
}

#[test]
fn queued_prompt_does_not_replace_dispatched_same_text() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // Regression: once a local echo has been accepted as a normal prompt,
    // a later queued prompt with the same text is a separate message. Do
    // not remove the earlier transcript block while rendering the queued
    // marker.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "repeat".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
        session_id: "s1".into(),
        text: "repeat".into(),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    sync(&handle);

    assert!(vt.screen_contains(80, "repeat (queued)"));
    assert_eq!(
        vt.screen_text(80)
            .iter()
            .filter(|row| row.contains("repeat"))
            .count(),
        2,
        "queued prompt should not remove an earlier dispatched prompt with the same text, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn three_queued_prompts_render_sequentially() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // Three rapid prompts.
    for i in 0..3 {
        if i == 0 {
            renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: "s1".into(),
                text: format!("msg-{i}"),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }));
            renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
                "sp-0", "s1",
            )));
        } else {
            renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
                session_id: "s1".into(),
                text: format!("msg-{i}"),
                message_class: tau_proto::PromptMessageClass::User,
            }));
        }
    }

    // Process all three sequentially, flushing between each.
    for i in 0..3 {
        let spid: tau_proto::SessionPromptId = format!("sp-{i}").into();
        if i > 0 {
            renderer.handle(&Event::SessionPromptCreated(SessionPromptCreated {
                session_prompt_id: spid.clone(),
                ..session_prompt_created("sp-ignore", "s1")
            }));
        }
        renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
            session_prompt_id: spid.clone(),
            text: format!("partial-{i}"),
            thinking: None,
            originator: tau_proto::PromptOriginator::User,
        }));
        renderer.handle(&Event::ProviderResponseFinished(finished_response(
            spid.as_ref(),
            vec![assistant_message_item(format!("response-{i}"))],
        )));
        sync(&handle);
    }

    // All three responses should be visible.
    // Extra flush to catch any delayed renders.
    sync(&handle);
    for i in 0..3 {
        assert!(
            vt.screen_contains(80, &format!("response-{i}")),
            "response-{i} should be visible, got: {:?}",
            vt.screen_text(80)
        );
    }
    // No stale "..." blocks.
    assert!(
        !vt.screen_contains(80, "…"),
        "no '…' should remain, got: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn streaming_indicator_appends_during_updates() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-0", "s1",
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "…"));

    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "Hello".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hello …"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("Hello")],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "Hello"));
    assert!(!vt.screen_contains(80, "Hello …"));
}

#[test]
fn compaction_lifecycle_renders_status_line() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::SessionCompactionStarted(
        tau_proto::SessionCompactionStarted {
            session_id: "s1".into(),
            originator: tau_proto::PromptOriginator::User,
            original_input_tokens: Some(226_200),
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "compact #226.2k …"));

    renderer.handle(&Event::SessionCompacted(tau_proto::SessionCompacted {
        session_id: "s1".into(),
        originator: tau_proto::PromptOriginator::User,
        original_input_tokens: Some(226_200),
        compacted_input_tokens: Some(4_500),
        replacement_window: vec![assistant_message_item("Conversation compacted.")],
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "compact #226.2k …"));
    assert!(!vt.screen_contains(80, "compact ok"));

    renderer.handle(&Event::SessionCompactionFinished(
        tau_proto::SessionCompactionFinished {
            session_id: "s1".into(),
            originator: tau_proto::PromptOriginator::User,
            original_input_tokens: Some(226_200),
            compacted_input_tokens: Some(4_500),
            outcome: tau_proto::SessionCompactionOutcome::Succeeded,
            message: None,
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "compact #226.2k ok: #4.5k"));
    assert!(!vt.screen_contains(80, "compact #226.2k …"));
}

#[test]
fn render_compaction_block_styles_token_chips_like_context_status() {
    let theme = tau_themes::Theme::builtin();

    // Regression: compaction status text is mostly lifecycle status, but the
    // `#…` token chips carry the same semantic theme as context stats in the
    // status bar and delegate progress chips.
    let block =
        render_compaction_block(&theme, "#10.9k/258.4k ok: #4.5k", CompactionStatus::Success);
    let spans = block.content.spans();
    let context_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::STATUS_CONTEXT);
    let success_style =
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::TOOL_STATUS_SUCCESS);

    let original_chip = spans
        .iter()
        .find(|span| span.text == "#10.9k/258.4k")
        .expect("original token chip span");
    let compacted_chip = spans
        .iter()
        .find(|span| span.text == "#4.5k")
        .expect("compacted token chip span");
    let success_text = spans
        .iter()
        .find(|span| span.text == "ok:")
        .expect("success status span");

    assert_eq!(original_chip.style, context_style);
    assert_eq!(compacted_chip.style, context_style);
    assert_eq!(success_text.style, success_style);

    let progress_block = render_compaction_block(&theme, "#226.2k …", CompactionStatus::Progress);
    let progress_spans = progress_block.content.spans();
    let progress_style =
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::PROGRESS_INDICATOR);
    let progress_chip = progress_spans
        .iter()
        .find(|span| span.text == "#226.2k")
        .expect("progress token chip span");
    let ellipsis = progress_spans
        .iter()
        .find(|span| span.text == "…")
        .expect("progress ellipsis span");

    assert_eq!(progress_chip.style, context_style);
    assert_eq!(ellipsis.style, progress_style);
}

#[test]
fn replayed_compacted_event_renders_success_status() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::SessionCompacted(tau_proto::SessionCompacted {
        session_id: "s1".into(),
        originator: tau_proto::PromptOriginator::User,
        original_input_tokens: Some(226_200),
        compacted_input_tokens: Some(4_500),
        replacement_window: vec![assistant_message_item("Conversation compacted.")],
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "compact #226.2k ok: #4.5k"));
}

#[test]
fn delegate_progress_redraws_live_parent_block() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    let delegate_args = CborValue::Map(vec![(
        CborValue::Text("task_name".into()),
        CborValue::Text("[probe]".into()),
    )]);
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-delegate".into(),
            name: tau_proto::ToolName::new("delegate"),
            tool_type: tau_proto::ToolType::Function,
            arguments: delegate_args.clone(),
        })],
    )));
    renderer.handle(&tool_started("call-delegate", "delegate", delegate_args));
    sync(&handle);
    assert!(vt.screen_contains(100, "[probe]"));
    assert!(!vt.screen_contains(100, "%3/3"));

    // Regression: `ToolDelegateProgress` mutates the already-visible
    // parent `delegate` block. That live mutation must request its own
    // redraw because suppressed sub-agent tool events will not repaint it.
    renderer.handle(&Event::ToolDelegateProgress(tau_proto::DelegateProgress {
        call_id: "call-delegate".into(),
        task_name: "probe".into(),
        role: Some("engineer".to_owned()),
        execution_mode: Some(tau_proto::ToolExecutionMode::Exclusive),
        ctx_percent: None,
        ctx_input_tokens: None,
        ctx_window: None,
        tools_in_flight: 0,
        tools_total: 3,
        display: Some(tau_proto::ToolDisplay {
            args: "[probe]".into(),
            progress_counters: vec![tau_proto::ProgressCounter {
                label: Some("tools".into()),
                unit: tau_proto::ProgressUnit::Count,
                complete: Some(3),
                total: Some(3),
            }],
            status: tau_proto::ToolDisplayStatus::InProgress,
            status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
            ..Default::default()
        }),
    }));

    assert!(
        eventually_screen_contains(&vt, 100, "+engineer"),
        "delegate progress should repaint the role suffix without an explicit test redraw: {:?}",
        vt.screen_text(100)
    );
    assert!(
        eventually_screen_contains(&vt, 100, "%3/3"),
        "delegate progress should repaint without an explicit test redraw: {:?}",
        vt.screen_text(100)
    );
}

#[test]
fn side_conversation_compaction_is_hidden_from_main_transcript() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    let originator = tau_proto::PromptOriginator::Extension {
        name: "core-subagents".into(),
        query_id: "delegate-1".to_owned(),
    };

    // Regression: sub-agent compaction lifecycle events are still
    // delivered to the main UI, but the main transcript must not show
    // their progress/result blocks.
    renderer.handle(&Event::SessionCompactionStarted(
        tau_proto::SessionCompactionStarted {
            session_id: "s1".into(),
            originator: originator.clone(),
            original_input_tokens: Some(10_000),
        },
    ));
    renderer.handle(&Event::SessionCompacted(tau_proto::SessionCompacted {
        session_id: "s1".into(),
        originator: originator.clone(),
        original_input_tokens: Some(10_000),
        compacted_input_tokens: Some(1_000),
        replacement_window: vec![assistant_message_item("Conversation compacted.")],
    }));
    renderer.handle(&Event::SessionCompactionFinished(
        tau_proto::SessionCompactionFinished {
            session_id: "s1".into(),
            originator,
            original_input_tokens: Some(10_000),
            compacted_input_tokens: Some(1_000),
            outcome: tau_proto::SessionCompactionOutcome::Succeeded,
            message: None,
        },
    ));

    sync(&handle);
    assert!(!vt.screen_contains(80, "compact"));
}

#[test]
fn failed_compaction_renders_error_status() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::SessionCompactionStarted(
        tau_proto::SessionCompactionStarted {
            session_id: "s1".into(),
            originator: tau_proto::PromptOriginator::User,
            original_input_tokens: Some(226_200),
        },
    ));
    renderer.handle(&Event::SessionCompactionFinished(
        tau_proto::SessionCompactionFinished {
            session_id: "s1".into(),
            originator: tau_proto::PromptOriginator::User,
            original_input_tokens: Some(226_200),
            compacted_input_tokens: None,
            outcome: tau_proto::SessionCompactionOutcome::Failed,
            message: Some("provider unavailable".to_owned()),
        },
    ));
    sync(&handle);
    assert!(vt.screen_contains(80, "compact #226.2k err: provider unavailable"));
}

/// Regression: invalid provider tool calls can fail schema validation before
/// the harness emits `ToolStarted`. The UI must reserve transcript order but
/// must not show a fake live tool while waiting for the provider error.
#[test]
fn provider_tool_error_before_tool_started_does_not_show_live_tool() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "bad-mode".into(),
                name: tau_proto::ToolName::new("delegate"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("execution_mode".into()),
                    CborValue::Text("invalid".into()),
                )]),
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(!vt.screen_contains(80, "delegate 0s …"));

    renderer.handle_recorded_at(
        &Event::ProviderToolError(ToolError {
            call_id: "bad-mode".into(),
            tool_name: tau_proto::ToolName::new("delegate"),
            tool_type: tau_proto::ToolType::Function,
            message: "invalid arguments for tool `delegate`".to_owned(),
            details: None,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "delegate err: invalid"));
    assert!(!vt.screen_contains(80, "delegate 0s …"));
}

/// Schema validation failures only emit `ProviderToolError`. The UI still has
/// a live block from the preceding `ToolStarted`, so it must treat that
/// provider-facing error as terminal and render the error in history.
#[test]
fn provider_tool_error_without_logical_tool_error_finishes_live_tool() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "bad-args".into(),
                name: tau_proto::ToolName::new("strict_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started("bad-args", "strict_tool", CborValue::Map(Vec::new())),
        tau_proto::UnixMicros::new(1_500_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "strict_tool 0s …"));

    renderer.handle_recorded_at(
        &Event::ProviderToolError(ToolError {
            call_id: "bad-args".into(),
            tool_name: tau_proto::ToolName::new("strict_tool"),
            tool_type: tau_proto::ToolType::Function,
            message: "invalid arguments: unexpected argument `extra`".to_owned(),
            details: None,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "err: invalid"));
    assert!(!vt.screen_contains(80, "strict_tool 0s …"));
}

#[test]
fn running_tool_call_shows_ellipsis_until_result() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "read",
            CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs 0s …"));

    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Map(vec![
                (
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                ),
                (
                    CborValue::Text("content".into()),
                    CborValue::Text("fn main() {}\n".into()),
                ),
            ]),
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolDisplay {
                args: "src/main.rs".into(),
                stats: tau_proto::ToolDisplayStats {
                    matches: None,
                    lines: Some(1),
                    bytes: Some(13),
                },
                status: tau_proto::ToolDisplayStatus::Success,
                status_text: "ok".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(3_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs 1L, 13B 2s ok"));
    assert!(!vt.screen_contains(80, "read src/main.rs …"));
}

#[test]
fn backgrounded_tool_stays_visibly_running_until_background_result() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    let in_progress = renderer.agent_in_progress_state();

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("shell"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("command".into()),
                    CborValue::Text("sleep 10".into()),
                )]),
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "shell",
            CborValue::Map(vec![(
                CborValue::Text("command".into()),
                CborValue::Text("sleep 10".into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ProviderToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text(
                "tau_internal: true\n\nTool call `call-1` is running in the background.".into(),
            ),
            kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);
    assert!(in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(vt.screen_contains(80, "shell sleep 10 0s …"));
    assert!(!vt.screen_contains(80, "shell 1s ok"));
    assert!(vt.screen_contains(80, "0/1"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-final",
        vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "done for now".into(),
            }],
            phase: None,
        })],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "0/1"));

    renderer.handle_recorded_at(
        &Event::ToolBackgroundResult(ToolBackgroundResult {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("done".into()),
            display: Some(tau_proto::ToolDisplay {
                args: "sleep 10".into(),
                status: tau_proto::ToolDisplayStatus::Success,
                status_text: "ok".into(),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(4_000_000),
    );
    sync(&handle);
    assert!(!in_progress.load(std::sync::atomic::Ordering::Relaxed));
    assert!(vt.screen_contains(80, "shell sleep 10 3s ok"));
    assert!(vt.screen_contains(80, "1/1"));
}

/// Regression coverage for multiline `shell` calls in `show-tools=full`:
/// the running block must already reserve/show the command body, matching the
/// final result block and avoiding a layout jump when the command finishes.
#[test]
fn running_shell_tool_shows_multiline_command_body_in_full_mode() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    let command = "printf hello\nprintf world";

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("shell"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("command".into()),
                    CborValue::Text(command.into()),
                )]),
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "shell",
            CborValue::Map(vec![(
                CborValue::Text("command".into()),
                CborValue::Text(command.into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);

    assert!(vt.screen_contains(100, "shell printf hello 0s …"));
    assert!(
        vt.screen_text(100)
            .iter()
            .any(|row| row.trim() == "printf world"),
        "running shell command body should be on its own row"
    );

    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Null,
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolDisplay {
                args: "printf hello".into(),
                status: tau_proto::ToolDisplayStatus::Success,
                status_text: "ok".into(),
                payload: Some(tau_proto::ToolDisplayPayload::Text {
                    text: command.into(),
                }),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(2_000_000),
    );
    sync(&handle);

    assert!(vt.screen_contains(100, "shell printf hello 1s ok"));
    assert!(
        vt.screen_text(100)
            .iter()
            .any(|row| row.trim() == "printf world"),
        "finished shell command body should stay on its own row"
    );
}

#[test]
fn finished_tool_result_preserves_message_and_tool_item_order() {
    let (_term, handle, vt) = setup(100, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![
            assistant_message_item("before tool"),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
            }),
            assistant_message_item("after tool"),
        ],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolDisplay {
            args: "src/main.rs".into(),
            status: tau_proto::ToolDisplayStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    let lines = vt.screen_text(100);
    let before = lines
        .iter()
        .position(|line| line.contains("before tool"))
        .unwrap_or_else(|| panic!("missing first message: {lines:?}"));
    let tool = lines
        .iter()
        .position(|line| line.contains("read src/main.rs"))
        .unwrap_or_else(|| panic!("missing tool call: {lines:?}"));
    let after = lines
        .iter()
        .position(|line| line.contains("after tool"))
        .unwrap_or_else(|| panic!("missing second message: {lines:?}"));
    assert!(
        before < tool && tool < after,
        "output_items order should be preserved; lines: {lines:?}",
    );
}

#[test]
fn live_tool_timer_updates_do_not_mutate_scrolled_history() {
    // Running tool calls live in the fixed active-tools area above the prompt.
    // Timer ticks should therefore repaint that visible area only, not trigger a
    // hidden-prefix full redraw of old transcript rows that have moved to
    // scrollback.
    let (_term, handle, vt) = setup(80, 5);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-history",
        (0..10)
            .map(|i| assistant_message_item(format!("history line {i}")))
            .collect(),
    )));
    let read_args = CborValue::Map(vec![(
        CborValue::Text("path".into()),
        CborValue::Text("src/main.rs".into()),
    )]);
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-tool",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: read_args.clone(),
        })],
    )));
    renderer.handle(&tool_started("call-1", "read", read_args));
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs"));

    let full_renders_before = handle.full_render_count();
    renderer.handle_tool_timer_tick();
    sync(&handle);

    assert_eq!(
        handle.full_render_count(),
        full_renders_before,
        "live timer ticks must not full-redraw hidden transcript rows",
    );
    assert!(vt.screen_contains(80, "read src/main.rs"));
}

#[test]
fn show_tools_summarize_turn_summarizes_tool_batch() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    renderer.apply_setting("show-tools", "summarize-turn");

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-2".into(),
                name: tau_proto::ToolName::new("grep"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("pattern".into()),
                    CborValue::Text("foo".into()),
                )]),
            }),
        ],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 0/2 …"));
    assert!(!vt.screen_contains(80, "read src/main.rs"));

    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolDisplay {
            args: "src/main.rs".into(),
            stats: tau_proto::ToolDisplayStats {
                matches: None,
                lines: Some(1),
                bytes: Some(13),
            },
            status: tau_proto::ToolDisplayStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ToolError(tau_proto::ToolError {
        call_id: "call-2".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        message: "nope".into(),
        details: None,
        display: Some(tau_proto::ToolDisplay {
            args: "foo".into(),
            status: tau_proto::ToolDisplayStatus::Error,
            status_text: "err: nope".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 2/2 1L, 13B ok: 1 err: 1"));
    assert!(!vt.screen_contains(80, "read src/main.rs 1L, 13B ok"));
    assert!(!vt.screen_contains(80, "grep foo err: nope"));
}

#[test]
fn show_tools_summarize_prompt_aggregates_across_tool_followups() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    renderer.apply_setting("show-tools", "summarize-prompt");

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
        })],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolDisplay {
            args: "src/main.rs".into(),
            stats: tau_proto::ToolDisplayStats {
                matches: None,
                lines: Some(1),
                bytes: Some(13),
            },
            status: tau_proto::ToolDisplayStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 1/1 1L, 13B ok: 1"));

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-1",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-2".into(),
            name: tau_proto::ToolName::new("grep"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("pattern".into()),
                CborValue::Text("foo".into()),
            )]),
        })],
    )));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 1/2 1L, 13B ok: 1 …"));
    assert!(!vt.screen_contains(80, "tools 1/1"));
    assert!(!vt.screen_contains(80, "grep foo"));

    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "call-2".into(),
        tool_name: tau_proto::ToolName::new("grep"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolDisplay {
            args: "foo".into(),
            stats: tau_proto::ToolDisplayStats {
                matches: Some(3),
                lines: None,
                bytes: None,
            },
            status: tau_proto::ToolDisplayStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "tools 2/2 3, 1L, 13B ok: 2"));
    assert!(!vt.screen_contains(80, "read src/main.rs 1L, 13B ok"));
    assert!(!vt.screen_contains(80, "grep foo (3 matches) ok"));
}

#[test]
fn show_tools_compact_hides_payload_body() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    renderer.apply_setting("show-tools", "compact");

    renderer.handle_recorded_at(
        &Event::ProviderResponseFinished(finished_response(
            "sp-0",
            vec![ContextItem::ToolCall(ToolCallItem {
                call_id: "call-1".into(),
                name: tau_proto::ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".into()),
                    CborValue::Text("src/main.rs".into()),
                )]),
            })],
        )),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &tool_started(
            "call-1",
            "read",
            CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
        ),
        tau_proto::UnixMicros::new(1_000_000),
    );
    renderer.handle_recorded_at(
        &Event::ToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Null,
            kind: tau_proto::ToolResultKind::Final,
            display: Some(tau_proto::ToolDisplay {
                args: "src/main.rs".into(),
                stats: tau_proto::ToolDisplayStats {
                    matches: None,
                    lines: Some(1),
                    bytes: Some(13),
                },
                status: tau_proto::ToolDisplayStatus::Success,
                status_text: "ok".into(),
                payload: Some(tau_proto::ToolDisplayPayload::Text {
                    text: "fn main() {}\n".into(),
                }),
                ..Default::default()
            }),
            originator: tau_proto::PromptOriginator::User,
        }),
        tau_proto::UnixMicros::new(1_000_000),
    );
    sync(&handle);
    assert!(vt.screen_contains(80, "read src/main.rs 1L, 13B 0s ok"));
    assert!(!vt.screen_contains(80, "fn main()"));
}

#[test]
fn show_tools_off_hides_tool_blocks() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );
    renderer.apply_setting("show-tools", "off");

    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "call-1".into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".into()),
                CborValue::Text("src/main.rs".into()),
            )]),
        })],
    )));
    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Null,
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(!vt.screen_contains(80, "tools"));
    assert!(!vt.screen_contains(80, "read"));
}

#[test]
fn websearch_tool_result_shows_result_count_and_size() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::ToolResult(ToolResult {
        call_id: "call-web".into(),
        tool_name: tau_proto::ToolName::new("websearch_exa"),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text(
            "Title: One\nURL: https://one.example\n\nTitle: Two\nURL: https://two.example\n".into(),
        ),
        kind: tau_proto::ToolResultKind::Final,
        display: Some(tau_proto::ToolDisplay {
            args: String::new(),
            stats: tau_proto::ToolDisplayStats {
                matches: Some(2),
                lines: Some(193),
                bytes: Some(7370),
            },
            status: tau_proto::ToolDisplayStatus::Success,
            status_text: "ok".into(),
            ..Default::default()
        }),
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(vt.screen_contains(80, "websearch_exa 2, 193L, 7.2kB ok"));
}

#[test]
fn streaming_block_does_not_duplicate_on_finish() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-0", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "hello!".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item("hello!")],
    )));
    sync(&handle);

    // Count how many rows contain "hello!".
    let count = vt
        .screen_text(80)
        .iter()
        .filter(|r| r.contains("hello!"))
        .count();
    assert_eq!(
        count,
        1,
        "response should appear exactly once, got {count}: {:?}",
        vt.screen_text(80)
    );
}

#[test]
fn agents_md_loaded_event_shows_output_stats() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::ExtAgentsMdAvailable(ExtAgentsMdAvailable {
        file_path: "/tmp/AGENTS.md".into(),
        content: "alpha\nbeta\n".into(),
    }));
    sync(&handle);

    let rows = vt.screen_text(80);
    assert!(
        rows.iter()
            .any(|row| row.contains("loaded: /tmp/AGENTS.md 2L, 11B")),
        "loaded event should include output stats: {rows:?}"
    );
}

#[test]
fn render_tool_display_assembles_chips_in_order() {
    use tau_proto::{ToolDisplay, ToolDisplayStats, ToolDisplayStatus};

    // grep-style: matches + stats + status.
    let display = ToolDisplay {
        args: "\"foo\" in src".into(),
        stats: ToolDisplayStats {
            matches: Some(3),
            lines: Some(7),
            bytes: Some(120),
        },
        status: ToolDisplayStatus::Success,
        status_text: "ok".into(),
        ..Default::default()
    };
    let rendered = render_tool_display("grep", &display);
    assert_eq!(rendered.tool_name, "grep");
    assert_eq!(rendered.args, "\"foo\" in src");
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["3, 7L, 120B", "ok"]);
    assert!(matches!(
        rendered.suffixes.last().expect("status suffix").status,
        ToolStatus::Success
    ));
}

#[test]
fn render_delegate_display_pulls_legacy_role_args_into_first_suffix() {
    use tau_proto::{ProgressCounter, ProgressUnit, ToolDisplay, ToolDisplayStatus};

    // Regression: delegate roles used to be embedded in `ToolDisplay.args`,
    // which made `+engineer` inherit the tool-args color. Rendering delegates now
    // strips that legacy suffix and reinserts the role as the first dedicated
    // suffix so later progress chips keep their existing order.
    let display = ToolDisplay {
        args: "[probe] +engineer".into(),
        progress_counters: vec![ProgressCounter {
            label: Some("tools".into()),
            unit: ProgressUnit::Count,
            complete: Some(3),
            total: Some(3),
        }],
        status: ToolDisplayStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    let rendered = render_delegate_display(&display, Some("engineer"), None);
    assert_eq!(rendered.tool_name, "delegate");
    assert_eq!(rendered.args, "[probe]");
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["+engineer", "%3/3", tau_proto::PROGRESS_INDICATOR_TEXT]
    );
    assert!(matches!(rendered.suffixes[0].status, ToolStatus::Role));
}

#[test]
fn render_delegate_display_adds_execution_mode_marker_after_role() {
    use tau_proto::{ToolDisplay, ToolDisplayStatus, ToolExecutionMode};

    // Delegate scheduling mode is a compact chip beside the role so users can
    // see whether a sub-agent is shared (`s`), update-locked (`u`), or exclusive
    // (`x`) without reading the full tool arguments.
    let display = ToolDisplay {
        args: "[probe]".into(),
        status: ToolDisplayStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    for (mode, marker) in [
        (ToolExecutionMode::Exclusive, "x"),
        (ToolExecutionMode::Update, "u"),
        (ToolExecutionMode::Shared, "s"),
    ] {
        let rendered = render_delegate_display(&display, Some("engineer"), Some(mode));
        let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["+engineer", marker, tau_proto::PROGRESS_INDICATOR_TEXT]
        );
        assert!(matches!(rendered.suffixes[1].status, ToolStatus::Info));
    }

    let rendered = render_delegate_display(&display, None, Some(ToolExecutionMode::Shared));
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["s", tau_proto::PROGRESS_INDICATOR_TEXT]);
}

#[test]
fn render_delegate_completion_keeps_execution_mode_marker() {
    use tau_proto::{ToolDisplay, ToolDisplayStats, ToolDisplayStatus, ToolExecutionMode};

    // Completion rendering is rebuilt from the cached DelegateProgress. Keep the
    // mode chip when the running line becomes the final `ok` / `err` line.
    let cached = ToolDisplay {
        args: "[audit]".into(),
        stats: ToolDisplayStats {
            matches: None,
            lines: Some(10),
            bytes: Some(200),
        },
        status: ToolDisplayStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };
    let display =
        build_delegate_completion_display(Some(&cached), &CborValue::Text("ok\nmore".into()), None);

    let rendered =
        render_delegate_display(&display, Some("engineer"), Some(ToolExecutionMode::Update));
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["+engineer", "u", "↘︎10L, 200B", "↖︎2L, 7B", "ok"]);
}

#[test]
fn render_delegate_display_marks_input_and_output_stats() {
    use tau_proto::{
        ProgressCounter, ProgressUnit, ToolDisplay, ToolDisplayStats, ToolDisplayStatus,
    };

    let input = ToolDisplay {
        args: "[audit]".into(),
        stats: ToolDisplayStats {
            matches: None,
            lines: Some(2),
            bytes: Some(12),
        },
        status: ToolDisplayStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };
    let rendered = render_delegate_display(&input, None, None);
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["↘︎2L, 12B", tau_proto::PROGRESS_INDICATOR_TEXT]);

    let output = ToolDisplay {
        args: "[audit]".into(),
        stats: ToolDisplayStats {
            matches: None,
            lines: Some(3),
            bytes: Some(24),
        },
        progress_counters: vec![ProgressCounter {
            label: Some("tools".into()),
            unit: ProgressUnit::Count,
            complete: Some(2),
            total: Some(2),
        }],
        status: ToolDisplayStatus::Success,
        status_text: "ok".into(),
        info_chips: vec!["↘︎2L, 12B".into()],
        ..Default::default()
    };
    let rendered = render_delegate_display(&output, None, None);
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["↘︎2L, 12B", "↖︎3L, 24B", "%2/2", "ok"]);
}

#[test]
fn render_delegate_display_styles_role_like_status_bar() {
    use tau_proto::{ToolDisplay, ToolDisplayStatus};

    // Regression: the delegate role is visually the same semantic chip as the
    // bottom status-bar role, not part of the free-form tool args string.
    let theme = tau_themes::Theme::builtin();
    let display = ToolDisplay {
        args: "[probe]".into(),
        status: ToolDisplayStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    let rendered = render_delegate_display(&display, Some("engineer"), None);
    let block = render_tool_block(&theme, &rendered);
    let role_span = block
        .content
        .spans()
        .iter()
        .find(|span| span.text == "+engineer")
        .expect("delegate role span");

    assert_eq!(
        role_span.style,
        tau_cli_term::resolve::resolve(&theme, tau_themes::names::STATUS_ROLE)
    );
}

#[test]
fn delegate_completion_keeps_input_stats_with_output_stats() {
    use tau_proto::{ToolDisplay, ToolDisplayStats, ToolDisplayStatus};

    let cached = ToolDisplay {
        args: "[audit]".into(),
        stats: ToolDisplayStats {
            matches: None,
            lines: Some(10),
            bytes: Some(200),
        },
        status: ToolDisplayStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    let display =
        build_delegate_completion_display(Some(&cached), &CborValue::Text("ok\nmore".into()), None);

    assert_eq!(display.args, "[audit]");
    assert_eq!(display.stats, ToolDisplayStats::for_text("ok\nmore"));
    assert_eq!(display.info_chips, vec!["↘︎10L, 200B"]);
    assert_eq!(display.status, ToolDisplayStatus::Success);
    assert_eq!(display.status_text, "ok");
}

#[test]
fn delegate_completion_uses_output_stats_from_duration_result_map() {
    use tau_proto::{ToolDisplay, ToolDisplayStats, ToolDisplayStatus};

    let cached = ToolDisplay {
        args: "[audit]".into(),
        stats: ToolDisplayStats {
            matches: None,
            lines: Some(10),
            bytes: Some(200),
        },
        status: ToolDisplayStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };
    let details = CborValue::Map(vec![
        (
            CborValue::Text("output".into()),
            CborValue::Text("ok\nmore".into()),
        ),
        (
            CborValue::Text("duration_seconds".into()),
            CborValue::Integer(6.into()),
        ),
    ]);

    let display = build_delegate_completion_display(Some(&cached), &details, None);

    assert_eq!(display.args, "[audit]");
    assert_eq!(display.stats, ToolDisplayStats::for_text("ok\nmore"));
    assert_eq!(display.info_chips, vec!["↘︎10L, 200B"]);
    assert_eq!(display.status, ToolDisplayStatus::Success);
    assert_eq!(display.status_text, "ok");
}

#[test]
fn delegate_completion_keeps_input_stats_for_empty_output() {
    use tau_proto::{ToolDisplay, ToolDisplayStats, ToolDisplayStatus};

    let cached = ToolDisplay {
        args: "[audit]".into(),
        stats: ToolDisplayStats {
            matches: None,
            lines: Some(10),
            bytes: Some(200),
        },
        status: ToolDisplayStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    let display =
        build_delegate_completion_display(Some(&cached), &CborValue::Text(String::new()), None);

    assert_eq!(display.stats, ToolDisplayStats::default());
    assert_eq!(display.info_chips, vec!["↘︎10L, 200B"]);
    assert_eq!(display.status, ToolDisplayStatus::Success);
    assert_eq!(display.status_text, "ok");
}

#[test]
fn render_tool_display_token_progress_formats_context_like_status_bar() {
    use tau_proto::{ProgressCounter, ProgressUnit, ToolDisplay, ToolDisplayStatus};

    let display = ToolDisplay {
        args: "[research]".into(),
        progress_counters: vec![ProgressCounter {
            label: Some("ctx".into()),
            unit: ProgressUnit::Tokens,
            complete: Some(133_400),
            total: Some(200_000),
        }],
        status: ToolDisplayStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.into(),
        ..Default::default()
    };

    let rendered = render_tool_display("delegate", &display);
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["#133.4k/200k", tau_proto::PROGRESS_INDICATOR_TEXT]
    );
}

#[test]
fn render_tool_display_text_payload_is_preserved_for_block_rendering() {
    use tau_proto::{ToolDisplay, ToolDisplayPayload, ToolDisplayStatus};

    let display = ToolDisplay {
        args: "printf hello".into(),
        status: ToolDisplayStatus::Success,
        status_text: "ok".into(),
        payload: Some(ToolDisplayPayload::Text {
            text: "printf hello\nprintf world".into(),
        }),
        ..Default::default()
    };
    let rendered = render_tool_display("shell", &display);
    assert_eq!(rendered.args, "printf hello");
    assert_eq!(rendered.payload, display.payload);
}

#[test]
fn render_tool_display_diff_payload_adds_plus_minus_chips() {
    use tau_proto::{DiffSummary, ToolDisplay, ToolDisplayPayload, ToolDisplayStatus};

    let display = ToolDisplay {
        args: "src/main.rs".into(),
        status: ToolDisplayStatus::Success,
        status_text: "ok".into(),
        payload: Some(ToolDisplayPayload::Diff(DiffSummary {
            added: 12,
            removed: 3,
            hunks: vec![],
        })),
        ..Default::default()
    };
    let rendered = render_tool_display("edit", &display);
    let texts: Vec<&str> = rendered.suffixes.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, vec!["+12", "-3", "ok"]);
    assert!(matches!(rendered.suffixes[0].status, ToolStatus::DiffAdded));
    assert!(matches!(
        rendered.suffixes[1].status,
        ToolStatus::DiffRemoved
    ));
}

#[test]
fn synthesize_fallback_display_is_minimal() {
    let ok = synthesize_fallback_display("my_tool", None);
    assert_eq!(ok.args, "");
    assert_eq!(ok.status_text, "ok");
    assert!(matches!(ok.status, tau_proto::ToolDisplayStatus::Success));

    let err =
        synthesize_fallback_display("my_tool", Some("failure description\nwith trailing line"));
    assert_eq!(err.status_text, "failure description");
    assert!(matches!(err.status, tau_proto::ToolDisplayStatus::Error));
}

#[test]
fn fallback_error_status_is_abbreviated_only_by_renderer() {
    let message =
        "failed to access /home/dpc/agent/.agents/skills: No such file or directory (os error 2)";
    let display = synthesize_fallback_display("ls", Some(message));
    assert_eq!(display.status_text, message);
    assert!(!display.status_text.contains("err:"));
    assert!(!display.status_text.contains('…'));

    let rendered = render_tool_display("ls", &display);
    let block = render_tool_block(&tau_themes::Theme::builtin(), &rendered);
    let text: String = block
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(text.contains('┄'));
    assert!(!text.contains('…'));
}

#[test]
fn render_tool_display_error_status_picks_error_severity() {
    use tau_proto::{ToolDisplay, ToolDisplayStatus};

    let display = ToolDisplay {
        args: "/etc".into(),
        status: ToolDisplayStatus::Error,
        status_text: "permission denied".into(),
        ..Default::default()
    };
    let rendered = render_tool_display("ls", &display);
    assert_eq!(rendered.suffixes.len(), 1);
    assert_eq!(rendered.suffixes[0].text, "err: permission denied");
    assert!(matches!(rendered.suffixes[0].status, ToolStatus::Error));

    let legacy_display = ToolDisplay {
        args: "/etc".into(),
        status: ToolDisplayStatus::Error,
        status_text: "err: permission denied".into(),
        ..Default::default()
    };
    let rendered = render_tool_display("ls", &legacy_display);
    assert_eq!(rendered.suffixes[0].text, "err: permission denied");
}

#[test]
fn render_tool_block_abbreviates_inline_args_and_error_but_preserves_payload() {
    use tau_proto::{ToolDisplay, ToolDisplayPayload, ToolDisplayStatus};

    let payload = "full payload line one\nfull payload line two".to_owned();
    let display = ToolDisplay {
        args: "LOG_MODULE_WALLETV2|LOG_CLIENT_MODULE_WALLETV2 in modules/fedimint-walletv2-server/src modules/fedimint-walletv2-client/src".into(),
        status: ToolDisplayStatus::Error,
        status_text: "ripgrep error: rg: modules/fedimint-walletv2-server/src modules/fedimint-walletv2-client/src: IO error for operation".into(),
        payload: Some(ToolDisplayPayload::Text {
            text: payload.clone(),
        }),
        ..Default::default()
    };
    let rendered = render_tool_display("grep", &display);
    let block = render_tool_block(&tau_themes::Theme::builtin(), &rendered);
    let text: String = block
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(text.contains("LOG_MODULE_WALLETV2|┄-walletv2-client/src"));
    assert!(text.contains("err: ripgrep error: ┄ error for operation"));
    assert!(!text.contains(&display.args));
    assert!(!text.contains(&display.status_text));
    assert!(text.contains(&payload));
}

#[test]
fn render_shell_block_abbreviates_inline_command_and_status_but_preserves_output() {
    let command = "printf 1234567890123456789012345678901234567890";
    let status = "err: command failed after printing a very long diagnostic";
    let output = "full output line one\nfull output line two";
    let block = render_shell_block(&tau_themes::Theme::builtin(), command, output, Some(status));
    let text: String = block
        .content
        .spans()
        .iter()
        .map(|span| span.text.as_str())
        .collect();

    assert!(text.contains("printf 1234567890123┄12345678901234567890"));
    assert!(text.contains("err: command failed ┄very long diagnostic"));
    assert!(!text.contains(status));
    assert!(text.contains(output));
}

#[test]
fn build_osc1337_set_user_var_encodes_value_and_respects_tmux() {
    let plain = build_osc1337_set_user_var("user-notification", "hello", false);
    assert_eq!(plain, "\x1b]1337;SetUserVar=user-notification=aGVsbG8=\x07");
    let wrapped = build_osc1337_set_user_var("user-notification", "hello", true);
    assert_eq!(
        wrapped,
        "\x1bPtmux;\x1b\x1b]1337;SetUserVar=user-notification=aGVsbG8=\x07\x1b\\",
    );
}

#[test]
fn format_turn_stats_line_formats_short_latencies_as_millis() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 17_341,
        prompt_cached_tokens: 16_896,
        response_received_tokens: 29,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 100_000,
                cached_tokens: 50_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 16_000,
        response_received_tokens: 1_341,
        ..Default::default()
    };
    let line = format_turn_stats_line(
        &usage,
        Some(&previous_usage),
        Some(Duration::from_millis(1_240)),
        Some(Duration::from_millis(4_560)),
    );

    assert_eq!(line, "Δ97% 16.8k/17.3k ↑0 ↓29 1240ms Σ ↑50k/100k ↓0 4560ms",);
}

#[test]
fn format_turn_stats_line_formats_long_latencies_compactly() {
    let usage = tau_proto::ProviderTokenUsage {
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 1_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let line = format_turn_stats_line(
        &usage,
        None,
        Some(Duration::from_millis(18_723)),
        Some(Duration::from_secs(5 * 60 + 1)),
    );

    assert_eq!(line, "Δ0% 0/0 ↑0 ↓0 18s Σ ↑0/1k ↓0 5m");
}

#[test]
fn format_turn_stats_line_uses_previous_turn_for_hit_percent() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_100,
        prompt_cached_tokens: 19_000,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 40_100,
                cached_tokens: 19_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_000,
        ..Default::default()
    };
    let line = format_turn_stats_line(&usage, Some(&previous_usage), None, None);

    assert_eq!(line, "Δ95% 19k/20k ↑100 ↓0 Σ ↑19k/40.1k ↓0");
}

#[test]
fn format_turn_stats_line_shows_zero_hit_when_nothing_could_be_cached() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 1_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let line = format_turn_stats_line(&usage, None, None, None);

    assert_eq!(line, "Δ0% 0/0 ↑1k ↓0 Σ ↑0/1k ↓0");
}

#[test]
fn format_turn_stats_line_shows_zero_hit_when_no_prompt_sent() {
    let usage = tau_proto::ProviderTokenUsage::default();
    let line = format_turn_stats_line(&usage, None, None, None);

    assert_eq!(line, "Δ0% 0/0 ↑0 ↓0 Σ ↑0/0 ↓0");
}

#[test]
fn render_action_output_block_highlights_approval_ids_and_labels() {
    let theme = tau_themes::Theme::builtin();
    let block = render_action_output_block(
        &theme,
        "Incoming approval 7\nstatus: pending\n8 account=personal folder=INBOX\n",
    );
    let spans = block.content.spans();
    let id_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_ID);
    let label_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_LABEL);

    let heading_id = spans
        .iter()
        .find(|span| span.text == "7")
        .expect("heading approval id span");
    let row_id = spans
        .iter()
        .find(|span| span.text == "8")
        .expect("list row approval id span");
    let status_label = spans
        .iter()
        .find(|span| span.text == "status:")
        .expect("status label span");
    let account_label = spans
        .iter()
        .find(|span| span.text == "account=")
        .expect("key-value label span");

    assert_eq!(heading_id.style, id_style);
    assert_eq!(row_id.style, id_style);
    assert_eq!(status_label.style, label_style);
    assert_eq!(account_label.style, label_style);
}

#[test]
fn render_action_error_block_uses_action_error_styles() {
    let theme = tau_themes::Theme::builtin();
    let block = render_action_error_block(&theme, "7", "invalid input");
    let spans = block.content.spans();
    let id_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_ID);
    let error_style = tau_cli_term::resolve::resolve(&theme, tau_themes::names::ACTION_ERROR);

    assert_eq!(spans[0].text, "7");
    assert_eq!(spans[0].style, id_style);
    assert_eq!(spans[2].text, "invalid input");
    assert_eq!(spans[2].style, error_style);
}

#[test]
fn render_turn_stats_block_uses_dedicated_styles() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000,
        prompt_cached_tokens: 900,
        response_received_tokens: 42,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 2_000,
                cached_tokens: 1_000,
                received_tokens: 100,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 1_000,
        ..Default::default()
    };
    let block = render_turn_stats_block(
        &tau_themes::Theme::builtin(),
        &usage,
        Some(&previous_usage),
        None,
        None,
    );
    let spans = block.content.spans();

    assert_eq!(spans[0].text, "Δ");
    assert!(spans[0].style.bold);
    assert_eq!(spans[0].style.fg, Some(Color::DarkGrey));
    assert_eq!(spans[1].text, "90% 900/1k");
    assert!(!spans[1].style.bold);
    assert_eq!(spans[1].style.fg, Some(Color::DarkGrey));
    let sigma = spans
        .iter()
        .find(|span| span.text == " Σ")
        .expect("sigma span is rendered");
    assert!(sigma.style.bold);
    assert_eq!(sigma.style.fg, Some(Color::DarkGrey));
}

#[test]
fn render_turn_stats_block_greys_cache_hit_within_512_rounding_bucket() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 20_100,
        prompt_cached_tokens: 19_456,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 40_100,
                cached_tokens: 19_456,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 19_500,
        ..Default::default()
    };
    let block = render_turn_stats_block(
        &tau_themes::Theme::builtin(),
        &usage,
        Some(&previous_usage),
        None,
        None,
    );
    let spans = block.content.spans();

    assert_eq!(spans[1].text, "99% 19.4k/19.5k");
    assert_eq!(spans[1].style.fg, Some(Color::DarkGrey));
}

#[test]
fn render_turn_stats_block_warns_cache_hit_above_90_percent() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_100,
        prompt_cached_tokens: 9_100,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 20_100,
                cached_tokens: 9_100,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_000,
        ..Default::default()
    };
    let block = render_turn_stats_block(
        &tau_themes::Theme::builtin(),
        &usage,
        Some(&previous_usage),
        None,
        None,
    );
    let spans = block.content.spans();

    assert_eq!(spans[1].text, "91% 9.1k/10k");
    assert_eq!(spans[1].style.fg, Some(Color::DarkYellow));
}

#[test]
fn render_turn_stats_block_highlights_cache_hit_at_or_below_90_percent() {
    let usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_100,
        prompt_cached_tokens: 9_000,
        stats: tau_proto::TokenUsageStats {
            total: tau_proto::TokenUsageCounts {
                sent_tokens: 20_100,
                cached_tokens: 9_000,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let previous_usage = tau_proto::ProviderTokenUsage {
        prompt_sent_tokens: 10_000,
        ..Default::default()
    };
    let block = render_turn_stats_block(
        &tau_themes::Theme::builtin(),
        &usage,
        Some(&previous_usage),
        None,
        None,
    );
    let spans = block.content.spans();

    assert_eq!(spans[1].text, "90% 9k/10k");
    assert_eq!(spans[1].style.fg, Some(Color::Red));
}

#[test]
fn cache_hit_percent_clamps_to_possible_cached_tokens() {
    assert_eq!(cache_hit_percent(Some(2_000), Some(1_500)), Some(75));
    assert_eq!(cache_hit_percent(Some(2_000), Some(3_000)), Some(100));
    assert_eq!(cache_hit_percent(Some(0), Some(0)), Some(0));
    assert_eq!(cache_hit_percent(Some(2_000), None), None);
}

#[test]
fn streaming_block_handles_each_trailing_case() {
    let theme = tau_themes::Theme::builtin();
    let cases = [
        ("", "…"),
        ("Hello", "Hello …"),
        ("Hello ", "Hello …"),
        ("Hello\t", "Hello\t…"),
        ("line\n", "line\n…"),
        ("line\n  ", "line\n  …"),
    ];
    for (input, expected) in cases {
        let block = streaming_block(&theme, tau_themes::names::AGENT_RESPONSE, input);
        let actual: String = block
            .content
            .spans()
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(actual, expected, "input was {input:?}");
    }
}

/// Reproduces the user-reported bug: send 3 prompts during the
/// first response's streaming. After all responses complete, the
/// prompt must be visible and all 3 responses rendered.
#[test]
fn three_prompts_during_streaming_all_render_correctly() {
    let (_term, handle, vt) = setup(80, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    // User sends first prompt.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-0", "s1",
    )));

    // Agent starts streaming response 1.
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "Hello".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "Hello"),
        "streaming should show, got: {:?}",
        vt.screen_text(80)
    );

    // User sends 2nd and 3rd prompts while streaming.
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
        session_id: "s1".into(),
        text: "hi".into(),
        message_class: tau_proto::PromptMessageClass::User,
    }));
    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptQueued(SessionPromptQueued {
        session_id: "s1".into(),
        text: "hi".into(),
        message_class: tau_proto::PromptMessageClass::User,
    }));

    // More streaming updates (multi-line, like a real LLM).
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: "Hello!\n\nHow can I help you today?".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);

    // Response 1 finishes.
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item(
            "Hello!\n\nHow can I help you today?",
        )],
    )));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "How can I help you today?"),
        "response 1 should be in history, got: {:?}",
        vt.screen_text(80)
    );

    // Second prompt dispatched.
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-1", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-1".into(),
        text: "Hello again!\n\nHow can I help you?".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-1",
        vec![assistant_message_item(
            "Hello again!\n\nHow can I help you?",
        )],
    )));
    sync(&handle);
    assert!(
        vt.screen_contains(80, "How can I help you?"),
        "response 2 should be visible, got: {:?}",
        vt.screen_text(80)
    );

    // Third prompt dispatched.
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-2", "s1",
    )));
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-2".into(),
        text: "Hi there!\n\nWhat can I help you with?".into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-2",
        vec![assistant_message_item(
            "Hi there!\n\nWhat can I help you with?",
        )],
    )));
    sync(&handle);

    // All three responses should be visible.
    assert!(
        vt.screen_contains(80, "How can I help you today?"),
        "response 1 missing, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "How can I help you?"),
        "response 2 missing, got: {:?}",
        vt.screen_text(80)
    );
    assert!(
        vt.screen_contains(80, "What can I help you with?"),
        "response 3 missing, got: {:?}",
        vt.screen_text(80)
    );

    // The prompt must be visible at the bottom.
    assert!(
        vt.screen_contains(80, "> "),
        "prompt should be visible after all responses, got: {:?}",
        vt.screen_text(80)
    );

    // No stale streaming blocks should remain.
    assert!(
        !vt.screen_contains(80, "…"),
        "no '…' should remain, got: {:?}",
        vt.screen_text(80)
    );
}

/// Emoji (wide characters) in responses must not corrupt the
/// layout. Each emoji occupies 2 terminal columns; if we count
/// them as 1, text after the emoji shifts right and wraps
/// incorrectly.
#[test]
fn emoji_in_response_renders_correctly() {
    let (_term, handle, vt) = setup(40, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-0", "s1",
    )));

    // Response with emoji followed by text on next line.
    let response = "Hello! 👋\n\nHow can I help you today?";
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: response.into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item(response)],
    )));
    sync(&handle);

    let text = vt.screen_text(40);

    // "Hello! 👋" should be on its own line, not merged with the
    // next line.
    assert!(
        vt.screen_contains(40, "Hello!"),
        "emoji line missing, got: {:?}",
        text
    );
    // The text after \n\n should start at column 0, not offset.
    assert!(
        text.iter().any(|r| r.starts_with("How can I help")),
        "text after emoji should start at column 0, got: {:?}",
        text
    );
    // Prompt must be visible.
    assert!(
        vt.screen_contains(40, "> "),
        "prompt missing, got: {:?}",
        text
    );
}

/// Multiple emoji in a single line must not cause column drift.
#[test]
fn multiple_emoji_no_column_drift() {
    let (_term, handle, vt) = setup(40, 24);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hi".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-0", "s1",
    )));

    // 3 emoji = 6 columns + "end" = 9 columns total.
    let response = "🎉🎊🎈end\nnext line here";
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item(response)],
    )));
    sync(&handle);

    let text = vt.screen_text(40);
    // "next line here" should start at column 0.
    assert!(
        text.iter().any(|r| r.starts_with("next line here")),
        "line after emoji should start at col 0, got: {:?}",
        text
    );
}

/// Replacing a long streaming block with its final settled output
/// must not leave stale partial lines behind, even when the live
/// block overflowed the viewport while streaming.
#[test]
fn overflowing_stream_replaced_cleanly_on_finish() {
    let (_term, handle, vt) = setup(40, 5);
    let mut renderer = EventRenderer::new(
        handle.clone(),
        tau_cli_term::CompletionData::new(),
        tau_themes::Theme::builtin(),
    );

    renderer.handle(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "overflow please".into(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    renderer.handle(&Event::SessionPromptCreated(session_prompt_created(
        "sp-0", "s1",
    )));

    let partial = "stream 0\nstream 1\nstream 2\nstream 3\nPARTIAL ONLY";
    renderer.handle(&Event::ProviderResponseUpdated(ProviderResponseUpdated {
        session_prompt_id: "sp-0".into(),
        text: partial.into(),
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    sync(&handle);
    assert!(
        vt.screen_contains(40, "PARTIAL ONLY"),
        "partial overflowed response should be visible before finish, got: {:?}",
        vt.screen_text(40)
    );

    let final_text = "final 0\nfinal 1\nfinal 2";
    renderer.handle(&Event::ProviderResponseFinished(finished_response(
        "sp-0",
        vec![assistant_message_item(final_text)],
    )));
    sync(&handle);

    let text = vt.screen_text(40);
    assert!(
        vt.screen_contains(40, "final 1"),
        "final response missing, got: {:?}",
        text
    );
    assert!(
        vt.screen_contains(40, "final 2"),
        "final response tail missing, got: {:?}",
        text
    );
    assert!(
        !vt.screen_contains(40, "PARTIAL ONLY"),
        "stale partial content should be gone, got: {:?}",
        text
    );
    assert!(
        vt.screen_contains(40, "> "),
        "prompt should remain visible, got: {:?}",
        text
    );
}
