//! Append-only on-disk persistence of per-session protocol events.
//!
//! Each session is just a CBOR event log plus a small JSON sidecar.
//! The in-memory [`SessionTree`] is a *derived* view, folded from the
//! persisted events via [`SessionTree::from_events`]; nothing else
//! mutates it. Writers go through [`SessionStore::append_session_event`],
//! which appends one durable record to disk and applies the same
//! event to the cached tree.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tau_proto::{ConnectionId, Event, LogEventId, NodeId, SessionId};

use crate::session::{PersistedSessionEvent, SessionMeta, SessionTree};

/// Errors returned by the append-only session store.
#[derive(Debug)]
pub enum SessionStoreError {
    CreateParentDirectory {
        path: PathBuf,
        source: io::Error,
    },
    Open {
        path: PathBuf,
        source: io::Error,
    },
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Decode {
        path: PathBuf,
        source: tau_proto::DecodeError,
    },
    Encode {
        path: PathBuf,
        source: tau_proto::EncodeError,
    },
    /// Another process holds the exclusive lock on this session.
    Locked {
        path: PathBuf,
        holder: String,
    },
    InvalidSessionDir {
        path: PathBuf,
    },
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateParentDirectory { path, source } => write!(
                f,
                "failed to create parent directory for session store {}: {source}",
                path.display()
            ),
            Self::Open { path, source } => {
                write!(
                    f,
                    "failed to open session store {}: {source}",
                    path.display()
                )
            }
            Self::Read { path, source } => {
                write!(
                    f,
                    "failed to read session store {}: {source}",
                    path.display()
                )
            }
            Self::Write { path, source } => {
                write!(
                    f,
                    "failed to write session store {}: {source}",
                    path.display()
                )
            }
            Self::Decode { path, source } => write!(
                f,
                "failed to decode session store record from {}: {source}",
                path.display()
            ),
            Self::Encode { path, source } => write!(
                f,
                "failed to encode session store record for {}: {source}",
                path.display()
            ),
            Self::Locked { path, holder } => write!(
                f,
                "session lock at {} held by another process ({})",
                path.display(),
                holder.trim()
            ),
            Self::InvalidSessionDir { path } => write!(
                f,
                "invalid session directory name (non-utf8): {}",
                path.display()
            ),
        }
    }
}

impl Error for SessionStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateParentDirectory { source, .. } => Some(source),
            Self::Open { source, .. } => Some(source),
            Self::Read { source, .. } => Some(source),
            Self::Write { source, .. } => Some(source),
            Self::Decode { source, .. } => Some(source),
            Self::Encode { source, .. } => Some(source),
            Self::Locked { .. } => None,
            Self::InvalidSessionDir { .. } => None,
        }
    }
}

/// Append-only persistence for per-session protocol events, with a
/// derived [`SessionTree`] cached in memory.
///
/// Each session lives in its own directory under `state_dir`:
///
/// ```text
/// <state_dir>/<session_id>/
///   events.cbor   # length-prefixed PersistedSessionEvent stream — the source of truth
///   meta.json     # SessionMeta sidecar (cwd, created_at, last_touched)
///   lock          # exclusively flock'd while this store has the session loaded for write
/// ```
///
/// Existing session dirs are loaded lazily. Startup constructs an
/// empty store and loads individual session trees on first access.
/// Flocks are still taken lazily on first write so read-only
/// consumers (e.g. inspection commands) don't contend with a running
/// daemon.
/// Result of one [`SessionStore::append_session_event_at`] call:
/// the durable event id and, when the event produced a tree node,
/// that node's id. Callers maintaining a per-conversation branch
/// cursor advance it from `folded_node_id` rather than from the
/// global `tree.head()` so non-folding events (e.g. an
/// `AgentResponseFinished` carrying only tool calls) don't sync
/// the cursor onto a sibling conversation's last fold.
#[derive(Clone, Debug)]
pub struct AppendOutcome {
    pub id: LogEventId,
    pub folded_node_id: Option<NodeId>,
}

