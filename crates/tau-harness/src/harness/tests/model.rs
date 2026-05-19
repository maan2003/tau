use tau_proto::{
    Effort, HarnessInfoLevel, ModelId, ProviderModelInfo, ProviderModelsUpdated, ThinkingSummary,
    Verbosity,
};

use super::*;

/// Scan the harness event log for an `Important` `HarnessInfo`
/// containing `needle` and return its message. The startup paths emit
/// these synchronously before the constructor returns, so by the time
/// the test inspects the log every check_*_parses event is already
/// committed — no need to pump the bus.
fn find_important_info(h: &Harness, needle: &str) -> Option<String> {
    let mut seq = 0;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq + 1;
        if let Event::HarnessInfo(info) = &entry.event
            && info.level == HarnessInfoLevel::Important
            && info.message.contains(needle)
        {
            return Some(info.message.clone());
        }
    }
    None
}

fn provider_model(id: ModelId, context_window: u64) -> ProviderModelInfo {
    ProviderModelInfo {
        id,
        display_name: None,
        context_window,
        efforts: vec![Effort::Off, Effort::High],
        verbosities: vec![Verbosity::Low, Verbosity::High],
        thinking_summaries: vec![ThinkingSummary::Off, ThinkingSummary::Auto],
        supports_compaction: false,
    }
}

fn provider_models(
    models: impl IntoIterator<Item = ProviderModelInfo>,
) -> std::collections::HashMap<ModelId, ProviderModelInfo> {
    models
        .into_iter()
        .map(|info| (info.id.clone(), info))
        .collect()
}

/// The echo harness publishes `echo/model` during startup so daemon-style tests
/// exercise the normal provider model route. Tests that assert the
/// before-any-model-snapshot state clear that startup snapshot first.
fn clear_startup_echo_models(h: &mut Harness) {
    let provider_id = h
        .extension_connection_id("provider")
        .expect("echo provider")
        .to_owned();
    h.handle_extension_event(
        &provider_id,
        Frame::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: Vec::new(),
        })),
    )
    .expect("clear startup echo provider models");
}

fn connect_provider_source(h: &mut Harness, name: &str) {
    let _frames = connect_test_client(h, name, tau_proto::ClientKind::Provider);
}

/// Role info keeps the machine-readable model/knob summary separate from the
/// free-form role description so completion UIs do not have to parse user text.
#[test]
fn role_infos_include_configured_role_description() {
    let model: ModelId = "openai/gpt-4.1".parse().expect("model id");
    let mut roles = std::collections::HashMap::new();
    roles.insert(
        "smart".to_owned(),
        tau_config::settings::AgentRole {
            description: Some("Balanced coding helper".to_owned()),
            model: Some(model.clone()),
            effort: Some(Effort::High),
            ..Default::default()
        },
    );
    let provider_models = provider_models([provider_model(model.clone(), 128_000)]);
    let infos = role_infos(
        &provider_models,
        &roles,
        &tau_config::settings::ToolsProfiles::default(),
        &[model],
    );

    assert_eq!(infos.len(), 1);
    assert!(infos[0].description.contains("model=openai/gpt-4.1"));
    assert_eq!(
        infos[0].role_description.as_deref(),
        Some("Balanced coding helper")
    );
}

/// Provider snapshots are runtime registry input, not just private extension
/// chatter: the harness must retain metadata/routes and re-emit refreshed UI
/// state for clients that are already connected.
#[test]
fn provider_models_snapshot_updates_available_models() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    connect_provider_source(&mut h, "provider-ext");

    let model_id: ModelId = "openai/gpt-4.1".parse().expect("model id");
    assert!(!h.available_models.contains(&model_id));
    h.handle_extension_event(
        "provider-ext",
        Frame::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(model_id.clone(), 128_000)],
        })),
    )
    .expect("handle provider snapshot");
    assert!(h.available_models.contains(&model_id));
    assert_eq!(
        h.provider_model_info
            .get(&model_id)
            .map(|info| info.context_window),
        Some(128_000),
    );
    assert_eq!(
        h.provider_model_routes.get(&model_id).map(|id| id.as_str()),
        Some("provider-ext"),
    );

    let mut saw_provider_snapshot = false;
    let mut saw_harness_models = false;
    let mut saw_harness_roles = false;
    let mut seq = 0;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq + 1;
        match entry.event {
            Event::ProviderModelsUpdated(update)
                if entry.source.as_deref() == Some("provider-ext") =>
            {
                saw_provider_snapshot = update.models.iter().any(|info| info.id == model_id);
            }
            Event::HarnessModelsAvailable(available) => {
                saw_harness_models = available.models.contains(&model_id);
            }
            Event::HarnessRolesAvailable(_) => {
                saw_harness_roles = true;
            }
            _ => {}
        }
    }
    assert!(saw_provider_snapshot);
    assert!(saw_harness_models);
    assert!(saw_harness_roles);
}

