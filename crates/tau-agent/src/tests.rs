use super::*;

#[test]
fn no_config_resolves_none() {
    let models = ModelRegistry::default();
    let mut auth = tau_provider::storage::AuthStore::default();
    assert!(resolve_backend("fake/model", &models, &mut auth).is_none());
}

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
fn codex_backend_enables_prompt_cache_support() {
    let provider = ProviderConfig::default();

    assert!(supports_prompt_cache_key(
        &provider,
        "https://chatgpt.com/backend-api"
    ));
    assert!(supports_prompt_cache_retention(
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
