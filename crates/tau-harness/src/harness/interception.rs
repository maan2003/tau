//! Event-emission interception subsystem.
//!
//! Owns the [`InterceptorRegistry`] (exact + prefix selectors keyed by
//! `(priority, connection_id)`), the [`PendingIntercept`] / [`DeferredPublish`]
//! queue state, and the methods that drive the interception chain.
//!
//! Flow: a publish enters via [`Harness::enqueue_publish`]. If no intercept
//! is in flight, [`Harness::dispatch_publish_step`] consults the registry —
//! either dispatching an `InterceptRequest` and parking the publish in
//! `pending_intercept`, or falling through to `commit_event`. While a
//! publish is parked, further publishes queue onto `deferred_publishes` so
//! the log order matches the original publish order.
//!
//! Replies and disconnects feed back through
//! [`Harness::handle_intercept_reply`]
//! / [`Harness::fail_pending_intercept_for_disconnect`], which advance the
//! chain and then drain the deferred queue.

use std::collections::{BTreeMap, BTreeSet};

use tau_proto::{
    Event, EventName, EventSelector, ExtensionName, Frame, InterceptAction, InterceptReply,
    InterceptRequest, InterceptionPriority, Message, SessionId,
};

use crate::conversation::ConversationId;
use crate::harness::Harness;

/// Snapshot of a publish that's currently waiting on an interceptor's
/// reply. The harness stops draining further publishes while one of
/// these is alive so the persisted log order matches publish order.
pub(crate) struct PendingIntercept {
    /// Connection that owes us an [`InterceptReply`].
    pub(crate) conn_id: String,
    /// Event sent in the [`InterceptRequest`]. Returned to the chain
    /// if the reply is `Pass(None)`, replaced if `Pass(Some(_))`.
    pub(crate) event: Event,
    /// Whether the original publisher requested transient delivery.
    /// Carried so the eventual commit honours the call site's intent.
    pub(crate) transient: bool,
    /// Original source connection id from the publish call (for log
    /// persistence + bus broadcast).
    pub(crate) source: Option<String>,
    /// If `true`, an interceptor returning `Drop` is overridden:
    /// `tracing::warn!` and continue with the original event.
    pub(crate) must_pass: bool,
    /// Conversation that originated this publish, if any. When the
    /// event eventually commits, the harness syncs this
    /// conversation's `head` to the post-fold `tree.head()`. Set
    /// only by `publish_for_conversation*`; `publish_event` leaves
    /// it `None`.
    pub(crate) sync_head_for: Option<ConversationHeadSync>,
    /// Cursor for the next interceptor lookup *after* this reply
    /// resolves. Set to the registration we just dispatched to, so
    /// the chain advances strictly past it.
    pub(crate) cursor: (InterceptionPriority, String),
}

/// A publish that arrived while another publish was in interception
/// limbo. Replayed through the normal entry point once the in-flight
/// interception resolves.
pub(crate) struct DeferredPublish {
    pub(crate) source: Option<String>,
    pub(crate) event: Event,
    pub(crate) transient: bool,
    pub(crate) must_pass: bool,
    pub(crate) sync_head_for: Option<ConversationHeadSync>,
}

/// Carried on a publish so that, once the event commits and the
/// `SessionTree` fold advances `tree.head()`, the harness can sync
/// the originating conversation's cached `head` to the new node and
/// persist the event to the originating session even if call-level
/// tracking has been cleared while the publish was deferred.
/// Replaces the old "publish then read `tree.head()`" idiom which
/// breaks when an interceptor parks the publish.
#[derive(Clone)]
pub(crate) struct ConversationHeadSync {
    pub(crate) cid: ConversationId,
    pub(crate) session_id: SessionId,
}

/// Event types where a `Drop` reply from an interceptor is
/// overridden into `Pass(None)` with a `tracing::warn!`.
///
/// These events carry state changes the harness can't reasonably
/// continue without — silently dropping a `UiPromptSubmitted`, for
/// example, would leave the UI staring at a half-typed prompt while
/// the harness believes nothing happened. Interceptors that try to
/// drop one of these are almost certainly buggy.
const MUST_PASS_BY_DEFAULT: &[EventName] = &[
    // User-message-bearing events: dropping any of these would
    // make the user's input vanish silently while the harness
    // believes the prompt was delivered.
    EventName::UI_PROMPT_SUBMITTED,
    EventName::SESSION_USER_MESSAGE_INJECTED,
    EventName::SESSION_PROMPT_STEERED,
    // Durable compaction state: once the harness has accepted a provider
    // compaction result, dropping this event would make the UI report
    // success while the next prompt still replays the un-compacted branch.
    EventName::SESSION_COMPACTED,
    // Agent request life-cycle: the agent extension consumes normal
    // `SessionPromptCreated` turns and `SessionCompactionRequested`
    // requests to know when to talk to the LLM. Dropping either wedges
    // the conversation.
    EventName::SESSION_COMPACTION_REQUESTED,
    EventName::SESSION_PROMPT_CREATED,
    // Agent response: dropping this would wedge `c.head` /
    // `prompt_conversations` bookkeeping and the conversation
    // would never advance.
    EventName::PROVIDER_RESPONSE_FINISHED,
    // Tool round-trip closure: a missing `tool.result`/`tool.error`
    // for a tool that was actually invoked leaves the agent waiting
    // forever.
    EventName::TOOL_RESULT,
    EventName::TOOL_ERROR,
];

