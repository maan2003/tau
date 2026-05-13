use tau_config::settings::{self, PromptCacheRetention, ProviderConfig};

use super::*;

#[test]
fn public_openai_api_enables_prompt_cache_support() {
    let provider = ProviderConfig::default();

    assert!(supports_prompt_cache_key(
        &provider,
        "https://api.openai.com/v1"
    ));
    assert!(supports_prompt_cache_retention(
        &provider,
        "https://api.openai.com/v1/"
    ));
}

#[test]
fn codex_backend_enables_prompt_cache_key_but_not_retention() {
    let provider = ProviderConfig::default();

    assert!(supports_prompt_cache_key(
        &provider,
        "https://chatgpt.com/backend-api"
    ));
    // chatgpt.com/backend-api 400s on `prompt_cache_retention` —
    // only the public REST API accepts it.
    assert!(!supports_prompt_cache_retention(
        &provider,
        "https://chatgpt.com/backend-api/"
    ));
}

#[test]
fn provider_flags_enable_prompt_cache_support_for_non_openai_backends() {
    let provider = ProviderConfig {
        compat: settings::ProviderCompat {
            supports_prompt_cache_key: true,
            supports_prompt_cache_retention: true,
            ..settings::ProviderCompat::default()
        },
        ..ProviderConfig::default()
    };

    assert!(supports_prompt_cache_key(
        &provider,
        "https://example.com/v1"
    ));
    assert!(supports_prompt_cache_retention(
        &provider,
        "https://example.com/v1"
    ));
}

#[test]
fn public_openai_api_defaults_retention_to_24h_on_supported_models() {
    let provider = ProviderConfig::default();

    assert_eq!(
        prompt_cache_retention(&provider, "https://api.openai.com/v1", "gpt-5.5"),
        Some(PromptCacheRetention::Extended24h)
    );
    assert_eq!(
        prompt_cache_retention(&provider, "https://api.openai.com/v1/", "gpt-5.5-pro"),
        Some(PromptCacheRetention::Extended24h)
    );
}

#[test]
fn codex_backend_skips_retention_default_even_on_supported_models() {
    let provider = ProviderConfig::default();

    // Regression: defaulting `prompt_cache_retention` to 24h on the
    // Codex Responses backend caused HTTP 400 — the routing there
    // doesn't accept the param, even on gpt-5.5+.
    assert_eq!(
        prompt_cache_retention(&provider, "https://chatgpt.com/backend-api", "gpt-5.5"),
        None
    );
    assert_eq!(
        prompt_cache_retention(&provider, "https://chatgpt.com/backend-api/", "gpt-5.5-pro"),
        None
    );
}

#[test]
fn builtin_openai_skips_retention_default_on_older_models() {
    let provider = ProviderConfig::default();

    assert_eq!(
        prompt_cache_retention(&provider, "https://api.openai.com/v1", "gpt-5.4"),
        None
    );
    assert_eq!(
        prompt_cache_retention(&provider, "https://api.openai.com/v1", "gpt-4o"),
        None
    );
}

#[test]
fn explicit_provider_retention_wins_over_model_default() {
    let provider = ProviderConfig {
        prompt_cache_retention: Some(PromptCacheRetention::InMemory),
        ..ProviderConfig::default()
    };

    assert_eq!(
        prompt_cache_retention(&provider, "https://api.openai.com/v1", "gpt-5.5"),
        Some(PromptCacheRetention::InMemory)
    );
}

#[test]
fn non_builtin_backend_skips_retention_default() {
    let provider = ProviderConfig {
        compat: settings::ProviderCompat {
            supports_prompt_cache_retention: true,
            ..settings::ProviderCompat::default()
        },
        ..ProviderConfig::default()
    };

    assert_eq!(
        prompt_cache_retention(&provider, "https://example.com/v1", "gpt-5.5"),
        None
    );
}

