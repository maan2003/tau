//! Registry for extension-provided UI actions.
//!
//! The harness owns routing and pending-invocation privacy; this registry keeps
//! the live schema snapshot and resolves `action.invoke` owner tuples to the
//! extension connection that published them.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use tau_proto::{ActionInvoke, CborValue, ConnectionId, ExtensionInstanceId, ExtensionName};

/// One harness-stamped schema provider currently known to the registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionProviderSchema {
    /// Connection id that published this schema.
    pub connection_id: ConnectionId,
    /// Extension name stamped by the harness.
    pub extension_name: ExtensionName,
    /// Extension instance id stamped by the harness.
    pub instance_id: ExtensionInstanceId,
    /// Validated action schema.
    pub schema: tau_actions::ActionSchema,
}

/// Error returned when registering an invalid action schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionRegistryError {
    message: String,
}

impl ActionRegistryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Human-readable registry failure.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ActionRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ActionRegistryError {}

/// Error returned when an action invocation cannot be routed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionRouteError {
    /// No live extension owns the requested action tuple.
    NoProvider {
        /// Extension name in the request.
        extension_name: ExtensionName,
        /// Extension instance id in the request.
        instance_id: ExtensionInstanceId,
        /// Stable action id in the request.
        action_id: String,
    },
    /// The invocation payload does not match the registered schema.
    InvalidInvocation {
        /// Human-readable validation failure.
        reason: String,
    },
}

impl fmt::Display for ActionRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProvider {
                extension_name,
                instance_id,
                action_id,
            } => write!(
                f,
                "no live provider for action {extension_name}#{instance_id}:{action_id}"
            ),
            Self::InvalidInvocation { reason } => write!(f, "invalid action invocation: {reason}"),
        }
    }
}

impl std::error::Error for ActionRouteError {}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ActionRouteKey {
    extension_name: ExtensionName,
    instance_id: ExtensionInstanceId,
    action_id: String,
}

impl ActionRouteKey {
    fn new(
        extension_name: ExtensionName,
        instance_id: ExtensionInstanceId,
        action_id: String,
    ) -> Self {
        Self {
            extension_name,
            instance_id,
            action_id,
        }
    }

    fn from_invoke(invoke: &ActionInvoke) -> Self {
        Self::new(
            invoke.extension_name.clone(),
            invoke.instance_id,
            invoke.action_id.clone(),
        )
    }
}

/// Live extension action schemas and route table.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ActionRegistry {
    schemas_by_connection: HashMap<ConnectionId, ActionProviderSchema>,
    routes: HashMap<ActionRouteKey, ConnectionId>,
}

impl ActionRegistry {
    /// Create an empty action registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or replace the schema for one extension connection.
    pub fn register_schema(
        &mut self,
        connection_id: &str,
        extension_name: ExtensionName,
        instance_id: ExtensionInstanceId,
        schema: tau_actions::ActionSchema,
    ) -> Result<(), ActionRegistryError> {
        let action_ids = schema
            .executable_action_ids()
            .map_err(|error| ActionRegistryError::new(format!("invalid action schema: {error}")))?;
        let connection_id = ConnectionId::from(connection_id);
        let route_keys = action_ids
            .into_iter()
            .map(|action_id| ActionRouteKey::new(extension_name.clone(), instance_id, action_id))
            .collect::<Vec<_>>();
        for key in &route_keys {
            if let Some(owner) = self.routes.get(key)
                && owner != &connection_id
            {
                return Err(ActionRegistryError::new(format!(
                    "action route collision for {}#{}:{} already owned by {}",
                    key.extension_name, key.instance_id, key.action_id, owner
                )));
            }
        }
        self.unregister_connection(connection_id.as_str());

        for key in route_keys {
            self.routes.insert(key, connection_id.clone());
        }
        self.schemas_by_connection.insert(
            connection_id.clone(),
            ActionProviderSchema {
                connection_id,
                extension_name,
                instance_id,
                schema,
            },
        );
        Ok(())
    }

    /// Remove any schema and actions owned by one connection.
    pub fn unregister_connection(&mut self, connection_id: &str) -> Option<ActionProviderSchema> {
        let connection_id = ConnectionId::from(connection_id);
        let removed = self.schemas_by_connection.remove(&connection_id)?;
        self.routes.retain(|_, provider| provider != &connection_id);
        Some(removed)
    }