#[derive(Clone, Debug, Eq, PartialEq)]
struct InterceptorRegistration {
    priority: InterceptionPriority,
    component_name: ExtensionName,
    connection_id: tau_proto::ConnectionId,
}

impl Ord for InterceptorRegistration {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| {
                self.component_name
                    .as_str()
                    .cmp(other.component_name.as_str())
            })
            .then_with(|| {
                self.connection_id
                    .as_str()
                    .cmp(other.connection_id.as_str())
            })
    }
}

impl PartialOrd for InterceptorRegistration {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
pub(crate) struct InterceptorRegistry {
    exact: BTreeMap<tau_proto::EventName, BTreeSet<InterceptorRegistration>>,
    prefix: BTreeMap<String, BTreeSet<InterceptorRegistration>>,
}

impl InterceptorRegistry {
    pub(crate) fn replace_for_connection(
        &mut self,
        connection_id: &str,
        component_name: ExtensionName,
        selectors: Vec<EventSelector>,
        priority: InterceptionPriority,
    ) {
        self.remove_connection(connection_id);
        let registration = InterceptorRegistration {
            priority,
            component_name,
            connection_id: connection_id.into(),
        };
        for selector in selectors {
            match selector {
                EventSelector::Exact(name) => {
                    self.exact
                        .entry(name)
                        .or_default()
                        .insert(registration.clone());
                }
                EventSelector::Prefix(prefix) => {
                    self.prefix
                        .entry(prefix)
                        .or_default()
                        .insert(registration.clone());
                }
            }
        }
    }

    pub(crate) fn remove_connection(&mut self, connection_id: &str) {
        for registrations in self.exact.values_mut() {
            registrations.retain(|r| r.connection_id.as_str() != connection_id);
        }
        self.exact
            .retain(|_, registrations| !registrations.is_empty());
        for registrations in self.prefix.values_mut() {
            registrations.retain(|r| r.connection_id.as_str() != connection_id);
        }
        self.prefix
            .retain(|_, registrations| !registrations.is_empty());
    }

    fn next_for(
        &self,
        event: &Event,
        cursor: Option<(InterceptionPriority, &str)>,
    ) -> Option<InterceptorRegistration> {
        let name = event.name();
        if let Some(next) = self.next_in_set(self.exact.get(&name), cursor) {
            return Some(next);
        }
        self.prefix
            .iter()
            .filter(|(prefix, _)| name.matches_prefix(prefix))
            .filter_map(|(_, registrations)| self.next_in_set(Some(registrations), cursor))
            .min()
    }

    fn next_in_set(
        &self,
        registrations: Option<&BTreeSet<InterceptorRegistration>>,
        cursor: Option<(InterceptionPriority, &str)>,
    ) -> Option<InterceptorRegistration> {
        registrations?
            .iter()
            .find(|registration| {
                cursor.is_none_or(|(priority, connection_id)| {
                    priority < registration.priority
                        || (priority == registration.priority
                            && connection_id < registration.connection_id.as_str())
                })
            })
            .cloned()
    }
}

impl Harness {
    /// Entry point for any publish call. Defers if interception is
    /// in flight; otherwise drives the publish through the
    /// interception chain and into the bus.
    pub(crate) fn enqueue_publish(
        &mut self,
        source: Option<&str>,
        event: Event,
        transient: bool,
        must_pass: bool,
        sync_head_for: Option<ConversationHeadSync>,
    ) {
        if self.pending_intercept.is_some() {
            self.deferred_publishes.push_back(DeferredPublish {
                source: source.map(str::to_owned),
                event,
                transient,
                must_pass,
                sync_head_for,
            });
            return;
        }
        self.dispatch_publish_step(
            source.map(str::to_owned),
            event,
            transient,
            must_pass,
            sync_head_for,
            None,
        );
    }

