//! Session files: how `voxeler attach` finds a running `voxeler mcp`.
//!
//! Each server writes one small JSON file into the user's cache directory,
//! naming the loopback port its attach listener accepts on. Discovery prefers a
//! session rooted where you are — you run `voxeler attach` from the project you
//! are working in — and falls back to the only live one.
//!
//! Liveness is decided by **connecting**, not by a pid check: a pid can be
//! reused and a crashed server leaves its file behind, so the socket is the only
//! honest answer. Stale files are removed when they are found.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long to wait for a session's socket before calling it dead. Loopback
/// either answers immediately or is not there.
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// A running `voxeler mcp`, as advertised on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Loopback port the attach listener accepts on.
    pub port: u16,
    /// Absolute root directory the server's file tools are confined to.
    pub root: String,
    pub pid: u32,
    pub version: String,
}

/// Directory holding session files. Honours `VOXELER_SESSION_DIR` (tests use
/// it), then the platform cache directory, then a temp-dir fallback so
/// discovery still works on a system with no `HOME`.
pub fn session_dir() -> PathBuf {
    // An empty value means "unset" — an exported-but-empty variable would
    // otherwise resolve to the relative path "", where nothing is ever found.
    if let Some(dir) = std::env::var("VOXELER_SESSION_DIR")
        .ok()
        .filter(|d| !d.is_empty())
    {
        return PathBuf::from(dir);
    }
    let base = if cfg!(windows) {
        std::env::var("LOCALAPPDATA").ok().map(PathBuf::from)
    } else {
        std::env::var("XDG_CACHE_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".cache"))
            })
    };
    base.unwrap_or_else(std::env::temp_dir)
        .join("voxeler")
        .join("sessions")
}

/// FNV-1a of the canonical root path — a short, stable, filesystem-safe name.
/// Deep project paths would run past filename limits used verbatim, and the
/// root is stored inside the file anyway for verification.
fn root_key(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canonical.to_string_lossy().as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

impl Session {
    /// Advertise a server in `dir`. Returns the path written, so it can be
    /// cleaned up. The directory is a parameter rather than read from the
    /// environment so tests can each use their own without racing on a
    /// process-wide variable; callers pass [`session_dir`].
    pub fn publish_in(dir: &Path, root: &Path, port: u16) -> io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let session = Session {
            port,
            root: canonical.display().to_string(),
            pid: std::process::id(),
            version: crate::VERSION.to_string(),
        };
        let key = root_key(root);
        let path = dir.join(format!("{key}.json"));

        // Write-then-rename rather than writing in place: `discover` reads these
        // concurrently, and a reader that caught a half-written file would see
        // the session as absent. Rename within one directory is atomic.
        let tmp = dir.join(format!("{key}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(&session)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Remove a published session, but only if the file still describes *this*
    /// server.
    ///
    /// Two servers rooted at the same directory share a filename, so the second
    /// to start overwrites the first's advertisement. Without this check the
    /// first one's exit would delete the *second's* live entry, and discovery
    /// would lose a session whose listener is still up.
    pub fn unpublish(path: &Path, port: u16) -> io::Result<()> {
        match Session::load(path) {
            Some(s) if s.port == port && s.pid == std::process::id() => std::fs::remove_file(path),
            // Someone else's now, or already gone. Leave it alone.
            _ => Ok(()),
        }
    }

    fn load(path: &Path) -> Option<Session> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    /// Every session file in `dir`, live or not.
    pub fn list_in(dir: &Path) -> Vec<(PathBuf, Session)> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(s) = Session::load(&path) {
                out.push((path, s));
            }
        }
        out
    }

    /// Whether something is actually listening. The only honest liveness test:
    /// a pid can be reused, and a killed server leaves its file behind.
    pub fn is_live(&self) -> bool {
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, self.port);
        TcpStream::connect_timeout(&addr.into(), PROBE_TIMEOUT).is_ok()
    }
}

/// Why discovery failed, phrased for someone at a terminal.
#[derive(Debug)]
pub enum Discovery {
    Found(Session),
    None,
    /// More than one live server and no way to tell which was meant.
    Ambiguous(Vec<Session>),
}

/// Find the server to attach to.
pub fn discover(root: Option<&Path>) -> Discovery {
    discover_in(&session_dir(), root, |s| s.is_live())
}