#[derive(Debug)]
pub struct SessionStore {
    state_dir: PathBuf,
    sessions: HashMap<SessionId, SessionTree>,
    /// Held flocks per session, acquired lazily on first write. Released
    /// when this store is dropped (the OS releases the flock when the
    /// file handle closes).
    locks: HashMap<SessionId, File>,
}

impl SessionStore {
    /// Opens the session store rooted at `state_dir`, eagerly loading
    /// every session subdirectory found there.
    pub fn open(state_dir: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let state_dir = state_dir.into();
        let mut store = Self::open_lazy(state_dir.clone())?;
        for entry in fs::read_dir(&state_dir).map_err(|source| SessionStoreError::Read {
            path: state_dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| SessionStoreError::Read {
                path: state_dir.clone(),
                source,
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let events_path = path.join("events.cbor");
            if !events_path.exists() {
                continue;
            }
            let session_id_str = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| SessionStoreError::InvalidSessionDir { path: path.clone() })?;
            store.load_session_if_needed(session_id_str)?;
        }
        Ok(store)
    }

    /// Opens the session store rooted at `state_dir` without loading
    /// session event logs. Individual sessions are loaded on write;
    /// callers that need a pre-existing tree should use [`Self::open`].
    pub fn open_lazy(state_dir: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let state_dir = state_dir.into();
        fs::create_dir_all(&state_dir).map_err(|source| {
            SessionStoreError::CreateParentDirectory {
                path: state_dir.clone(),
                source,
            }
        })?;

        Ok(Self {
            state_dir,
            sessions: HashMap::new(),
            locks: HashMap::new(),
        })
    }

    fn load_session_if_needed(&mut self, session_id: &str) -> Result<(), SessionStoreError> {
        let sid: SessionId = session_id.into();
        if self.sessions.contains_key(&sid) {
            return Ok(());
        }
        let events_path = self.session_dir(session_id).join("events.cbor");
        if !events_path.exists() {
            return Ok(());
        }
        let events = load_session_events(&events_path)?;
        let tree = SessionTree::from_events(sid.clone(), &events);
        self.sessions.insert(sid, tree);
        Ok(())
    }

    /// Returns the path to one session's directory (created lazily on
    /// write).
    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.state_dir.join(session_id)
    }

    /// Acquires an exclusive flock on the session's `lock` file if not
    /// already held.
    fn ensure_locked(&mut self, session_id: &str) -> Result<(), SessionStoreError> {
        let sid: SessionId = session_id.into();
        if self.locks.contains_key(&sid) {
            return Ok(());
        }
        let session_dir = self.session_dir(session_id);
        fs::create_dir_all(&session_dir).map_err(|source| {
            SessionStoreError::CreateParentDirectory {
                path: session_dir.clone(),
                source,
            }
        })?;
        let lock_path = session_dir.join("lock");
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| SessionStoreError::Open {
                path: lock_path.clone(),
                source,
            })?;
        if FileExt::try_lock_exclusive(&file).is_err() {
            let mut holder = String::new();
            let _ = file.read_to_string(&mut holder);
            return Err(SessionStoreError::Locked {
                path: lock_path,
                holder,
            });
        }
        // Replace lock contents with our PID + start time.
        file.set_len(0).map_err(|source| SessionStoreError::Write {
            path: lock_path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|source| SessionStoreError::Write {
                path: lock_path.clone(),
                source,
            })?;
        let pid = std::process::id();
        let now = unix_now();
        writeln!(&mut file, "pid={pid} start={now}").map_err(|source| {
            SessionStoreError::Write {
                path: lock_path.clone(),
                source,
            }
        })?;
        self.locks.insert(sid, file);
        Ok(())
    }

    /// Appends one non-transient protocol event to the durable
    /// per-session event log and applies it to the in-memory tree.
    /// The persisted event is the single source of truth — both the
    /// on-disk log and the derived [`SessionTree`] are populated from
    /// it here, so they cannot drift.
    ///
    /// Convenience wrapper around
    /// [`SessionStore::append_session_event_at`] that uses the
    /// session tree's current head as the fold parent — the legacy
    /// behaviour from before per-conversation parent stamping.
    pub fn append_session_event(
        &mut self,
        session_id: &str,
        source: Option<ConnectionId>,
        event: Event,
    ) -> Result<AppendOutcome, SessionStoreError> {
        self.append_session_event_at(session_id, source, None, event)
    }

