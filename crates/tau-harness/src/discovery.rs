//! Snapshot records for skills and AGENTS.md files announced by extensions
//! during session init.

use std::path::PathBuf;

/// A skill discovered by an extension.
pub(crate) struct DiscoveredSkill {
    pub(crate) source_id: tau_proto::ConnectionId,
    pub(crate) description: String,
    pub(crate) file_path: PathBuf,
    pub(crate) add_to_prompt: bool,
}

/// One AGENTS.md file discovered by an extension.
pub(crate) struct DiscoveredAgentsFile {
    pub(crate) source_id: tau_proto::ConnectionId,
    pub(crate) file_path: PathBuf,
    pub(crate) content: String,
}
