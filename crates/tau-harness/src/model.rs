//! Provider-model helpers: loading harness-owned roles, computing valid
//! effort/verbosity/thinking-summary levels from provider metadata, and gauging
//! context-window usage.

use std::collections::{HashMap, HashSet};

use tau_config::settings::{AgentRole, HarnessSettings};
use tau_proto::{ModelId, ModelParams, ProviderModelInfo};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MissingDefaultRole {
    /// Configured startup role name that does not exist in the effective roles.
    pub requested: String,
    /// Role selected instead so startup can continue.
    pub fallback: String,
}

const BASE_AGENT_ROLE: &str = "senior-engineer";

pub(crate) struct LoadedRoles {
    /// Effective roles loaded from config.
    pub roles: HashMap<String, AgentRole>,
    /// Runtime role overrides for this process. Startup begins with none.
    pub role_overrides: HashMap<String, AgentRole>,
    /// Role selected for startup.
    pub selected_role: String,
    /// Effective role groups used for UI navigation.
    pub role_groups: Vec<tau_proto::HarnessRoleGroup>,
    /// Missing configured default role warning to surface after startup.
    pub missing_default_role: Option<MissingDefaultRole>,
}

/// Load configured roles and the startup role.
/// Runtime model availability is provider-owned and is therefore not loaded
/// from config here.
pub(crate) fn load_roles(harness_settings: &HarnessSettings) -> LoadedRoles {
    let roles = harness_settings.roles.clone();
    let role_overrides = HashMap::new();
    let role_groups = role_groups_for_roles(&roles, &harness_settings.role_groups);
    let (selected_role, missing_default_role) =
        select_startup_role(harness_settings, &roles, &role_groups);
    LoadedRoles {
        roles,
        role_overrides,
        selected_role,
        role_groups,
        missing_default_role,
    }
}

fn select_startup_role(
    harness_settings: &HarnessSettings,
    roles: &HashMap<String, AgentRole>,
    role_groups: &[tau_proto::HarnessRoleGroup],
) -> (String, Option<MissingDefaultRole>) {
    let fallback = first_grouped_role(role_groups).unwrap_or_else(|| fallback_role(roles));
    let Some(default_role) = harness_settings.default_role.as_ref() else {
        return (fallback, None);
    };
    if roles.contains_key(default_role) {
        return (default_role.clone(), None);
    }
    (
        fallback.clone(),
        Some(MissingDefaultRole {
            requested: default_role.clone(),
            fallback,
        }),
    )
}

fn first_grouped_role(role_groups: &[tau_proto::HarnessRoleGroup]) -> Option<String> {
    role_groups
        .iter()
        .find_map(|group| group.roles.first().cloned())
}

/// Build the effective navigation groups for the available role set. Configured
/// groups keep their configured order; roles not named by any group remain
/// reachable as single-role groups after configured groups.
pub(crate) fn role_groups_for_roles(
    roles: &HashMap<String, AgentRole>,
    configured_groups: &[tau_config::settings::RoleGroup],
) -> Vec<tau_proto::HarnessRoleGroup> {
    let mut grouped = HashSet::new();
    let mut out = Vec::new();
    for group in configured_groups {
        let group_roles: Vec<_> = group
            .roles
            .iter()
            .filter(|role| roles.contains_key(*role))
            .inspect(|role| {
                grouped.insert((*role).clone());
            })
            .cloned()
            .collect();
        if !group_roles.is_empty() {
            out.push(tau_proto::HarnessRoleGroup {
                name: group.name.clone(),
                roles: group_roles,
            });
        }
    }

    let mut ungrouped: Vec<_> = roles
        .keys()
        .filter(|role| !grouped.contains(*role))
        .cloned()
        .collect();
    ungrouped.sort();
    out.extend(
        ungrouped
            .into_iter()
            .map(|role| tau_proto::HarnessRoleGroup {
                name: role.clone(),
                roles: vec![role],
            }),
    );
    out
}