    /// Like [`SessionStore::append_session_event`] but folds the
    /// event onto an explicit fold parent instead of the session
    /// tree's current write cursor. The harness uses this when
    /// publishing on a conversation's behalf, so cross-conversation
    /// events don't have to bounce a shared `head` cursor through
    /// `UiNavigateTree`.
    ///
    /// `parent_node_id` is a tri-state matching
    /// [`SessionTree::apply_event_at`]: `None` inherits the tree's
    /// head (legacy), `Some(None)` folds at root, `Some(Some(id))`
    /// folds under `id`.
    pub fn append_session_event_at(
        &mut self,
        session_id: &str,
        source: Option<ConnectionId>,
        parent_node_id: Option<Option<NodeId>>,
        event: Event,
    ) -> Result<AppendOutcome, SessionStoreError> {
        self.ensure_locked(session_id)?;
        self.load_session_if_needed(session_id)?;
        let session_dir = self.session_dir(session_id);
        fs::create_dir_all(&session_dir).map_err(|source| {
            SessionStoreError::CreateParentDirectory {
                path: session_dir.clone(),
                source,
            }
        })?;
        let events_path = session_dir.join("events.cbor");
        let next_id = next_session_event_id(&events_path)?;
        let record = PersistedSessionEvent {
            id: next_id,
            source,
            event: event.clone(),
            // Persistence stores only the inner Option<NodeId>;
            // explicit-root (`Some(None)`) and inherit-head (`None`)
            // collapse to `None` on the wire. See `from_events`.
            parent_node_id: parent_node_id.flatten(),
        };
        append_cbor_record(&events_path, &record)?;
        touch_meta_for_event(&session_dir.join("meta.json"), &event)?;

        let sid: SessionId = session_id.into();
        let tree = self
            .sessions
            .entry(sid.clone())
            .or_insert_with(|| SessionTree::from_events(sid, &[]));
        let folded_node_id = tree.apply_event_at(parent_node_id, &event);

        Ok(AppendOutcome {
            id: next_id,
            folded_node_id,
        })
    }

    /// Loads durable per-session protocol events.
    pub fn session_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<PersistedSessionEvent>, SessionStoreError> {
        let path = self.session_dir(session_id).join("events.cbor");
        load_session_events(&path)
    }

    /// Returns the state dir this store is rooted at.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Returns one session tree if it exists, loading a persisted log
    /// on demand.
    pub fn load_session(
        &mut self,
        session_id: &str,
    ) -> Result<Option<&SessionTree>, SessionStoreError> {
        self.load_session_if_needed(session_id)?;
        Ok(self.sessions.get(&SessionId::from(session_id)))
    }

    /// Returns one already-loaded session tree if it exists.
    #[must_use]
    pub fn session(&self, session_id: &str) -> Option<&SessionTree> {
        self.sessions.get(&SessionId::from(session_id))
    }

    /// Returns all known sessions.
    #[must_use]
    pub fn sessions(&self) -> Vec<&SessionTree> {
        self.sessions.values().collect()
    }

    /// Records initial cwd metadata for a session if not already
    /// present. Idempotent: subsequent calls only update
    /// `last_touched` via [`touch_meta`].
    pub fn record_session_meta(
        &mut self,
        session_id: &str,
        cwd: Option<PathBuf>,
    ) -> Result<(), SessionStoreError> {
        self.ensure_locked(session_id)?;
        let path = self.session_dir(session_id).join("meta.json");
        let now = unix_now();
        let mut meta = read_meta(&path).unwrap_or_default();
        if meta.created_at == 0 {
            meta.created_at = now;
        }
        if meta.cwd.is_none() {
            meta.cwd = cwd;
        }
        meta.last_touched = now;
        write_meta(&path, &meta)
    }
}