/// Model snapshots are an execution-provider contract. A tool connection that
/// publishes `provider.models_updated` must not be able to claim a model route,
/// otherwise the next prompt could be sent to a non-provider participant.
#[test]
fn provider_models_snapshot_from_non_provider_is_ignored() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    let _frames = connect_test_client(&mut h, "tool-ext", tau_proto::ClientKind::Tool);

    let model_id: ModelId = "evil/model".parse().expect("model id");
    h.handle_extension_event(
        "tool-ext",
        Frame::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(model_id.clone(), 1)],
        })),
    )
    .expect("handle forged provider snapshot");

    assert!(!h.available_models.contains(&model_id));
    assert!(!h.provider_model_info.contains_key(&model_id));
    assert!(!h.provider_model_routes.contains_key(&model_id));
    let mut seq = 0;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq + 1;
        assert!(
            !matches!(entry.event, Event::ProviderModelsUpdated(_))
                || entry.source.as_deref() != Some("tool-ext"),
            "forged provider snapshot must not be published"
        );
    }
}

/// Socket clients are UI participants. Even though their frames enter through
/// the client handler instead of the extension handler, provider-category
/// events from them must not mutate provider routing or get published.
#[test]
fn provider_models_snapshot_from_ui_client_is_ignored() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    let _frames = connect_test_client(&mut h, "ui-client", tau_proto::ClientKind::Ui);

    let model_id: ModelId = "evil/ui-model".parse().expect("model id");
    h.handle_client_event_inner(
        "ui-client",
        Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(model_id.clone(), 1)],
        }),
    )
    .expect("handle forged client provider snapshot");

    assert!(!h.available_models.contains(&model_id));
    assert!(!h.provider_model_info.contains_key(&model_id));
    assert!(!h.provider_model_routes.contains_key(&model_id));
    let mut seq = 0;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq + 1;
        assert!(
            !matches!(entry.event, Event::ProviderModelsUpdated(_))
                || entry.source.as_deref() != Some("ui-client"),
            "client-forged provider snapshot must not be published"
        );
    }
}

/// Startup no longer selects config-file models. A provider snapshot is the
/// moment a runtime model exists, so it should also unblock queued prompts by
/// choosing the first model through the normal harness-owned selection path.
#[test]
fn provider_models_snapshot_selects_first_model_and_drains_queue() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    connect_provider_source(&mut h, "provider-ext");
    assert!(h.selected_model.is_none());

    assert_eq!(
        h.submit_user_prompt("s1".into(), "hello".to_owned())
            .expect("submit prompt"),
        PromptSubmission::Queued,
    );
    assert_eq!(
        h.conversations[&h.default_conversation_id]
            .pending_prompts
            .len(),
        1,
    );

    let model_id: ModelId = "openai/gpt-4.1".parse().expect("model id");
    h.handle_extension_event(
        "provider-ext",
        Frame::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(model_id.clone(), 128_000)],
        })),
    )
    .expect("handle provider snapshot");

    assert_eq!(h.selected_model.as_ref(), Some(&model_id));
    assert_eq!(h.selected_params.effort, Effort::High);
    let conv = &h.conversations[&h.default_conversation_id];
    assert!(conv.pending_prompts.is_empty());
    assert!(matches!(
        conv.turn_state,
        ConversationTurnState::AgentThinking { .. }
    ));
}

