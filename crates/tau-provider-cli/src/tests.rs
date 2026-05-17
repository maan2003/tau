use super::*;

#[test]
fn default_names_match_builtin_provider_names() {
    assert_eq!(default_provider_name(&ProviderKind::OpenaiCodex), "chatgpt");
    assert_eq!(
        default_provider_name(&ProviderKind::GithubCopilot),
        "github-copilot"
    );
}

#[test]
fn known_oauth_names_can_login_with_runtime_model_publication() {
    let chatgpt = ProviderName::new("chatgpt");
    let copilot = ProviderName::new("github-copilot");

    assert_eq!(
        default_oauth_kind_for_name(&chatgpt),
        Some(ProviderKind::OpenaiCodex)
    );
    assert_eq!(
        default_oauth_kind_for_name(&copilot),
        Some(ProviderKind::GithubCopilot)
    );
}

#[test]
fn credentials_status_reports_oauth_expiry_without_provider_config() {
    let valid = Credentials::Oauth {
        provider_kind: ProviderKind::OpenaiCodex,
        access_token: "access".to_owned(),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: now_ms().saturating_add(60_000),
        account_id: None,
    };
    let expired = Credentials::Oauth {
        provider_kind: ProviderKind::OpenaiCodex,
        access_token: "access".to_owned(),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: 0,
        account_id: None,
    };

    assert_eq!(credentials_status(&valid), "logged in");
    assert_eq!(credentials_status(&expired), "expired");
}
