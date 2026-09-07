//! The viewer's half: connect to a running `voxeler mcp` and keep a local copy
//! of its model up to date.
//!
//! Polling rather than a push, because the server is client-driven — with nobody
//! attached it does no work, which is what makes attaching something you can do
//! halfway through a build without changing what the session would have done.
//! Twenty times a second is well under what a hand or an agent can change, and
//! an unchanged answer is a single byte.

use std::io::{BufReader, BufWriter};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Duration;

use voxel_core::VoxelModel;

use super::protocol::{self, Sync as SyncReply};
use crate::mcp::session::Session;

/// How often to ask. Fast enough that an agent's edit appears as it happens,
/// slow enough that an idle viewer is doing nothing measurable.
const POLL_EVERY: Duration = Duration::from_millis(50);

/// A live connection to a session, feeding models to the window.
pub struct Attached {
    models: Receiver<VoxelModel>,
    /// What the window puts in its title bar, so it is obvious this is somebody
    /// else's document rather than a file you opened.
    pub label: String,
}

impl Attached {
    /// The newest model that has arrived, or `None`.
    ///
    /// Only the newest: an update supersedes every one before it, and a viewer
    /// that fell behind should catch up rather than replay.
    pub fn latest(&self) -> Option<VoxelModel> {
        self.models.try_iter().last()
    }
}

/// Connect to `session` and start following it.
///
/// Returns the model as it stands, so the window opens on something rather than
/// on an empty volume that fills in a frame later. `wake` is called whenever a
/// new one arrives — the event loop waits on window events, and would otherwise
/// not notice until the next mouse move.
pub fn connect(
    session: &Session,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> Result<(Attached, VoxelModel), String> {
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, session.port);
    let stream = TcpStream::connect(addr)
        .map_err(|e| format!("cannot reach the session on port {}: {e}", session.port))?;
    stream.set_nodelay(true).ok();

    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| format!("cannot read the session: {e}"))?,
    );
    let mut writer = BufWriter::new(stream);

    protocol::write_hello(&mut writer).map_err(|e| format!("handshake: {e}"))?;
    let (their_version, ok) =
        protocol::read_hello_reply(&mut reader).map_err(|e| format!("handshake: {e}"))?;
    if !ok {
        return Err(format!(
            "that session speaks attach protocol {their_version} and this build speaks {} \
             — the two were built from different sources",
            protocol::PROTOCOL_VERSION
        ));
    }

    // The first sync is synchronous, so a failure is reported at the terminal
    // rather than as a window that opens empty and never fills.
    protocol::write_sync_request(&mut writer, 0).map_err(|e| format!("first sync: {e}"))?;
    let first = match protocol::read_sync(&mut reader).map_err(|e| format!("first sync: {e}"))? {
        SyncReply::Model { revision, bytes } => {
            let model = voxel_core::format::native::decode(&bytes)
                .map_err(|e| format!("the session sent a model this build cannot read: {e}"))?;
            (revision, model)
        }
        SyncReply::Unchanged => return Err("the session answered a first sync with 'unchanged'".into()),
    };

    let (tx, rx) = mpsc::channel();
    let label = format!("{} (attached)", session.root);
    let mut revision = first.0;
    std::thread::spawn(move || loop {
        std::thread::sleep(POLL_EVERY);
        if protocol::write_sync_request(&mut writer, revision).is_err() {
            break;
        }
        match protocol::read_sync(&mut reader) {
            Ok(SyncReply::Unchanged) => {}
            Ok(SyncReply::Model { revision: r, bytes }) => {
                let Ok(model) = voxel_core::format::native::decode(&bytes) else {
                    // A model this build cannot read is a version mismatch that
                    // slipped past the handshake. Stop rather than spin on it.
                    eprintln!("voxeler attach: the session sent a model this build cannot read");
                    break;
                };
                revision = r;
                if tx.send(model).is_err() {
                    break; // the window has gone
                }
                wake();
            }
            Err(e) => {
                eprintln!("voxeler attach: the session ended: {e}");
                break;
            }
        }
    });

    Ok((Attached { models: rx, label }, first.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Editor;
    use std::path::PathBuf;

    /// End to end over a real socket: the listener from `attach::server`, this
    /// client, and an edit made in between that has to arrive.
    #[test]
    fn a_viewer_opens_on_the_current_model_and_then_follows_it() {
        let dir = std::env::temp_dir().join("voxeler-attach-client");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut model = VoxelModel::new(8, 8, 8);
        model.set(1, 1, 1, 3);
        let shared = crate::mcp::stdio::Shared::new(Editor::new(model, PathBuf::from("t.vxm")));
        let server = super::super::server::start(shared.clone(), &dir).unwrap();

        let session = Session {
            port: server.port(),
            root: dir.display().to_string(),
            pid: std::process::id(),
            version: crate::VERSION.into(),
        };
        let (attached, first) = connect(&session, Arc::new(|| {})).unwrap();
        assert_eq!(first.get(1, 1, 1), 3, "the window opens on what is there now");
        assert!(attached.latest().is_none(), "and nothing has changed yet");

        {
            let mut live = shared.lock();
            live.editor_mut()
                .apply_batch("t", 9, |_, _| vec![[5, 5, 5]], |_, _| {});
            live.bump();
        }

        let mut seen = None;
        for _ in 0..100 {
            if let Some(m) = attached.latest() {
                seen = Some(m);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(seen.expect("the edit never arrived").get(5, 5, 5), 9);

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only the newest matters: a viewer that fell behind catches up rather
    /// than replaying every step it missed.
    #[test]
    fn only_the_newest_model_is_taken_from_the_queue() {
        let (tx, rx) = mpsc::channel();
        let attached = Attached {
            models: rx,
            label: String::new(),
        };
        for i in 1..=3u8 {
            let mut m = VoxelModel::new(4, 4, 4);
            m.set(0, 0, 0, i);
            tx.send(m).unwrap();
        }
        assert_eq!(attached.latest().unwrap().get(0, 0, 0), 3);
        assert!(attached.latest().is_none(), "and the queue is drained");
    }
}