    /// One step through the interception chain for a single publish.
    ///
    /// `cursor` is `None` on the first dispatch and `Some((priority,
    /// connection_id))` on subsequent steps so the lookup advances
    /// strictly past the interceptor that just replied. If a matching
    /// interceptor is found, an [`InterceptRequest`] is sent and the
    /// publish parks in `pending_intercept` waiting for its reply.
    /// If no further interceptor matches, the event commits.
    fn dispatch_publish_step(
        &mut self,
        source: Option<String>,
        event: Event,
        transient: bool,
        must_pass: bool,
        sync_head_for: Option<ConversationHeadSync>,
        cursor: Option<(InterceptionPriority, &str)>,
    ) {
        if let Some(interceptor) = self.interceptors.next_for(&event, cursor) {
            tracing::debug!(
                target: "tau_harness::interception",
                event = %event.name(),
                priority = interceptor.priority.get(),
                component = %interceptor.component_name,
                connection_id = %interceptor.connection_id,
                "intercepting event emission"
            );
            let conn_id = interceptor.connection_id.as_str().to_owned();
            let _ = self.bus.send_to(
                &conn_id,
                None,
                Frame::Message(Message::InterceptRequest(InterceptRequest {
                    event: Box::new(event.clone()),
                    transient,
                })),
            );
            self.pending_intercept = Some(PendingIntercept {
                conn_id: conn_id.clone(),
                event,
                transient,
                source,
                must_pass,
                sync_head_for,
                cursor: (interceptor.priority, conn_id),
            });
            return;
        }
        self.commit_event(source.as_deref(), event, transient, sync_head_for);
    }

    /// Resolve a parked interception with the extension's reply.
    /// Advances the chain (next interceptor, or commit), then drains
    /// any publishes that arrived while we were waiting.
    pub(crate) fn handle_intercept_reply(&mut self, conn_id: &str, reply: InterceptReply) {
        let Some(pending) = self.pending_intercept.take() else {
            tracing::warn!(
                target: "tau_harness::interception",
                connection_id = conn_id,
                "InterceptReply received without a pending intercept; ignoring",
            );
            return;
        };
        if pending.conn_id != conn_id {
            tracing::warn!(
                target: "tau_harness::interception",
                connection_id = conn_id,
                expected = %pending.conn_id,
                "InterceptReply from unexpected connection; ignoring and \
                 continuing to wait",
            );
            // Restore — we're still waiting on the original responder.
            self.pending_intercept = Some(pending);
            return;
        }
        self.advance_pending_intercept(pending, reply.action);
        self.drain_deferred_publishes();
    }

    /// Resolve a pending intercept whose responder disconnected.
    /// Defaults to `Pass(None)` so the original event still flows —
    /// extensions cannot wedge the harness by going away mid-reply.
    pub(crate) fn fail_pending_intercept_for_disconnect(&mut self, conn_id: &str) {
        let Some(pending) = self.pending_intercept.take() else {
            return;
        };
        if pending.conn_id != conn_id {
            self.pending_intercept = Some(pending);
            return;
        }
        tracing::warn!(
            target: "tau_harness::interception",
            connection_id = conn_id,
            "interceptor disconnected mid-reply; treating as Pass(None)",
        );
        self.advance_pending_intercept(pending, InterceptAction::Pass(None));
        self.drain_deferred_publishes();
    }

    /// Apply an [`InterceptAction`] to a pending intercept and drive
    /// the next chain step (or commit, or drop).
    fn advance_pending_intercept(&mut self, pending: PendingIntercept, action: InterceptAction) {
        let PendingIntercept {
            conn_id: _,
            event: original_event,
            transient,
            source,
            must_pass,
            sync_head_for,
            cursor,
        } = pending;

        let event_name = original_event.name();
        let next_event = match action {
            InterceptAction::Pass(None) => Some(original_event),
            InterceptAction::Pass(Some(boxed)) => {
                let new_event = *boxed;
                if new_event.name() != event_name {
                    tracing::warn!(
                        target: "tau_harness::interception",
                        original = %event_name,
                        replacement = %new_event.name(),
                        "interceptor returned a different event type; \
                         falling back to the original",
                    );
                    Some(original_event)
                } else {
                    Some(new_event)
                }
            }
            InterceptAction::Drop => {
                let must_pass_default = MUST_PASS_BY_DEFAULT.contains(&event_name);
                if must_pass || must_pass_default {
                    tracing::warn!(
                        target: "tau_harness::interception",
                        event = %event_name,
                        must_pass_caller = must_pass,
                        must_pass_default = must_pass_default,
                        "interceptor tried to Drop a must-pass event; \
                         publishing original instead",
                    );
                    Some(original_event)
                } else {
                    tracing::debug!(
                        target: "tau_harness::interception",
                        event = %event_name,
                        "interceptor dropped event",
                    );
                    None
                }
            }
        };

        let Some(event) = next_event else {
            return;
        };

        self.dispatch_publish_step(
            source,
            event,
            transient,
            must_pass,
            sync_head_for,
            Some((cursor.0, cursor.1.as_str())),
        );
    }

    /// Drain `deferred_publishes` until either it's empty or one of
    /// them parks a new intercept.
    fn drain_deferred_publishes(&mut self) {
        while self.pending_intercept.is_none() {
            let Some(deferred) = self.deferred_publishes.pop_front() else {
                break;
            };
            self.dispatch_publish_step(
                deferred.source,
                deferred.event,
                deferred.transient,
                deferred.must_pass,
                deferred.sync_head_for,
                None,
            );
        }
    }
}
