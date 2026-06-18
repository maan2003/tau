//! Harness usage counters.

use tau_proto::TokenUsageStats;

/// Mutable counters and cached usage values scoped to the running harness.
#[derive(Debug, Default)]
pub(crate) struct HarnessUsageState {
    /// Input tokens consumed by the most recent agent response, if the provider
    /// reported it. `None` until the first usage report for the current model.
    pub(crate) context_input_tokens: Option<u64>,
    /// Cached input tokens consumed by the most recent agent response, if the
    /// provider reported them.
    pub(crate) context_cached_tokens: Option<u64>,
    /// Percentage of the selected model's context window currently used. `None`
    /// when the model's context window is unknown.
    pub(crate) context_percent_used: Option<u8>,
    /// Harness token usage totals.
    pub(crate) token_usage: TokenUsageStats,
}
