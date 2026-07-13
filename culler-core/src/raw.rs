//! RAW container parsing: locate the full-size JPEG preview a RAW file embeds,
//! so the existing JPEG decode path can render it. Pure, GUI-free, dependency-
//! free — byte parsing only. Fuji RAF is the only format supported today;
//! `preview_supported` is the seam where more plug in.

/// True if a RAW file with this lower-cased, no-dot extension has an embedded-
/// preview extractor. Extension-gated because `scan` groups by extension and
/// must decide cullability without opening every file; the real decode still
/// content-dispatches through `embedded_jpeg`, so a mislabeled file fails closed.
pub fn preview_supported(ext_lower: &str) -> bool {
    matches!(ext_lower, "raf")
}

/// The 16-byte magic every Fuji RAF starts with.
const RAF_MAGIC: &[u8; 16] = b"FUJIFILMCCD-RAW ";
/// Big-endian u32 embedded-JPEG *offset* field position in the RAF header.
const RAF_JPEG_OFFSET_POS: usize = 84;
/// Big-endian u32 embedded-JPEG *length* field position.
const RAF_JPEG_LENGTH_POS: usize = 88;

/// The full-size embedded JPEG a Fuji RAF carries, as a zero-copy slice of
/// `data`. `None` (never a panic) for a non-RAF file, a too-short header, an
/// out-of-bounds or zero-length preview pointer, or a slice that doesn't itself
/// start with a JPEG SOI. The returned slice is a complete JPEG (its own EXIF /
/// orientation intact), ready for `decode`'s existing JPEG internals.
pub fn embedded_jpeg(data: &[u8]) -> Option<&[u8]> {
    // Need through byte 91 for both header fields; the magic check then can't panic.
    if data.len() < RAF_JPEG_LENGTH_POS + 4 || &data[..16] != RAF_MAGIC {
        return None;
    }
    let off = u32::from_be_bytes(
        data[RAF_JPEG_OFFSET_POS..RAF_JPEG_OFFSET_POS + 4].try_into().ok()?,
    ) as usize;
    let len = u32::from_be_bytes(
        data[RAF_JPEG_LENGTH_POS..RAF_JPEG_LENGTH_POS + 4].try_into().ok()?,
    ) as usize;
    if len == 0 {
        return None;
    }
    let end = off.checked_add(len)?;
    let slice = data.get(off..end)?;
    // The embedded preview must itself be a JPEG (SOI FF D8 FF).
    if slice.len() >= 3 && slice[0] == 0xFF && slice[1] == 0xD8 && slice[2] == 0xFF {
        Some(slice)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal synthetic RAF: 16-byte magic, header zero-padded to `jpeg_off`,
    /// then `jpeg`; the offset(@84)/length(@88) fields point at it (big-endian).
    fn synth_raf(jpeg: &[u8], jpeg_off: usize) -> Vec<u8> {
        assert!(jpeg_off >= 92, "the offset/length header fields occupy bytes 84..92");
        let mut raf = Vec::new();
        raf.extend_from_slice(RAF_MAGIC);
        raf.resize(jpeg_off, 0);
        raf[84..88].copy_from_slice(&(jpeg_off as u32).to_be_bytes());
        raf[88..92].copy_from_slice(&(jpeg.len() as u32).to_be_bytes());
        raf.extend_from_slice(jpeg);
        raf
    }

    fn fake_jpeg() -> Vec<u8> {
        vec![0xFF, 0xD8, 0xFF, 0xEE, 0x11, 0x22, 0x33, 0xFF, 0xD9]
    }

    #[test]
    fn preview_supported_is_raf_only() {
        assert!(preview_supported("raf"));
        assert!(!preview_supported("cr3"));
        assert!(!preview_supported("nef"));
        assert!(!preview_supported("jpg"));
        assert!(!preview_supported(""));
    }

    #[test]
    fn embedded_jpeg_extracts_the_slice() {
        let jpeg = fake_jpeg();
        let raf = synth_raf(&jpeg, 128);
        assert_eq!(embedded_jpeg(&raf), Some(jpeg.as_slice()));
    }

    #[test]
    fn embedded_jpeg_rejects_non_raf_and_short_inputs() {
        assert_eq!(embedded_jpeg(b"not a raf file at all, but long enough........................................................"), None);
        assert_eq!(embedded_jpeg(b"FUJIFILMCCD-RAW "), None); // magic only, no header fields
        assert_eq!(embedded_jpeg(&[]), None);
    }

    #[test]
    fn embedded_jpeg_rejects_bad_pointers() {
        let jpeg = fake_jpeg();
        // Zero length.
        let mut zero = synth_raf(&jpeg, 128);
        zero[88..92].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(embedded_jpeg(&zero), None);
        // Offset+length past EOF.
        let mut over = synth_raf(&jpeg, 128);
        over[88..92].copy_from_slice(&9999u32.to_be_bytes());
        assert_eq!(embedded_jpeg(&over), None);
    }

    #[test]
    fn embedded_jpeg_rejects_non_jpeg_slice() {
        // Slice bytes present and in-bounds, but not a JPEG SOI.
        let not_jpeg = vec![0x00, 0x01, 0x02, 0x03];
        let raf = synth_raf(&not_jpeg, 128);
        assert_eq!(embedded_jpeg(&raf), None);
    }
}
