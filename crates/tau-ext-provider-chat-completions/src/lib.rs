//! OpenAI-compatible Chat Completions provider extension.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};

use dialoguer::Input;
use serde::{Deserialize, Serialize};
use tau_proto::{
    Ack, ClientKind, ConfigError, ContentPart, ContextItem, ContextRole, Event, EventName, Frame,
    FrameReader, FrameWriter, Message, ModelId, ModelName, ProviderBackend, ProviderBackendKind,
    ProviderBackendTransport, ProviderModelInfo, ProviderModelsUpdated, ProviderName,
    ProviderPromptSubmitted, ProviderResponseFinished, ProviderResponseUpdated, ProviderStopReason,
    ProviderTokenUsage, SessionPromptId, ThinkingSummary, ToolCallItem, ToolChoice, ToolDefinition,
    ToolResponseHeader, ToolResultStatus, ToolType,
};
use tau_provider::storage::AuthFile;

/// `tracing` target for events emitted from this extension.
pub const LOG_TARGET: &str = "provider-chat-completions";

const EXTENSION_NAME: &str = "tau-ext-provider-chat-completions";
/// Auth file name for OpenAI-compatible Chat Completions providers.
pub const AUTH_FILE_NAME: &str = "provider-chat-completions";
const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// Authentication and provider configuration for the Chat Completions
/// extension.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChatCompletionsAuth {
    /// Named Chat Completions-compatible providers. Each key becomes the Tau
    /// provider namespace for its configured models.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<ProviderName, ChatCompletionsProvider>,
}

/// One Chat Completions-compatible provider entry.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChatCompletionsProvider {
    /// Base URL without `/chat/completions`, e.g. `https://api.openai.com/v1`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// Bearer token sent in the `Authorization` header. Empty for local or
    /// otherwise keyless providers.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Model ids to publish under this provider namespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ChatCompletionsModel>,
    /// Extra JSON fields merged into each Chat Completions request body.
    ///
    /// Local and OpenAI-compatible servers use non-standard knobs for reasoning
    /// (`chat_template_kwargs`, `reasoning`, `enable_thinking`, etc.). Keeping
    /// this map provider-scoped lets users opt into those fields without Tau
    /// needing a compatibility switch for every backend.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_body: BTreeMap<String, serde_json::Value>,
    /// Explicit provider compatibility switches.
    #[serde(default)]
    pub compat: ChatCompletionsCompat,
}

/// One model published by a Chat Completions-compatible provider.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatCompletionsModel {
    /// Upstream model id sent in the `model` request field.
    pub id: ModelName,
    /// Optional UI display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Context window size surfaced to the harness.
    #[serde(default = "default_context_window")]
    pub context_window: u64,
}

/// Compatibility switches for OpenAI-compatible Chat Completions APIs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionsCompat {
    /// Whether to send `stream_options: { include_usage: true }`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream_options: bool,
    /// Whether to send `parallel_tool_calls` when tools are declared.
    #[serde(default, skip_serializing_if = "is_false")]
    pub parallel_tool_calls: bool,
    /// Whether to send OpenAI's `prompt_cache_key` field.
    #[serde(default, skip_serializing_if = "is_false")]
    pub prompt_cache_key: bool,
    /// Whether to send `reasoning_effort`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reasoning_effort: bool,
    /// Whether to use `max_completion_tokens` for future output caps.
    #[serde(default, skip_serializing_if = "is_false")]
    pub max_completion_tokens: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

const fn default_context_window() -> u64 {
    DEFAULT_CONTEXT_WINDOW
}

impl ChatCompletionsCompat {
    /// Compatibility switches for OpenAI's public Chat Completions API.
    #[must_use]
    pub const fn openai_defaults() -> Self {
        Self {
            stream_options: true,
            parallel_tool_calls: true,
            prompt_cache_key: true,
            reasoning_effort: true,
            max_completion_tokens: true,
        }
    }
}

/// Runs provider-specific setup commands for Chat Completions-compatible APIs.
pub fn run_provider_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    match args.first().map(String::as_str).unwrap_or("help") {
        "add" => cmd_add()?,
        "remove" | "delete" => cmd_remove(args.get(1).map(String::as_str))?,
        "list" | "status" => cmd_list()?,
        "help" | "--help" | "-h" => println!("{PROVIDER_CLI_HELP}"),
        other => {
            return Err(format!("unknown chat-completions provider subcommand: {other}").into());
        }
    }
    Ok(())
}

const PROVIDER_CLI_HELP: &str = "\
Usage: tau provider chat-completions <subcommand>

Subcommands:
  add             Add or replace a provider entry
  remove <name>   Remove a provider entry
  list            List provider entries";

fn cmd_add() -> Result<(), Box<dyn Error>> {
    let name_input: String = Input::new()
        .with_prompt("Provider namespace")
        .default("openai".to_owned())
        .interact_text()?;
    let name = parse_provider_name(&name_input)?;
    let base_url: String = Input::new()
        .with_prompt("Base URL")
        .default("https://api.openai.com/v1".to_owned())
        .interact_text()?;
    let api_key: String = Input::new()
        .with_prompt("API key (empty for keyless/local providers)")
        .allow_empty(true)
        .interact_text()?;
    let models_input: String = Input::new()
        .with_prompt("Models (comma-separated)")
        .default("gpt-4o,gpt-4o-mini".to_owned())
        .interact_text()?;
    let models = parse_model_list(&models_input)?;
    let compat = prompt_chat_completions_compat()?;

    let file = AuthFile::<ChatCompletionsAuth>::open_default(AUTH_FILE_NAME)?;
    file.with_lock(|locked| {
        let mut auth = locked.load()?.unwrap_or_default();
        auth.providers.insert(
            name,
            ChatCompletionsProvider {
                base_url,
                api_key,
                models,
                extra_body: BTreeMap::new(),
                compat,
            },
        );
        locked.save(&auth)
    })?;
    eprintln!("Provider saved to: {}", file.path().display());
    Ok(())
}

