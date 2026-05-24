//! Client-side state for extension-provided slash actions.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use tau_proto::{ActionSchemaPublished, ExtensionInstanceId, ExtensionName};

use crate::locked;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActionOwner {
    extension_name: ExtensionName,
    instance_id: ExtensionInstanceId,
}

#[derive(Clone, Debug)]
struct RootBinding {
    owner: ActionOwner,
    schema: tau_actions::ActionSchema,
    description: String,
}

#[derive(Clone, Debug, Default)]
struct ActionCommandInner {
    schemas: BTreeMap<(String, u64), (ActionOwner, tau_actions::ActionSchema)>,
    roots: BTreeMap<String, RootBinding>,
}

/// Parsed dynamic action invocation ready to send to the harness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActionDispatch {
    /// Extension name from the schema snapshot selected by the UI.
    pub(crate) extension_name: ExtensionName,
    /// Extension instance id from the schema snapshot selected by the UI.
    pub(crate) instance_id: ExtensionInstanceId,
    /// Parsed slash action.
    pub(crate) parsed: tau_actions::ParsedAction,
}

/// Shared action schema snapshot used by the renderer, completer, and input
/// loop.
#[derive(Clone, Debug)]
pub(crate) struct ActionCommandState {
    builtin_roots: Arc<BTreeSet<String>>,
    inner: Arc<Mutex<ActionCommandInner>>,
}

impl ActionCommandState {
    /// Create an empty action-command state, filtering out any dynamic root
    /// that collides with the provided built-in command names.
    pub(crate) fn new(builtin_roots: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let builtin_roots = builtin_roots
            .into_iter()
            .map(|root| root.as_ref().to_owned())
            .collect();
        Self {
            builtin_roots: Arc::new(builtin_roots),
            inner: Arc::new(Mutex::new(ActionCommandInner::default())),
        }
    }

    /// Apply one harness-stamped action schema publication.
    pub(crate) fn apply_schema_published(&self, published: &ActionSchemaPublished) {
        if let Err(error) = published.schema.validate() {
            tracing::warn!(
                target: "tau_cli::actions",
                extension = %published.extension_name,
                instance_id = published.instance_id.get(),
                %error,
                "ignoring invalid published action schema"
            );
            return;
        }

        let mut schema = published.schema.clone();
        schema
            .roots
            .retain(|root| !self.builtin_roots.contains(&root.name));
        let key = Self::owner_key(&published.extension_name, published.instance_id);
        let owner = ActionOwner {
            extension_name: published.extension_name.clone(),
            instance_id: published.instance_id,
        };
        let mut inner = locked(&self.inner);
        if schema.roots.is_empty() {
            inner.schemas.remove(&key);
        } else {
            inner.schemas.insert(key, (owner, schema));
        }
        Self::rebuild_roots(&mut inner);
    }

    /// Remove all action roots published by one extension instance.
    pub(crate) fn remove_extension(
        &self,
        extension_name: &ExtensionName,
        instance_id: ExtensionInstanceId,
    ) {
        let mut inner = locked(&self.inner);
        inner
            .schemas
            .remove(&Self::owner_key(extension_name, instance_id));
        Self::rebuild_roots(&mut inner);
    }

    /// Return true when the line begins with a currently known dynamic action
    /// root.
    pub(crate) fn is_known_action_line(&self, text: &str) -> bool {
        let Some(root) = text.split_whitespace().next() else {
            return false;
        };
        locked(&self.inner).roots.contains_key(root)
    }

    /// Parse a line if it belongs to a currently known dynamic action root.
    pub(crate) fn parse_line(
        &self,
        line: &str,
    ) -> Option<Result<ActionDispatch, tau_actions::ParseError>> {
        let root = line.split_whitespace().next()?;
        let binding = locked(&self.inner).roots.get(root).cloned()?;
        Some(
            binding
                .schema
                .parse_line(line)
                .map(|parsed| ActionDispatch {
                    extension_name: binding.owner.extension_name,
                    instance_id: binding.owner.instance_id,
                    parsed,
                }),
        )
    }

    /// Build root-level completion entries for all active dynamic action roots.
    pub(crate) fn dynamic_slash_commands(&self) -> Vec<tau_cli_term::SlashCommand> {
        locked(&self.inner)
            .roots
            .iter()
            .map(|(root, binding)| {
                tau_cli_term::SlashCommand::new(root.clone(), binding.description.clone())
            })
            .collect()
    }

    fn owner_key(
        extension_name: &ExtensionName,
        instance_id: ExtensionInstanceId,
    ) -> (String, u64) {
        (extension_name.to_string(), instance_id.get())
    }

    fn rebuild_roots(inner: &mut ActionCommandInner) {
        let mut roots = BTreeMap::new();
        for (owner, schema) in inner.schemas.values() {
            for root in &schema.roots {
                roots
                    .entry(root.name.clone())
                    .or_insert_with(|| RootBinding {
                        owner: owner.clone(),
                        schema: tau_actions::ActionSchema {
                            version: schema.version,
                            roots: vec![root.clone()],
                        },
                        description: format!("{} ({})", root.description, owner.extension_name),
                    });
            }
        }
        inner.roots = roots;
    }
}

#[cfg(test)]
mod tests {
    use tau_actions::{ACTION_SCHEMA_VERSION, ActionCommand, ActionSchema};

    use super::*;

    fn schema(root: &str, action_id: &str) -> ActionSchema {
        ActionSchema {
            version: ACTION_SCHEMA_VERSION,
            roots: vec![ActionCommand {
                name: root.to_owned(),
                description: format!("{root} actions"),
                action_id: None,
                args: Vec::new(),
                children: vec![ActionCommand {
                    name: "list".to_owned(),
                    description: "List items".to_owned(),
                    action_id: Some(action_id.to_owned()),
                    args: Vec::new(),
                    children: Vec::new(),
                }],
            }],
        }
    }

    fn published(root: &str, action_id: &str, instance_id: u64) -> ActionSchemaPublished {
        ActionSchemaPublished {
            extension_name: "std-email".into(),
            instance_id: instance_id.into(),
            schema: schema(root, action_id),
        }
    }

    #[test]
    fn parses_known_dynamic_action_line() {
        let state = ActionCommandState::new(["/quit"]);
        state.apply_schema_published(&published("/email", "email.list", 1));

        let dispatch = state
            .parse_line("/email list")
            .expect("known root")
            .expect("valid action");

        assert_eq!(dispatch.extension_name, ExtensionName::from("std-email"));
        assert_eq!(dispatch.instance_id, ExtensionInstanceId::from(1));
        assert_eq!(dispatch.parsed.action_id, "email.list");
    }

    #[test]
    fn ignores_roots_that_collide_with_builtin_commands() {
        let state = ActionCommandState::new(["/quit"]);
        state.apply_schema_published(&published("/quit", "quit.dynamic", 1));

        assert!(!state.is_known_action_line("/quit list"));
        assert!(state.dynamic_slash_commands().is_empty());
    }

    #[test]
    fn removes_schema_for_exited_extension() {
        let state = ActionCommandState::new(["/quit"]);
        state.apply_schema_published(&published("/email", "email.list", 2));

        state.remove_extension(&ExtensionName::from("std-email"), 2.into());

        assert!(state.parse_line("/email list").is_none());
    }
}