/// Lists session metadata across `state_dir` without taking any flocks.
///
/// Sessions whose `meta.json` is missing or malformed are skipped silently;
/// the goal is best-effort discovery for `-r` resumption, not strict listing.
pub fn list_session_metas(state_dir: &Path) -> io::Result<Vec<(SessionId, SessionMeta)>> {
    let mut out = Vec::new();
    if !state_dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(state_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let meta_path = path.join("meta.json");
        let Ok(meta) = read_meta(&meta_path) else {
            continue;
        };
        out.push((SessionId::from(name), meta));
    }
    Ok(out)
}

/// Best-effort check whether a session's lock is currently held.
pub fn session_is_locked(state_dir: &Path, session_id: &str) -> io::Result<bool> {
    let lock_path = state_dir.join(session_id).join("lock");
    let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            Ok(false)
        }
        Err(_) => Ok(true),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_meta(path: &Path) -> io::Result<SessionMeta> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn write_meta(path: &Path, meta: &SessionMeta) -> Result<(), SessionStoreError> {
    let bytes = serde_json::to_vec_pretty(meta).map_err(|e| SessionStoreError::Write {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, e),
    })?;
    fs::write(path, bytes).map_err(|source| SessionStoreError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn touch_meta_for_event(path: &Path, event: &Event) -> Result<(), SessionStoreError> {
    let now = unix_now();
    let mut meta = read_meta(path).unwrap_or_default();
    if meta.created_at == 0 {
        meta.created_at = now;
    }
    meta.last_touched = now;
    if let Some(text) = user_prompt_text(event) {
        meta.latest_user_prompt_preview = Some(preview_text(text, 48));
    }
    write_meta(path, &meta)
}

fn user_prompt_text(event: &Event) -> Option<&str> {
    match event {
        Event::UiPromptSubmitted(prompt) => Some(&prompt.text),
        Event::SessionUserMessageInjected(injected) => Some(&injected.text),
        Event::SessionPromptSteered(steered) => Some(&steered.text),
        _ => None,
    }
}

fn preview_text(text: &str, max: usize) -> String {
    let single_line: String = text
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    if single_line.chars().count() < max + 1 {
        single_line
    } else {
        format!("{}…", single_line.chars().take(max).collect::<String>())
    }
}

fn append_cbor_record<T: Serialize>(path: &Path, record: &T) -> Result<(), SessionStoreError> {
    let mut encoded = Vec::new();
    ciborium::into_writer(record, &mut encoded).map_err(|source| SessionStoreError::Encode {
        path: path.to_path_buf(),
        source,
    })?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| SessionStoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&(encoded.len() as u64).to_le_bytes())
        .map_err(|source| SessionStoreError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&encoded)
        .map_err(|source| SessionStoreError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    file.flush().map_err(|source| SessionStoreError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn load_session_events(path: &Path) -> Result<Vec<PersistedSessionEvent>, SessionStoreError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    read_cbor_records(path, |record: PersistedSessionEvent| {
        events.push(record);
    })?;
    Ok(events)
}

fn next_session_event_id(path: &Path) -> Result<LogEventId, SessionStoreError> {
    let events = load_session_events(path)?;
    Ok(events
        .last()
        .map(|record| LogEventId::new(record.id.get() + 1))
        .unwrap_or_else(|| LogEventId::new(0)))
}

fn read_cbor_records<T, F>(path: &Path, mut handle: F) -> Result<(), SessionStoreError>
where
    T: for<'de> Deserialize<'de>,
    F: FnMut(T),
{
    let mut file = File::open(path).map_err(|source| SessionStoreError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    loop {
        let mut length_bytes = [0_u8; 8];
        match file.read_exact(&mut length_bytes) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(source) => {
                return Err(SessionStoreError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }

        let record_length = u64::from_le_bytes(length_bytes) as usize;
        let mut record_bytes = vec![0_u8; record_length];
        file.read_exact(&mut record_bytes)
            .map_err(|source| SessionStoreError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        let record: T = ciborium::from_reader(record_bytes.as_slice()).map_err(|source| {
            SessionStoreError::Decode {
                path: path.to_path_buf(),
                source,
            }
        })?;
        handle(record);
    }
}
