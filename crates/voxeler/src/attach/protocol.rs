//! The wire protocol between a running `voxeler mcp` and an attached
//! `voxeler attach`.
//!
//! ```text
//! HELLO →  [0x00][version u8]
//!       ←  [version u8][ok u8]
//!
//! SYNC  →  [0x01][have_revision u64 LE]
//!       ←  [0x00]                                   unchanged
//!       ←  [0x01][revision u64 LE][len u32 LE][.vxm bytes]
//! ```
//!
//! # Why the model and not the picture
//!
//! `kessel attach` streams framebuffers, because a fantasy console renders a
//! 320² indexed screen and 57 KiB a frame over loopback is nothing. This editor
//! renders up to 1.4 million pixels, which is 5 MiB a frame — hopeless. So the
//! *model* goes over the wire instead and the client renders it, which also puts
//! the camera where it belongs: orbiting is the viewer's business, and a view
//! that needed a round trip per mouse move would be unusable.
//!
//! # Why the whole model every time
//!
//! The bytes are `format::native::encode` — the same sparse `.vxm` the editor
//! saves, so a typical model is a few kilobytes and carries its layers, palette
//! and names with no second encoding to keep in agreement. A diff would trade
//! nothing measurable for a second piece of state both sides have to agree on,
//! and this one already has a revision counter to make "nothing changed" free.
//!
//! # Client-driven
//!
//! The server never pushes. With nobody attached it does no work at all, and an
//! agent's session behaves exactly as it does with no viewer — which is what
//! makes attaching something you can do at any moment, including halfway
//! through a build.

use std::io::{self, Read, Write};

/// Bumped on any incompatible change. A mismatched client is refused with a
/// clear message rather than left to misparse the stream.
pub const PROTOCOL_VERSION: u8 = 1;

pub const MSG_HELLO: u8 = 0x00;
pub const MSG_SYNC: u8 = 0x01;

/// A model larger than this is a framing error, not a model: the volume ceiling
/// is 256³, and even a solid one of those is 64 MiB of sparse records.
pub const MAX_MODEL_BYTES: u32 = 96 * 1024 * 1024;

/// The answer to a [`MSG_SYNC`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sync {
    /// The client's revision is current; nothing to send.
    Unchanged,
    Model { revision: u64, bytes: Vec<u8> },
}

pub fn write_hello(w: &mut impl Write) -> io::Result<()> {
    w.write_all(&[MSG_HELLO, PROTOCOL_VERSION])?;
    w.flush()
}

/// Read a HELLO and answer it.
///
/// `Ok(None)` means the peer hung up without saying anything, which is not a
/// fault: `Session::is_live` decides whether a server is running by *connecting*
/// to it, so every discovery leaves one of these behind. Treating it as an error
/// makes the server log a failure every time someone runs `voxeler attach`.
///
/// `Ok(Some(false))` means the versions disagree, which the caller reports and
/// then hangs up on.
pub fn read_hello(r: &mut impl Read, w: &mut impl Write) -> io::Result<Option<bool>> {
    let mut buf = [0u8; 2];
    match r.read_exact(&mut buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let ok = buf[0] == MSG_HELLO && buf[1] == PROTOCOL_VERSION;
    w.write_all(&[PROTOCOL_VERSION, u8::from(ok)])?;
    w.flush()?;
    Ok(Some(ok))
}

/// Read the answer to a HELLO, returning the server's version and whether it
/// accepted ours.
pub fn read_hello_reply(r: &mut impl Read) -> io::Result<(u8, bool)> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)?;
    Ok((buf[0], buf[1] != 0))
}

pub fn write_sync_request(w: &mut impl Write, have: u64) -> io::Result<()> {
    let mut buf = [0u8; 9];
    buf[0] = MSG_SYNC;
    buf[1..].copy_from_slice(&have.to_le_bytes());
    w.write_all(&buf)?;
    w.flush()
}

/// Read one request. `Ok(None)` at a clean end of stream, which is how an
/// attached viewer ordinarily leaves.
pub fn read_request(r: &mut impl Read) -> io::Result<Option<u64>> {
    let mut kind = [0u8; 1];
    match r.read_exact(&mut kind) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    if kind[0] != MSG_SYNC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected message {:#04x}", kind[0]),
        ));
    }
    let mut have = [0u8; 8];
    r.read_exact(&mut have)?;
    Ok(Some(u64::from_le_bytes(have)))
}

pub fn write_sync(w: &mut impl Write, sync: &Sync) -> io::Result<()> {
    match sync {
        Sync::Unchanged => w.write_all(&[0])?,
        Sync::Model { revision, bytes } => {
            let mut head = [0u8; 13];
            head[0] = 1;
            head[1..9].copy_from_slice(&revision.to_le_bytes());
            head[9..].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
            w.write_all(&head)?;
            w.write_all(bytes)?;
        }
    }
    w.flush()
}

