//! Live tool registration tracking and routing of `tool.request` events
//! to the connection that owns each tool.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use tau_proto::{
    CborValue, ConnectionId, PromptFragment, ToolName, ToolRegister, ToolRequest, ToolSpec,
    ToolStarted, ToolType,
};

use crate::connection::RouteError;

/// One live provider registered for a tool name.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolProvider {
    /// Connection that registered this tool.
    pub connection_id: ConnectionId,
    /// Tool metadata advertised to the model and used for routing.
    pub tool: ToolSpec,
    /// Optional prompt fragment template contributed while this tool is
    /// enabled.
    pub prompt_fragment: Option<PromptFragment>,
}

/// Warning emitted by the tool registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRegistryWarning {
    DuplicateRegistration {
        tool_name: ToolName,
        existing_provider_ids: Vec<ConnectionId>,
    },
}

/// Summary of one registration call.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RegisterToolReport {
    pub warnings: Vec<ToolRegistryWarning>,
}

/// Error returned when a tool tool request cannot be routed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRouteError {
    NoProvider { tool_name: ToolName },
    Route(RouteError),
}

/// Error returned when a tool call's arguments do not match its JSON schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolArgumentValidationError {
    path: String,
    message: String,
}

impl ToolArgumentValidationError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ToolArgumentValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path == "$" {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl Error for ToolArgumentValidationError {}

impl fmt::Display for ToolRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProvider { tool_name } => write!(f, "no live provider for tool: {tool_name}"),
            Self::Route(error) => write!(f, "failed to route tool tool request: {error}"),
        }
    }
}

impl Error for ToolRouteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NoProvider { .. } => None,
            Self::Route(error) => Some(error),
        }
    }
}

/// Summary of one `tool.request` routing decision.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolRouteReport {
    pub provider_connection_id: ConnectionId,
    pub invoke: ToolStarted,
}

/// Validates a model-produced function-tool argument object against the tool's
/// JSON Schema parameters.
///
/// Tau tool schemas intentionally use a small JSON Schema subset: object
/// properties, required fields, closed objects via `additionalProperties:
/// false`, primitive `type`, `enum`, array `items`, and numeric/string/array
/// bounds. Unknown schema keywords are ignored so richer third-party schemas do
/// not become harness errors.
pub fn validate_tool_arguments(
    tool: &ToolSpec,
    arguments: &CborValue,
) -> Result<(), ToolArgumentValidationError> {
    if !matches!(tool.tool_type, ToolType::Function) {
        return Ok(());
    }
    let Some(schema) = tool.parameters.as_ref() else {
        return Ok(());
    };
    validate_json_schema(schema, arguments, "$")
}

fn validate_json_schema(
    schema: &serde_json::Value,
    value: &CborValue,
    path: &str,
) -> Result<(), ToolArgumentValidationError> {
    match schema {
        serde_json::Value::Bool(true) => return Ok(()),
        serde_json::Value::Bool(false) => {
            return Err(ToolArgumentValidationError::new(
                path,
                "value is rejected by schema",
            ));
        }
        _ => {}
    }

    let Some(schema) = schema.as_object() else {
        return Ok(());
    };

    if let Some(type_schema) = schema.get("type")
        && !schema_type_matches(type_schema, value)
    {
        return Err(type_error(path, type_schema));
    }

    if let Some(enum_values) = schema.get("enum").and_then(serde_json::Value::as_array)
        && !enum_values
            .iter()
            .any(|allowed| tau_proto::json_to_cbor(allowed) == *value)
    {
        return Err(ToolArgumentValidationError::new(
            path,
            "must be one of the schema enum values",
        ));
    }

    match value {
        CborValue::Map(entries) => validate_object_schema(schema, entries, path),
        CborValue::Array(values) => validate_array_schema(schema, values, path),
        CborValue::Text(text) => validate_string_schema(schema, text, path),
        CborValue::Integer(_) | CborValue::Float(_) => validate_number_schema(schema, value, path),
        _ => Ok(()),
    }
}

