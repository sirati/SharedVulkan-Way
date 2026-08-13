//! A PNG, wrapped in an `.ico` so a Windows shortcut can wear it.
//!
//! The pixels come from the *host's* icon theme, which is §16's finding in a
//! new place: `xdg_toplevel.set_app_id` names a desktop entry that exists only
//! on the machine the application runs on, so the icon has to cross the
//! hypervisor boundary in both the taskbar's case and the shortcut's. Stage 11
//! ships them over `wc_icon_manager_v1` at run time; a shortcut needs a file on
//! disk, once, at install time, and `wc-session icon` is the same lookup on the
//! same theme.
//!
//! **No re-encoding.** An ICO directory entry may point at a PNG rather than at
//! a DIB — Windows has read that since Vista — so this is a 22-byte header in
//! front of the bytes the host sent. A resampler here would be a second image
//! decoder in a program whose job is to start another program quickly.

/// Wrap one PNG in a single-image `.ico`.
///
/// `width`/`height` are recorded in the directory entry; 256 and above are
/// written as 0, which is how the format spells "256 or larger".
pub fn wrap_png(png: &[u8]) -> Result<Vec<u8>, String> {
    let (w, h) = png_size(png)?;
    let mut out = Vec::with_capacity(png.len() + 22);
    // ICONDIR: reserved, type 1 (icon), one image.
    out.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
    // ICONDIRENTRY.
    out.push(if w >= 256 { 0 } else { w as u8 });
    out.push(if h >= 256 { 0 } else { h as u8 });
    out.push(0); // palette size: none, it is a PNG
    out.push(0); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&(png.len() as u32).to_le_bytes());
    out.extend_from_slice(&22u32.to_le_bytes()); // the image starts after this
    out.extend_from_slice(png);
    Ok(out)
}

/// Read a PNG's `IHDR`, and refuse anything that is not one. The alternative —
/// trusting the sender — would put a shortcut with a corrupt icon on the
/// desktop, and Explorer's answer to that is a blank rectangle with no
/// diagnosis anywhere.
pub fn png_size(png: &[u8]) -> Result<(u32, u32), String> {
    const MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if png.len() < 24 || png[..8] != MAGIC {
        return Err("not a PNG".into());
    }
    if &png[12..16] != b"IHDR" {
        return Err("PNG without an IHDR first".into());
    }
    let n = |o: usize| u32::from_be_bytes([png[o], png[o + 1], png[o + 2], png[o + 3]]);
    let (w, h) = (n(16), n(20));
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return Err(format!("PNG is {w}x{h}"));
    }
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 128x128 PNG header, enough for the parts this reads.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v.extend_from_slice(&[0; 4]);
        v
    }

    #[test]
    fn the_wrapper_is_twenty_two_bytes_and_the_payload_is_untouched() {
        let p = png(128, 128);
        let ico = wrap_png(&p).unwrap();
        assert_eq!(ico.len(), p.len() + 22);
        assert_eq!(&ico[22..], &p[..]);
        assert_eq!(&ico[..6], &[0, 0, 1, 0, 1, 0]);
        assert_eq!(ico[6], 128);
        assert_eq!(ico[7], 128);
        assert_eq!(u32::from_le_bytes(ico[14..18].try_into().unwrap()), p.len() as u32);
        assert_eq!(u32::from_le_bytes(ico[18..22].try_into().unwrap()), 22);
    }

    #[test]
    fn two_hundred_and_fifty_six_is_written_as_zero() {
        let ico = wrap_png(&png(256, 256)).unwrap();
        assert_eq!((ico[6], ico[7]), (0, 0));
    }

    #[test]
    fn anything_that_is_not_a_png_is_refused_rather_than_wrapped() {
        assert!(wrap_png(b"<html>not found</html>").is_err());
        assert!(wrap_png(&[]).is_err());
        let mut bad = png(64, 64);
        bad[12] = b'X';
        assert!(wrap_png(&bad).is_err());
        assert!(wrap_png(&png(0, 64)).is_err());
    }
}
