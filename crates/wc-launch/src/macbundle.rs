//! §103.8. What the macOS launcher writes for one Linux application: an `.app`
//! bundle whose identity is the application's, so its Dock tile has the
//! application's name and icon, and "Keep in Dock" pins *it*. Pure, so on
//! every target and tested here; `mac.rs` only writes these bytes to disk.
//!
//! A bundle, not a per-window property, because macOS has nothing like
//! Windows' AppUserModelID: a Dock tile is a running process, named and iconed
//! by the bundle it was started from. So each Linux application gets a bundle,
//! the bundle's executable is a hard link to this launcher, and the launcher
//! reads which application it is from the bundle's own `Info.plist`.

use crate::appmeta::Meta;

/// Where an app's bundles live, under the user's home.
pub const APPS_DIR: &str = "Applications/Linux";
/// The `Info.plist` key naming the `[app.*]` section the bundle starts.
pub const APP_KEY: &str = "WCApp";
/// The executable's name inside `Contents/MacOS`.
pub const EXECUTABLE: &str = "wc-launch";

/// `org.waycompose.linux.<id>`, with the id reduced to what a bundle
/// identifier may hold (letters, digits, `-` and `.`).
pub fn bundle_id(section: &str) -> String {
    let id: String = section
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '-' })
        .collect();
    format!("org.waycompose.linux.{}", id.trim_matches('.'))
}

/// A file name for the bundle: the application's name, without the
/// characters a path or Finder would trip over.
pub fn bundle_name(name: &str) -> String {
    let n: String = name
        .chars()
        .filter(|c| !matches!(c, '/' | ':' | '\0'))
        .collect::<String>()
        .trim()
        .to_string();
    if n.is_empty() { "Linux Application".into() } else { n }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The bundle's `Info.plist`.
pub fn info_plist(section: &str, name: &str) -> String {
    let id = bundle_id(section);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>{exe}</string>
	<key>CFBundleIdentifier</key>
	<string>{id}</string>
	<key>CFBundleName</key>
	<string>{name}</string>
	<key>CFBundleDisplayName</key>
	<string>{name}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleIconFile</key>
	<string>app.icns</string>
	<key>CFBundleShortVersionString</key>
	<string>1.0</string>
	<key>CFBundleVersion</key>
	<string>1</string>
	<key>LSMinimumSystemVersion</key>
	<string>14.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>{key}</key>
	<string>{section}</string>
</dict>
</plist>
"#,
        exe = EXECUTABLE,
        id = xml(&id),
        name = xml(name),
        key = APP_KEY,
        section = xml(section),
    )
}

/// The `[app.*]` section a bundle's `Info.plist` names, read back the way
/// [`info_plist`] wrote it. Not a general plist parser: this launcher wrote
/// the file, and a bundle it did not write is not one it should start.
pub fn section_from_plist(plist: &str) -> Option<String> {
    let at = plist.find(&format!("<key>{APP_KEY}</key>"))?;
    let rest = &plist[at..];
    let s = rest.find("<string>")? + "<string>".len();
    let e = rest[s..].find("</string>")?;
    let v = rest[s..s + e]
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&");
    (!v.is_empty()).then_some(v)
}

/// An `.icns` holding one PNG. Since 10.7 the PNG types are read directly;
/// the type code says the size, so the PNG must be exactly that size, which
/// [`square_png`] makes sure of.
pub fn icns(png: &[u8], size: u32) -> Result<Vec<u8>, String> {
    let kind: &[u8; 4] = match size {
        16 => b"icp4",
        32 => b"icp5",
        64 => b"icp6",
        128 => b"ic07",
        256 => b"ic08",
        512 => b"ic09",
        1024 => b"ic10",
        _ => return Err(format!("no icns type for {size}x{size}")),
    };
    let (w, h) = crate::ico::png_size(png)?;
    if (w, h) != (size, size) {
        return Err(format!("a {w}x{h} PNG in a {size}x{size} slot"));
    }
    let entry = 8 + png.len();
    let total = 8 + entry;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(&(entry as u32).to_be_bytes());
    out.extend_from_slice(png);
    Ok(out)
}

