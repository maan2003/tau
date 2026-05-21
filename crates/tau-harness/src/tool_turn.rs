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
    /// Shared/exclusive mode resolved at enqueue time.
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

    /// True when all in-flight invocations match `predicate`.
    #[cfg(test)]
    pub(crate) fn all_in_flight_modes(
        &self,
        predicate: impl FnMut(&ToolExecutionMode) -> bool,
    ) -> bool {
        self.in_flight_tool_execution_modes
            .values()
            .map(|in_flight| &in_flight.execution_mode)
            .all(predicate)
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
            let compatible = match pending.execution_mode {
                ToolExecutionMode::Shared => {
                    !self.has_exclusive_in_flight_for(&pending.conversation_id)
                }
                ToolExecutionMode::Exclusive => !self.any_in_flight_for(&pending.conversation_id),
            };
            if compatible {
                return Some(idx);
            }
            blocked_convs.insert(&pending.conversation_id);
        }
        None
    }

    fn has_exclusive_in_flight_for(&self, conversation_id: &ConversationId) -> bool {
        self.in_flight_tool_execution_modes
            .values()
            .any(|in_flight| {
                &in_flight.conversation_id == conversation_id
                    && in_flight.foreground_pending
                    && matches!(in_flight.execution_mode, ToolExecutionMode::Exclusive)
            })
    }
}

#[cfg(test)]
mod tests {
    use tau_proto::{BackgroundSupport, CborValue, ToolExecutionMode};

    use super::*;

    fn cid(value: &str) -> ConversationId {
        ConversationId::new(value)
    }

    fn call(id: &str) -> AgentToolCall {
        AgentToolCall {
            id: id.into(),
            name: ToolName::new("tool"),
            tool_type: ToolType::Function,
            arguments: CborValue::Null,
            display: None,
        }
    }

    fn push(
        machine: &mut ToolTurnMachine,
        cid: &ConversationId,
        id: &str,
        mode: ToolExecutionMode,
    ) {
        machine.push(cid.clone(), call(id), mode, BackgroundSupport::Never);
    }

    fn pop_id(machine: &mut ToolTurnMachine) -> Option<String> {
        machine
            .pop_dispatchable(Instant::now())
            .map(|(pending, _)| pending.invocation.id.as_str().to_owned())
    }

    #[test]
    fn shared_calls_from_same_conversation_dispatch_together() {
        let mut machine = ToolTurnMachine::default();
        let conv = cid("conv");
        push(&mut machine, &conv, "a", ToolExecutionMode::Shared);
        push(&mut machine, &conv, "b", ToolExecutionMode::Shared);

        assert_eq!(pop_id(&mut machine).as_deref(), Some("a"));
        assert_eq!(pop_id(&mut machine).as_deref(), Some("b"));
        assert_eq!(machine.in_flight_len(), 2);
    }

    #[test]
    fn exclusive_waits_while_same_conversation_shared_is_in_flight() {
        let mut machine = ToolTurnMachine::default();
        let conv = cid("conv");
        push(&mut machine, &conv, "shared", ToolExecutionMode::Shared);
        push(
            &mut machine,
            &conv,
            "exclusive",
            ToolExecutionMode::Exclusive,
        );

        assert_eq!(pop_id(&mut machine).as_deref(), Some("shared"));
        assert_eq!(pop_id(&mut machine), None);
        assert_eq!(
            machine.pending(0).unwrap().invocation.id.as_str(),
            "exclusive"
        );
    }

    #[test]
    fn shared_behind_blocked_exclusive_is_not_skipped_for_same_conversation() {
        let mut machine = ToolTurnMachine::default();
        let conv = cid("conv");
        push(
            &mut machine,
            &conv,
            "shared-in-flight",
            ToolExecutionMode::Shared,
        );
        push(
            &mut machine,
            &conv,
            "exclusive",
            ToolExecutionMode::Exclusive,
        );
        push(
            &mut machine,
            &conv,
            "shared-behind",
            ToolExecutionMode::Shared,
        );

        assert_eq!(pop_id(&mut machine).as_deref(), Some("shared-in-flight"));
        assert_eq!(pop_id(&mut machine), None);
        assert_eq!(machine.pending_len(), 2);
        assert_eq!(
            machine.pending(0).unwrap().invocation.id.as_str(),
            "exclusive"
        );
        assert_eq!(
            machine.pending(1).unwrap().invocation.id.as_str(),
            "shared-behind"
        );
    }

    #[test]
    fn different_conversations_progress_independently() {
        let mut machine = ToolTurnMachine::default();
        let a = cid("a");
        let b = cid("b");
        push(&mut machine, &a, "a-shared", ToolExecutionMode::Shared);
        push(
            &mut machine,
            &a,
            "a-exclusive",
            ToolExecutionMode::Exclusive,
        );
        push(
            &mut machine,
            &b,
            "b-exclusive",
            ToolExecutionMode::Exclusive,
        );

        assert_eq!(pop_id(&mut machine).as_deref(), Some("a-shared"));
        assert_eq!(pop_id(&mut machine).as_deref(), Some("b-exclusive"));
        assert_eq!(machine.pending_len(), 1);
        assert_eq!(
            machine.pending(0).unwrap().invocation.id.as_str(),
            "a-exclusive"
        );
    }

    #[test]
    fn completion_releases_blocked_queued_calls() {
        let mut machine = ToolTurnMachine::default();
        let conv = cid("conv");
        push(&mut machine, &conv, "shared", ToolExecutionMode::Shared);
        push(
            &mut machine,
            &conv,
            "exclusive",
            ToolExecutionMode::Exclusive,
        );

        assert_eq!(pop_id(&mut machine).as_deref(), Some("shared"));
        assert_eq!(pop_id(&mut machine), None);
        machine.mark_complete(&ToolCallId::from("shared"));
        assert_eq!(pop_id(&mut machine).as_deref(), Some("exclusive"));
    }