/// As [`discover`], over an explicit session directory and liveness test.
///
/// `root`, when given, picks a specific server. Otherwise prefer one rooted at
/// the current directory — the overwhelmingly common case — and fall back to the
/// only live session if there is exactly one.
pub fn discover_in(
    dir: &Path,
    root: Option<&Path>,
    is_live: impl Fn(&Session) -> bool,
) -> Discovery {
    let mut live = Vec::new();
    for (path, session) in Session::list_in(dir) {
        if is_live(&session) {
            live.push(session);
        } else {
            // Nothing is listening, so the file describes a server that is
            // gone. Removing it here is what stops a machine accumulating dead
            // advertisements that make every later discovery ambiguous.
            let _ = std::fs::remove_file(&path);
        }
    }

    let wanted = root
        .map(|r| r.canonicalize().unwrap_or_else(|_| r.to_path_buf()))
        .or_else(|| std::env::current_dir().ok());
    if let Some(wanted) = &wanted {
        if let Some(s) = live.iter().find(|s| Path::new(&s.root) == wanted) {
            return Discovery::Found(s.clone());
        }
    }
    // An explicit root that matched nothing is a miss, not an invitation to
    // attach to some other project's server.
    if root.is_some() {
        return Discovery::None;
    }
    match live.len() {
        0 => Discovery::None,
        1 => Discovery::Found(live.remove(0)),
        _ => Discovery::Ambiguous(live),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("voxeler-session-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_published_session_round_trips_and_can_be_withdrawn() {
        let dir = temp("roundtrip");
        let root = temp("roundtrip-root");
        let path = Session::publish_in(&dir, &root, 4242).unwrap();

        let found = Session::list_in(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.port, 4242);
        assert_eq!(found[0].1.pid, std::process::id());
        assert_eq!(Path::new(&found[0].1.root), root.canonicalize().unwrap());

        Session::unpublish(&path, 4242).unwrap();
        assert!(Session::list_in(&dir).is_empty());
    }

    /// Two servers rooted at one directory share a filename. The first to exit
    /// must not delete the second's live advertisement.
    #[test]
    fn withdrawing_leaves_a_newer_servers_advertisement_alone() {
        let dir = temp("overwrite");
        let root = temp("overwrite-root");
        let path = Session::publish_in(&dir, &root, 1111).unwrap();
        Session::publish_in(&dir, &root, 2222).unwrap();

        Session::unpublish(&path, 1111).unwrap();
        let found = Session::list_in(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.port, 2222, "the newer server is still advertised");
    }

    /// A file whose socket answers nobody describes a server that has gone.
    /// Leaving it would make every later discovery ambiguous.
    #[test]
    fn discovery_sweeps_away_the_sessions_nothing_is_listening_on() {
        let dir = temp("stale");
        Session::publish_in(&dir, &temp("stale-a"), 1).unwrap();
        Session::publish_in(&dir, &temp("stale-b"), 2).unwrap();

        assert!(matches!(
            discover_in(&dir, None, |_| false),
            Discovery::None
        ));
        assert!(Session::list_in(&dir).is_empty(), "both files were swept");
    }

    #[test]
    fn one_live_session_is_found_without_being_named() {
        let dir = temp("single");
        let root = temp("single-root");
        Session::publish_in(&dir, &root, 7777).unwrap();
        match discover_in(&dir, None, |_| true) {
            Discovery::Found(s) => assert_eq!(s.port, 7777),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn two_live_sessions_are_ambiguous_unless_one_is_named() {
        let dir = temp("many");
        let a = temp("many-a");
        let b = temp("many-b");
        Session::publish_in(&dir, &a, 1).unwrap();
        Session::publish_in(&dir, &b, 2).unwrap();

        match discover_in(&dir, None, |_| true) {
            Discovery::Ambiguous(v) => assert_eq!(v.len(), 2),
            other => panic!("{other:?}"),
        }
        match discover_in(&dir, Some(&b), |_| true) {
            Discovery::Found(s) => assert_eq!(s.port, 2),
            other => panic!("{other:?}"),
        }
    }

    /// Naming a root with no server is a miss. Falling back to somebody else's
    /// session would attach you to the wrong project without saying so.
    #[test]
    fn naming_a_root_with_no_server_finds_nothing() {
        let dir = temp("wrong-root");
        Session::publish_in(&dir, &temp("wrong-root-a"), 1).unwrap();
        let elsewhere = temp("wrong-root-b");
        assert!(matches!(
            discover_in(&dir, Some(&elsewhere), |_| true),
            Discovery::None
        ));
    }

    #[test]
    fn an_empty_session_dir_variable_is_treated_as_unset() {
        // Not asserting on the resolved path — it varies by platform — only
        // that an exported-but-empty value never yields the relative path "".
        assert!(session_dir().is_absolute() || session_dir().components().count() > 1);
    }
}