fn schema_type_matches(type_schema: &serde_json::Value, value: &CborValue) -> bool {
    match type_schema {
        serde_json::Value::String(kind) => schema_type_name_matches(kind, value),
        serde_json::Value::Array(kinds) => kinds.iter().any(|kind| {
            kind.as_str()
                .is_some_and(|kind| schema_type_name_matches(kind, value))
        }),
        _ => true,
    }
}

fn schema_type_name_matches(kind: &str, value: &CborValue) -> bool {
    match kind {
        "object" => matches!(value, CborValue::Map(_)),
        "array" => matches!(value, CborValue::Array(_)),
        "string" => matches!(value, CborValue::Text(_)),
        "boolean" => matches!(value, CborValue::Bool(_)),
        "integer" => matches!(value, CborValue::Integer(_)),
        "number" => matches!(value, CborValue::Integer(_) | CborValue::Float(_)),
        "null" => matches!(value, CborValue::Null),
        _ => true,
    }
}

fn type_error(path: &str, type_schema: &serde_json::Value) -> ToolArgumentValidationError {
    let expected = match type_schema {
        serde_json::Value::String(kind) => kind.clone(),
        serde_json::Value::Array(kinds) => kinds
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "expected schema type".to_owned(),
    };
    if path == "$" && expected == "object" {
        ToolArgumentValidationError::new(path, "arguments must be an object")
    } else {
        ToolArgumentValidationError::new(path, format!("must be {expected}"))
    }
}

fn validate_object_schema(
    schema: &serde_json::Map<String, serde_json::Value>,
    entries: &[(CborValue, CborValue)],
    path: &str,
) -> Result<(), ToolArgumentValidationError> {
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object);

    if let Some(required) = schema.get("required").and_then(serde_json::Value::as_array) {
        for required_name in required.iter().filter_map(serde_json::Value::as_str) {
            if !entries
                .iter()
                .any(|(key, _)| cbor_key_matches(key, required_name))
            {
                return Err(missing_required_error(path, required_name));
            }
        }
    }

    for (key, field_value) in entries {
        let CborValue::Text(field_name) = key else {
            return Err(ToolArgumentValidationError::new(
                path,
                "object keys must be strings",
            ));
        };
        if let Some(field_schema) = properties.and_then(|properties| properties.get(field_name)) {
            validate_json_schema(field_schema, field_value, &child_path(path, field_name))?;
            continue;
        }
        match schema.get("additionalProperties") {
            Some(serde_json::Value::Bool(false)) => {
                return Err(unexpected_property_error(path, field_name));
            }
            Some(additional_schema @ serde_json::Value::Object(_)) => {
                validate_json_schema(
                    additional_schema,
                    field_value,
                    &child_path(path, field_name),
                )?;
            }
            Some(serde_json::Value::Bool(true)) | None => {}
            Some(_) => {}
        }
    }

    Ok(())
}

fn cbor_key_matches(key: &CborValue, expected: &str) -> bool {
    matches!(key, CborValue::Text(text) if text == expected)
}

fn child_path(parent: &str, field: &str) -> String {
    if parent == "$" {
        format!("$.{field}")
    } else {
        format!("{parent}.{field}")
    }
}

fn item_path(parent: &str, index: usize) -> String {
    format!("{parent}[{index}]")
}

fn missing_required_error(path: &str, name: &str) -> ToolArgumentValidationError {
    if path == "$" {
        ToolArgumentValidationError::new(path, format!("missing required argument `{name}`"))
    } else {
        ToolArgumentValidationError::new(path, format!("missing required property `{name}`"))
    }
}

fn unexpected_property_error(path: &str, name: &str) -> ToolArgumentValidationError {
    if path == "$" {
        ToolArgumentValidationError::new(path, format!("unexpected argument `{name}`"))
    } else {
        ToolArgumentValidationError::new(path, format!("unexpected property `{name}`"))
    }
}

