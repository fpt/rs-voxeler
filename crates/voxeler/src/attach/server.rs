//! The attach listener that runs inside `voxeler mcp`.
//!
//! Binds a loopback port, publishes a [`Session`] so `voxeler attach` can find
//! it, and answers SYNC requests from however many viewers are watching. Every
//! answer reads the *same* editor the `voxel_*` tools drive, so the agent and
//! the viewers are looking at one model.
//!
//! Two properties are load-bearing:
//!
//! - **Loopback only.** It binds `127.0.0.1`, never `0.0.0.0` — this shows the
//!   user's document to whoever connects, and that must not be reachable
//!   off-box.
//! - **Client-driven.** The server never pushes. With nobody attached it does no
//!   work at all, so attaching is something you can do at any moment without
//!   having changed what the session would otherwise have done.

use std::io::{BufReader, BufWriter};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use super::protocol::{self, Sync};
use crate::mcp::session::Session;
use crate::mcp::stdio::Shared;

/// A published listener. Dropping it removes the session file, so a clean exit
/// leaves nothing for the next `voxeler attach` to trip over.
pub struct AttachServer {
    session_path: PathBuf,
    port: u16,
}

impl AttachServer {
    /// The loopback port the session was published on. Only the tests ask —
    /// everything else finds it through the session file, which is the point of
    /// publishing one.
    #[cfg(test)]
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for AttachServer {
    fn drop(&mut self) {
        let _ = Session::unpublish(&self.session_path, self.port);
    }
}

/// Start listening and publish the session.
///
/// A failure is reported and ignored by the caller: the attach bridge is a
/// convenience, and an agent's session must not die because a port was busy or
/// a cache directory was read-only.
pub fn start(shared: Shared, root: &Path) -> std::io::Result<AttachServer> {
    // Port 0: the OS picks a free one and the session file carries it. A fixed
    // port would mean two projects could not have a server each.
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let session_path = Session::publish_in(&crate::mcp::session::session_dir(), root, port)?;

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let shared = shared.clone();
            std::thread::spawn(move || {
                if let Err(e) = serve(shared, stream) {
                    // A viewer closing its window mid-request is ordinary.
                    if !matches!(
                        e.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::UnexpectedEof
                    ) {
                        eprintln!("voxeler mcp: attach connection ended: {e}");
                    }
                }
            });
        }
    });

    Ok(AttachServer { session_path, port })
}

fn serve(shared: Shared, stream: TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    match protocol::read_hello(&mut reader, &mut writer)? {
        Some(true) => {}
        Some(false) => {
            eprintln!("voxeler mcp: refused an attach with a different protocol version");
            return Ok(());
        }
        // A liveness probe from `voxeler attach`'s discovery. Silence is the
        // right amount of noise for something that happens on every attach.
        None => return Ok(()),
    }
    eprintln!("voxeler mcp: a viewer attached");

    while let Some(have) = protocol::read_request(&mut reader)? {
        // The lock is held only long enough to read the revision and, when it
        // has moved, encode the model — never across the write, which is where
        // a slow or stalled viewer would otherwise block the agent's next tool
        // call.
        let answer = {
            let live = shared.lock();
            if live.revision() == have {
                Sync::Unchanged
            } else {
                Sync::Model {
                    revision: live.revision(),
                    bytes: voxel_core::format::native::encode(live.model()),
                }
            }
        };
        protocol::write_sync(&mut writer, &answer)?;
    }
    eprintln!("voxeler mcp: a viewer detached");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Editor;
    use std::io::Write;
    use std::path::PathBuf;
    use voxel_core::VoxelModel;

    fn shared() -> Shared {
        let mut model = VoxelModel::new(8, 8, 8);
        model.set(1, 2, 3, 7);
        Shared::new(Editor::new(model, PathBuf::from("t.vxm")))
    }

    /// The whole exchange over a real socket: handshake, first sync, the
    /// "nothing changed" answer, and a change the viewer then picks up.
    #[test]
    fn a_viewer_syncs_the_model_and_then_only_what_changed() {
        let dir = std::env::temp_dir().join("voxeler-attach-serve");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("VOXELER_SESSION_DIR", &dir);

        let shared = shared();
        let server = start(shared.clone(), &dir).unwrap();
        let port = server.port();

        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;

        protocol::write_hello(&mut writer).unwrap();
        let (version, ok) = protocol::read_hello_reply(&mut reader).unwrap();
        assert_eq!(version, protocol::PROTOCOL_VERSION);
        assert!(ok);

        // Nothing yet, so anything the server has is news.
        protocol::write_sync_request(&mut writer, 0).unwrap();
        let first = protocol::read_sync(&mut reader).unwrap();
        let Sync::Model { revision, bytes } = first else {
            panic!("expected the model");
        };
        let model = voxel_core::format::native::decode(&bytes).unwrap();
        assert_eq!(model.get(1, 2, 3), 7);

        // Asking again with that revision costs nothing.
        protocol::write_sync_request(&mut writer, revision).unwrap();
        assert_eq!(protocol::read_sync(&mut reader).unwrap(), Sync::Unchanged);

        // An edit through the shared editor is what the viewer is watching for.
        {
            let mut live = shared.lock();
            live.editor_mut()
                .apply_batch("t", 5, |_, _| vec![[4, 4, 4]], |_, _| {});
            live.bump();
        }
        protocol::write_sync_request(&mut writer, revision).unwrap();
        let Sync::Model { bytes, .. } = protocol::read_sync(&mut reader).unwrap() else {
            panic!("expected the changed model");
        };
        assert_eq!(
            voxel_core::format::native::decode(&bytes)
                .unwrap()
                .get(4, 4, 4),
            5
        );

        writer.flush().unwrap();
        drop(writer);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A client from a different build is told so, rather than left to
    /// misparse whatever comes next.
    #[test]
    fn a_client_of_the_wrong_version_is_refused_and_told() {
        let dir = std::env::temp_dir().join("voxeler-attach-version");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let server = start(shared(), &dir).unwrap();
        let stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;

        writer
            .write_all(&[protocol::MSG_HELLO, protocol::PROTOCOL_VERSION + 7])
            .unwrap();
        let (_, ok) = protocol::read_hello_reply(&mut reader).unwrap();
        assert!(!ok);

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