    /// Resolve an action invocation to the owning extension connection.
    pub fn route_action_invoke(
        &self,
        invoke: &ActionInvoke,
    ) -> Result<ConnectionId, ActionRouteError> {
        let key = ActionRouteKey::from_invoke(invoke);
        let provider =
            self.routes
                .get(&key)
                .cloned()
                .ok_or_else(|| ActionRouteError::NoProvider {
                    extension_name: invoke.extension_name.clone(),
                    instance_id: invoke.instance_id,
                    action_id: invoke.action_id.clone(),
                })?;
        let schema = self.schemas_by_connection.get(&provider).ok_or_else(|| {
            ActionRouteError::InvalidInvocation {
                reason: format!("missing schema for provider {provider}"),
            }
        })?;
        validate_invoke_against_schema(invoke, &schema.schema)?;
        Ok(provider)
    }

    /// Return current schemas in deterministic order for late-join replay.
    #[must_use]
    pub fn published_schemas(&self) -> Vec<ActionProviderSchema> {
        let mut by_key = BTreeMap::new();
        for schema in self.schemas_by_connection.values() {
            by_key.insert(
                (
                    schema.extension_name.to_string(),
                    schema.instance_id.get(),
                    schema.connection_id.to_string(),
                ),
                schema.clone(),
            );
        }
        by_key.into_values().collect()
    }

    /// Return true when the connection currently has a published schema.
    #[must_use]
    pub fn has_schema_for_connection(&self, connection_id: &str) -> bool {
        self.schemas_by_connection.contains_key(connection_id)
    }
}

fn validate_invoke_against_schema(
    invoke: &ActionInvoke,
    schema: &tau_actions::ActionSchema,
) -> Result<(), ActionRouteError> {
    let parsed = schema.parse_line(&invoke.raw_line).map_err(|error| {
        ActionRouteError::InvalidInvocation {
            reason: error.to_string(),
        }
    })?;
    if parsed.action_id != invoke.action_id {
        return Err(ActionRouteError::InvalidInvocation {
            reason: format!(
                "raw_line selected action `{}` but invoke requested `{}`",
                parsed.action_id, invoke.action_id
            ),
        });
    }
    if parsed.argv != invoke.argv {
        return Err(ActionRouteError::InvalidInvocation {
            reason: "argv does not match raw_line/schema parse".to_owned(),
        });
    }
    let expected_arguments = parsed_action_arguments(&parsed.named_args);
    if expected_arguments != invoke.arguments {
        return Err(ActionRouteError::InvalidInvocation {
            reason: "typed arguments do not match raw_line/schema parse".to_owned(),
        });
    }
    Ok(())
}