fn validate_array_schema(
    schema: &serde_json::Map<String, serde_json::Value>,
    values: &[CborValue],
    path: &str,
) -> Result<(), ToolArgumentValidationError> {
    if let Some(min_items) = schema
        .get("minItems")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        && values.len() < min_items
    {
        return Err(ToolArgumentValidationError::new(
            path,
            format!("must contain at least {min_items} item(s)"),
        ));
    }
    if let Some(max_items) = schema
        .get("maxItems")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        && max_items < values.len()
    {
        return Err(ToolArgumentValidationError::new(
            path,
            format!("must contain at most {max_items} item(s)"),
        ));
    }
    if let Some(item_schema) = schema.get("items") {
        for (idx, item) in values.iter().enumerate() {
            validate_json_schema(item_schema, item, &item_path(path, idx))?;
        }
    }
    Ok(())
}

fn validate_string_schema(
    schema: &serde_json::Map<String, serde_json::Value>,
    text: &str,
    path: &str,
) -> Result<(), ToolArgumentValidationError> {
    let len = text.chars().count();
    if let Some(min_len) = schema
        .get("minLength")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        && len < min_len
    {
        return Err(ToolArgumentValidationError::new(
            path,
            format!("must contain at least {min_len} character(s)"),
        ));
    }
    if let Some(max_len) = schema
        .get("maxLength")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        && max_len < len
    {
        return Err(ToolArgumentValidationError::new(
            path,
            format!("must contain at most {max_len} character(s)"),
        ));
    }
    Ok(())
}

fn validate_number_schema(
    schema: &serde_json::Map<String, serde_json::Value>,
    value: &CborValue,
    path: &str,
) -> Result<(), ToolArgumentValidationError> {
    let Some(number) = cbor_number_as_f64(value) else {
        return Ok(());
    };
    if let Some(minimum) = schema.get("minimum").and_then(serde_json::Value::as_f64)
        && number < minimum
    {
        return Err(ToolArgumentValidationError::new(
            path,
            format!("must be at least {minimum}"),
        ));
    }
    if let Some(maximum) = schema.get("maximum").and_then(serde_json::Value::as_f64)
        && maximum < number
    {
        return Err(ToolArgumentValidationError::new(
            path,
            format!("must be at most {maximum}"),
        ));
    }
    Ok(())
}

fn cbor_number_as_f64(value: &CborValue) -> Option<f64> {
    match value {
        CborValue::Integer(value) => {
            let value: i128 = (*value).into();
            Some(value as f64)
        }
        CborValue::Float(value) => Some(*value),
        _ => None,
    }
}

/// Live tool registration state keyed by connection and tool name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolRegistry {
    providers_by_tool: HashMap<ToolName, Vec<ToolProvider>>,
    tools_by_connection: HashMap<ConnectionId, Vec<ToolName>>,
}

impl ToolRegistry {
    /// Creates an empty tool registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one tool for a live provider connection without a prompt
    /// fragment.
    pub fn register(&mut self, connection_id: &str, tool: ToolSpec) -> RegisterToolReport {
        self.register_with_prompt_fragment(
            connection_id,
            ToolRegister {
                tool,
                prompt_fragment: None,
            },
        )
    }

    /// Registers one tool for a live provider connection, including any prompt
    /// fragment attached to the registration.
    pub fn register_with_prompt_fragment(
        &mut self,
        connection_id: &str,
        registration: ToolRegister,
    ) -> RegisterToolReport {
        let ToolRegister {
            tool,
            prompt_fragment,
        } = registration;
        let tool_name = tool.name.clone();
        let providers = self.providers_by_tool.entry(tool_name.clone()).or_default();

        let existing_provider_ids = providers
            .iter()
            .map(|provider| provider.connection_id.clone())
            .collect::<Vec<_>>();
        let mut report = RegisterToolReport::default();
        if !existing_provider_ids.is_empty() {
            report
                .warnings
                .push(ToolRegistryWarning::DuplicateRegistration {
                    tool_name: tool_name.clone(),
                    existing_provider_ids,
                });
        }

        if let Some(existing_provider) = providers
            .iter_mut()
            .find(|provider| provider.connection_id == connection_id)
        {
            existing_provider.tool = tool;
            existing_provider.prompt_fragment = prompt_fragment;
        } else {
            providers.push(ToolProvider {
                connection_id: connection_id.into(),
                tool,
                prompt_fragment,
            });
        }

        let connection_tools = self
            .tools_by_connection
            .entry(connection_id.into())
            .or_default();
        if !connection_tools.contains(&tool_name) {
            connection_tools.push(tool_name);
        }

        report
    }