pub fn read_sync(r: &mut impl Read) -> io::Result<Sync> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)?;
    if tag[0] == 0 {
        return Ok(Sync::Unchanged);
    }
    let mut head = [0u8; 12];
    r.read_exact(&mut head)?;
    let revision = u64::from_le_bytes(head[..8].try_into().unwrap());
    let len = u32::from_le_bytes(head[8..].try_into().unwrap());
    // Checked before allocating: a corrupt length must be an error, not an
    // attempt to reserve four gigabytes.
    if len > MAX_MODEL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("model of {len} bytes is past the {MAX_MODEL_BYTES} limit"),
        ));
    }
    let mut bytes = vec![0; len as usize];
    r.read_exact(&mut bytes)?;
    Ok(Sync::Model { revision, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_handshake_is_accepted_and_a_mismatched_one_refused() {
        let mut out = Vec::new();
        assert_eq!(
            read_hello(&mut &[MSG_HELLO, PROTOCOL_VERSION][..], &mut out).unwrap(),
            Some(true)
        );
        assert_eq!(read_hello_reply(&mut &out[..]).unwrap(), (PROTOCOL_VERSION, true));

        let mut out = Vec::new();
        assert_eq!(
            read_hello(&mut &[MSG_HELLO, PROTOCOL_VERSION + 9][..], &mut out).unwrap(),
            Some(false)
        );
        // Refused, but still *answered*: a client that gets silence cannot tell
        // a version mismatch from a crash.
        assert_eq!(read_hello_reply(&mut &out[..]).unwrap(), (PROTOCOL_VERSION, false));
    }

    #[test]
    fn a_hello_written_here_is_the_one_read_there() {
        let mut wire = Vec::new();
        write_hello(&mut wire).unwrap();
        let mut reply = Vec::new();
        assert_eq!(read_hello(&mut &wire[..], &mut reply).unwrap(), Some(true));
    }

    /// Liveness is decided by connecting, so every `voxeler attach` leaves a
    /// connection that says nothing. It must not read as a failure.
    #[test]
    fn a_peer_that_hangs_up_without_speaking_is_a_probe_not_a_fault() {
        let mut reply = Vec::new();
        assert_eq!(read_hello(&mut &[][..], &mut reply).unwrap(), None);
        assert!(reply.is_empty(), "and it is not worth answering");
    }

    #[test]
    fn a_sync_request_carries_the_revision_the_client_already_has() {
        let mut wire = Vec::new();
        write_sync_request(&mut wire, 0xdead_beef).unwrap();
        assert_eq!(read_request(&mut &wire[..]).unwrap(), Some(0xdead_beef));
    }

    #[test]
    fn an_empty_stream_is_a_clean_end_rather_than_an_error() {
        assert_eq!(read_request(&mut &[][..]).unwrap(), None);
    }

    #[test]
    fn a_message_this_protocol_has_no_name_for_is_an_error() {
        assert!(read_request(&mut &[0x7f, 0, 0, 0, 0, 0, 0, 0, 0][..]).is_err());
    }

    #[test]
    fn both_answers_to_a_sync_round_trip() {
        for sync in [
            Sync::Unchanged,
            Sync::Model {
                revision: 42,
                bytes: b"VXM2 and then some".to_vec(),
            },
            // An empty model is a real model, and a length of zero must not be
            // mistaken for "unchanged".
            Sync::Model {
                revision: 1,
                bytes: Vec::new(),
            },
        ] {
            let mut wire = Vec::new();
            write_sync(&mut wire, &sync).unwrap();
            assert_eq!(read_sync(&mut &wire[..]).unwrap(), sync);
        }
    }

    /// A corrupt length must be refused before it becomes an allocation.
    #[test]
    fn an_absurd_length_is_an_error_not_an_allocation() {
        let mut wire = vec![1u8];
        wire.extend_from_slice(&7u64.to_le_bytes());
        wire.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_sync(&mut &wire[..]).is_err());
    }

    /// Several exchanges down one connection: the framing has to leave the
    /// stream exactly where the next read expects it.
    #[test]
    fn the_stream_stays_in_step_across_several_exchanges() {
        let mut wire = Vec::new();
        write_sync(&mut wire, &Sync::Unchanged).unwrap();
        write_sync(
            &mut wire,
            &Sync::Model {
                revision: 2,
                bytes: vec![1, 2, 3],
            },
        )
        .unwrap();
        write_sync(&mut wire, &Sync::Unchanged).unwrap();

        let mut r = &wire[..];
        assert_eq!(read_sync(&mut r).unwrap(), Sync::Unchanged);
        assert!(matches!(read_sync(&mut r).unwrap(), Sync::Model { revision: 2, .. }));
        assert_eq!(read_sync(&mut r).unwrap(), Sync::Unchanged);
        assert!(r.is_empty());
    }
}