/// Re-encode any PNG as a `size`x`size` RGBA PNG: centred, aspect kept,
/// box-filtered down or nearest up. Icon themes ship 48, 64, 128 and 256
/// pixel icons and the bundle wants one exact size.
pub fn square_png(png: &[u8], size: u32) -> Result<Vec<u8>, String> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(png));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("PNG too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width as usize, info.height as usize);
    let rgba: Vec<[u8; 4]> = match info.color_type {
        png::ColorType::Rgba => buf.chunks_exact(4).map(|p| [p[0], p[1], p[2], p[3]]).collect(),
        png::ColorType::Rgb => buf.chunks_exact(3).map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.chunks_exact(2).map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buf.iter().map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("indexed PNG after normalisation".into()),
    };
    let n = size as usize;
    let scale = (w.max(h) as f64) / (n as f64);
    let (dw, dh) = (((w as f64) / scale).round() as usize, ((h as f64) / scale).round() as usize);
    let (ox, oy) = ((n - dw.min(n)) / 2, (n - dh.min(n)) / 2);
    let mut out = vec![0u8; n * n * 4];
    for y in 0..dh.min(n) {
        for x in 0..dw.min(n) {
            // The source box this destination pixel covers; one pixel when
            // scaling up.
            let (sx0, sy0) = ((x as f64 * scale) as usize, (y as f64 * scale) as usize);
            let (sx1, sy1) = ((((x + 1) as f64) * scale).ceil() as usize, (((y + 1) as f64) * scale).ceil() as usize);
            let (sx1, sy1) = (sx1.clamp(sx0 + 1, w), sy1.clamp(sy0 + 1, h));
            let mut acc = [0u64; 4];
            let mut cnt = 0u64;
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let p = rgba[sy * w + sx];
                    // Premultiply while averaging, so transparent pixels do not
                    // bleed their colour into the edge.
                    let a = u64::from(p[3]);
                    acc[0] += u64::from(p[0]) * a;
                    acc[1] += u64::from(p[1]) * a;
                    acc[2] += u64::from(p[2]) * a;
                    acc[3] += a;
                    cnt += 1;
                }
            }
            let o = ((oy + y) * n + ox + x) * 4;
            if acc[3] > 0 {
                out[o] = (acc[0] / acc[3]) as u8;
                out[o + 1] = (acc[1] / acc[3]) as u8;
                out[o + 2] = (acc[2] / acc[3]) as u8;
            }
            out[o + 3] = (acc[3] / cnt.max(1)) as u8;
        }
    }
    let mut png_out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_out, size, size);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().map_err(|e| e.to_string())?;
        wr.write_image_data(&out).map_err(|e| e.to_string())?;
    }
    Ok(png_out)
}

/// What the Dock menu shows for an application: the desktop entry's actions,
/// then the frequent pages (§92's jump list, in the Dock). The key is what the
/// launcher turns back into a request: `action:<id>` or `open:<url>`.
pub fn dock_items(meta: &Meta, pages: &[crate::appmeta::Page]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> =
        meta.actions.iter().map(|a| (a.name.clone(), format!("action:{}", a.id))).collect();
    for p in pages.iter().take(8) {
        if crate::appmeta::url_ok(&p.url) {
            let t = if p.title.trim().is_empty() { p.url.clone() } else { p.title.clone() };
            v.push((t, format!("open:{}", p.url)));
        }
    }
    v
}

/// A Dock-menu key back into a launch request.
pub fn request_for(key: &str) -> crate::Request<'_> {
    if let Some(id) = key.strip_prefix("action:") {
        crate::Request::Action(id)
    } else if let Some(url) = key.strip_prefix("open:") {
        crate::Request::Open(url)
    } else {
        crate::Request::Run
    }
}