fn parsed_action_arguments(
    args: &std::collections::BTreeMap<String, tau_actions::ParsedArgValue>,
) -> CborValue {
    CborValue::Map(
        args.iter()
            .map(|(name, value)| {
                let value = match value {
                    tau_actions::ParsedArgValue::String(value) => CborValue::Text(value.clone()),
                    tau_actions::ParsedArgValue::Integer(value) => {
                        CborValue::Integer((*value).into())
                    }
                };
                (CborValue::Text(name.clone()), value)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use tau_actions::{
        ACTION_SCHEMA_VERSION, ActionArg, ActionArgKind, ActionCommand, ActionSchema,
    };
    use tau_proto::{ActionInvocationId, CborValue, SessionId};

    use super::*;

    fn schema(action_id: &str) -> ActionSchema {
        ActionSchema {
            version: ACTION_SCHEMA_VERSION,
            roots: vec![ActionCommand {
                name: "/email".to_owned(),
                description: "Email approvals".to_owned(),
                action_id: None,
                args: Vec::new(),
                children: vec![ActionCommand {
                    name: "out".to_owned(),
                    description: "Outgoing".to_owned(),
                    action_id: None,
                    args: Vec::new(),
                    children: vec![ActionCommand {
                        name: "approve".to_owned(),
                        description: "Approve".to_owned(),
                        action_id: Some(action_id.to_owned()),
                        args: vec![ActionArg {
                            name: "id".to_owned(),
                            description: "Approval id".to_owned(),
                            required: true,
                            kind: ActionArgKind::String,
                        }],
                        children: Vec::new(),
                    }],
                }],
            }],
        }
    }

    fn invoke(action_id: &str, instance_id: u64) -> ActionInvoke {
        ActionInvoke {
            invocation_id: ActionInvocationId::from("act-1"),
            session_id: SessionId::from("s1"),
            extension_name: ExtensionName::from("std-email"),
            instance_id: ExtensionInstanceId::from(instance_id),
            action_id: action_id.to_owned(),
            raw_line: "/email out approve 123".to_owned(),
            argv: vec!["123".to_owned()],
            arguments: CborValue::Map(vec![(
                CborValue::Text("id".to_owned()),
                CborValue::Text("123".to_owned()),
            )]),
        }
    }

    #[test]
    fn register_schema_routes_invocations_to_owner() {
        let mut registry = ActionRegistry::new();
        registry
            .register_schema(
                "conn-a",
                "std-email".into(),
                1.into(),
                schema("email.out.approve"),
            )
            .expect("schema should register");

        assert_eq!(
            registry.route_action_invoke(&invoke("email.out.approve", 1)),
            Ok(ConnectionId::from("conn-a"))
        );
    }

    #[test]
    fn replacing_schema_removes_old_action_ids_for_connection() {
        let mut registry = ActionRegistry::new();
        registry
            .register_schema("conn-a", "std-email".into(), 1.into(), schema("email.old"))
            .expect("old schema should register");
        registry
            .register_schema("conn-a", "std-email".into(), 1.into(), schema("email.new"))
            .expect("new schema should register");

        assert!(
            registry
                .route_action_invoke(&invoke("email.old", 1))
                .is_err()
        );
        assert_eq!(
            registry.route_action_invoke(&invoke("email.new", 1)),
            Ok(ConnectionId::from("conn-a"))
        );
    }

    #[test]
    fn invocation_payload_must_match_owner_schema_parse() {
        let mut registry = ActionRegistry::new();
        registry
            .register_schema(
                "conn-a",
                "std-email".into(),
                1.into(),
                schema("email.out.approve"),
            )
            .expect("schema should register");

        let mut mismatched = invoke("email.out.approve", 1);
        mismatched.raw_line = "/email out approve".to_owned();
        assert!(matches!(
            registry.route_action_invoke(&mismatched),
            Err(ActionRouteError::InvalidInvocation { .. })
        ));

        let mut mismatched = invoke("email.out.approve", 1);
        mismatched.arguments = CborValue::Map(Vec::new());
        assert!(matches!(
            registry.route_action_invoke(&mismatched),
            Err(ActionRouteError::InvalidInvocation { .. })
        ));
    }

    #[test]
    fn duplicate_owner_action_routes_are_rejected_without_replacing_existing_owner() {
        let mut registry = ActionRegistry::new();
        registry
            .register_schema(
                "conn-a",
                "std-email".into(),
                1.into(),
                schema("email.out.approve"),
            )
            .expect("first schema should register");

        let error = registry
            .register_schema(
                "conn-b",
                "std-email".into(),
                1.into(),
                schema("email.out.approve"),
            )
            .expect_err("second owner must not steal route");
        assert!(error.message().contains("action route collision"));
        assert_eq!(
            registry.route_action_invoke(&invoke("email.out.approve", 1)),
            Ok(ConnectionId::from("conn-a"))
        );
    }

    #[test]
    fn disconnect_unregisters_actions() {
        let mut registry = ActionRegistry::new();
        registry
            .register_schema(
                "conn-a",
                "std-email".into(),
                1.into(),
                schema("email.out.approve"),
            )
            .expect("schema should register");

        assert!(registry.unregister_connection("conn-a").is_some());
        assert!(
            registry
                .route_action_invoke(&invoke("email.out.approve", 1))
                .is_err()
        );
    }

    #[test]
    fn invalid_schema_is_rejected_without_replacing_previous_schema() {
        let mut registry = ActionRegistry::new();
        registry
            .register_schema(
                "conn-a",
                "std-email".into(),
                1.into(),
                schema("email.out.approve"),
            )
            .expect("schema should register");
        let invalid = ActionSchema {
            version: ACTION_SCHEMA_VERSION,
            roots: vec![ActionCommand {
                name: "email".to_owned(),
                description: "missing slash".to_owned(),
                action_id: Some("email.invalid".to_owned()),
                args: Vec::new(),
                children: Vec::new(),
            }],
        };

        assert!(
            registry
                .register_schema("conn-a", "std-email".into(), 1.into(), invalid)
                .is_err()
        );
        assert_eq!(
            registry.route_action_invoke(&invoke("email.out.approve", 1)),
            Ok(ConnectionId::from("conn-a"))
        );
    }
}