fn cmd_remove(name_arg: Option<&str>) -> Result<(), Box<dyn Error>> {
    let name_input = match name_arg {
        Some(name) => name.to_owned(),
        None => Input::new()
            .with_prompt("Provider namespace")
            .interact_text()?,
    };
    let name = parse_provider_name(&name_input)?;
    let file = AuthFile::<ChatCompletionsAuth>::open_default(AUTH_FILE_NAME)?;
    let removed = file.with_lock(|locked| {
        let mut auth = locked.load()?.unwrap_or_default();
        let removed = auth.providers.remove(&name).is_some();
        locked.save(&auth)?;
        Ok(removed)
    })?;
    if removed {
        eprintln!("Removed provider '{name_input}'.");
    } else {
        eprintln!("Provider '{name_input}' was not configured.");
    }
    Ok(())
}

fn cmd_list() -> Result<(), Box<dyn Error>> {
    let auth = AuthFile::<ChatCompletionsAuth>::open_default(AUTH_FILE_NAME)?
        .load()?
        .unwrap_or_default();
    if auth.providers.is_empty() {
        println!("No chat-completions providers configured.");
        return Ok(());
    }
    for (name, provider) in auth.providers {
        let auth_status = if provider.api_key.trim().is_empty() {
            "no-api-key"
        } else {
            "api-key"
        };
        let models = provider
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>()
            .join(",");
        println!("{name}\t{}\t{models}\t{auth_status}", provider.base_url);
    }
    Ok(())
}

fn parse_provider_name(name: &str) -> Result<ProviderName, Box<dyn Error>> {
    ProviderName::try_new(name.trim().to_owned())
        .map_err(|error| format!("invalid provider namespace '{name}': {error}").into())
}

fn parse_model_list(input: &str) -> Result<Vec<ChatCompletionsModel>, Box<dyn Error>> {
    let mut models = Vec::new();
    for raw in input.split(',') {
        let model = raw.trim();
        if model.is_empty() {
            continue;
        }
        models.push(ChatCompletionsModel {
            id: ModelName::try_new(model.to_owned())?,
            display_name: None,
            context_window: DEFAULT_CONTEXT_WINDOW,
        });
    }
    if models.is_empty() {
        return Err("at least one model is required".into());
    }
    Ok(models)
}

fn prompt_chat_completions_compat() -> Result<ChatCompletionsCompat, Box<dyn Error>> {
    let defaults = ChatCompletionsCompat::openai_defaults();
    Ok(ChatCompletionsCompat {
        stream_options: prompt_bool("Send stream_options.include_usage", defaults.stream_options)?,
        parallel_tool_calls: prompt_bool("Send parallel_tool_calls", defaults.parallel_tool_calls)?,
        prompt_cache_key: prompt_bool("Send prompt_cache_key", defaults.prompt_cache_key)?,
        reasoning_effort: prompt_bool("Send reasoning_effort", defaults.reasoning_effort)?,
        max_completion_tokens: prompt_bool(
            "Use max_completion_tokens when supported",
            defaults.max_completion_tokens,
        )?,
    })
}

fn prompt_bool(prompt: &str, default: bool) -> Result<bool, Box<dyn Error>> {
    let default_text = if default { "yes" } else { "no" };
    let answer: String = Input::new()
        .with_prompt(prompt)
        .default(default_text.to_owned())
        .interact_text()?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" | "1" => Ok(true),
        "n" | "no" | "false" | "0" => Ok(false),
        other => Err(format!("expected yes/no, got '{other}'").into()),
    }
}

/// Runs the extension on stdin/stdout.
pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

/// Runs the extension over arbitrary reader/writer streams.
pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    let auth = load_auth();
    run_with_auth(reader, writer, auth)
}

fn load_auth() -> ChatCompletionsAuth {
    match AuthFile::<ChatCompletionsAuth>::open_default(AUTH_FILE_NAME).and_then(|file| file.load())
    {
        Ok(Some(auth)) => auth,
        Ok(None) => ChatCompletionsAuth::default(),
        Err(error) => {
            tracing::warn!(
                target: LOG_TARGET,
                error = %error,
                "failed to load provider auth; publishing no models"
            );
            ChatCompletionsAuth::default()
        }
    }
}

#[cfg(test)]
fn run_with_auth<R, W>(
    reader: R,
    writer: W,
    auth: ChatCompletionsAuth,
) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    run_inner(reader, writer, auth)
}

#[cfg(not(test))]
fn run_with_auth<R, W>(
    reader: R,
    writer: W,
    auth: ChatCompletionsAuth,
) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    run_inner(reader, writer, auth)
}

fn run_inner<R, W>(
    reader: R,
    writer: W,
    auth_file_auth: ChatCompletionsAuth,
) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    let mut reader = FrameReader::new(BufReader::new(reader));
    let mut writer = FrameWriter::new(BufWriter::new(writer));
    let mut auth = auth_file_auth.clone();

    // No past events requested: SessionPromptCreated is a work request.
    // Replaying old prompts would rerun completed turns; models are
    // announced from current auth below.
    tau_extension::Handshake::with_kind(EXTENSION_NAME, ClientKind::Provider)
        .subscribe([EventName::SESSION_PROMPT_CREATED])
        .announce_event(Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: models_for_auth(&auth),
        }))
        .ready_message("chat-completions provider ready")
        .run(&mut writer)?;

    while let Some(frame) = reader.read_frame()? {
        if handle_frame(frame, &mut writer, &mut auth, &auth_file_auth)? {
            return Ok(());
        }
    }
    Ok(())
}