/// The ChatGPT Codex Responses endpoint auto-enables `supports_phase`
/// for models known to emit the field (`gpt-5.3-codex` and later) so
/// users on the built-in OAuth flow get the field plumbed through
/// without having to touch their settings. Older Codex models stay
/// off the feature — older variants reject unknown fields, and the
/// docs only call out 5.3+.
#[test]
fn codex_backend_auto_enables_phase_for_supported_models() {
    let provider = ProviderConfig::default();

    assert!(supports_phase(
        &provider,
        "https://chatgpt.com/backend-api",
        "gpt-5.3-codex"
    ));
    assert!(supports_phase(
        &provider,
        "https://chatgpt.com/backend-api/",
        "gpt-5.3-codex-2026-01-15"
    ));
    assert!(supports_phase(
        &provider,
        "https://chatgpt.com/backend-api",
        "gpt-5.4-codex"
    ));
    assert!(
        !supports_phase(&provider, "https://chatgpt.com/backend-api", "gpt-5-codex"),
        "the pre-5.3 codex line predates the field"
    );
    assert!(
        !supports_phase(
            &provider,
            "https://chatgpt.com/backend-api",
            "gpt-5.2-codex"
        ),
        "5.2-codex is below the doc-cited 5.3 floor"
    );
    assert!(
        !supports_phase(&provider, "https://chatgpt.com/backend-api", "gpt-5.5"),
        "non-codex models don't get the auto-enable"
    );
}

/// Explicit `supports_phase` on a provider's compat block overrides
/// the model-id heuristic. This is the escape hatch for self-hosted
/// or proxy backends that mimic the Codex shape but don't appear in
/// our built-in whitelist.
#[test]
fn explicit_provider_phase_flag_wins() {
    let provider = ProviderConfig {
        compat: settings::ProviderCompat {
            supports_phase: true,
            ..settings::ProviderCompat::default()
        },
        ..ProviderConfig::default()
    };

    assert!(supports_phase(
        &provider,
        "https://example.com/v1",
        "some-custom-model"
    ));
}

/// The public OpenAI REST API (`api.openai.com/v1`) is NOT in the
/// auto-enable list: Tau doesn't route Responses through it today,
/// and `phase` is a Codex-surface concept per the deployment
/// checklist. The flag must stay off there absent an explicit
/// opt-in, just like `prompt_cache_retention` is gated separately.
#[test]
fn public_openai_api_does_not_auto_enable_phase() {
    let provider = ProviderConfig::default();
    assert!(!supports_phase(
        &provider,
        "https://api.openai.com/v1",
        "gpt-5.3-codex"
    ));
}

// -----------------------------------------------------------------------
// supports_encrypted_reasoning
// -----------------------------------------------------------------------
//
// Unlike `supports_phase`, there's no model-id gate here. `include:
// ["reasoning.encrypted_content"]` is a request-side opt-in: the
// server either fills in `encrypted_content` on each reasoning output
// item or it doesn't, and the agent only captures items that actually
// carry the blob. So enabling it broadly costs nothing for models
// that don't emit reasoning and rescues every reasoning model
// (including new snapshots not yet listed anywhere) from the silent
// "rs_… not found" retry loop that a stale whitelist used to cause.

/// The ChatGPT Codex Responses endpoint auto-enables
/// `supports_encrypted_reasoning` for every model. The per-item
/// `encrypted_content` check on the agent side handles the
/// "this model doesn't actually emit reasoning" case for us.
#[test]
fn codex_backend_auto_enables_encrypted_reasoning() {
    let provider = ProviderConfig::default();

    for model_id in [
        "gpt-5.3-codex",
        "gpt-5.3-codex-2026-01-15",
        "gpt-5.4-codex",
        "gpt-5.5",
        "gpt-5-codex",
        "gpt-5.2-codex",
    ] {
        assert!(
            supports_encrypted_reasoning(&provider, "https://chatgpt.com/backend-api"),
            "expected auto-enable on Codex endpoint for {model_id}",
        );
    }
    assert!(supports_encrypted_reasoning(
        &provider,
        "https://chatgpt.com/backend-api/"
    ));
}

/// Explicit `supports_encrypted_reasoning` on a provider's compat
/// block forces the flag on for non-Codex endpoints too. Escape
/// hatch for self-hosted or proxy backends that mimic the Codex
/// shape.
#[test]
fn explicit_provider_encrypted_reasoning_flag_wins() {
    let provider = ProviderConfig {
        compat: settings::ProviderCompat {
            supports_encrypted_reasoning: true,
            ..settings::ProviderCompat::default()
        },
        ..ProviderConfig::default()
    };

    assert!(supports_encrypted_reasoning(
        &provider,
        "https://example.com/v1"
    ));
}

/// The public OpenAI REST API is NOT in the auto-enable list:
/// Tau doesn't route through it today, and the encrypted-reasoning
/// surface was scoped to the Codex endpoint.
#[test]
fn public_openai_api_does_not_auto_enable_encrypted_reasoning() {
    let provider = ProviderConfig::default();
    assert!(!supports_encrypted_reasoning(
        &provider,
        "https://api.openai.com/v1"
    ));
}