/// `(run id, pages)` from the compositor's arena line, which is what the
/// launcher hands to `wc-session up` (§103.8).
pub fn arena_of(line: &str) -> Option<(String, String)> {
    let run = line.split("run ").nth(1)?.split(',').next()?.trim().to_string();
    let pages = line.split(" pages").next()?.rsplit(' ').next()?.to_string();
    (run.starts_with("0x") && !pages.is_empty() && pages.chars().all(|c| c.is_ascii_digit())).then_some((run, pages))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_arena_line_gives_run_and_pages() {
        let l = "wc-compositor: arena 0 provided from guest RAM: run 0x30731521103037c5, 67108864 bytes, 16384 pages, locked=true, pinning=wired";
        assert_eq!(super::arena_of(l), Some(("0x30731521103037c5".into(), "16384".into())));
        assert_eq!(super::arena_of("wc-compositor: listening on wl-x"), None);
    }

    use super::*;

    #[test]
    fn bundle_ids_are_reverse_dns_and_safe() {
        assert_eq!(bundle_id("firefox"), "org.waycompose.linux.firefox");
        assert_eq!(bundle_id("weston term"), "org.waycompose.linux.weston-term");
        assert_eq!(bundle_name("Firefox / Nightly"), "Firefox  Nightly");
        assert_eq!(bundle_name("  "), "Linux Application");
    }

    #[test]
    fn the_plist_names_the_section_and_reads_back() {
        let p = info_plist("firefox", "Firefox <Linux> & co");
        assert!(p.contains("<string>org.waycompose.linux.firefox</string>"));
        assert!(p.contains("Firefox &lt;Linux&gt; &amp; co"));
        assert!(p.contains("<key>CFBundleExecutable</key>\n\t<string>wc-launch</string>"));
        assert_eq!(section_from_plist(&p).as_deref(), Some("firefox"));
        assert_eq!(section_from_plist("<plist></plist>"), None);
    }

    fn png_of(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        let px: Vec<u8> = (0..w * h).flat_map(|i| [(i % 256) as u8, 100, 200, 255]).collect();
        wr.write_image_data(&px).unwrap();
        drop(wr);
        out
    }

    #[test]
    fn an_icon_of_any_size_becomes_an_exact_icns_entry() {
        for (w, h) in [(48, 48), (64, 64), (300, 200), (16, 16)] {
            let sq = square_png(&png_of(w, h), 256).unwrap();
            assert_eq!(crate::ico::png_size(&sq).unwrap(), (256, 256), "{w}x{h}");
            let icns = icns(&sq, 256).unwrap();
            assert_eq!(&icns[..4], b"icns");
            assert_eq!(u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize, icns.len());
            assert_eq!(&icns[8..12], b"ic08");
        }
        assert!(icns(&png_of(48, 48), 256).is_err(), "a mismatched size is refused");
    }

    #[test]
    fn dock_keys_round_trip_to_requests() {
        let meta = Meta {
            actions: vec![crate::appmeta::Action { id: "new-window".into(), name: "New Window".into(), ..Default::default() }],
            ..Default::default()
        };
        let pages = vec![crate::appmeta::Page {
            kind: crate::appmeta::PageKind::Frequent,
            url: "https://example.org/".into(),
            title: "Example".into(),
            icon: None,
        }];
        let items = dock_items(&meta, &pages);
        assert_eq!(items[0], ("New Window".into(), "action:new-window".into()));
        assert_eq!(items[1].1, "open:https://example.org/");
        assert!(matches!(request_for("action:new-window"), crate::Request::Action("new-window")));
        assert!(matches!(request_for("open:https://example.org/"), crate::Request::Open("https://example.org/")));
        assert!(matches!(request_for(""), crate::Request::Run));
    }
}