fn handle_frame<W: Write>(
    frame: Frame,
    writer: &mut FrameWriter<W>,
    auth: &mut ChatCompletionsAuth,
    auth_file_auth: &ChatCompletionsAuth,
) -> Result<bool, Box<dyn Error>> {
    let (log_id, inner) = frame.peel_log();
    match inner {
        Frame::Message(Message::Configure(msg)) => {
            handle_configure(msg, writer, auth, auth_file_auth)?;
        }
        Frame::Event(Event::SessionPromptCreated(prompt)) => {
            handle_prompt(prompt, writer, auth)?;
        }
        Frame::Message(Message::Disconnect(_)) => return Ok(true),
        _ => {}
    }
    if let Some(id) = log_id {
        writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
        writer.flush()?;
    }
    Ok(false)
}

fn handle_configure<W: Write>(
    msg: tau_proto::Configure,
    writer: &mut FrameWriter<W>,
    auth: &mut ChatCompletionsAuth,
    auth_file_auth: &ChatCompletionsAuth,
) -> Result<(), Box<dyn Error>> {
    match tau_extension::parse_config::<ChatCompletionsAuth>(&msg.config) {
        Ok(config_auth) => {
            *auth = merge_config_and_auth(config_auth, auth_file_auth.clone());
            writer.write_frame(&Frame::Event(Event::ProviderModelsUpdated(
                ProviderModelsUpdated {
                    models: models_for_auth(auth),
                },
            )))?;
        }
        Err(message) => {
            tracing::warn!(target: LOG_TARGET, error = %message, "rejecting config");
            writer.write_frame(&Frame::Message(Message::ConfigError(ConfigError {
                message,
            })))?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn handle_prompt<W: Write>(
    prompt: tau_proto::SessionPromptCreated,
    writer: &mut FrameWriter<W>,
    auth: &ChatCompletionsAuth,
) -> Result<(), Box<dyn Error>> {
    let session_prompt_id = prompt.session_prompt_id.clone();
    writer.write_frame(&Frame::Event(Event::ProviderPromptSubmitted(
        ProviderPromptSubmitted {
            session_prompt_id: session_prompt_id.clone(),
            originator: prompt.originator.clone(),
        },
    )))?;
    writer.flush()?;
    let finished = match prompt
        .model
        .as_ref()
        .and_then(|model| resolve_backend(auth, model))
    {
        Some((provider, model)) => run_prompt(&session_prompt_id, &prompt, provider, model, writer),
        None => missing_backend_finished(session_prompt_id, prompt),
    };
    writer.write_frame(&Frame::Event(Event::ProviderResponseFinished(finished)))?;
    writer.flush()?;
    Ok(())
}

fn run_prompt<W: Write>(
    session_prompt_id: &SessionPromptId,
    prompt: &tau_proto::SessionPromptCreated,
    provider: ResolvedProvider,
    model: ChatCompletionsModel,
    writer: &mut FrameWriter<W>,
) -> ProviderResponseFinished {
    let mut on_update = |text: &str, thinking: Option<&str>| {
        let _ = writer.write_frame(&Frame::Event(Event::ProviderResponseUpdated(
            ProviderResponseUpdated {
                session_prompt_id: session_prompt_id.clone(),
                text: text.to_owned(),
                thinking: thinking.map(str::to_owned),
                originator: prompt.originator.clone(),
            },
        )));
        let _ = writer.flush();
    };
    match chat_completions_stream(&provider, &model, prompt, &mut on_update) {
        Ok(state) => finish_success(session_prompt_id, prompt, &provider, state),
        Err(error) => finish_error(session_prompt_id, prompt, &provider, error),
    }
}

fn missing_backend_finished(
    session_prompt_id: SessionPromptId,
    prompt: tau_proto::SessionPromptCreated,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        session_prompt_id,
        output_items: vec![assistant_text_item(
            "No Chat Completions backend is configured for the selected model.",
        )],
        stop_reason: ProviderStopReason::Error,
        originator: prompt.originator,
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

#[derive(Clone)]
struct ResolvedProvider {
    base_url: String,
    api_key: String,
    extra_body: BTreeMap<String, serde_json::Value>,
    compat: ChatCompletionsCompat,
}

fn resolve_backend(
    auth: &ChatCompletionsAuth,
    model: &ModelId,
) -> Option<(ResolvedProvider, ChatCompletionsModel)> {
    let provider = auth.providers.get(&model.provider)?;
    let configured_model = provider
        .models
        .iter()
        .find(|configured| configured.id == model.model)
        .cloned()?;
    Some((
        ResolvedProvider {
            base_url: provider.base_url.clone(),
            api_key: provider.api_key.clone(),
            extra_body: provider.extra_body.clone(),
            compat: provider.compat,
        },
        configured_model,
    ))
}

fn merge_config_and_auth(
    mut config_auth: ChatCompletionsAuth,
    auth_file_auth: ChatCompletionsAuth,
) -> ChatCompletionsAuth {
    for (name, provider) in auth_file_auth.providers {
        config_auth.providers.insert(name, provider);
    }
    config_auth
}

fn models_for_auth(auth: &ChatCompletionsAuth) -> Vec<ProviderModelInfo> {
    let mut models = Vec::new();
    for (provider_name, provider) in &auth.providers {
        for model in &provider.models {
            models.push(ProviderModelInfo {
                id: ModelId::new(provider_name.clone(), model.id.clone()),
                display_name: model.display_name.clone(),
                default_affinity: 0,
                context_window: model.context_window,
                efforts: model_efforts(provider.compat),
                verbosities: vec![tau_proto::Verbosity::Medium],
                thinking_summaries: vec![ThinkingSummary::Off],
                supports_compaction: false,
            });
        }
    }
    models
}

fn model_efforts(compat: ChatCompletionsCompat) -> Vec<tau_proto::Effort> {
    if compat.reasoning_effort {
        vec![
            tau_proto::Effort::Off,
            tau_proto::Effort::Minimal,
            tau_proto::Effort::Low,
            tau_proto::Effort::Medium,
            tau_proto::Effort::High,
            tau_proto::Effort::XHigh,
        ]
    } else {
        vec![tau_proto::Effort::Off]
    }
}

#[derive(Debug)]
enum LlmError {
    Http(Box<ureq::Error>),
    HttpStatus(u16, String),
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(error) => write!(f, "HTTP error: {error}"),
            Self::HttpStatus(code, body) => write!(f, "HTTP {code}: {body}"),
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Json(error) => write!(f, "JSON error: {error}"),
        }
    }
}

struct StreamState {
    text: String,
    thinking: String,
    pending_content: String,
    in_think_tag: bool,
    tool_calls: HashMap<usize, ToolCallAccumulator>,
    input_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    output_tokens: Option<u64>,
    stop_reason: ProviderStopReason,
}

impl StreamState {
    fn new() -> Self {
        Self {
            text: String::new(),
            thinking: String::new(),
            pending_content: String::new(),
            in_think_tag: false,
            tool_calls: HashMap::new(),
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            stop_reason: ProviderStopReason::EndTurn,
        }
    }

    fn output_items(&self) -> Vec<ContextItem> {
        let mut items = Vec::new();
        if !self.text.is_empty() {
            items.push(assistant_text_item(self.text.clone()));
        }
        let mut tool_calls = self.tool_calls.iter().collect::<Vec<_>>();
        tool_calls.sort_by_key(|(index, _)| **index);
        for (_, call) in tool_calls {
            if !call.name.is_empty() {
                items.push(ContextItem::ToolCall(ToolCallItem {
                    call_id: call.id.clone().into(),
                    name: tau_proto::ToolName::new(call.name.clone()),
                    tool_type: ToolType::Function,
                    arguments: serde_json::from_str::<serde_json::Value>(&call.arguments)
                        .map(|value| json_to_cbor(&value))
                        .unwrap_or(tau_proto::CborValue::Null),
                }));
            }
        }
        items
    }

    fn usage(&self) -> Option<ProviderTokenUsage> {
        let input = self.input_tokens.unwrap_or(0);
        let cached = self.cached_tokens.unwrap_or(0);
        let output = self.output_tokens.unwrap_or(0);
        if input == 0 && cached == 0 && output == 0 {
            None
        } else {
            Some(ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input,
                prompt_cached_tokens: cached,
                response_received_tokens: output,
                stats: Default::default(),
            })
        }
    }
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

fn chat_completions_stream(
    provider: &ResolvedProvider,
    model: &ChatCompletionsModel,
    prompt: &tau_proto::SessionPromptCreated,
    on_update: &mut impl FnMut(&str, Option<&str>),
) -> Result<StreamState, LlmError> {
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let body = build_request(provider, model, prompt);
    let body_str = serde_json::to_string(&body).map_err(LlmError::Json)?;
    let mut request = tau_provider::oauth::proxy_agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream");
    if !provider.api_key.trim().is_empty() {
        request = request.set("Authorization", &format!("Bearer {}", provider.api_key));
    }
    let response = request
        .send_string(&body_str)
        .map_err(|error| match error {
            ureq::Error::Status(code, response) => {
                LlmError::HttpStatus(code, response.into_string().unwrap_or_default())
            }
            other => LlmError::Http(Box::new(other)),
        })?;

    let mut state = StreamState::new();
    let reader = BufReader::new(response.into_reader());
    for line in reader.lines() {
        let line = line.map_err(LlmError::Io)?;
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let event: serde_json::Value = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(_) => continue,
        };
        apply_event(&mut state, &event, on_update);
    }
    flush_pending_content(&mut state, on_update);
    Ok(state)
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'static str>,
    #[serde(flatten)]
    extra_body: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

fn build_request(
    provider: &ResolvedProvider,
    model: &ChatCompletionsModel,
    prompt: &tau_proto::SessionPromptCreated,
) -> ChatRequest {
    let mut messages = Vec::new();
    if !prompt.system_prompt.trim().is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": prompt.system_prompt,
        }));
    }
    for item in &prompt.context_items {
        append_context_item(item, &mut messages);
    }
    let tools = prompt
        .tools
        .iter()
        .filter_map(convert_tool_definition)
        .collect::<Vec<_>>();
    let tool_choice = match (prompt.tool_choice, tools.is_empty()) {
        (ToolChoice::None, _) => Some("none"),
        (ToolChoice::Auto, false) => Some("auto"),
        (ToolChoice::Auto, true) => None,
    };
    ChatRequest {
        model: model.id.as_str().to_owned(),
        messages,
        stream: true,
        stream_options: provider.compat.stream_options.then_some(StreamOptions {
            include_usage: true,
        }),
        parallel_tool_calls: (provider.compat.parallel_tool_calls && !tools.is_empty())
            .then_some(true),
        prompt_cache_key: provider
            .compat
            .prompt_cache_key
            .then(|| format!("tau:{}", prompt.session_id)),
        reasoning_effort: provider
            .compat
            .reasoning_effort
            .then(|| effort_wire(prompt.model_params.effort)),
        extra_body: provider.extra_body.clone(),
        tools,
        tool_choice,
    }
}

