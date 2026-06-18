//! Core event bus, routing, and connection abstractions.
//!
//! This crate keeps transport details outside the routing layer. Stdio, Unix
//! socket, and in-memory test clients can all plug into the same bus through a
//! small [`ConnectionSink`] interface.

mod action_registry;
mod agent_store;
mod agent_tree;
mod bus;
mod connection;
mod memory;
mod policy;
mod tool_registry;

#[cfg(test)]
mod tests;

pub use action_registry::{
    ActionProviderSchema, ActionRegistry, ActionRegistryError, ActionRouteError,
};
pub use agent_store::{
    AgentAppendOutcome, AgentStore, AgentStoreError, agent_is_locked, list_agent_metas,
};
pub use agent_tree::{
    AgentEntry, AgentEventParent, AgentEventValidationError, AgentMessageDirection, AgentMeta,
    AgentMetadataEntry, AgentNode, AgentTree, BackgroundToolCallState, BackgroundToolCompletion,
    BackgroundToolPlaceholder, NodeId, PersistedAgentEvent, PersistedAgentEventSeq,
};
pub use bus::EventBus;
pub use connection::{
    AllowAll, Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError,
    ConnectionSink, DeliveryFailure, RouteError, RouteReport, RoutedFrame, VisibilityFilter,
};
pub use memory::{MemoryInbox, memory_connection};
pub use policy::{
    DefaultSubscriptionPolicy, PolicyStore, PolicyStoreError, SubscriptionApproval,
    SubscriptionPolicy, SubscriptionPolicyError,
};
pub use tool_registry::{
    RegisterToolReport, ToolArgumentValidationError, ToolProvider, ToolProviderKind, ToolRegistry,
    ToolRegistryWarning, ToolRouteError, ToolRouteReport, ToolRouteTarget, validate_tool_arguments,
};
