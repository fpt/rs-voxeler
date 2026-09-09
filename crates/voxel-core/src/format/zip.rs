//! Just enough ZIP to write a container, and no more.
//!
//! 3MF is a ZIP of XML, so writing one needs an archive writer. This is the
//! same trade the PNG encoder makes next door: the alternative is a dependency
//! that reads archives, streams them, decompresses six methods and handles
//! encryption, for a format we only ever *write*, with a handful of small
//! entries, in one shape.
//!
//! **Stored, never deflated.** A ZIP entry may be stored uncompressed and every
//! reader accepts it, so the compressor is the one part that can be left out
//! entirely. A 3MF of a large model is then bigger than it needs to be, which
//! is a thing to fix by adding deflate here if it ever matters — not a reason
//! to take a dependency now.
//!
//! No Zip64: the offsets are `u32`, so an archive stops at 4 GiB. A voxel model
//! that reached it would have exhausted a great deal else first.

/// CRC-32, the one every ZIP entry carries.
///
/// Computed on the fly rather than from a table built at startup: the entries
/// here are a few tens of kilobytes, and a 1 KiB table to save arithmetic
/// nobody will notice is a trade in the wrong direction.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            // The reflected polynomial, which is what ZIP uses.
            crc = (crc >> 1) ^ (0xEDB8_8320 & (!(crc & 1)).wrapping_add(1));
        }
    }
    !crc
}

struct Entry {
    name: String,
    crc: u32,
    size: u32,
    offset: u32,
}

/// Build an archive one file at a time.
#[derive(Default)]
pub struct Zip {
    out: Vec<u8>,
    entries: Vec<Entry>,
}

impl Zip {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, name: &str, data: &[u8]) {
        let offset = self.out.len() as u32;
        let crc = crc32(data);
        self.entries.push(Entry {
            name: name.to_string(),
            crc,
            size: data.len() as u32,
            offset,
        });
        self.out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local header
        self.out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        self.out.extend_from_slice(&0u16.to_le_bytes()); // flags
        self.out.extend_from_slice(&0u16.to_le_bytes()); // stored
                                                         // No timestamp. A file whose bytes depend on when it was written cannot
                                                         // be compared with the one written a moment ago, and an exporter whose
                                                         // output is reproducible is one a test can assert on.
        self.out.extend_from_slice(&0u16.to_le_bytes()); // time
        self.out.extend_from_slice(&0u16.to_le_bytes()); // date
        self.out.extend_from_slice(&crc.to_le_bytes());
        self.out
            .extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.out
            .extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.out
            .extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // extra
        self.out.extend_from_slice(name.as_bytes());
        self.out.extend_from_slice(data);
    }

    /// Close the archive: the central directory, then the end record.
    pub fn finish(mut self) -> Vec<u8> {
        let start = self.out.len() as u32;
        for e in &self.entries {
            self.out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            self.out.extend_from_slice(&20u16.to_le_bytes()); // made by
            self.out.extend_from_slice(&20u16.to_le_bytes()); // needed
            self.out.extend_from_slice(&0u16.to_le_bytes()); // flags
            self.out.extend_from_slice(&0u16.to_le_bytes()); // stored
            self.out.extend_from_slice(&0u16.to_le_bytes()); // time
            self.out.extend_from_slice(&0u16.to_le_bytes()); // date
            self.out.extend_from_slice(&e.crc.to_le_bytes());
            self.out.extend_from_slice(&e.size.to_le_bytes());
            self.out.extend_from_slice(&e.size.to_le_bytes());
            self.out
                .extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            self.out.extend_from_slice(&0u16.to_le_bytes()); // extra
            self.out.extend_from_slice(&0u16.to_le_bytes()); // comment
            self.out.extend_from_slice(&0u16.to_le_bytes()); // disk
            self.out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            self.out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            self.out.extend_from_slice(&e.offset.to_le_bytes());
            self.out.extend_from_slice(e.name.as_bytes());
        }
        let size = self.out.len() as u32 - start;
        let n = self.entries.len() as u16;
        self.out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // disk
        self.out.extend_from_slice(&0u16.to_le_bytes()); // directory disk
        self.out.extend_from_slice(&n.to_le_bytes());
        self.out.extend_from_slice(&n.to_le_bytes());
        self.out.extend_from_slice(&size.to_le_bytes());
        self.out.extend_from_slice(&start.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // comment
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published check value for CRC-32. Getting the polynomial reflected
    /// the wrong way gives a plausible-looking number that no reader accepts,
    /// and the failure surfaces as "the archive is corrupt" with nothing to
    /// point at.
    #[test]
    fn crc_matches_the_published_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    /// A reader finds entries through the central directory, so the offsets in
    /// it have to point at the local headers exactly.
    #[test]
    fn the_directory_points_at_every_local_header() {
        let mut z = Zip::new();
        z.add("a.txt", b"hello");
        z.add("dir/b.txt", b"world!");
        let bytes = z.finish();

        // Walk the directory the way a reader does: find the end record, take
        // the offset, and check each entry lands on a local header.
        let eocd = bytes.len() - 22;
        assert_eq!(&bytes[eocd..eocd + 4], &0x0605_4b50u32.to_le_bytes());
        let count = u16::from_le_bytes([bytes[eocd + 10], bytes[eocd + 11]]);
        assert_eq!(count, 2);
        let mut at = u32::from_le_bytes([
            bytes[eocd + 16],
            bytes[eocd + 17],
            bytes[eocd + 18],
            bytes[eocd + 19],
        ]) as usize;
        for expected in ["a.txt", "dir/b.txt"] {
            assert_eq!(&bytes[at..at + 4], &0x0201_4b50u32.to_le_bytes());
            let name_len = u16::from_le_bytes([bytes[at + 28], bytes[at + 29]]) as usize;
            let local = u32::from_le_bytes([
                bytes[at + 42],
                bytes[at + 43],
                bytes[at + 44],
                bytes[at + 45],
            ]) as usize;
            assert_eq!(&bytes[at + 46..at + 46 + name_len], expected.as_bytes());
            assert_eq!(
                &bytes[local..local + 4],
                &0x0403_4b50u32.to_le_bytes(),
                "{expected} does not point at a local header"
            );
            at += 46 + name_len;
        }
    }

    /// Nothing in the output may depend on the clock, or two exports of the
    /// same model differ and no test can assert on the bytes.
    #[test]
    fn the_same_input_gives_the_same_bytes() {
        let build = || {
            let mut z = Zip::new();
            z.add("only.xml", b"<x/>");
            z.finish()
        };
        assert_eq!(build(), build());
    }
}