fn append_context_item(item: &ContextItem, messages: &mut Vec<serde_json::Value>) {
    match item {
        ContextItem::Message(message) => {
            let text = message_text(message);
            if message.role == ContextRole::User && text.trim().is_empty() {
                return;
            }
            if text.is_empty() {
                return;
            }
            messages.push(serde_json::json!({
                "role": role_wire(&message.role),
                "content": text,
            }));
        }
        ContextItem::ToolCall(call) => {
            messages.push(serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": call.call_id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": cbor_to_json(&call.arguments).to_string(),
                    }
                }]
            }));
        }
        ContextItem::ToolResult(result) => {
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": result.call_id,
                "content": tool_result_text(result.status.clone(), &result.output),
            }));
        }
        ContextItem::Reasoning(_)
        | ContextItem::Compaction(_)
        | ContextItem::UnknownProviderItem(_) => {}
    }
}

fn message_text(message: &tau_proto::MessageItem) -> String {
    let mut text = String::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text: part } => text.push_str(part),
        }
    }
    text
}

fn role_wire(role: &ContextRole) -> &'static str {
    match role {
        ContextRole::System => "system",
        ContextRole::Developer => "system",
        ContextRole::User => "user",
        ContextRole::Assistant => "assistant",
    }
}

fn tool_result_text(status: ToolResultStatus, output: &tau_proto::ToolResponse) -> String {
    match status {
        ToolResultStatus::Success => output.render(),
        ToolResultStatus::Error { message } => {
            let mut response = output.clone();
            response.headers.insert(
                0,
                ToolResponseHeader {
                    key: "error".to_owned(),
                    value: message,
                },
            );
            response.render()
        }
        ToolResultStatus::Cancelled { reason } => tau_proto::ToolResponse {
            raw: tau_proto::CborValue::Null,
            headers: vec![ToolResponseHeader {
                key: "cancelled".to_owned(),
                value: reason,
            }],
            body: String::new(),
        }
        .render(),
    }
}