/// Return the role Tau should select if no configured default role is usable.
/// Built-ins make `engineer` available in normal operation; the final fallback
/// keeps tests and malformed intermediate states deterministic.
pub(crate) fn fallback_role(roles: &HashMap<String, AgentRole>) -> String {
    roles
        .contains_key(BASE_AGENT_ROLE)
        .then(|| BASE_AGENT_ROLE.to_owned())
        .or_else(|| roles.keys().min().cloned())
        .unwrap_or_else(|| BASE_AGENT_ROLE.to_owned())
}

/// Resolve the model for `role` from provider-published model metadata. Roles
/// without an explicit model use the model with the highest provider-published
/// default affinity.
pub(crate) fn model_for_role(
    provider_models: &HashMap<ModelId, ProviderModelInfo>,
    roles: &HashMap<String, AgentRole>,
    role: &str,
) -> Option<ModelId> {
    let model = roles
        .get(role)
        .and_then(|r| r.model.clone())
        .or_else(|| default_model(provider_models))?;
    provider_models.contains_key(&model).then_some(model)
}

fn default_model(provider_models: &HashMap<ModelId, ProviderModelInfo>) -> Option<ModelId> {
    provider_models
        .iter()
        .max_by(|(a_id, a_info), (b_id, b_info)| {
            a_info
                .default_affinity
                .cmp(&b_info.default_affinity)
                .then_with(|| b_id.cmp(a_id))
        })
        .map(|(id, _)| id.clone())
}

/// Resolve the current model from the selected role and provider-published
/// runtime model metadata.
pub(crate) fn select_model_for_role(
    provider_models: &HashMap<ModelId, ProviderModelInfo>,
    roles: &HashMap<String, AgentRole>,
    selected_role: &str,
) -> Option<ModelId> {
    model_for_role(provider_models, roles, selected_role)
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
    role: &str,
    _available: &[ModelId],
) -> String {
    let Some(model) = model_for_role(provider_models, roles, role) else {
        return "no model".to_owned();
    };
    let params = selected_params_for_role(provider_models, roles, role, &model);
    let current = roles.get(role);
    let service_tier = params
        .service_tier
        .map(|tier| format!(", service-tier={}", tier.as_str()))
        .unwrap_or_default();
    let tools = current
        .and_then(|r| r.tools.as_ref())
        .map(|tools| {
            format!(
                ", tools={}",
                tools
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("|")
            )
        })
        .unwrap_or_default();
    let enable_tools = current
        .filter(|r| !r.enable_tools.is_empty())
        .map(|r| {
            format!(
                ", enable-tools={}",
                r.enable_tools
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("|")
            )
        })
        .unwrap_or_default();
    let disable_tools = current
        .filter(|r| !r.disable_tools.is_empty())
        .map(|r| {
            format!(
                ", disable-tools={}",
                r.disable_tools
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("|")
            )
        })
        .unwrap_or_default();
    format!(
        "model={}, effort={}, verbosity={}, thinking-summary={}{}{}{}{}",
        model,
        params.effort,
        params.verbosity,
        params.thinking_summary,
        service_tier,
        tools,
        enable_tools,
        disable_tools
    )
}

/// Build UI role descriptions from harness roles and provider model metadata.
pub(crate) fn role_infos(
    provider_models: &HashMap<ModelId, ProviderModelInfo>,
    roles: &HashMap<String, AgentRole>,
    available: &[ModelId],
) -> Vec<tau_proto::HarnessRoleInfo> {
    let mut out: Vec<_> = roles
        .keys()
        .map(|name| tau_proto::HarnessRoleInfo {
            name: name.clone(),
            description: describe_role_inner(provider_models, roles, name, available),
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

/// Resolve provider fallback parameters for `model`.
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

/// Resolve baseline parameters for a selected role/model pair from config.
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

/// Default verbosity when no role preference exists.
pub(crate) fn default_verbosity(allowed: &[tau_proto::Verbosity]) -> tau_proto::Verbosity {
    use tau_proto::Verbosity as V;
    if allowed.contains(&V::Low) {
        return V::Low;
    }
    allowed.first().copied().unwrap_or(V::Low)
}

/// Default thinking-summary mode when no role preference exists.
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
