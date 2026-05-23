//! Pure scheduling state for one agent tool turn.
//!
//! The harness owns side effects (publishing, routing, and follow-up prompts).
//! This module only decides which queued tool invocation can dispatch next and
//! tracks calls that have been selected but not completed yet. Background
//! deadlines are measured from the dispatch instant recorded here, not from the
//! start of the agent turn, so queued calls do not spend their foreground
//! budget before they have actually started.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use tau_proto::{BackgroundSupport, ToolCallId, ToolExecutionMode, ToolName, ToolType};

use crate::conversation::ConversationId;
use crate::harness::AgentToolCall;

/// A tool call emitted by an agent response but not yet completed.
#[derive(Clone, Debug)]
pub(crate) struct PendingToolInvocation {
    /// Conversation that owns the tool call.
    pub(crate) conversation_id: ConversationId,
    /// Tool call payload to route when selected.
    pub(crate) invocation: AgentToolCall,
    /// Execution mode resolved at enqueue time.
    pub(crate) execution_mode: ToolExecutionMode,
    /// Foreground/background support resolved at enqueue time.
    pub(crate) background_support: BackgroundSupport,
}

/// Pure queue and in-flight state for tool dispatch during agent turns.
#[derive(Debug, Default)]
pub(crate) struct ToolTurnMachine {
    /// Tool invocations waiting for dispatch.
    pending_tool_invocations: VecDeque<PendingToolInvocation>,
    /// Tool calls selected for dispatch and still actually running.
    in_flight_tool_execution_modes: HashMap<ToolCallId, InFlightToolInvocation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ForegroundAction {
    /// Nothing should be published to close the foreground yet.
    None,
    /// Publish a synthetic terminal tool result for this call.
    Background { call_id: ToolCallId },
}

#[derive(Clone, Debug)]
struct InFlightToolInvocation {
    conversation_id: ConversationId,
    execution_mode: ToolExecutionMode,
    foreground_pending: bool,
    backgrounded: bool,
    foreground_deadline: Option<Instant>,
}

impl ToolTurnMachine {
    /// Enqueue one tool invocation at the back of the turn queue.
    pub(crate) fn push(
        &mut self,
        conversation_id: ConversationId,
        invocation: AgentToolCall,
        execution_mode: ToolExecutionMode,
        background_support: BackgroundSupport,
    ) {
        self.pending_tool_invocations
            .push_back(PendingToolInvocation {
                conversation_id,
                invocation,
                execution_mode,
                background_support,
            });
    }

    /// Returns the next invocation the scheduler would dispatch, without
    /// removing it or marking it in flight.
    pub(crate) fn next_dispatchable(&self) -> Option<&PendingToolInvocation> {
        let idx = self.next_dispatchable_index()?;
        self.pending_tool_invocations.get(idx)
    }

    /// Select the next dispatchable invocation and mark it in flight.
    pub(crate) fn pop_dispatchable(
        &mut self,
        now: Instant,
    ) -> Option<(PendingToolInvocation, ForegroundAction)> {
        let idx = self.next_dispatchable_index()?;
        let pending = self
            .pending_tool_invocations
            .remove(idx)
            .expect("index just located");
        let action = self.record_in_flight(&pending, now);
        Some((pending, action))
    }

    /// Mark an invocation as in flight without queueing it first.
    #[cfg(test)]
    pub(crate) fn record_in_flight_for_test(
        &mut self,
        conversation_id: ConversationId,
        call_id: ToolCallId,
        execution_mode: ToolExecutionMode,
    ) {
        self.in_flight_tool_execution_modes.insert(
            call_id,
            InFlightToolInvocation {
                conversation_id,
                execution_mode,
                foreground_pending: true,
                backgrounded: false,
                foreground_deadline: None,
            },
        );
    }

    /// Remove a call from the in-flight set after its real result arrives.
    pub(crate) fn mark_complete(&mut self, call_id: &ToolCallId) -> Option<ToolExecutionMode> {
        self.in_flight_tool_execution_modes
            .remove(call_id)
            .map(|in_flight| in_flight.execution_mode)
    }

    /// Roll back an in-flight mark after synchronous dispatch failure.
    pub(crate) fn rollback_dispatch(&mut self, call_id: &ToolCallId) -> Option<ToolExecutionMode> {
        self.mark_complete(call_id)
    }

    /// Mark one running call as completed in the foreground by the synthetic
    /// background placeholder. The real call remains actual-running.
    pub(crate) fn mark_backgrounded(&mut self, call_id: &ToolCallId) -> bool {
        let Some(in_flight) = self.in_flight_tool_execution_modes.get_mut(call_id) else {
            return false;
        };
        if !in_flight.foreground_pending {
            return false;
        }
        in_flight.foreground_pending = false;
        in_flight.backgrounded = true;
        in_flight.foreground_deadline = None;
        true
    }

    /// True when this call has already been completed in the foreground but is
    /// still actually running.
    pub(crate) fn is_backgrounded(&self, call_id: &ToolCallId) -> bool {
        self.in_flight_tool_execution_modes
            .get(call_id)
            .is_some_and(|in_flight| in_flight.backgrounded)
    }

    /// Backgrounded calls still actually running for `conversation_id`.
    pub(crate) fn backgrounded_calls_for(
        &self,
        conversation_id: &ConversationId,
    ) -> Vec<ToolCallId> {
        self.in_flight_tool_execution_modes
            .iter()
            .filter_map(|(call_id, in_flight)| {
                (&in_flight.conversation_id == conversation_id && in_flight.backgrounded)
                    .then_some(call_id.clone())
            })
            .collect()
    }