    /// Unregisters one tool from one provider connection.
    pub fn unregister(&mut self, connection_id: &str, tool_name: &str) -> bool {
        let mut removed = false;

        if let Some(providers) = self.providers_by_tool.get_mut(tool_name) {
            let initial_len = providers.len();
            providers.retain(|provider| provider.connection_id != connection_id);
            removed = providers.len() != initial_len;
            if providers.is_empty() {
                self.providers_by_tool.remove(tool_name);
            }
        }

        if removed {
            self.remove_tool_from_connection(connection_id, tool_name);
        }

        removed
    }

    /// Unregisters all tools owned by one disconnected provider connection.
    pub fn unregister_connection(&mut self, connection_id: &str) -> Vec<ToolName> {
        let Some(tool_names) = self.tools_by_connection.remove(connection_id) else {
            return Vec::new();
        };

        for tool_name in &tool_names {
            if let Some(providers) = self.providers_by_tool.get_mut(tool_name) {
                providers.retain(|provider| provider.connection_id != connection_id);
                if providers.is_empty() {
                    self.providers_by_tool.remove(tool_name);
                }
            }
        }

        tool_names
    }

    /// Returns all currently live providers for a tool name.
    #[must_use]
    pub fn providers_for(&self, tool_name: &str) -> Vec<ToolProvider> {
        self.providers_by_tool
            .get(tool_name)
            .cloned()
            .unwrap_or_default()
    }

    /// Returns all unique tool names currently registered.
    #[must_use]
    pub fn all_tool_names(&self) -> Vec<&ToolName> {
        self.providers_by_tool.keys().collect()
    }

    /// Returns all unique tool specs, one per tool name (first provider wins).
    #[must_use]
    pub fn all_tools(&self) -> Vec<&ToolSpec> {
        self.all_tool_providers()
            .into_iter()
            .map(|provider| &provider.tool)
            .collect()
    }

    /// Returns all unique tool providers, one per tool name (first provider
    /// wins), sorted by tool name for deterministic prompt and tool assembly.
    #[must_use]
    pub fn all_tool_providers(&self) -> Vec<&ToolProvider> {
        let mut providers: Vec<_> = self
            .providers_by_tool
            .values()
            .filter_map(|providers| providers.first())
            .collect();
        providers.sort_by(|a, b| a.tool.name.as_str().cmp(b.tool.name.as_str()));
        providers
    }

    /// Picks one currently live provider for a tool name.
    #[must_use]
    pub fn resolve_provider(&self, tool_name: &str) -> Option<&ToolProvider> {
        self.providers_by_tool
            .get(tool_name)
            .and_then(|providers| providers.first())
    }

    /// Resolves a `tool.request` to one live provider and builds the
    /// corresponding `tool.started` event.
    ///
    /// Success means the request is accepted and the harness can publish the
    /// started event. Failure means no provider was invoked; the harness
    /// reports that as a rejection event.
    pub fn route_tool_request(
        &self,
        request: ToolRequest,
    ) -> Result<ToolRouteReport, ToolRouteError> {
        let tool_name = request.tool_name.clone();
        let provider_connection_id = self
            .resolve_provider(tool_name.as_str())
            .map(|provider| provider.connection_id.clone())
            .ok_or_else(|| ToolRouteError::NoProvider {
                tool_name: tool_name.clone(),
            })?;

        Ok(ToolRouteReport {
            provider_connection_id,
            invoke: ToolStarted {
                call_id: request.call_id,
                tool_name,
                arguments: request.arguments,
                originator: request.originator,
            },
        })
    }

    fn remove_tool_from_connection(&mut self, connection_id: &str, tool_name: &str) {
        if let Some(tool_names) = self.tools_by_connection.get_mut(connection_id) {
            tool_names.retain(|name| name != tool_name);
            if tool_names.is_empty() {
                self.tools_by_connection.remove(connection_id);
            }
        }
    }
}
