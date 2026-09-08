//! A minimal PNG writer, so a framebuffer can leave the process as a picture.
//!
//! Used for model thumbnails and for looking at what the renderer produced
//! without opening a window — which is also how the renderer gets checked on a
//! machine with no display.
//!
//! The deflate stream is *stored* blocks only: no compression, just the
//! framing. A real compressor is a large amount of code for output that is
//! written once and looked at once, and PNG's own filtering already costs
//! nothing to skip. The file is bigger than it needs to be and every decoder
//! reads it.

/// Encode a `0RGB` framebuffer as an 8-bit RGB PNG.
pub fn encode(width: u32, height: u32, pixels: &[u32]) -> Vec<u8> {
    assert_eq!(
        pixels.len(),
        (width as usize) * (height as usize),
        "pixel count must match the image size"
    );

    // Each scanline is preceded by its filter byte; 0 is "none".
    let mut raw = Vec::with_capacity((height as usize) * (1 + width as usize * 3));
    for y in 0..height as usize {
        raw.push(0);
        for x in 0..width as usize {
            let p = pixels[y * width as usize + x];
            raw.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolour, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Write `pixels` to `path` as a PNG.
pub fn write(
    path: &std::path::Path,
    width: u32,
    height: u32,
    pixels: &[u32],
) -> std::io::Result<()> {
    std::fs::write(path, encode(width, height, pixels))
}

fn chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(id);
    out.extend_from_slice(body);
    out.extend_from_slice(&crc32(&out[start..]).to_be_bytes());
}

/// A zlib stream of stored deflate blocks. Each block carries at most 65 535
/// bytes, its length, and that length's complement.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // deflate, 32K window, no preset dictionary
    let mut chunks = data.chunks(0xFFFF).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    }
    while let Some(block) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none())); // BFINAL on the last block
        out.extend_from_slice(&(block.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + *byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    b << 16 | a
}

fn crc32(data: &[u8]) -> u32 {
    // The table is built on the fly; a 256-entry constant would be more code
    // than the polynomial it encodes, for a function called four times.
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_starts_with_the_png_signature_and_ends_with_iend() {
        let png = encode(2, 2, &[0; 4]);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    /// Known-answer checks: both checksums are easy to get subtly wrong and
    /// impossible to notice without a decoder.
    #[test]
    fn the_checksums_match_their_published_values() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(adler32(b"abc"), 0x024D_0127);
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn the_ihdr_records_the_size_it_was_given() {
        let png = encode(3, 5, &[0; 15]);
        // 8 signature + 4 length + 4 id, then width and height.
        assert_eq!(&png[16..20], &3u32.to_be_bytes());
        assert_eq!(&png[20..24], &5u32.to_be_bytes());
    }

    /// A stored block holds at most 65 535 bytes, so an image whose raw data
    /// exceeds that must produce several — and only the last may be final.
    #[test]
    fn a_large_image_is_split_into_several_stored_blocks() {
        let w = 400u32;
        let h = 100u32;
        let raw_len = h as usize * (1 + w as usize * 3);
        assert!(raw_len > 0xFFFF, "the fixture must actually span blocks");

        let z = zlib_stored(&vec![0u8; raw_len]);
        // 2 header + n * (5 + payload) + 4 adler
        let blocks = raw_len.div_ceil(0xFFFF);
        assert_eq!(z.len(), 2 + raw_len + blocks * 5 + 4);
        assert_eq!(z[2], 0, "the first of several blocks is not final");

        let png = encode(w, h, &vec![0u32; (w * h) as usize]);
        assert!(png.len() > raw_len);
    }

    #[test]
    fn pixels_are_written_as_r_g_b_in_row_order() {
        let png = encode(1, 1, &[0x00AA_BBCC]);
        // The single scanline is a filter byte then one RGB triple, stored
        // uncompressed, so it appears verbatim in the IDAT payload.
        let idat = png
            .windows(4)
            .position(|w| w == b"IDAT")
            .expect("an IDAT chunk");
        let body = &png[idat + 4..];
        assert!(
            body.windows(4).any(|w| w == [0x00, 0xAA, 0xBB, 0xCC]),
            "expected filter 0 followed by AA BB CC"
        );
    }
}
