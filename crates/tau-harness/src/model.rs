//! Provider-model helpers: loading harness-owned roles, computing valid
//! effort/verbosity/thinking-summary levels from provider metadata, persisting
//! the user's selection, and gauging context-window usage.

use std::collections::HashMap;

use tau_config::settings::{AgentRole, HarnessSettings};
use tau_proto::{ModelId, ModelParams, ProviderModelInfo};

const BASE_AGENT_ROLE: &str = "smart";

/// Load configured roles, persisted role overrides, and the selected role.
/// Runtime model availability is provider-owned and is therefore not loaded
/// from config here.
pub(crate) fn load_roles(
    dirs: &tau_config::settings::TauDirs,
    harness_settings: &HarnessSettings,
) -> (
    HashMap<String, AgentRole>,
    HashMap<String, AgentRole>,
    String,
) {
    let mut role_overrides = load_role_overrides(dirs);
    let mut roles = harness_settings.roles.clone();
    role_overrides.retain(|name, _| roles.contains_key(name));
    for (name, role) in &role_overrides {
        let mut effective_role = role.clone();
        if let Some(configured_role) = roles.get(name) {
            effective_role.description = configured_role.description.clone();
            effective_role.prompt = configured_role.prompt.clone();
            effective_role.orchestrator = configured_role.orchestrator;
            effective_role.extra_prompt = configured_role.extra_prompt.clone();
        }
        roles.insert(name.clone(), effective_role);
    }
    let selected_role = load_last_selected_role(dirs)
        .filter(|role| roles.contains_key(role))
        .unwrap_or_else(|| fallback_role(&roles));
    (roles, role_overrides, selected_role)
}

/// Return the role Tau should select when persisted state does not name a
/// usable role. Built-ins make `smart` available in normal operation; the final
/// fallback keeps tests and malformed intermediate states deterministic.
pub(crate) fn fallback_role(roles: &HashMap<String, AgentRole>) -> String {
    roles
        .contains_key(BASE_AGENT_ROLE)
        .then(|| BASE_AGENT_ROLE.to_owned())
        .or_else(|| roles.keys().min().cloned())
        .unwrap_or_else(|| BASE_AGENT_ROLE.to_owned())
}

/// Resolve the model for `role` from the currently provider-published model
/// list. Roles without an explicit model use the first available model.
pub(crate) fn model_for_role(
    roles: &HashMap<String, AgentRole>,
    role: &str,
    available: &[ModelId],
) -> Option<ModelId> {
    let model = roles
        .get(role)
        .and_then(|r| r.model.clone())
        .or_else(|| available.first().cloned())?;
    available.contains(&model).then_some(model)
}

/// Resolve the current model from the selected role and provider-published
/// runtime model list.
pub(crate) fn select_model_for_available(
    roles: &HashMap<String, AgentRole>,
    selected_role: &str,
    available: &[ModelId],
) -> Option<ModelId> {
    model_for_role(roles, selected_role, available)
}

/// Resolve selected prompt parameters from a role and provider metadata.
pub(crate) fn selected_params_for_role(
    provider_models: &HashMap<ModelId, ProviderModelInfo>,
    roles: &HashMap<String, AgentRole>,
    role: &str,
    model: &ModelId,
) -> ModelParams {
    let allowed_effort = efforts_for_model(provider_models, model);
    let allowed_verbosity = verbosities_for_model(provider_models, model);
    let allowed_thinking = thinking_summaries_for_model(provider_models, model);
    selected_params_for_role_with_allowed(
        roles,
        role,
        &allowed_effort,
        &allowed_verbosity,
        &allowed_thinking,
    )
}

fn selected_params_for_role_with_allowed(
    roles: &HashMap<String, AgentRole>,
    role: &str,
    allowed_effort: &[tau_proto::Effort],
    allowed_verbosity: &[tau_proto::Verbosity],
    allowed_thinking: &[tau_proto::ThinkingSummary],
) -> ModelParams {
    let current = roles.get(role);
    let effort = current
        .and_then(|r| r.effort)
        .unwrap_or_else(|| middle_effort(allowed_effort));
    let verbosity = current
        .and_then(|r| r.verbosity)
        .unwrap_or_else(|| default_verbosity(allowed_verbosity));
    let thinking_summary = current
        .and_then(|r| r.thinking_summary)
        .unwrap_or_else(|| default_thinking_summary(allowed_thinking));
    let service_tier = current.and_then(|r| r.service_tier);

    ModelParams {
        effort: clamp_effort(effort, allowed_effort),
        verbosity: clamp_verbosity(verbosity, allowed_verbosity),
        thinking_summary: clamp_thinking_summary(thinking_summary, allowed_thinking),
        service_tier,
    }
}

