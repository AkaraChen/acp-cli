//! Persistent state: the auth marker and the sessions created by the agent,
//! one `sessions/<id>.json` file per session so that agents sharing the
//! directory do not overwrite each other's sessions.
//!
//! Without a state directory everything lives in memory for the lifetime of
//! the process.

use std::io;
use std::path::Path;
use std::path::PathBuf;

use crate::scenario::Scenario;
use crate::scenario::SessionRecord;

pub const STATE_ENV: &str = "ACPC_MOCK_STATE";
const AUTH_MARKER: &str = "authenticated";
const SESSIONS_DIR: &str = "sessions";

#[derive(Debug, Clone, Default)]
pub struct StateDir {
    root: Option<PathBuf>,
}

impl StateDir {
    /// `state_dir` from the scenario, else `$ACPC_MOCK_STATE`, else none.
    pub fn resolve(scenario: &Scenario) -> Self {
        let root = scenario.state_dir.clone().or_else(|| {
            std::env::var_os(STATE_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        });
        Self { root }
    }

    #[cfg(test)]
    pub fn at(root: &Path) -> Self {
        Self {
            root: Some(root.to_path_buf()),
        }
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub fn is_authenticated(&self) -> bool {
        self.root
            .as_ref()
            .is_some_and(|root| root.join(AUTH_MARKER).exists())
    }

    pub fn set_authenticated(&self, authenticated: bool) -> io::Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let marker = root.join(AUTH_MARKER);
        if authenticated {
            std::fs::create_dir_all(root)?;
            std::fs::write(marker, b"ok\n")
        } else {
            match std::fs::remove_file(marker) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
                _ => Ok(()),
            }
        }
    }

    /// Every stored session. Unreadable files are logged and skipped.
    pub fn load_sessions(&self) -> io::Result<Vec<SessionRecord>> {
        let Some(root) = &self.root else {
            return Ok(Vec::new());
        };
        let entries = match std::fs::read_dir(root.join(SESSIONS_DIR)) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                paths.push(path);
            }
        }
        paths.sort();
        let mut records = Vec::new();
        for path in paths {
            let parsed = std::fs::read(&path).and_then(|bytes| {
                serde_json::from_slice::<SessionRecord>(&bytes).map_err(io::Error::other)
            });
            match parsed {
                Ok(record) => records.push(record),
                Err(err) => log!("ignoring unreadable session {}: {err}", path.display()),
            }
        }
        Ok(records)
    }

    /// Atomically replaces the session's file.
    pub fn save_session(&self, record: &SessionRecord) -> io::Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let dir = root.join(SESSIONS_DIR);
        std::fs::create_dir_all(&dir)?;
        let bytes = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
        let name = session_file(&record.session_id);
        let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(tmp, dir.join(name))
    }

    /// Removes the session's file, if any.
    pub fn delete_session(&self, session_id: &str) -> io::Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let path = root.join(SESSIONS_DIR).join(session_file(session_id));
        match std::fs::remove_file(path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }
}

/// `<id>.json`, with every byte outside `[A-Za-z0-9_-]` percent-encoded so
/// any session id maps to a distinct, plain file name.
fn session_file(session_id: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut name = String::with_capacity(session_id.len() + 5);
    for byte in session_id.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
            name.push(char::from(byte));
        } else {
            name.push('%');
            name.push(char::from(HEX[usize::from(byte >> 4)]));
            name.push(char::from(HEX[usize::from(byte & 0xf)]));
        }
    }
    name.push_str(".json");
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn auth_marker_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::at(&dir.path().join("nested"));
        assert!(!state.is_authenticated());
        state.set_authenticated(true).expect("login");
        assert!(state.is_authenticated());
        state.set_authenticated(false).expect("logout");
        state.set_authenticated(false).expect("logout twice");
        assert!(!state.is_authenticated());
    }

    fn record(session_id: &str, title: &str) -> SessionRecord {
        SessionRecord {
            session_id: session_id.to_string(),
            cwd: PathBuf::from("/abs"),
            title: Some(title.to_string()),
            updated_at: None,
            history: Vec::new(),
        }
    }

    #[test]
    fn sessions_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::at(dir.path());
        assert!(state.load_sessions().expect("empty").is_empty());
        state.save_session(&record("s1", "t")).expect("save");
        state.save_session(&record("a/../b", "odd")).expect("save");
        state.save_session(&record("s1", "t2")).expect("overwrite");
        std::fs::write(dir.path().join("sessions/bad.json"), b"{").expect("write");
        let loaded = state.load_sessions().expect("load");
        let ids: Vec<(&str, Option<&str>)> = loaded
            .iter()
            .map(|r| (r.session_id.as_str(), r.title.as_deref()))
            .collect();
        assert_eq!(ids, vec![("a/../b", Some("odd")), ("s1", Some("t2"))]);

        state.delete_session("a/../b").expect("delete");
        state.delete_session("a/../b").expect("delete twice");
        assert_eq!(state.load_sessions().expect("load").len(), 1);
    }

    #[test]
    fn session_files_are_escaped() {
        assert_eq!(session_file("sess_1-a"), "sess_1-a.json");
        assert_eq!(session_file("a/../b"), "a%2F%2E%2E%2Fb.json");
        assert_ne!(session_file("a.b"), session_file("a%2Eb"));
    }

    #[test]
    fn in_memory_state_is_a_no_op() {
        let state = StateDir::default();
        assert!(state.root().is_none());
        state.set_authenticated(true).expect("no-op");
        assert!(!state.is_authenticated());
        state.save_session(&record("s1", "t")).expect("no-op");
        state.delete_session("s1").expect("no-op");
        assert!(state.load_sessions().expect("no-op").is_empty());
    }
}
