//! CLI entrypoint for `tau provider` subcommands.

use std::io::{self, Write};

use dialoguer::Input;
use tau_cli_picker::{PickerItem, pick};
use tau_proto::ProviderName;
use tau_provider::oauth;
use tau_provider::storage::{self, Credentials, ProviderKind};

fn parse_provider_name(name: &str) -> Result<ProviderName, Box<dyn std::error::Error>> {
    ProviderName::try_new(name.to_owned())
        .map_err(|e| format!("invalid provider name '{name}': {e}").into())
}

const HELP_TEXT: &str = "\
Usage: tau provider <subcommand>

Subcommands:
  add                 Add provider credentials (interactive wizard)
  remove [name]       Remove provider credentials
  list                List configured provider credentials
  login [name]        Log in / refresh OAuth token for a provider
  list-models [name]  Explain runtime provider model publication";

/// Run the provider CLI with the given subcommand arguments.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let subcommand = args.first().map(String::as_str).unwrap_or("help");

    match subcommand {
        "add" => cmd_add()?,
        "remove" => cmd_remove(args.get(1).map(String::as_str))?,
        "list" => cmd_list()?,
        "login" => cmd_login(args.get(1).map(String::as_str))?,
        "list-models" => cmd_list_models(args.get(1).map(String::as_str))?,
        "help" | "--help" | "-h" => println!("{HELP_TEXT}"),
        other => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("{HELP_TEXT}");
            return Err(format!("unknown subcommand: {other}").into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tau provider add
// ---------------------------------------------------------------------------

fn cmd_add() -> Result<(), Box<dyn std::error::Error>> {
    let kinds = ProviderKind::all();
    let kind_names = kinds
        .iter()
        .map(|kind| PickerItem::enabled(kind.display_name()))
        .collect::<Vec<_>>();

    let selection = pick("Provider type", &kind_names)?;
    let kind = kinds[selection].clone();

    let default_name = default_provider_name(&kind);
    let name_input: String = Input::new()
        .with_prompt("Name for this provider")
        .default(default_name.to_string())
        .interact_text()?;
    let name = parse_provider_name(&name_input)?;

    let creds = match &kind {
        ProviderKind::Ollama => {
            let base_url: String = Input::new()
                .with_prompt("Base URL")
                .default("http://localhost:11434".to_string())
                .interact_text()?;
            Credentials::None {
                provider_kind: kind.clone(),
                base_url: Some(base_url),
            }
        }
        ProviderKind::Openai => {
            let api_key: String = Input::new().with_prompt("API key").interact_text()?;
            Credentials::ApiKey {
                provider_kind: kind.clone(),
                api_key,
            }
        }
        ProviderKind::Anthropic => {
            let api_key: String = Input::new().with_prompt("API key").interact_text()?;
            Credentials::ApiKey {
                provider_kind: kind.clone(),
                api_key,
            }
        }
        ProviderKind::OpenaiCodex => {
            eprintln!("\nStarting OpenAI login flow...");
            run_openai_codex_login(&kind)?
        }
        ProviderKind::GithubCopilot => {
            eprintln!("\nStarting GitHub Copilot login flow...");
            run_github_copilot_login(&kind)?
        }
    };

    storage::save_provider(&name, &creds)?;

    if let Ok(path) = storage::provider_auth_path(&name) {
        eprintln!("\nCredentials saved to: {}", path.display());
    }
    eprintln!("Provider extensions publish models at runtime.");

    Ok(())
}

fn default_provider_name(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Ollama => "local",
        ProviderKind::Openai => "openai",
        ProviderKind::OpenaiCodex => "chatgpt",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::GithubCopilot => "github-copilot",
    }
}

// ---------------------------------------------------------------------------
// tau provider remove
// ---------------------------------------------------------------------------

fn cmd_remove(name_arg: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let store = storage::load()?;

    let name = match name_arg {
        Some(n) => parse_provider_name(n)?,
        None => {
            let mut names: Vec<&ProviderName> = store.providers.keys().collect();
            names.sort();

            if names.is_empty() {
                eprintln!("No providers to remove.");
                return Ok(());
            }

            let items = names
                .iter()
                .map(|name| PickerItem::enabled(name.as_str()))
                .collect::<Vec<_>>();
            let sel = pick("Which provider to remove?", &items)?;
            names[sel].clone()
        }
    };

    if storage::delete_provider(&name)? {
        eprintln!("Removed credentials for '{name}'.");
    } else {
        eprintln!("Provider '{name}' not found.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// tau provider list
// ---------------------------------------------------------------------------

fn cmd_list() -> Result<(), Box<dyn std::error::Error>> {
    use comfy_table::{ContentArrangement, Table};

    let store = storage::load()?;

    if store.providers.is_empty() {
        eprintln!("No providers configured. Use `tau provider add` to add one.");
        return Ok(());
    }

    let mut names: Vec<&ProviderName> = store.providers.keys().collect();
    names.sort();

    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.load_preset(comfy_table::presets::NOTHING);
    table.set_header(["Name", "Kind", "Auth"]);

    for name in names {
        let creds = &store.providers[name];
        table.add_row([
            name.as_str(),
            creds.provider_kind().display_name(),
            &credentials_status(creds),
        ]);
    }

    println!("{table}");
    Ok(())
}

fn credentials_status(creds: &Credentials) -> String {
    match creds {
        Credentials::Oauth { expires_at_ms, .. } => {
            if now_ms() < *expires_at_ms {
                "logged in".to_owned()
            } else {
                "expired".to_owned()
            }
        }
        Credentials::ApiKey { .. } => "api-key".to_owned(),
        Credentials::None { .. } => "none".to_owned(),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// tau provider login
// ---------------------------------------------------------------------------

fn cmd_login(name_arg: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let store = storage::load()?;

    let name = match name_arg {
        Some(n) => parse_provider_name(n)?,
        None => {
            let mut names: Vec<&ProviderName> = store
                .providers
                .iter()
                .filter(|(_, creds)| creds.provider_kind().requires_oauth())
                .map(|(name, _)| name)
                .collect();
            names.sort();

            if names.is_empty() {
                eprintln!("No OAuth providers configured.");
                eprintln!("Use `tau provider add` or `tau provider login chatgpt` first.");
                return Ok(());
            }

            let items = names
                .iter()
                .map(|name| PickerItem::enabled(name.as_str()))
                .collect::<Vec<_>>();
            let sel = pick("Which provider to log in to?", &items)?;
            names[sel].clone()
        }
    };

    let kind = store
        .providers
        .get(&name)
        .map(|creds| creds.provider_kind().clone())
        .or_else(|| default_oauth_kind_for_name(&name))
        .ok_or_else(|| {
            format!(
                "provider '{name}' is not configured; use `tau provider add` or a known OAuth name"
            )
        })?;

    let new_creds = match kind {
        ProviderKind::OpenaiCodex => run_openai_codex_login(&ProviderKind::OpenaiCodex)?,
        ProviderKind::GithubCopilot => run_github_copilot_login(&ProviderKind::GithubCopilot)?,
        ProviderKind::Ollama | ProviderKind::Openai | ProviderKind::Anthropic => {
            eprintln!(
                "Provider '{name}' ({}) does not use OAuth login.",
                kind.display_name()
            );
            return Ok(());
        }
    };

    storage::save_provider(&name, &new_creds)?;
    eprintln!("Login refreshed for '{name}'.");
    Ok(())
}

fn default_oauth_kind_for_name(name: &ProviderName) -> Option<ProviderKind> {
    match name.as_str() {
        "chatgpt" => Some(ProviderKind::OpenaiCodex),
        "github-copilot" => Some(ProviderKind::GithubCopilot),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// tau provider list-models
// ---------------------------------------------------------------------------

fn cmd_list_models(name_arg: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(name) = name_arg {
        let _ = parse_provider_name(name)?;
    }
    eprintln!(
        "Models are published by provider extensions at runtime. Start `tau` and use `/model` to see the current harness model list."
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// OAuth flow runners
// ---------------------------------------------------------------------------

fn run_openai_codex_login(kind: &ProviderKind) -> Result<Credentials, Box<dyn std::error::Error>> {
    let (auth_url, expected_state, verifier) = oauth::openai_codex_auth_url();

    eprintln!("\nOpen this URL in your browser:\n");
    eprintln!("{auth_url}");
    // OSC 8 hyperlink for terminals that support it.
    eprintln!("\x1b]8;;{auth_url}\x1b\\Or click here.\x1b]8;;\x1b\\");
    eprintln!();
    eprintln!("After logging in, you'll be redirected to a page that won't load.");
    eprintln!("Copy the full URL from your browser's address bar and paste it here:\n");

    io::stdout().flush()?;
    let redirect_input: String = Input::new().with_prompt("Redirect URL").interact_text()?;

    let (code, state) = oauth::parse_redirect_url(&redirect_input)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    if state != expected_state {
        return Err("state mismatch — possible CSRF attack or stale URL".into());
    }

    eprintln!("Exchanging code for tokens...");
    let tokens = oauth::openai_codex_exchange(&code, &verifier)?;

    eprintln!("Login successful!");
    Ok(Credentials::Oauth {
        provider_kind: kind.clone(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: tokens.expires_at_ms,
        account_id: tokens.account_id,
    })
}

fn run_github_copilot_login(
    kind: &ProviderKind,
) -> Result<Credentials, Box<dyn std::error::Error>> {
    let device = oauth::github_device_code_start()?;

    eprintln!("\nGo to: {}", device.verification_uri);
    eprintln!("Enter code: {}\n", device.user_code);
    eprintln!("Waiting for authorization...");

    let github_token =
        oauth::github_device_code_poll(&device.device_code, device.interval, device.expires_in)?;

    eprintln!("GitHub authorized. Fetching Copilot token...");
    let tokens = oauth::github_copilot_token(&github_token)?;

    eprintln!("Login successful!");
    Ok(Credentials::Oauth {
        provider_kind: kind.clone(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: tokens.expires_at_ms,
        account_id: tokens.account_id,
    })
}

#[cfg(test)]
mod tests;
