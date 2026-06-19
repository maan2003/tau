//! Subscription policy: file-backed approvals and the default policy that
//! gates which event categories socket clients are allowed to subscribe to.

use std::cell::RefCell;
use std::error::Error;
use std::path::PathBuf;
use std::{fmt, fs};

use serde::{Deserialize, Serialize};
use tau_proto::EventSelector;

use crate::connection::{ConnectionMetadata, ConnectionOrigin};

/// Error returned by the file-backed subscription policy store.
#[derive(Debug)]
pub enum PolicyStoreError {
    CreateParentDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    Decode {
        path: PathBuf,
        source: ciborium::de::Error<std::io::Error>,
    },
    Encode {
        path: PathBuf,
        source: ciborium::ser::Error<std::io::Error>,
    },
}

impl fmt::Display for PolicyStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateParentDirectory { path, source } => {
                write!(
                    f,
                    "failed to create parent directory for policy store {}: {source}",
                    path.display()
                )
            }
            Self::Read { path, source } => {
                write!(
                    f,
                    "failed to read policy store {}: {source}",
                    path.display()
                )
            }
            Self::Write { path, source } => {
                write!(
                    f,
                    "failed to write policy store {}: {source}",
                    path.display()
                )
            }
            Self::Decode { path, source } => {
                write!(
                    f,
                    "failed to decode policy store record from {}: {source}",
                    path.display()
                )
            }
            Self::Encode { path, source } => {
                write!(
                    f,
                    "failed to encode policy store record for {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl Error for PolicyStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateParentDirectory { source, .. }
            | Self::Read { source, .. }
            | Self::Write { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source),
            Self::Encode { source, .. } => Some(source),
        }
    }
}

/// Persisted approval for one subscription request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionApproval {
    pub connection_name: String,
    pub connection_origin: ConnectionOrigin,
    pub selectors: Vec<EventSelector>,
}

/// File-backed store of approved subscription sets.
#[derive(Debug)]
pub struct PolicyStore {
    path: PathBuf,
    approvals: Vec<SubscriptionApproval>,
}

impl PolicyStore {
    /// Opens a policy store, loading any existing approvals.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, PolicyStoreError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| {
                PolicyStoreError::CreateParentDirectory {
                    path: parent.to_path_buf(),
                    source,
                }
            })?;
        }

        let approvals = if path.exists() {
            let bytes = fs::read(&path).map_err(|source| PolicyStoreError::Read {
                path: path.clone(),
                source,
            })?;
            if bytes.is_empty() {
                Vec::new()
            } else {
                ciborium::from_reader(bytes.as_slice()).map_err(|source| {
                    PolicyStoreError::Decode {
                        path: path.clone(),
                        source,
                    }
                })?
            }
        } else {
            Vec::new()
        };

        Ok(Self { path, approvals })
    }

    /// Returns true when the exact approval is already present.
    #[must_use]
    pub fn contains(&self, approval: &SubscriptionApproval) -> bool {
        self.approvals.iter().any(|existing| existing == approval)
    }

    /// Records one approval and persists it if it is new.
    pub fn record(&mut self, approval: SubscriptionApproval) -> Result<(), PolicyStoreError> {
        if self.contains(&approval) {
            return Ok(());
        }
        self.approvals.push(approval);

        let mut encoded = Vec::new();
        ciborium::into_writer(&self.approvals, &mut encoded).map_err(|source| {
            PolicyStoreError::Encode {
                path: self.path.clone(),
                source,
            }
        })?;
        fs::write(&self.path, encoded).map_err(|source| PolicyStoreError::Write {
            path: self.path.clone(),
            source,
        })
    }

    /// Returns all persisted approvals.
    #[must_use]
    pub fn approvals(&self) -> &[SubscriptionApproval] {
        &self.approvals
    }
}

/// Policy error returned when a subscription request is rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionPolicyError {
    reason: String,
}

impl SubscriptionPolicyError {
    /// Creates a new policy error.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// Returns the rejection reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for SubscriptionPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl Error for SubscriptionPolicyError {}

/// Subscription-time policy hook.
pub trait SubscriptionPolicy {
    fn evaluate(
        &self,
        connection: &ConnectionMetadata,
        selectors: &[EventSelector],
    ) -> Result<(), SubscriptionPolicyError>;
}

/// Default MVP subscription policy.
#[derive(Debug, Default)]
pub struct DefaultSubscriptionPolicy {
    store: Option<RefCell<PolicyStore>>,
}

impl DefaultSubscriptionPolicy {
    /// Creates the default in-memory-only policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the default policy backed by one approval store.
    pub fn with_store(store: PolicyStore) -> Self {
        Self {
            store: Some(RefCell::new(store)),
        }
    }

    fn record_approval(
        &self,
        connection: &ConnectionMetadata,
        selectors: &[EventSelector],
    ) -> Result<(), SubscriptionPolicyError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        if connection.origin != ConnectionOrigin::Socket {
            return Ok(());
        }

        store
            .borrow_mut()
            .record(SubscriptionApproval {
                connection_name: connection.name.clone(),
                connection_origin: connection.origin.clone(),
                selectors: selectors.to_vec(),
            })
            .map_err(|error| SubscriptionPolicyError::new(error.to_string()))
    }
}

impl SubscriptionPolicy for DefaultSubscriptionPolicy {
    fn evaluate(
        &self,
        connection: &ConnectionMetadata,
        selectors: &[EventSelector],
    ) -> Result<(), SubscriptionPolicyError> {
        if connection.origin == ConnectionOrigin::Socket {
            // Closed list of categories a socket client is allowed to
            // subscribe to. The unknown `EventCategory::Other` is
            // rejected outright. Adding a new `EventCategory` variant
            // upstream forces this match to be revisited (no `_` arm).
            fn category_allowed(category: &tau_proto::EventCategory) -> bool {
                use tau_proto::EventCategory as C;
                match category {
                    C::Tool
                    | C::Action
                    | C::Agent
                    | C::Extension
                    | C::Provider
                    | C::Ui
                    | C::Harness
                    | C::Shell
                    | C::Term => true,
                    C::Other(_) => false,
                }
            }
            for selector in selectors {
                let allowed = match selector {
                    EventSelector::Exact(name) => category_allowed(name.category()),
                    EventSelector::Prefix(prefix) => {
                        // The category portion of the prefix must
                        // resolve to a known, allowed category. A
                        // bare prefix like "tool" (no dot) falls
                        // through the `unwrap_or` and is parsed as a
                        // category directly — relies on
                        // `EventCategory::from_wire` recognizing the
                        // bare category strings.
                        let category_str = prefix.split_once('.').map(|(c, _)| c).unwrap_or(prefix);
                        let category = tau_proto::EventCategory::from_wire(category_str);
                        category_allowed(&category)
                    }
                };
                if !allowed {
                    return Err(SubscriptionPolicyError::new(
                        "socket clients may only subscribe to allowed event families",
                    ));
                }
            }
        }

        self.record_approval(connection, selectors)
    }
}
