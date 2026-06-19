//! Internal directory helpers. Read-only agent and policy inspection
//! lives in the standalone `tau-agent-inspect` crate.

use std::path::{Path, PathBuf};

pub(crate) fn policy_store_path_from(state_dir: &Path) -> PathBuf {
    state_dir.join("policy.cbor")
}