/// Provider metadata must replace config compat data once a provider-owned
/// model is selected, otherwise the UI loses context-window and knob choices.
#[test]
fn provider_model_metadata_drives_selection_state() {
    let td = TempDir::new().expect("tempdir");
    let mut h = echo_harness(td.path()).expect("harness");
    clear_startup_echo_models(&mut h);
    connect_provider_source(&mut h, "provider-ext");

    let model_id: ModelId = "openai/gpt-4.1".parse().expect("model id");
    h.handle_extension_event(
        "provider-ext",
        Frame::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![provider_model(model_id.clone(), 123_456)],
        })),
    )
    .expect("handle provider snapshot");

    assert_eq!(h.selected_role, "smart");
    assert_eq!(h.selected_model.as_ref(), Some(&model_id));
    assert_eq!(h.selected_params.effort, Effort::High);
    assert_eq!(h.selected_params.verbosity, Verbosity::Low);
    assert_eq!(h.selected_params.thinking_summary, ThinkingSummary::Auto);

    let mut seq = 0;
    let mut selected = None;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq + 1;
        if let Event::HarnessRoleSelected(event) = entry.event
            && event.model.as_ref() == Some(&model_id)
        {
            selected = Some(event);
        }
    }
    let selected = selected.expect("model selection event");
    assert_eq!(selected.context_window, Some(123_456));

    h.handle_extension_event(
        "provider-ext",
        Frame::Event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![ProviderModelInfo {
                id: model_id.clone(),
                display_name: None,
                context_window: 654_321,
                efforts: vec![Effort::Off],
                verbosities: vec![Verbosity::High],
                thinking_summaries: vec![ThinkingSummary::Off],
                supports_compaction: false,
            }],
        })),
    )
    .expect("refresh provider metadata");

    assert_eq!(h.selected_params.effort, Effort::Off);
    assert_eq!(h.selected_params.verbosity, Verbosity::High);
    assert_eq!(h.selected_params.thinking_summary, ThinkingSummary::Off);

    let mut seq = 0;
    let mut selected = None;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq + 1;
        if let Event::HarnessRoleSelected(event) = entry.event
            && event.model.as_ref() == Some(&model_id)
        {
            selected = Some(event);
        }
    }
    let selected = selected.expect("refreshed model selection event");
    assert_eq!(selected.context_window, Some(654_321));
}

/// Selected role params come from the role, then clamp against provider-owned
/// metadata for the resolved model. This keeps runtime selection role-centric
/// while still respecting each provider's supported knob levels.
#[test]
fn selected_role_params_are_clamped_by_provider_metadata() {
    let openai: ModelId = "openai/gpt-4.1".parse().expect("model id");
    let local: ModelId = "local/llama".parse().expect("model id");
    let provider_models = provider_models([
        ProviderModelInfo {
            id: openai.clone(),
            display_name: None,
            context_window: 128_000,
            efforts: vec![Effort::Off, Effort::High],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
        },
        ProviderModelInfo {
            id: local.clone(),
            display_name: None,
            context_window: 8_192,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
        },
    ]);

    let mut roles = std::collections::HashMap::new();
    let mut openai_role = tau_config::settings::AgentRole {
        model: Some(openai.clone()),
        effort: Some(Effort::High),
        ..Default::default()
    };
    roles.insert("openai".to_owned(), openai_role.clone());
    openai_role.model = Some(local.clone());
    roles.insert("local".to_owned(), openai_role);

    assert_eq!(
        selected_params_for_role(&provider_models, &roles, "openai", &openai).effort,
        Effort::High,
    );
    assert_eq!(
        selected_params_for_role(&provider_models, &roles, "local", &local).effort,
        Effort::Off,
    );
}

