//! Model-registry helpers: loading the available model list, computing
//! valid effort levels per model, persisting the user's selection, and
//! gauging context-window usage.

use tau_proto::ModelId;

use crate::settings::load_harness_settings_or_warn;

/// Load model registry and harness settings, build the flat model list
/// and determine the initially selected model.
///
/// Priority: default_model from harness.json5 → last used from state →
/// first available → empty (no model).
pub(crate) fn load_model_list(
    dirs: &tau_config::settings::TauDirs,
) -> (
    Vec<ModelId>,
    ModelId,
    tau_config::settings::ModelRegistry,
    tau_config::settings::HarnessSettings,
) {
    let model_registry = tau_config::settings::load_models_in(dirs).unwrap_or_default();
    let harness_settings = load_harness_settings_or_warn(dirs);
    let mut available: Vec<ModelId> = Vec::new();
    for (provider_name, provider_cfg) in &model_registry.providers {
        for model in &provider_cfg.models {
            available.push(format!("{provider_name}/{}", model.id).into());
        }
    }
    available.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let selected = harness_settings
        .default_model
        .as_ref()
        .filter(|m| available.iter().any(|a| a.as_str() == m.as_str()))
        .map(|m| ModelId::from(m.clone()))
        .or_else(|| {
            load_last_selected_model(dirs)
                .filter(|m| available.iter().any(|a| a.as_str() == m.as_str()))
                .map(ModelId::from)
        })
        .or_else(|| available.first().cloned())
        .unwrap_or_default();
    (available, selected, model_registry, harness_settings)
}

/// Returns the efforts valid for `model` (a `provider/model_id`
/// string). Empty list means no effort applies — no model selected, or
/// the provider doesn't support reasoning. Otherwise returns the
/// canonical [Off, Minimal, Low, Medium, High] set; xhigh is gated on
/// future per-model config (Pi only enables it for codex-max).
pub(crate) fn efforts_for_model(
    registry: &tau_config::settings::ModelRegistry,
    model: &str,
) -> Vec<tau_proto::Effort> {
    use tau_proto::Effort as L;
    if model.is_empty() {
        return Vec::new();
    }
    let Some((provider_name, _)) = model.split_once('/') else {
        return Vec::new();
    };
    let Some(provider) = registry.providers.get(provider_name) else {
        return Vec::new();
    };
    if !provider.compat.supports_reasoning_effort {
        return vec![L::Off];
    }
    vec![L::Off, L::Minimal, L::Low, L::Medium, L::High]
}

pub(crate) fn model_context_window(
    registry: &tau_config::settings::ModelRegistry,
    model: &str,
) -> Option<u64> {
    let (provider_name, model_id) = model.split_once('/')?;
    let provider = registry.providers.get(provider_name)?;
    provider
        .models
        .iter()
        .find(|candidate| candidate.id == model_id)
        .and_then(|candidate| candidate.context_window)
}

pub(crate) fn context_percent_used(input_tokens: u64, context_window: u64) -> u8 {
    if context_window == 0 {
        return 0;
    }
    let percent = input_tokens.saturating_mul(100) / context_window;
    percent.min(100) as u8
}

pub(crate) fn clamp_effort(
    requested: tau_proto::Effort,
    allowed: &[tau_proto::Effort],
) -> tau_proto::Effort {
    if allowed.iter().any(|level| *level == requested) {
        return requested;
    }
    if allowed.iter().any(|level| *level == tau_proto::Effort::Off) {
        return tau_proto::Effort::Off;
    }
    allowed.first().copied().unwrap_or(tau_proto::Effort::Off)
}

fn parse_effort(value: &str) -> Option<tau_proto::Effort> {
    value.parse().ok()
}

fn load_last_efforts(
    dirs: &tau_config::settings::TauDirs,
) -> std::collections::HashMap<String, tau_proto::Effort> {
    let Some(path) = dirs.state_dir.as_ref().map(|d| d.join("harness.json5")) else {
        return std::collections::HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return std::collections::HashMap::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return std::collections::HashMap::new();
    };

    let mut levels = std::collections::HashMap::new();
    if let Some(map) = json["last_efforts"].as_object() {
        for (model, level) in map {
            let Some(level) = level.as_str().and_then(parse_effort) else {
                continue;
            };
            levels.insert(model.clone(), level);
        }
    }

    levels
}

pub(crate) fn selected_effort_for_model(
    dirs: &tau_config::settings::TauDirs,
    harness_settings: &tau_config::settings::HarnessSettings,
    registry: &tau_config::settings::ModelRegistry,
    model: &str,
) -> tau_proto::Effort {
    let allowed = efforts_for_model(registry, model);
    let requested = harness_settings
        .default_efforts
        .get(model)
        .copied()
        .or_else(|| load_last_efforts(dirs).remove(model))
        .unwrap_or_else(|| middle_effort(&allowed));
    clamp_effort(requested, &allowed)
}

/// Pick the middle element of `allowed`, or `Off` for an empty list.
/// First-time users (no `default_efforts` entry, no persisted last
/// effort) get a sensible reasoning level instead of always landing on
/// `Off` — for the standard `[Off, Minimal, Low, Medium, High]` list
/// that's `Low`. Returns `Off` for `[Off]`-only providers and the
/// empty case.
pub(crate) fn middle_effort(allowed: &[tau_proto::Effort]) -> tau_proto::Effort {
    if allowed.is_empty() {
        return tau_proto::Effort::Off;
    }
    allowed[allowed.len() / 2]
}

/// Load the last-selected model from `<state_dir>/harness.json5`.
fn load_last_selected_model(dirs: &tau_config::settings::TauDirs) -> Option<String> {
    let path = dirs.state_dir.as_ref()?.join("harness.json5");
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json["last_selected_model"].as_str().map(String::from)
}

/// Persist model + effort to `<state_dir>/harness.json5`.
pub(crate) fn save_harness_state(
    dirs: &tau_config::settings::TauDirs,
    model: &str,
    effort: tau_proto::Effort,
) {
    let Some(dir) = dirs.state_dir.as_ref() else {
        return;
    };
    let path = dir.join("harness.json5");
    let _ = std::fs::create_dir_all(dir);
    let mut last_efforts = load_last_efforts(dirs);
    if !model.is_empty() {
        last_efforts.insert(model.to_owned(), effort);
    }
    let effort_json = last_efforts
        .into_iter()
        .map(|(model, level)| (model, serde_json::Value::String(level.as_str().to_owned())))
        .collect::<serde_json::Map<String, serde_json::Value>>();
    let json = serde_json::json!({
        "last_selected_model": model,
        "last_efforts": effort_json,
    });
    let _ = serde_json::to_string_pretty(&json)
        .ok()
        .and_then(|s| std::fs::write(&path, s).ok());
}