fn describe_role_inner(
    provider_models: &HashMap<ModelId, ProviderModelInfo>,
    roles: &HashMap<String, AgentRole>,
    tools_profiles: &tau_config::settings::ToolsProfiles,
    role: &str,
    available: &[ModelId],
) -> String {
    let Some(model) = model_for_role(roles, role, available) else {
        return "no model".to_owned();
    };
    let params = selected_params_for_role(provider_models, roles, role, &model);
    let current = roles.get(role);
    let service_tier = params
        .service_tier
        .map(|tier| format!(", service-tier={}", tier.as_str()))
        .unwrap_or_default();
    let tools_profile = current
        .and_then(|r| r.tools_profile.as_deref())
        .map(|name| {
            if tools_profiles.contains_key(name) {
                format!(", tools-profile={name}")
            } else {
                format!(", tools-profile={name} (missing)")
            }
        })
        .unwrap_or_default();
    format!(
        "model={}, effort={}, verbosity={}, thinking-summary={}{}{}",
        model,
        params.effort,
        params.verbosity,
        params.thinking_summary,
        service_tier,
        tools_profile
    )
}

/// Build UI role descriptions from harness roles and provider model metadata.
pub(crate) fn role_infos(
    provider_models: &HashMap<ModelId, ProviderModelInfo>,
    roles: &HashMap<String, AgentRole>,
    tools_profiles: &tau_config::settings::ToolsProfiles,
    available: &[ModelId],
) -> Vec<tau_proto::HarnessRoleInfo> {
    let mut out: Vec<_> = roles
        .keys()
        .map(|name| tau_proto::HarnessRoleInfo {
            name: name.clone(),
            description: describe_role_inner(
                provider_models,
                roles,
                tools_profiles,
                name,
                available,
            ),
            role_description: roles.get(name).and_then(|role| role.description.clone()),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Returns the effort levels published for `model` by its provider.
pub(crate) fn efforts_for_model(
    provider_models: &HashMap<ModelId, ProviderModelInfo>,
    model: &ModelId,
) -> Vec<tau_proto::Effort> {
    provider_models
        .get(model)
        .map(|info| info.efforts.clone())
        .unwrap_or_default()
}

/// Returns the verbosity levels published for `model` by its provider.
pub(crate) fn verbosities_for_model(
    provider_models: &HashMap<ModelId, ProviderModelInfo>,
    model: &ModelId,
) -> Vec<tau_proto::Verbosity> {
    provider_models
        .get(model)
        .map(|info| info.verbosities.clone())
        .unwrap_or_default()
}

/// Returns the thinking-summary modes published for `model` by its provider.
pub(crate) fn thinking_summaries_for_model(
    provider_models: &HashMap<ModelId, ProviderModelInfo>,
    model: &ModelId,
) -> Vec<tau_proto::ThinkingSummary> {
    provider_models
        .get(model)
        .map(|info| info.thinking_summaries.clone())
        .unwrap_or_default()
}

/// Returns the context window published for `model` by its provider.
pub(crate) fn context_window_for_model(
    provider_models: &HashMap<ModelId, ProviderModelInfo>,
    model: &ModelId,
) -> Option<u64> {
    provider_models.get(model).map(|info| info.context_window)
}

/// Convert used input tokens into a clamped percentage of the context window.
pub(crate) fn context_percent_used(input_tokens: u64, context_window: u64) -> u8 {
    if context_window == 0 {
        return 0;
    }
    let percent = input_tokens.saturating_mul(100) / context_window;
    percent.min(100) as u8
}

/// Clamp a requested effort against the levels supported by the selected model.
pub(crate) fn clamp_effort(
    requested: tau_proto::Effort,
    allowed: &[tau_proto::Effort],
) -> tau_proto::Effort {
    use tau_proto::Effort as L;
    if allowed.contains(&requested) {
        return requested;
    }
    if requested == L::XHigh && allowed.contains(&L::High) {
        return L::High;
    }
    if allowed.contains(&L::Off) {
        return L::Off;
    }
    allowed.first().copied().unwrap_or(L::Off)
}

/// Clamp a requested verbosity against the levels supported by the selected
/// model.
pub(crate) fn clamp_verbosity(
    requested: tau_proto::Verbosity,
    allowed: &[tau_proto::Verbosity],
) -> tau_proto::Verbosity {
    use tau_proto::Verbosity as V;
    if allowed.contains(&requested) {
        return requested;
    }
    if allowed.contains(&V::Medium) {
        return V::Medium;
    }
    allowed.first().copied().unwrap_or(V::Medium)
}

/// Clamp a requested thinking-summary mode against the selected model support.
pub(crate) fn clamp_thinking_summary(
    requested: tau_proto::ThinkingSummary,
    allowed: &[tau_proto::ThinkingSummary],
) -> tau_proto::ThinkingSummary {
    use tau_proto::ThinkingSummary as T;
    if allowed.contains(&requested) {
        return requested;
    }
    if allowed.contains(&T::Off) {
        return T::Off;
    }
    allowed.first().copied().unwrap_or(T::Off)
}

fn load_state_json(
    dirs: &tau_config::settings::TauDirs,
) -> serde_json::Map<String, serde_json::Value> {
    let Some(path) = dirs.state_dir.as_ref().map(|d| d.join("harness.json5")) else {
        return serde_json::Map::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return serde_json::Map::new();
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// Resolve provider fallback parameters for `model`, ignoring persisted state.
pub(crate) fn baseline_params_for_model(
    provider_models: &HashMap<ModelId, ProviderModelInfo>,
    model: &ModelId,
) -> ModelParams {
    let allowed_effort = efforts_for_model(provider_models, model);
    let allowed_verbosity = verbosities_for_model(provider_models, model);
    let allowed_thinking = thinking_summaries_for_model(provider_models, model);
    baseline_params_for_model_with_allowed(&allowed_effort, &allowed_verbosity, &allowed_thinking)
}

fn baseline_params_for_model_with_allowed(
    allowed_effort: &[tau_proto::Effort],
    allowed_verbosity: &[tau_proto::Verbosity],
    allowed_thinking: &[tau_proto::ThinkingSummary],
) -> ModelParams {
    ModelParams {
        effort: middle_effort(allowed_effort),
        verbosity: default_verbosity(allowed_verbosity),
        thinking_summary: default_thinking_summary(allowed_thinking),
        service_tier: None,
    }
}

/// Resolve baseline parameters for a selected role/model pair, ignoring
/// persisted role overrides.
pub(crate) fn baseline_params_for_selection(
    harness_settings: &HarnessSettings,
    provider_models: &HashMap<ModelId, ProviderModelInfo>,
    role: &str,
    model: &ModelId,
) -> ModelParams {
    if harness_settings.roles.contains_key(role) {
        return selected_params_for_role(provider_models, &harness_settings.roles, role, model);
    }

    baseline_params_for_model(provider_models, model)
}

/// Pick the middle element of `allowed`, or `Off` for an empty list.
pub(crate) fn middle_effort(allowed: &[tau_proto::Effort]) -> tau_proto::Effort {
    if allowed.is_empty() {
        return tau_proto::Effort::Off;
    }
    allowed[allowed.len() / 2]
}

/// Default verbosity when no persisted preference exists.
pub(crate) fn default_verbosity(allowed: &[tau_proto::Verbosity]) -> tau_proto::Verbosity {
    use tau_proto::Verbosity as V;
    if allowed.contains(&V::Low) {
        return V::Low;
    }
    allowed.first().copied().unwrap_or(V::Low)
}

/// Default thinking-summary mode when no persisted preference exists.
pub(crate) fn default_thinking_summary(
    allowed: &[tau_proto::ThinkingSummary],
) -> tau_proto::ThinkingSummary {
    use tau_proto::ThinkingSummary as T;
    if allowed.contains(&T::Auto) {
        return T::Auto;
    }
    if allowed.contains(&T::Off) {
        return T::Off;
    }
    allowed.first().copied().unwrap_or(T::Off)
}

fn load_last_selected_role(dirs: &tau_config::settings::TauDirs) -> Option<String> {
    let json = load_state_json(dirs);
    let role = json.get("last_selected_role")?.as_str()?.to_owned();
    (!role.is_empty()).then_some(role)
}

fn role_without_config_metadata(mut role: AgentRole) -> AgentRole {
    role.description = None;
    role.prompt = None;
    role.orchestrator = None;
    role.extra_prompt = None;
    role
}

fn load_role_overrides(dirs: &tau_config::settings::TauDirs) -> HashMap<String, AgentRole> {
    let json = load_state_json(dirs);
    let mut out = HashMap::new();
    if let Some(map) = json
        .get("role_overrides")
        .and_then(serde_json::Value::as_object)
    {
        for (name, entry) in map {
            if let Ok(role) = serde_json::from_value::<AgentRole>(entry.clone()) {
                out.insert(name.clone(), role_without_config_metadata(role));
            }
        }
    }
    out
}

/// Persist role overrides and the currently selected role.
pub(crate) fn save_role_overrides(
    dirs: &tau_config::settings::TauDirs,
    selected_role: &str,
    roles: &HashMap<String, AgentRole>,
) {
    let Some(dir) = dirs.state_dir.as_ref() else {
        return;
    };
    let path = dir.join("harness.json5");
    let _ = std::fs::create_dir_all(dir);
    let mut json = serde_json::Map::new();
    json.insert(
        "last_selected_role".to_owned(),
        serde_json::Value::String(selected_role.to_owned()),
    );
    let overrides = roles
        .iter()
        .map(|(name, role)| {
            (
                name.clone(),
                serde_json::to_value(role_without_config_metadata(role.clone()))
                    .unwrap_or(serde_json::Value::Null),
            )
        })
        .collect::<serde_json::Map<String, serde_json::Value>>();
    json.insert(
        "role_overrides".to_owned(),
        serde_json::Value::Object(overrides),
    );
    let _ = serde_json::to_string_pretty(&serde_json::Value::Object(json))
        .ok()
        .and_then(|s| std::fs::write(&path, s).ok());
}