fn convert_tool_definition(tool: &ToolDefinition) -> Option<serde_json::Value> {
    if tool.tool_type != ToolType::Function {
        return None;
    }
    Some(serde_json::json!({
        "type": "function",
        "function": {
            "name": tool.model_visible_name.as_ref().unwrap_or(&tool.name),
            "description": tool.description,
            "parameters": tool.parameters,
        }
    }))
}

fn apply_event(
    state: &mut StreamState,
    event: &serde_json::Value,
    on_update: &mut impl FnMut(&str, Option<&str>),
) {
    if let Some(usage) = event.get("usage") {
        capture_usage(state, usage);
    }
    let Some(choice) = event["choices"]
        .as_array()
        .and_then(|choices| choices.first())
    else {
        return;
    };
    let delta = &choice["delta"];
    let mut changed = false;
    for key in ["reasoning_content", "reasoning", "thinking"] {
        if let Some(reasoning) = delta[key].as_str()
            && !reasoning.is_empty()
        {
            state.thinking.push_str(reasoning);
            changed = true;
        }
    }
    if let Some(content) = delta["content"].as_str()
        && !content.is_empty()
    {
        changed |= append_content_delta(state, content);
    }
    if changed {
        on_update(&state.text, thinking_for_update(state));
    }
    if let Some(tool_calls) = delta["tool_calls"].as_array() {
        for tool_call in tool_calls {
            let index = tool_call["index"].as_u64().unwrap_or(0) as usize;
            let entry = state.tool_calls.entry(index).or_default();
            if let Some(id) = tool_call["id"].as_str()
                && !id.is_empty()
            {
                entry.id = id.to_owned();
            }
            let function = &tool_call["function"];
            if let Some(name) = function["name"].as_str()
                && !name.is_empty()
            {
                entry.name = name.to_owned();
            }
            if let Some(arguments) = function["arguments"].as_str() {
                entry.arguments.push_str(arguments);
            }
        }
    }
    match choice["finish_reason"].as_str() {
        Some("tool_calls") => state.stop_reason = ProviderStopReason::ToolCalls,
        Some("stop") => state.stop_reason = ProviderStopReason::EndTurn,
        _ => {}
    }
}

fn append_content_delta(state: &mut StreamState, content: &str) -> bool {
    state.pending_content.push_str(content);
    let mut changed = false;
    loop {
        if state.pending_content.is_empty() {
            return changed;
        }
        if state.in_think_tag {
            if let Some(index) = state.pending_content.find("</think>") {
                state.thinking.push_str(&state.pending_content[..index]);
                state.pending_content.drain(..index + "</think>".len());
                state.in_think_tag = false;
                changed = true;
                continue;
            }
            let keep = partial_tag_suffix_len(&state.pending_content, "</think>");
            let emit_len = state.pending_content.len() - keep;
            if emit_len == 0 {
                return changed;
            }
            state.thinking.push_str(&state.pending_content[..emit_len]);
            state.pending_content.drain(..emit_len);
            return true;
        }

        if let Some(index) = state.pending_content.find("<think>") {
            state.text.push_str(&state.pending_content[..index]);
            state.pending_content.drain(..index + "<think>".len());
            state.in_think_tag = true;
            changed = true;
            continue;
        }
        let keep = partial_tag_suffix_len(&state.pending_content, "<think>");
        let emit_len = state.pending_content.len() - keep;
        if emit_len == 0 {
            return changed;
        }
        state.text.push_str(&state.pending_content[..emit_len]);
        state.pending_content.drain(..emit_len);
        return true;
    }
}

fn partial_tag_suffix_len(text: &str, tag: &str) -> usize {
    let mut keep = 0;
    for len in 1..tag.len() {
        if text.ends_with(&tag[..len]) {
            keep = len;
        }
    }
    keep
}

