//! Conversation prompt-queue dispatch.
//!
//! Each conversation owns a per-conversation `pending_prompts` queue.
//! The harness has no global agent slot — the agent extension serializes
//! its own consumption of `SessionPromptCreated` from the event log —
//! so the dispatch logic here just drains one prompt per *runnable*
//! conversation (Idle turn state, non-empty queue) and lets the agent
//! interleave them on its side.
//!
//! [`Harness::dispatch_user_prompt`] is the direct entry point for
//! interactive submission on the default conversation;
//! [`Harness::dispatch_prompt_for_conversation`] is the shared
//! per-conversation primitive (also used by side queries spawned via
//! `StartAgentRequest`). [`Harness::try_advance_queue`] is the
//! react-to-state-change drain that picks the next runnable
//! conversation and dispatches one prompt from its queue.
//!
//! [`Harness::dispatch_blocked_for`] is the predicate the rest of the
//! harness uses to decide whether to dispatch immediately or queue.

use tau_proto::{Event, SessionId};

use crate::conversation::{ConversationId, ConversationTurnState, PendingPrompt};
use crate::error::HarnessError;
use crate::harness::Harness;

impl Harness {
    pub(crate) fn dispatch_user_prompt(
        &mut self,
        session_id: SessionId,
        text: String,
    ) -> Result<(), HarnessError> {
        debug_assert_eq!(
            self.conversations[&self.default_conversation_id].session_id, session_id,
            "dispatch_user_prompt only valid for the default conversation",
        );
        let cid = self.default_conversation_id.clone();
        if self.maybe_start_auto_compaction_for_user_prompt(&cid, &text) {
            return Ok(());
        }
        self.dispatch_prompt_for_conversation(&cid, PendingPrompt::user(text))
    }

    /// Publish one pending prompt as a `UiPromptSubmitted` event on `cid`'s
    /// branch without dispatching an agent prompt yet.
    ///
    /// Callers that publish additional prompt-bearing events in the same batch
    /// can use this helper and then call
    /// [`Self::dispatch_prompt_after_publish_idle`] once the full batch has
    /// been queued. That keeps interception from sending the agent a prompt
    /// that only contains the first committed user-message event.
    pub(crate) fn publish_pending_prompt_for_conversation(
        &mut self,
        cid: &ConversationId,
        prompt: impl Into<PendingPrompt>,
    ) -> Result<(), HarnessError> {
        let prompt = prompt.into();
        let (session_id, originator) = match self.conversations.get(cid) {
            Some(c) => (c.session_id.clone(), c.originator.clone()),
            None => {
                return Err(HarnessError::Participant(format!(
                    "publish_pending_prompt_for_conversation: unknown conversation `{cid}`"
                )));
            }
        };
        self.publish_for_conversation(
            cid,
            Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
                session_id,
                text: prompt.text,
                target_agent_id: None,
                message_class: prompt.message_class,
                originator,
                ctx_id: None,
            }),
        );
        Ok(())
    }

    /// Dispatches one prompt for `cid`: publishes the
    /// `UiPromptSubmitted` event (head-bounced via
    /// `publish_for_conversation` so it lands on the conversation's
    /// branch), enters `AgentThinking`, and asks the agent for a
    /// completion.
    ///
    /// Used for both interactive user prompts on the default
    /// conversation and side-query prompts spawned by extensions.
    pub(crate) fn dispatch_prompt_for_conversation(
        &mut self,
        cid: &ConversationId,
        prompt: impl Into<PendingPrompt>,
    ) -> Result<(), HarnessError> {
        let prompt = prompt.into();
        if !prompt.is_internal() {
            let restore_prompts = self.take_pending_restore_prompts_for_user_prompt(cid);
            if !restore_prompts.is_empty() {
                for restore_prompt in restore_prompts {
                    self.publish_pending_prompt_for_conversation(cid, restore_prompt)?;
                }
                self.publish_pending_prompt_for_conversation(cid, prompt)?;
                self.dispatch_prompt_after_publish_idle(cid);
                return Ok(());
            }
        }

        self.publish_pending_prompt_for_conversation(cid, prompt)?;
        // If the publish parked in interception (or queued behind one
        // that is), defer the agent dispatch until this user-prompt
        // event actually commits. If it committed inline, the helper
        // dispatches now: the SessionTree already reflects the new
        // user message, so the message list assembled inside
        // `send_prompt_to_agent_for` will include it.
        self.dispatch_prompt_after_user_message_publish(cid);
        Ok(())
    }

    /// Drains every runnable conversation's pending prompt queue.
    ///
    /// There is no global agent slot — the agent extension serializes
    /// its own consumption of `SessionPromptCreated`. The harness emits
    /// one prompt per runnable conversation (Idle turn state, non-empty
    /// queue) and routes responses back via `prompt_conversations`.
    ///
    /// Session initialization still happens before prompt dispatch, so
    /// a fresh `chat-*` session can discover AGENTS.md and skills before
    /// the agent sees the first user message.
    pub(crate) fn try_advance_queue(&mut self) {
        if !self.turn_state.is_idle()
            || !self.extensions_all_ready()
            || self.selected_model.is_none()
        {
            return;
        }

        while let Some(cid) = self.next_runnable_conversation() {
            let session_id = self
                .conversations
                .get(&cid)
                .map(|c| c.session_id.clone())
                .expect("runnable conversation exists");

            if !self.session_initialized(&session_id) {
                // Reachable only if the bound session somehow lost its
                // `initialized_sessions` entry; treat as a re-init.
                // Init is global, so stop draining until it completes.
                self.start_session_init(session_id, tau_proto::SessionStartReason::Initial);
                return;
            }

            let prompt = self
                .conversations
                .get_mut(&cid)
                .and_then(|c| c.pending_prompts.pop_front())
                .expect("runnable conversation has a prompt");
            if let Err(error) = self.dispatch_prompt_for_conversation(&cid, prompt) {
                self.emit_info(&format!("failed to dispatch queued prompt: {error}"));
                // Reset the conversation so it doesn't wedge as
                // AgentThinking with no in-flight prompt.
                if let Some(conv) = self.conversations.get_mut(&cid) {
                    conv.in_flight_prompt = None;
                    conv.turn_state = ConversationTurnState::Idle;
                }
            }
        }
    }

    pub(crate) fn next_runnable_conversation(&self) -> Option<ConversationId> {
        self.conversations
            .iter()
            .find(|(cid, conv)| {
                !conv.pending_prompts.is_empty()
                    && matches!(conv.turn_state, ConversationTurnState::Idle)
                    && !self.has_deferred_prompt_dispatch_for(cid)
            })
            .map(|(cid, _)| cid.clone())
    }

    /// True when a fresh prompt for `cid` should *not* be sent
    /// immediately. Two layers of gating:
    /// - global: selected role has no resolved model, harness mid-init,
    ///   extensions not yet `Ready`;
    /// - per-conversation: that conversation already has a prompt in flight, is
    ///   waiting on tool results, or has a latent dispatch parked behind
    ///   interception.
    pub(crate) fn dispatch_blocked_for(&self, cid: &ConversationId) -> bool {
        if self.selected_model.is_none()
            || !self.turn_state.is_idle()
            || !self.extensions_all_ready()
        {
            return true;
        }
        match self.conversations.get(cid) {
            Some(conv) => {
                !matches!(conv.turn_state, ConversationTurnState::Idle)
                    || self.has_deferred_prompt_dispatch_for(cid)
            }
            None => true,
        }
    }
}