    #[test]
    fn conversation_predicates_report_pending_and_in_flight_work() {
        let mut machine = ToolTurnMachine::default();
        let conv = cid("conv");
        let other = cid("other");
        push(&mut machine, &conv, "shared", ToolExecutionMode::Shared);

        assert!(machine.any_pending_for(&conv));
        assert!(!machine.any_pending_for(&other));
        assert!(!machine.any_in_flight_for(&conv));

        assert_eq!(pop_id(&mut machine).as_deref(), Some("shared"));
        assert!(!machine.any_pending_for(&conv));
        assert!(machine.any_in_flight_for(&conv));
        assert!(!machine.any_in_flight_for(&other));
    }

    #[test]
    fn fifo_is_preserved_with_compatible_shared_and_exclusive_calls() {
        let mut machine = ToolTurnMachine::default();
        let conv = cid("conv");
        push(&mut machine, &conv, "shared-a", ToolExecutionMode::Shared);
        push(&mut machine, &conv, "shared-b", ToolExecutionMode::Shared);
        push(
            &mut machine,
            &conv,
            "exclusive",
            ToolExecutionMode::Exclusive,
        );

        assert_eq!(pop_id(&mut machine).as_deref(), Some("shared-a"));
        assert_eq!(pop_id(&mut machine).as_deref(), Some("shared-b"));
        assert_eq!(pop_id(&mut machine), None);
        machine.mark_complete(&ToolCallId::from("shared-a"));
        assert_eq!(pop_id(&mut machine), None);
        machine.mark_complete(&ToolCallId::from("shared-b"));
        assert_eq!(pop_id(&mut machine).as_deref(), Some("exclusive"));
    }

    /// Instant background support asks the harness to close the foreground at
    /// dispatch time while keeping the actual tool call tracked until its real
    /// result arrives.
    #[test]
    fn instant_background_completes_foreground_but_remains_running() {
        let mut machine = ToolTurnMachine::default();
        let conv = cid("conv");
        machine.push(
            conv.clone(),
            call("bg"),
            ToolExecutionMode::Exclusive,
            BackgroundSupport::Instant,
        );

        let (pending, action) = machine.pop_dispatchable(Instant::now()).expect("dispatch");
        assert_eq!(pending.invocation.id.as_str(), "bg");
        assert_eq!(
            action,
            ForegroundAction::Background {
                call_id: "bg".into()
            }
        );
        assert!(!machine.is_backgrounded(&"bg".into()));
        assert!(machine.any_in_flight_for(&conv));
        assert!(machine.mark_backgrounded(&"bg".into()));
        assert!(machine.is_backgrounded(&"bg".into()));
        assert!(!machine.any_in_flight_for(&conv));
        assert_eq!(machine.in_flight_len(), 1);
    }

    /// MinForegroundSeconds uses the dispatch instant as the start time. The
    /// harness event loop can sleep until `next_background_deadline` instead of
    /// polling.
    #[test]
    fn min_foreground_deadline_backgrounds_once_when_due() {
        let mut machine = ToolTurnMachine::default();
        let conv = cid("conv");
        let start = Instant::now();
        machine.push(
            conv,
            call("slow"),
            ToolExecutionMode::Shared,
            BackgroundSupport::MinForegroundSeconds(5),
        );
        let (_, action) = machine.pop_dispatchable(start).expect("dispatch");
        assert_eq!(action, ForegroundAction::None);
        assert_eq!(
            machine.background_due(start + std::time::Duration::from_secs(4)),
            Vec::<ToolCallId>::new()
        );

        assert_eq!(
            machine.background_due(start + std::time::Duration::from_secs(5)),
            vec![ToolCallId::from("slow")]
        );
        assert_eq!(
            machine.background_due(start + std::time::Duration::from_secs(6)),
            Vec::<ToolCallId>::new()
        );
        assert!(machine.is_backgrounded(&"slow".into()));
    }

    /// Never preserves old foreground behavior: no deadline is armed and the
    /// call blocks same-conversation exclusive dispatch until the real result.
    #[test]
    fn never_background_has_no_deadline() {
        let mut machine = ToolTurnMachine::default();
        let conv = cid("conv");
        machine.push(
            conv.clone(),
            call("never"),
            ToolExecutionMode::Exclusive,
            BackgroundSupport::Never,
        );
        machine.push(
            conv,
            call("behind"),
            ToolExecutionMode::Shared,
            BackgroundSupport::Never,
        );
        let (_, action) = machine.pop_dispatchable(Instant::now()).expect("dispatch");
        assert_eq!(action, ForegroundAction::None);
        assert!(machine.next_background_deadline().is_none());
        assert_eq!(pop_id(&mut machine), None);
    }

    /// A late real result removes actual-running state exactly once after the
    /// foreground was already closed by the synthetic background placeholder.
    #[test]
    fn late_background_completion_clears_actual_running_once() {
        let mut machine = ToolTurnMachine::default();
        let conv = cid("conv");
        machine.push(
            conv,
            call("late"),
            ToolExecutionMode::Shared,
            BackgroundSupport::Instant,
        );
        machine.pop_dispatchable(Instant::now()).expect("dispatch");
        assert!(machine.mark_backgrounded(&"late".into()));
        assert!(machine.is_backgrounded(&"late".into()));

        assert_eq!(
            machine.mark_complete(&"late".into()),
            Some(ToolExecutionMode::Shared)
        );
        assert_eq!(machine.mark_complete(&"late".into()), None);
        assert!(!machine.is_backgrounded(&"late".into()));
    }
}