/// Persisted role overrides are the live selection, but the baseline shown for
/// "reset to role" must come from `harness.json5`, not from state.
#[test]
fn role_baseline_ignores_persisted_role_overrides() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.json5"),
        r#"{
            roles: {
                smart: { model: "openai/gpt-4.1", effort: "high", verbosity: "medium" },
            },
        }"#,
    )
    .expect("write harness config");
    std::fs::write(
        state_dir.join("harness.json5"),
        r#"{
            "role_overrides": {
                "smart": { "model": "openai/gpt-4.1", "effort": "low", "verbosity": "high" }
            }
        }"#,
    )
    .expect("write state");

    let harness_settings =
        tau_config::settings::load_harness_settings_in(&dirs).expect("load harness settings");
    let (roles, _role_overrides, selected_role) = load_roles(&dirs, &harness_settings);
    let available = vec!["openai/gpt-4.1".parse().expect("model id")];
    let model =
        select_model_for_available(&roles, &selected_role, &available).expect("selected model");
    let provider_models = provider_models([ProviderModelInfo {
        id: model.clone(),
        display_name: None,
        context_window: 128_000,
        efforts: vec![Effort::Off, Effort::Low, Effort::High],
        verbosities: vec![Verbosity::Low, Verbosity::Medium, Verbosity::High],
        thinking_summaries: vec![ThinkingSummary::Off, ThinkingSummary::Auto],
        supports_compaction: false,
    }]);

    let selected = selected_params_for_role(&provider_models, &roles, "smart", &model);
    assert_eq!(selected.effort, Effort::Low);
    assert_eq!(selected.verbosity, Verbosity::High);

    let baseline =
        baseline_params_for_selection(&harness_settings, &provider_models, "smart", &model);
    assert_eq!(baseline.effort, Effort::High);
    assert_eq!(baseline.verbosity, Verbosity::Medium);
}

/// Persisted runtime role overrides must never carry prompt text or role
/// descriptions. Config-only metadata must come from `harness.json5` so changes
/// are reflected after a restart instead of being shadowed by stale state.
#[test]
fn persisted_role_overrides_do_not_shadow_configured_role_metadata() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.json5"),
        r#"{
            roles: {
                smart: {
                    description: "CURRENT CONFIG DESCRIPTION",
                    model: "openai/gpt-4.1",
                    prompt: "CURRENT CONFIG PROMPT",
                    extraPrompt: "CURRENT CONFIG EXTRA",
                },
            },
        }"#,
    )
    .expect("write harness config");
    std::fs::write(
        state_dir.join("harness.json5"),
        r#"{
            "last_selected_role": "smart",
            "role_overrides": {
                "smart": {
                    "description": "STALE STATE DESCRIPTION",
                    "model": "openai/gpt-4.1-mini",
                    "prompt": "STALE STATE PROMPT",
                    "extraPrompt": "STALE STATE EXTRA"
                }
            }
        }"#,
    )
    .expect("write state");

    let harness_settings =
        tau_config::settings::load_harness_settings_in(&dirs).expect("load harness settings");
    let (roles, role_overrides, selected_role) = load_roles(&dirs, &harness_settings);
    let role = roles.get("smart").expect("smart role");
    assert_eq!(selected_role, "smart");
    assert_eq!(
        role.model.as_ref().map(ToString::to_string).as_deref(),
        Some("openai/gpt-4.1-mini")
    );
    assert_eq!(
        role.description.as_deref(),
        Some("CURRENT CONFIG DESCRIPTION")
    );
    assert_eq!(
        role.prompt.as_ref().map(|prompt| prompt.as_str()),
        Some("CURRENT CONFIG PROMPT")
    );
    assert_eq!(
        role.extra_prompt.as_ref().map(|prompt| prompt.as_str()),
        Some("CURRENT CONFIG EXTRA")
    );
    let runtime_override = role_overrides.get("smart").expect("runtime override");
    assert!(runtime_override.description.is_none());
    assert!(runtime_override.prompt.is_none());
    assert!(runtime_override.extra_prompt.is_none());

    save_role_overrides(&dirs, &selected_role, &roles);
    let saved = std::fs::read_to_string(state_dir.join("harness.json5")).expect("read state");
    assert!(
        !saved.contains("description"),
        "saved state must strip description: {saved}"
    );
    assert!(
        !saved.contains("prompt"),
        "saved state must strip prompt fields: {saved}"
    );
    assert!(
        !saved.contains("extraPrompt"),
        "saved state must strip extraPrompt fields: {saved}"
    );
}

