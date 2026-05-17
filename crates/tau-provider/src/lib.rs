//! Provider management: authentication and OAuth flows.
//!
//! Supports multiple named provider instances with API key or OAuth
//! credentials stored in `~/.local/share/tau/auth.json`.

pub mod oauth;
pub mod storage;