    /// Return and mark any calls whose foreground deadline has expired.
    pub(crate) fn background_due(&mut self, now: Instant) -> Vec<ToolCallId> {
        let due: Vec<_> = self
            .in_flight_tool_execution_modes
            .iter()
            .filter_map(|(call_id, in_flight)| {
                (in_flight.foreground_pending
                    && in_flight
                        .foreground_deadline
                        .is_some_and(|deadline| deadline <= now))
                .then_some(call_id.clone())
            })
            .collect();
        for call_id in &due {
            self.mark_backgrounded(call_id);
        }
        due
    }

    /// Earliest foreground background deadline that still needs a wakeup.
    pub(crate) fn next_background_deadline(&self) -> Option<Instant> {
        self.in_flight_tool_execution_modes
            .values()
            .filter(|in_flight| in_flight.foreground_pending)
            .filter_map(|in_flight| in_flight.foreground_deadline)
            .min()
    }

    /// Remove all queued invocations for `conversation_id` whose call id is in
    /// `remaining`.
    pub(crate) fn cancel_queued_for(
        &mut self,
        conversation_id: &ConversationId,
        remaining: &HashSet<ToolCallId>,
    ) -> Vec<(ToolCallId, ToolName, ToolType)> {
        let mut queued = Vec::new();
        self.pending_tool_invocations.retain(|pending| {
            let should_cancel = &pending.conversation_id == conversation_id
                && remaining.contains(&pending.invocation.id);
            if should_cancel {
                queued.push((
                    pending.invocation.id.clone(),
                    pending.invocation.name.clone(),
                    pending.invocation.tool_type,
                ));
            }
            !should_cancel
        });
        queued
    }

    /// Remove all queued and in-flight scheduler state.
    pub(crate) fn clear(&mut self) {
        self.pending_tool_invocations.clear();
        self.in_flight_tool_execution_modes.clear();
    }

    /// True when no queued or in-flight tool calls remain.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.pending_tool_invocations.is_empty() && self.in_flight_tool_execution_modes.is_empty()
    }

    /// Number of queued invocations.
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.pending_tool_invocations.len()
    }

    /// Number of in-flight invocations.
    #[cfg(test)]
    pub(crate) fn in_flight_len(&self) -> usize {
        self.in_flight_tool_execution_modes.len()
    }

    /// Execution mode for an in-flight call.
    #[cfg(test)]
    pub(crate) fn in_flight_mode(&self, call_id: &ToolCallId) -> Option<&ToolExecutionMode> {
        self.in_flight_tool_execution_modes
            .get(call_id)
            .map(|in_flight| &in_flight.execution_mode)
    }

    /// Queued invocation by index.
    #[cfg(test)]
    pub(crate) fn pending(&self, idx: usize) -> Option<&PendingToolInvocation> {
        self.pending_tool_invocations.get(idx)
    }

    /// Whether `conversation_id` has queued work.
    #[cfg(test)]
    pub(crate) fn any_pending_for(&self, conversation_id: &ConversationId) -> bool {
        self.pending_tool_invocations
            .iter()
            .any(|pending| &pending.conversation_id == conversation_id)
    }

    /// Whether `conversation_id` has in-flight work.
    #[cfg(test)]
    pub(crate) fn any_in_flight_for(&self, conversation_id: &ConversationId) -> bool {
        self.in_flight_tool_execution_modes
            .values()
            .any(|in_flight| {
                &in_flight.conversation_id == conversation_id && in_flight.foreground_pending
            })
    }

    fn record_in_flight(
        &mut self,
        pending: &PendingToolInvocation,
        now: Instant,
    ) -> ForegroundAction {
        let (foreground_pending, backgrounded, foreground_deadline, action) =
            match pending.background_support {
                BackgroundSupport::Instant => (
                    true,
                    false,
                    None,
                    ForegroundAction::Background {
                        call_id: pending.invocation.id.clone(),
                    },
                ),
                BackgroundSupport::MinForegroundSeconds(seconds) => (
                    true,
                    false,
                    Some(now + std::time::Duration::from_secs(seconds)),
                    ForegroundAction::None,
                ),
                BackgroundSupport::Never => (true, false, None, ForegroundAction::None),
            };
        self.in_flight_tool_execution_modes.insert(
            pending.invocation.id.clone(),
            InFlightToolInvocation {
                conversation_id: pending.conversation_id.clone(),
                execution_mode: pending.execution_mode,
                foreground_pending,
                backgrounded,
                foreground_deadline,
            },
        );
        action
    }

    fn next_dispatchable_index(&self) -> Option<usize> {
        let mut blocked_convs: HashSet<&ConversationId> = HashSet::new();
        for (idx, pending) in self.pending_tool_invocations.iter().enumerate() {
            if blocked_convs.contains(&pending.conversation_id) {
                continue;
            }
            let compatible = !self
                .has_incompatible_in_flight_for(&pending.conversation_id, pending.execution_mode);
            if compatible {
                return Some(idx);
            }
            blocked_convs.insert(&pending.conversation_id);
        }
        None
    }

    fn has_incompatible_in_flight_for(
        &self,
        conversation_id: &ConversationId,
        execution_mode: ToolExecutionMode,
    ) -> bool {
        self.in_flight_tool_execution_modes
            .values()
            .any(|in_flight| {
                &in_flight.conversation_id == conversation_id
                    && !execution_mode.can_overlap_with(in_flight.execution_mode)
            })
    }
}

#[cfg(test)]
mod tests;