/// Roles without an explicit effort get the middle provider-published
/// reasoning level. Providers that publish only `Off` stay at `Off`.
#[test]
fn role_without_effort_picks_middle_provider_effort() {
    let openai: ModelId = "openai/gpt-4.1".parse().expect("model id");
    let local: ModelId = "local/llama".parse().expect("model id");
    let provider_models = provider_models([
        ProviderModelInfo {
            id: openai.clone(),
            display_name: None,
            context_window: 128_000,
            efforts: vec![
                Effort::Off,
                Effort::Minimal,
                Effort::Low,
                Effort::Medium,
                Effort::High,
            ],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
        },
        ProviderModelInfo {
            id: local.clone(),
            display_name: None,
            context_window: 8_192,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
        },
    ]);
    let roles = std::collections::HashMap::from([(
        "smart".to_owned(),
        tau_config::settings::AgentRole::default(),
    )]);

    assert_eq!(
        selected_params_for_role(&provider_models, &roles, "smart", &openai).effort,
        Effort::Low,
    );
    assert_eq!(
        selected_params_for_role(&provider_models, &roles, "smart", &local).effort,
        Effort::Off,
    );
}

/// A stale saved `default` role is not migrated. Runtime models are
/// provider-owned, so startup keeps the `smart` role and waits for a provider
/// snapshot before selecting a model.
#[test]
fn load_roles_falls_back_to_smart_role_while_models_are_provider_owned() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.json5"),
        r#"{
            roles: {
                smart: { model: "local/smart" },
                deep: { model: "local/deep" },
            },
        }"#,
    )
    .expect("write harness config");
    std::fs::write(
        state_dir.join("harness.json5"),
        r#"{
            "last_selected_role": "default",
            "role_overrides": {
                "default": { "model": "local/deep" }
            }
        }"#,
    )
    .expect("write state");

    let harness_settings =
        tau_config::settings::load_harness_settings_in(&dirs).expect("load harness settings");
    let (roles, role_overrides, selected_role) = load_roles(&dirs, &harness_settings);
    assert!(!role_overrides.contains_key("default"));
    assert!(!roles.contains_key("default"));
    assert_eq!(selected_role, "smart");

    let available = vec!["local/deep".into(), "local/smart".into()];
    assert_eq!(
        select_model_for_available(&roles, &selected_role, &available)
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("local/smart")
    );
}

/// Role settings stand on their own: a non-smart role with no model or effort
/// uses the first available model and the selected model's default effort, not
/// smart's configured model or effort.
#[test]
fn role_missing_fields_use_model_defaults() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.json5"),
        r#"{
            roles: {
                smart: { model: "local/smart", effort: "high" },
                plain: {},
            },
        }"#,
    )
    .expect("write harness config");
    std::fs::write(
        state_dir.join("harness.json5"),
        r#"{
            "last_selected_role": "plain"
        }"#,
    )
    .expect("write state");

    let harness_settings =
        tau_config::settings::load_harness_settings_in(&dirs).expect("load harness settings");
    let (roles, _role_overrides, selected_role) = load_roles(&dirs, &harness_settings);
    let available = vec!["local/aaa".into(), "local/smart".into()];
    let selected =
        select_model_for_available(&roles, &selected_role, &available).expect("selected model");
    assert_eq!(selected_role, "plain");
    assert_eq!(selected.to_string(), "local/aaa");

    let provider_models = provider_models([ProviderModelInfo {
        id: selected.clone(),
        display_name: None,
        context_window: 8_192,
        efforts: vec![Effort::Off, Effort::Low, Effort::High],
        verbosities: vec![Verbosity::Medium],
        thinking_summaries: vec![ThinkingSummary::Off],
        supports_compaction: false,
    }]);
    let params = selected_params_for_role(&provider_models, &roles, "plain", &selected);
    assert_eq!(params.effort, Effort::Low);
}