fn flush_pending_content(state: &mut StreamState, on_update: &mut impl FnMut(&str, Option<&str>)) {
    if state.pending_content.is_empty() {
        return;
    }
    if state.in_think_tag {
        state.thinking.push_str(&state.pending_content);
    } else {
        state.text.push_str(&state.pending_content);
    }
    state.pending_content.clear();
    on_update(&state.text, thinking_for_update(state));
}

fn thinking_for_update(state: &StreamState) -> Option<&str> {
    (!state.thinking.is_empty()).then_some(state.thinking.as_str())
}

fn capture_usage(state: &mut StreamState, usage: &serde_json::Value) {
    state.input_tokens = usage["prompt_tokens"].as_u64();
    state.output_tokens = usage["completion_tokens"].as_u64();
    state.cached_tokens = usage["prompt_tokens_details"]["cached_tokens"].as_u64();
}

fn finish_success(
    session_prompt_id: &SessionPromptId,
    prompt: &tau_proto::SessionPromptCreated,
    provider: &ResolvedProvider,
    state: StreamState,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        session_prompt_id: session_prompt_id.clone(),
        output_items: state.output_items(),
        stop_reason: state.stop_reason,
        originator: prompt.originator.clone(),
        usage: state.usage(),
        backend: Some(backend_descriptor(provider)),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn finish_error(
    session_prompt_id: &SessionPromptId,
    prompt: &tau_proto::SessionPromptCreated,
    provider: &ResolvedProvider,
    error: LlmError,
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        session_prompt_id: session_prompt_id.clone(),
        output_items: vec![assistant_text_item(format!("LLM error: {error}"))],
        stop_reason: ProviderStopReason::Error,
        originator: prompt.originator.clone(),
        usage: None,
        backend: Some(backend_descriptor(provider)),
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn backend_descriptor(provider: &ResolvedProvider) -> ProviderBackend {
    ProviderBackend {
        kind: ProviderBackendKind::ChatCompletions,
        base_url: provider.base_url.clone(),
        transport: ProviderBackendTransport::HttpSse,
        stale_chain_fallback: false,
    }
}

fn assistant_text_item(text: impl Into<String>) -> ContextItem {
    ContextItem::Message(tau_proto::MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text { text: text.into() }],
        phase: None,
    })
}

fn effort_wire(effort: tau_proto::Effort) -> &'static str {
    match effort {
        tau_proto::Effort::Off => "none",
        tau_proto::Effort::Minimal => "minimal",
        tau_proto::Effort::Low => "low",
        tau_proto::Effort::Medium => "medium",
        tau_proto::Effort::High => "high",
        tau_proto::Effort::XHigh => "high",
    }
}

