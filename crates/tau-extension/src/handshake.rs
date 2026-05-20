//! Reusable extension bootstrap helper.
//!
//! Every extension process opens its session with the same prelude:
//! `Hello` → optional `Subscribe` → optional `Intercept` → zero or
//! more startup `Event`s → `Ready`, then flushes. The exact mix varies
//! (some extensions register tools, some intercept, some subscribe to
//! several events, some announce model state) but the order and the
//! surrounding frame-shaping is fixed. Copy-pasting that sequence into
//! every crate is mechanical and drifts out of sync; this helper writes
//! it once and lets each extension declare only what differs.
//!
//! ```ignore
//! tau_extension::Handshake::tool("tau-ext-core-subagents")
//!     .subscribe([EventName::TOOL_INVOKE, EventName::EXTENSION_AGENT_QUERY_RESULT])
//!     .register_tool(tool_spec())
//!     .ready_message("core-subagents ready")
//!     .run(&mut writer)?;
//! ```
//!
//! `client_kind` defaults to [`ClientKind::Tool`] and
//! `protocol_version` to [`PROTOCOL_VERSION`] — every extension in
//! this workspace uses both, and they belong on the handshake, not
//! at each call site.

use std::io::Write;

use tau_proto::{
    ClientKind, EncodeError, Event, EventName, EventSelector, ExtensionName, Frame, FrameWriter,
    Hello, Intercept, InterceptionPriority, Message, PROTOCOL_VERSION, PromptFragment, Ready,
    Subscribe, ToolRegister, ToolSpec,
};

/// Builder for the opening frame sequence an extension sends to the
/// harness. See the module-level documentation for a worked example.
#[must_use = "Handshake does nothing until `.run()` is called"]
pub struct Handshake {
    client_name: ExtensionName,
    client_kind: ClientKind,
    selectors: Vec<EventSelector>,
    intercepts: Vec<Intercept>,
    tools: Vec<ToolRegister>,
    events: Vec<Event>,
    ready_message: Option<String>,
}

impl Handshake {
    /// Start a handshake for a tool-kind extension. The vast majority
    /// of extensions in this workspace are tools; use
    /// [`Handshake::with_kind`] for the rare exception.
    pub fn tool(client_name: impl Into<ExtensionName>) -> Self {
        Self::with_kind(client_name, ClientKind::Tool)
    }

    /// Start a handshake with an explicit `client_kind`.
    pub fn with_kind(client_name: impl Into<ExtensionName>, client_kind: ClientKind) -> Self {
        Self {
            client_name: client_name.into(),
            client_kind,
            selectors: Vec::new(),
            intercepts: Vec::new(),
            tools: Vec::new(),
            events: Vec::new(),
            ready_message: None,
        }
    }

    /// Subscribe to a set of events by exact name. Equivalent to
    /// extending the existing selectors with one `EventSelector::Exact`
    /// per item.
    pub fn subscribe(mut self, names: impl IntoIterator<Item = EventName>) -> Self {
        self.selectors
            .extend(names.into_iter().map(EventSelector::Exact));
        self
    }

    /// Append a pre-built `EventSelector` (e.g. `Prefix`, `Pattern`).
    pub fn subscribe_selector(mut self, selector: EventSelector) -> Self {
        self.selectors.push(selector);
        self
    }

    /// Intercept events matching `selector` at the given priority.
    pub fn intercept(mut self, selector: EventSelector, priority: InterceptionPriority) -> Self {
        self.intercepts.push(Intercept {
            selectors: vec![selector],
            priority,
        });
        self
    }

    /// Register a single tool without adding a prompt fragment.
    pub fn register_tool(self, tool: ToolSpec) -> Self {
        self.register_tool_with_prompt_fragment(tool, None)
    }

    /// Register a single tool and optionally attach a prompt fragment that
    /// the harness includes whenever the tool is enabled for the current role.
    pub fn register_tool_with_prompt_fragment(
        mut self,
        tool: ToolSpec,
        prompt_fragment: Option<PromptFragment>,
    ) -> Self {
        self.tools.push(ToolRegister {
            tool,
            prompt_fragment,
        });
        self
    }

    /// Register multiple tools at once without adding prompt fragments.
    pub fn register_tools(mut self, tools: impl IntoIterator<Item = ToolSpec>) -> Self {
        self.tools
            .extend(tools.into_iter().map(|tool| ToolRegister {
                tool,
                prompt_fragment: None,
            }));
        self
    }

    /// Announce one startup event before the terminal `Ready` frame.
    ///
    /// Use this for extension-owned state that the harness should see during
    /// startup, such as `provider.models_updated`. Tool registrations should
    /// continue to use [`Handshake::register_tool`] so their intent stays
    /// clear.
    pub fn announce_event(mut self, event: Event) -> Self {
        self.events.push(event);
        self
    }

    /// Announce multiple startup events before the terminal `Ready` frame.
    pub fn announce_events(mut self, events: impl IntoIterator<Item = Event>) -> Self {
        self.events.extend(events);
        self
    }

    /// Attach a human-readable message to the terminal `Ready` frame.
    pub fn ready_message(mut self, message: impl Into<String>) -> Self {
        self.ready_message = Some(message.into());
        self
    }

    /// Write the full sequence (`Hello`, optional `Subscribe`,
    /// `Intercept`s, startup `Event`s, `Ready`) and flush. Subscribe
    /// is omitted when no selectors have been added — sending an
    /// empty subscription would still be valid but adds noise on the
    /// wire.
    pub fn run<W: Write>(self, writer: &mut FrameWriter<W>) -> Result<(), EncodeError> {
        writer.write_frame(&Frame::Message(Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: self.client_name,
            client_kind: self.client_kind,
        })))?;
        if !self.selectors.is_empty() {
            writer.write_frame(&Frame::Message(Message::Subscribe(Subscribe {
                selectors: self.selectors,
            })))?;
        }
        for intercept in self.intercepts {
            writer.write_frame(&Frame::Message(Message::Intercept(intercept)))?;
        }
        for tool in self.tools {
            writer.write_frame(&Frame::Event(Event::ToolRegister(tool)))?;
        }
        for event in self.events {
            writer.write_frame(&Frame::Event(event))?;
        }
        writer.write_frame(&Frame::Message(Message::Ready(Ready {
            message: self.ready_message,
        })))?;
        writer.flush().map_err(EncodeError::Io)?;
        Ok(())
    }
}