/// Roles without an explicit verbosity default to low when the provider
/// supports the knob, keeping replies concise unless the user opts into more
/// detail. Providers without verbosity support publish a single fixed level.
#[test]
fn role_without_verbosity_picks_low_when_supported() {
    let openai: ModelId = "openai/gpt-5".parse().expect("model id");
    let local: ModelId = "local/llama".parse().expect("model id");
    let provider_models = provider_models([
        ProviderModelInfo {
            id: openai.clone(),
            display_name: None,
            context_window: 128_000,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Low, Verbosity::Medium, Verbosity::High],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
        },
        ProviderModelInfo {
            id: local.clone(),
            display_name: None,
            context_window: 8_192,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
        },
    ]);
    let roles = std::collections::HashMap::from([(
        "smart".to_owned(),
        tau_config::settings::AgentRole::default(),
    )]);

    assert_eq!(
        selected_params_for_role(&provider_models, &roles, "smart", &openai).verbosity,
        Verbosity::Low,
    );
    assert_eq!(
        selected_params_for_role(&provider_models, &roles, "smart", &local).verbosity,
        Verbosity::Medium,
    );
}

/// A malformed `harness.json5` must surface in the UI as an `Important`
/// `HarnessInfo`. Without this, the only symptom of a borked file is that
/// user-configured extensions or roles vanish.
#[test]
fn borked_harness_json5_emits_important_info() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.json5"),
        "{ extensions: { foo: { command: [ \"echo\" ",
    )
    .expect("write borked harness");

    let h = echo_harness_with_dirs("s1", state_dir, dirs).expect("harness");
    let message = find_important_info(&h, "harness.json5")
        .expect("expected Important HarnessInfo about harness.json5");
    assert!(
        message.contains("failed to parse"),
        "message should explain what happened, got: {message}"
    );
}

/// Provider snapshots are the only source for effort choices. The harness
/// should expose exactly what the provider published and report no choices for
/// unknown models rather than reviving config-derived defaults.
#[test]
fn efforts_for_model_uses_provider_snapshot_levels() {
    use tau_proto::Effort as L;

    let custom: ModelId = "openai/gpt-5.4-pro".parse().expect("model id");
    let local: ModelId = "local/llama".parse().expect("model id");
    let provider_models = provider_models([
        ProviderModelInfo {
            id: custom.clone(),
            display_name: None,
            context_window: 128_000,
            efforts: vec![L::Medium, L::High, L::XHigh],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
        },
        ProviderModelInfo {
            id: local.clone(),
            display_name: None,
            context_window: 8_192,
            efforts: vec![L::Off],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
        },
    ]);

    assert_eq!(
        efforts_for_model(&provider_models, &custom),
        vec![L::Medium, L::High, L::XHigh],
    );
    assert_eq!(efforts_for_model(&provider_models, &local), vec![L::Off],);
    assert!(
        efforts_for_model(
            &provider_models,
            &"openai/unknown-id".parse().expect("model id"),
        )
        .is_empty(),
        "unknown model yields no provider-published choices",
    );
}

/// `clamp_effort` must degrade `XHigh` to `High` (Pi-style) when the model
/// doesn't expose it, rather than silently dropping all the way to `Off`.
/// `Off` remains the fallback for other unsupported levels so users with a
/// no-reasoning provider don't get pinned to a level the model can't handle.
#[test]
fn clamp_effort_degrades_xhigh_to_high_when_unsupported() {
    use tau_proto::Effort as L;
    let without_xhigh = [L::Off, L::Minimal, L::Low, L::Medium, L::High];

    assert_eq!(clamp_effort(L::XHigh, &without_xhigh), L::High);
    // Sanity: when xhigh IS allowed, no demotion.
    let with_xhigh = [L::Off, L::Minimal, L::Low, L::Medium, L::High, L::XHigh];
    assert_eq!(clamp_effort(L::XHigh, &with_xhigh), L::XHigh);
    // Other unsupported requests still fall to Off.
    assert_eq!(clamp_effort(L::Minimal, &[L::Off]), L::Off);
    // No Off in the allowed set: degrade to the first entry.
    assert_eq!(clamp_effort(L::High, &[L::Medium, L::Low]), L::Medium);
}