fn cbor_to_json(value: &tau_proto::CborValue) -> serde_json::Value {
    match value {
        tau_proto::CborValue::Null => serde_json::Value::Null,
        tau_proto::CborValue::Bool(v) => serde_json::Value::Bool(*v),
        tau_proto::CborValue::Integer(v) => {
            let n: i128 = (*v).into();
            serde_json::json!(n)
        }
        tau_proto::CborValue::Float(v) => serde_json::Number::from_f64(*v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        tau_proto::CborValue::Text(v) => serde_json::Value::String(v.clone()),
        tau_proto::CborValue::Bytes(bytes) => serde_json::Value::Array(
            bytes
                .iter()
                .map(|byte| serde_json::Value::Number((*byte).into()))
                .collect(),
        ),
        tau_proto::CborValue::Array(items) => {
            serde_json::Value::Array(items.iter().map(cbor_to_json).collect())
        }
        tau_proto::CborValue::Map(entries) => {
            let mut map = serde_json::Map::new();
            for (key, value) in entries {
                let key = match key {
                    tau_proto::CborValue::Text(text) => text.clone(),
                    other => serde_json::to_string(&cbor_to_json(other)).unwrap_or_default(),
                };
                map.insert(key, cbor_to_json(value));
            }
            serde_json::Value::Object(map)
        }
        tau_proto::CborValue::Tag(_, inner) => cbor_to_json(inner),
        _ => serde_json::Value::Null,
    }
}

fn json_to_cbor(value: &serde_json::Value) -> tau_proto::CborValue {
    match value {
        serde_json::Value::Null => tau_proto::CborValue::Null,
        serde_json::Value::Bool(v) => tau_proto::CborValue::Bool(*v),
        serde_json::Value::Number(v) => {
            if let Some(n) = v.as_i64() {
                tau_proto::CborValue::Integer(n.into())
            } else if let Some(n) = v.as_u64() {
                tau_proto::CborValue::Integer(n.into())
            } else if let Some(n) = v.as_f64() {
                tau_proto::CborValue::Float(n)
            } else {
                tau_proto::CborValue::Null
            }
        }
        serde_json::Value::String(v) => tau_proto::CborValue::Text(v.clone()),
        serde_json::Value::Array(items) => {
            tau_proto::CborValue::Array(items.iter().map(json_to_cbor).collect())
        }
        serde_json::Value::Object(map) => tau_proto::CborValue::Map(
            map.iter()
                .map(|(key, value)| (tau_proto::CborValue::Text(key.clone()), json_to_cbor(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> ChatCompletionsAuth {
        ChatCompletionsAuth {
            providers: BTreeMap::from([(
                ProviderName::new("openai"),
                ChatCompletionsProvider {
                    base_url: "https://api.openai.com/v1".to_owned(),
                    api_key: "key".to_owned(),
                    models: vec![ChatCompletionsModel {
                        id: ModelName::new("gpt-4o"),
                        display_name: None,
                        context_window: 128_000,
                    }],
                    extra_body: BTreeMap::new(),
                    compat: ChatCompletionsCompat::openai_defaults(),
                },
            )]),
        }
    }

    #[test]
    fn parse_model_list_rejects_empty_lists() {
        // Provider-specific CLI setup should not write a provider entry that
        // publishes no models.
        assert!(parse_model_list(" , ").is_err());
    }

    #[test]
    fn parse_model_list_accepts_comma_separated_models() {
        // The interactive setup stores the exact upstream model ids that will be
        // published under the chosen provider namespace.
        let models = parse_model_list("gpt-4o, gpt-4o-mini").expect("models");

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id.as_str(), "gpt-4o");
        assert_eq!(models[1].id.as_str(), "gpt-4o-mini");
    }

    #[test]
    fn publishes_configured_models() {
        // Chat Completions has no built-in model registry; the auth file's
        // configured model list is the complete publication source.
        let models = models_for_auth(&auth());

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id.to_string(), "openai/gpt-4o");
        assert!(!models[0].supports_compaction);
    }

    #[test]
    fn publishes_keyless_configured_models() {
        // Local Chat Completions-compatible servers often do not require API
        // keys. Model publication should depend on configured models, not on a
        // secret being present in the auth file.
        let mut auth = auth();
        auth.providers
            .get_mut(&ProviderName::new("openai"))
            .expect("provider")
            .api_key
            .clear();

        let models = models_for_auth(&auth);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id.to_string(), "openai/gpt-4o");
    }

    #[test]
    fn handles_logged_prompt_created_and_acks_it() {
        // The harness delivers subscribed events through LogEvent envelopes.
        // Providers must peel the envelope, process the prompt, and ack the log
        // id so durable event replay can advance.
        let prompt = tau_proto::SessionPromptCreated {
            session_prompt_id: "sp-logged".into(),
            session_id: "s1".into(),
            system_prompt: "sys".to_owned(),
            context_items: Vec::new(),
            tools: Vec::new(),
            tools_ref: None,
            model: Some("missing/model".into()),
            model_params: Default::default(),
            tool_choice: ToolChoice::Auto,
            originator: tau_proto::PromptOriginator::User,
            share_user_cache_key: false,
            ctx_id: None,
            previous_response_candidate: None,
        };
        let mut auth = ChatCompletionsAuth::default();
        let auth_file_auth = ChatCompletionsAuth::default();
        let mut output = Vec::new();
        let mut writer = FrameWriter::new(&mut output);

        let disconnected = handle_frame(
            Frame::Message(Message::LogEvent(tau_proto::LogEvent {
                id: tau_proto::LogEventId::new(7),
                recorded_at: tau_proto::UnixMicros::default(),
                event: Box::new(Event::SessionPromptCreated(prompt)),
            })),
            &mut writer,
            &mut auth,
            &auth_file_auth,
        )
        .expect("handle logged prompt");

        assert!(!disconnected);
        let mut reader = FrameReader::new(std::io::Cursor::new(output));
        let submitted = reader
            .read_frame()
            .expect("read submitted")
            .expect("submitted frame");
        assert!(matches!(
            submitted,
            Frame::Event(Event::ProviderPromptSubmitted(ProviderPromptSubmitted {
                session_prompt_id,
                ..
            })) if session_prompt_id.as_str() == "sp-logged"
        ));
        assert!(matches!(
            reader
                .read_frame()
                .expect("read finished")
                .expect("finished frame"),
            Frame::Event(Event::ProviderResponseFinished(ProviderResponseFinished {
                session_prompt_id,
                ..
            })) if session_prompt_id.as_str() == "sp-logged"
        ));
        assert!(matches!(
            reader.read_frame().expect("read ack").expect("ack frame"),
            Frame::Message(Message::Ack(Ack { up_to })) if up_to.get() == 7
        ));
        assert!(reader.read_frame().expect("read eof").is_none());
    }

    #[test]
    fn auth_file_provider_replaces_matching_config_provider() {
        // Config and auth file entries are complete provider definitions. If
        // both define the same provider namespace, the auth-file entry replaces
        // the config entry as a whole instead of merging individual fields.
        let config_auth = ChatCompletionsAuth {
            providers: BTreeMap::from([(
                ProviderName::new("ollama"),
                ChatCompletionsProvider {
                    base_url: "http://localhost:11434/v1".to_owned(),
                    models: vec![ChatCompletionsModel {
                        id: ModelName::new("gemma-4"),
                        display_name: None,
                        context_window: 128_000,
                    }],
                    ..Default::default()
                },
            )]),
        };
        let auth_file_auth = ChatCompletionsAuth {
            providers: BTreeMap::from([(
                ProviderName::new("ollama"),
                ChatCompletionsProvider {
                    base_url: "https://example.invalid/v1".to_owned(),
                    api_key: "secret".to_owned(),
                    models: vec![ChatCompletionsModel {
                        id: ModelName::new("other-model"),
                        display_name: None,
                        context_window: 128_000,
                    }],
                    ..Default::default()
                },
            )]),
        };
        let merged = merge_config_and_auth(config_auth, auth_file_auth);
        let provider = &merged.providers[&ProviderName::new("ollama")];

        assert_eq!(provider.base_url, "https://example.invalid/v1");
        assert_eq!(provider.api_key, "secret");
        assert_eq!(provider.models[0].id.as_str(), "other-model");
        assert_eq!(
            models_for_auth(&merged)[0].id.to_string(),
            "ollama/other-model"
        );
    }

    #[test]
    fn tool_result_text_uses_structured_status_headers() {
        // Chat Completions and Responses API providers should expose identical
        // provider-facing text for non-success tool results, so model behavior
        // does not depend on the selected OpenAI-compatible API surface.
        let output = tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Text("body".into()));

        assert_eq!(
            tool_result_text(
                ToolResultStatus::Error {
                    message: "failed".to_owned(),
                },
                &output,
            ),
            "error: failed\n\nbody",
        );
        assert_eq!(
            tool_result_text(
                ToolResultStatus::Cancelled {
                    reason: "stopped".to_owned(),
                },
                &output,
            ),
            "cancelled: stopped\n\n",
        );
    }

    #[test]
    fn provider_with_reasoning_effort_publishes_effort_levels() {
        // Role effort selection is clamped to the provider-advertised levels.
        // Publishing only `off` made `compat.reasoning_effort` unusable because
        // a role configured with `effort: high` was downgraded before request
        // construction.
        let models = models_for_auth(&auth());

        assert!(models[0].efforts.contains(&tau_proto::Effort::High));
        assert!(models[0].efforts.contains(&tau_proto::Effort::Off));
    }

    #[test]
    fn build_request_flattens_extra_body_for_reasoning_knobs() {
        // OpenAI-compatible local servers disagree on reasoning controls. The
        // provider-level `extra_body` map is intentionally flattened into the
        // request so users can pass backend-specific fields like
        // `chat_template_kwargs.enable_thinking` without Tau hard-coding each
        // variant.
        let mut auth = auth();
        auth.providers
            .get_mut(&ProviderName::new("openai"))
            .expect("provider")
            .extra_body
            .insert(
                "chat_template_kwargs".to_owned(),
                serde_json::json!({ "enable_thinking": true }),
            );
        let (provider, model) = resolve_backend(&auth, &"openai/gpt-4o".into()).expect("backend");
        let prompt = tau_proto::SessionPromptCreated {
            session_prompt_id: "sp-extra".into(),
            session_id: "s1".into(),
            system_prompt: String::new(),
            context_items: Vec::new(),
            tools: Vec::new(),
            tools_ref: None,
            model: Some("openai/gpt-4o".into()),
            model_params: tau_proto::ModelParams {
                effort: tau_proto::Effort::High,
                ..Default::default()
            },
            tool_choice: ToolChoice::Auto,
            originator: tau_proto::PromptOriginator::User,
            share_user_cache_key: false,
            ctx_id: None,
            previous_response_candidate: None,
        };

        let request =
            serde_json::to_value(build_request(&provider, &model, &prompt)).expect("json");

        assert_eq!(request["reasoning_effort"], "high");
        assert_eq!(request["chat_template_kwargs"]["enable_thinking"], true);
    }

    #[test]
    fn apply_event_streams_reasoning_fields_and_think_tags() {
        // Reasoning-capable Chat Completions servers are not unified: some send
        // dedicated reasoning deltas, while others leave visible `<think>` tags
        // in content. Normalize both into ProviderResponseUpdated.thinking and
        // keep only answer text in the visible response.
        let mut state = StreamState::new();
        let mut updates = Vec::new();
        let mut on_update = |text: &str, thinking: Option<&str>| {
            updates.push((text.to_owned(), thinking.map(str::to_owned)));
        };

        apply_event(
            &mut state,
            &serde_json::json!({
                "choices": [{ "delta": { "reasoning_content": "plan " } }]
            }),
            &mut on_update,
        );
        apply_event(
            &mut state,
            &serde_json::json!({
                "choices": [{ "delta": { "content": "visible <thi" } }]
            }),
            &mut on_update,
        );
        apply_event(
            &mut state,
            &serde_json::json!({
                "choices": [{ "delta": { "content": "nk>tag</think> answer" } }]
            }),
            &mut on_update,
        );
        flush_pending_content(&mut state, &mut on_update);

        assert_eq!(state.text, "visible  answer");
        assert_eq!(state.thinking, "plan tag");
        assert_eq!(updates.last().expect("update").0, "visible  answer");
        assert_eq!(
            updates.last().expect("update").1.as_deref(),
            Some("plan tag")
        );
    }

    #[test]
    fn build_request_skips_blank_user_messages_and_emits_tools() {
        // Some OpenAI-compatible APIs reject whitespace-only user messages. The
        // request builder drops them while preserving tool declarations.
        let (provider, model) = resolve_backend(&auth(), &"openai/gpt-4o".into()).expect("backend");
        let prompt = tau_proto::SessionPromptCreated {
            session_prompt_id: "sp-1".into(),
            session_id: "s1".into(),
            system_prompt: "sys".to_owned(),
            context_items: vec![ContextItem::Message(tau_proto::MessageItem {
                role: ContextRole::User,
                content: vec![ContentPart::Text {
                    text: "   ".to_owned(),
                }],
                phase: None,
            })],
            tools: vec![ToolDefinition {
                name: tau_proto::ToolName::new("read"),
                model_visible_name: None,
                description: Some("Read a file".to_owned()),
                tool_type: ToolType::Function,
                parameters: Some(serde_json::json!({"type":"object"})),
                format: None,
            }],
            tools_ref: None,
            model: Some("openai/gpt-4o".into()),
            model_params: Default::default(),
            tool_choice: ToolChoice::Auto,
            originator: tau_proto::PromptOriginator::User,
            share_user_cache_key: false,
            ctx_id: None,
            previous_response_candidate: None,
        };

        let request =
            serde_json::to_value(build_request(&provider, &model, &prompt)).expect("json");

        assert_eq!(request["messages"].as_array().expect("messages").len(), 1);
        assert_eq!(request["tools"].as_array().expect("tools").len(), 1);
        assert_eq!(request["tool_choice"], "auto");
        assert_eq!(request["stream_options"]["include_usage"], true);
    }
}