/// Verbosity choices come from the provider snapshot. Providers that do not
/// support the knob publish a single fixed level, and unknown models expose no
/// levels.
#[test]
fn verbosities_for_model_uses_provider_snapshot_levels() {
    use tau_proto::Verbosity as V;

    let gpt: ModelId = "openai/gpt-5".parse().expect("model id");
    let locked: ModelId = "openai/gpt-5-locked".parse().expect("model id");
    let provider_models = provider_models([
        ProviderModelInfo {
            id: gpt.clone(),
            display_name: None,
            context_window: 128_000,
            efforts: vec![Effort::Off],
            verbosities: vec![V::Low, V::Medium, V::High],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
        },
        ProviderModelInfo {
            id: locked.clone(),
            display_name: None,
            context_window: 128_000,
            efforts: vec![Effort::Off],
            verbosities: vec![V::Medium],
            thinking_summaries: vec![ThinkingSummary::Off],
            supports_compaction: false,
        },
    ]);

    assert_eq!(
        verbosities_for_model(&provider_models, &gpt),
        vec![V::Low, V::Medium, V::High],
    );
    assert_eq!(
        verbosities_for_model(&provider_models, &locked),
        vec![V::Medium],
    );
    assert!(
        verbosities_for_model(
            &provider_models,
            &"local/missing".parse().expect("model id"),
        )
        .is_empty(),
    );
}

/// Thinking-summary choices come from the provider snapshot, so the harness no
/// longer consults provider compatibility flags in config.
#[test]
fn thinking_summaries_for_model_uses_provider_snapshot_levels() {
    use tau_proto::ThinkingSummary as T;

    let gpt: ModelId = "openai/gpt-5".parse().expect("model id");
    let local: ModelId = "local/llama".parse().expect("model id");
    let provider_models = provider_models([
        ProviderModelInfo {
            id: gpt.clone(),
            display_name: None,
            context_window: 128_000,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![T::Off, T::Auto, T::Concise, T::Detailed],
            supports_compaction: false,
        },
        ProviderModelInfo {
            id: local.clone(),
            display_name: None,
            context_window: 8_192,
            efforts: vec![Effort::Off],
            verbosities: vec![Verbosity::Medium],
            thinking_summaries: vec![T::Off],
            supports_compaction: false,
        },
    ]);

    assert_eq!(
        thinking_summaries_for_model(&provider_models, &gpt),
        vec![T::Off, T::Auto, T::Concise, T::Detailed],
    );
    assert_eq!(
        thinking_summaries_for_model(&provider_models, &local),
        vec![T::Off],
    );
}

/// Persisted role overrides are restored as the runtime role definition, then
/// clamped against provider-owned metadata for the resolved model.
#[test]
fn selected_params_restore_each_field_from_role_override() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.json5"),
        r#"{
            roles: {
                smart: { model: "openai/gpt-5" },
            },
        }"#,
    )
    .expect("write harness config");
    std::fs::write(
        state_dir.join("harness.json5"),
        r#"{
            "role_overrides": {
                "smart": {
                    "model": "openai/gpt-5",
                    "effort": "high",
                    "verbosity": "low",
                    "thinkingSummary": "concise"
                }
            }
        }"#,
    )
    .expect("write state");

    let harness_settings =
        tau_config::settings::load_harness_settings_in(&dirs).expect("load harness settings");
    let (roles, _role_overrides, selected_role) = load_roles(&dirs, &harness_settings);
    assert_eq!(selected_role, "smart");

    let model: ModelId = "openai/gpt-5".parse().expect("model id");
    let provider_models = provider_models([ProviderModelInfo {
        id: model.clone(),
        display_name: None,
        context_window: 128_000,
        efforts: vec![Effort::Off, Effort::Low, Effort::High],
        verbosities: vec![Verbosity::Low, Verbosity::High],
        thinking_summaries: vec![
            ThinkingSummary::Off,
            ThinkingSummary::Auto,
            ThinkingSummary::Concise,
        ],
        supports_compaction: false,
    }]);

    let params = selected_params_for_role(&provider_models, &roles, &selected_role, &model);
    assert_eq!(params.effort, Effort::High);
    assert_eq!(params.verbosity, Verbosity::Low);
    assert_eq!(params.thinking_summary, ThinkingSummary::Concise);
}
