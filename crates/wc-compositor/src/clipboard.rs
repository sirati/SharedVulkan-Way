//! The system clipboard, and the conversions between what Wayland calls a
//! selection and what Windows calls one.
//!
//! # What is here and what is in `platform/win32.rs`
//!
//! Everything in this file is arithmetic on bytes: which mime type maps to
//! which system format, UTF-8 to UTF-16 and back, the header Windows wraps HTML
//! in, the structure it wraps a file list in. None of it calls Win32, all of it
//! is wrong in ways that are silent — a clipboard that pastes the text with the
//! line endings doubled, or the last character missing, or the HTML with its
//! header still attached — and all of it is therefore unit-tested on the
//! machine that runs `cargo test` rather than only on the guest.
//!
//! `platform::win32` has the other half: `OpenClipboard`, `GetClipboardData`,
//! `SetClipboardData`, and the `WM_CLIPBOARDUPDATE` listener. It converts a
//! [`SystemFormat`] to a `UINT` and back and does nothing else with the bytes.
//!
//! # The three formats, and why exactly these three
//!
//! * **`CF_UNICODETEXT`** is the clipboard. Every Windows application that
//!   copies text sets it, most set nothing else, and a compositor that carried
//!   only this would already be useful.
//! * **A registered `text/html`** — Windows' `"HTML Format"` — because that is
//!   what a browser copies when the user selects part of a page, and losing it
//!   turns a rich paste into a plain one. It is *registered* rather than
//!   predefined: `RegisterClipboardFormatW` returns a number that differs
//!   between sessions, which is why the format is named by an enum here and
//!   resolved at runtime there.
//! * **`CF_HDROP`**, in one direction only, and that asymmetry is argued in
//!   [`hdrop_to_uri_list`] rather than hidden.

/// The generated bindings for `protocols/wc-clipboard-v1.xml`.
///
/// Server side only, exactly as `crate::icon::proto` is: wc-proxy writes the
/// requests by hand out of the same XML file (its `clip` module), which is how
/// the two ends are kept from drifting without a second code generator on the
/// host.
#[allow(missing_docs)]
pub mod proto {
    use smithay::reexports::wayland_server;

    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("protocols/wc-clipboard-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("protocols/wc-clipboard-v1.xml");
}

/// A clipboard format this compositor knows how to carry.
///
/// Deliberately not a `UINT`. Two of the three numbers are compile-time
/// constants and the third is not — `RegisterClipboardFormatW` mints
/// `"HTML Format"`'s at runtime — and a file that has to be compiled and tested
/// on Linux cannot hold either kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SystemFormat {
    /// `CF_UNICODETEXT`: UTF-16LE, NUL-terminated, CRLF line endings.
    UnicodeText,
    /// The registered `"HTML Format"`: UTF-8 with a byte-offset header.
    Html,
    /// `CF_HDROP`: a `DROPFILES` header and a double-NUL-terminated path list.
    HDrop,
}

impl SystemFormat {
    /// Every format this compositor can convert, in the order it prefers them.
    ///
    /// ADDED FOR §42, where the drop target asks an incoming `IDataObject`
    /// which of them it holds. A constant rather than three literals at that
    /// call site: a format added here without being added there would be one
    /// the clipboard understands and a drop silently does not, which is the
    /// asymmetry §29.5 spent a paragraph avoiding for `CF_HDROP`.
    /// Read only by the Win32 drop target, so off Windows the compiler is
    /// right that nothing uses it — the same note `platform::PopupBounds`
    /// carries for the same reason.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub const ALL: [SystemFormat; 3] = [
        SystemFormat::UnicodeText,
        SystemFormat::Html,
        SystemFormat::HDrop,
    ];

    /// For a log line, so a run says which formats were on the clipboard rather
    /// than which numbers were.
    pub fn name(self) -> &'static str {
        match self {
            SystemFormat::UnicodeText => "CF_UNICODETEXT",
            SystemFormat::Html => "HTML Format",
            SystemFormat::HDrop => "CF_HDROP",
        }
    }
}

/// The mime types announced for `CF_UNICODETEXT`, richest first.
///
/// All six, because clients disagree about which one they will accept and the
/// cost of announcing one more string is a string. GTK asks for
/// `text/plain;charset=utf-8`, Qt for `text/plain`, and the three bare X11
/// atom names are what a client ported from X selections still looks for —
/// `wl-paste` lists whatever it is offered and will happily take `STRING`.
pub const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

pub const HTML_MIME: &str = "text/html";
pub const URI_LIST_MIME: &str = "text/uri-list";

/// Which system format can answer a request for this mime type.
///
/// Matching is case-insensitive and ignores spaces around the parameter,
/// because `text/plain;charset=utf-8` and `text/plain; charset=UTF-8` are the
/// same thing and clients send both.
pub fn format_for(mime: &str) -> Option<SystemFormat> {
    let lowered: String = mime
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    let text = TEXT_MIMES.iter().any(|m| {
        let want: String = m.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
        want == lowered
    }) || lowered == "text/plain;charset=utf8";
    if text {
        return Some(SystemFormat::UnicodeText);
    }
    if lowered == HTML_MIME {
        return Some(SystemFormat::Html);
    }
    if lowered == URI_LIST_MIME {
        return Some(SystemFormat::HDrop);
    }
    None
}

/// The mime types to announce for a clipboard holding these formats, in the
/// order a client should prefer them.
pub fn mimes_for(formats: &[SystemFormat]) -> Vec<String> {
    let mut out = Vec::new();
    if formats.contains(&SystemFormat::Html) {
        out.push(HTML_MIME.to_owned());
    }
    if formats.contains(&SystemFormat::UnicodeText) {
        out.extend(TEXT_MIMES.iter().map(|m| (*m).to_owned()));
    }
    if formats.contains(&SystemFormat::HDrop) {
        out.push(URI_LIST_MIME.to_owned());
        // A file list is also a list of names, and an application that cannot
        // take files will usually take the paths as text. Only when there is no
        // real text on the clipboard, so this never displaces the text a user
        // actually copied.
        if !formats.contains(&SystemFormat::UnicodeText) {
            out.extend(TEXT_MIMES.iter().map(|m| (*m).to_owned()));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// UTF-8 to what `CF_UNICODETEXT` wants: UTF-16, CRLF, NUL-terminated.
///
/// **The line endings are the part that is silently wrong if it is skipped.**
/// Wayland's `text/plain` is a Unix convention and carries bare LF; Notepad
/// renders a bare LF as one long line, which looks exactly like a client that
/// sent its text without the newlines. A lone CR is left alone and an existing
/// CRLF is not doubled, because a client that already normalised is entitled
/// not to have it done twice.
pub fn utf16_from_utf8(text: &str) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(text.len() + 1);
    let mut prev_cr = false;
    for unit in text.encode_utf16() {
        if unit == b'\n' as u16 && !prev_cr {
            out.push(b'\r' as u16);
        }
        prev_cr = unit == b'\r' as u16;
        out.push(unit);
    }
    out.push(0);
    out
}

/// The inverse. Stops at the first NUL, which is where the clipboard's string
/// ends however many bytes the global memory block turned out to be.
pub fn utf8_from_utf16(units: &[u16]) -> String {
    let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
    let s = String::from_utf16_lossy(&units[..end]);
    s.replace("\r\n", "\n")
}

/// Reinterpret a byte block from the clipboard as UTF-16 code units.
///
/// `GlobalLock` hands back bytes and the alignment is the allocator's, not
/// ours, so this copies rather than casting. An odd trailing byte is dropped:
/// it cannot be part of a UTF-16 unit and a clipboard block is allowed to be
/// padded.
pub fn utf16_units(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .collect()
}

pub fn utf16_bytes(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len() * 2);
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------

/// The fixed part of a `CF_HTML` header, in bytes.
///
/// `Version:0.9` and four offsets, each written as exactly ten digits so that
/// the header's own length does not depend on the numbers in it — which it
/// would otherwise, and circularly, because the numbers are offsets from the
/// start of the header.
const HTML_HEADER_LEN: usize = 105;
const HTML_PREFIX: &str = "<html>\r\n<body>\r\n<!--StartFragment-->";
const HTML_SUFFIX: &str = "<!--EndFragment-->\r\n</body>\r\n</html>";

/// Wrap a fragment of HTML in the header Windows requires.
///
/// Without it, `SetClipboardData` succeeds, the format is on the clipboard, and
/// every application that reads it shows nothing: the header is not decoration,
/// it is how a reader is told where the fragment starts. That is the failure
/// this function exists to make impossible, and the assertion in the tests is
/// on the offsets rather than on the string.
pub fn html_wrap(fragment: &str) -> Vec<u8> {
    let start_fragment = HTML_HEADER_LEN + HTML_PREFIX.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + HTML_SUFFIX.len();
    let header = format!(
        "Version:0.9\r\n\
         StartHTML:{:010}\r\n\
         EndHTML:{:010}\r\n\
         StartFragment:{:010}\r\n\
         EndFragment:{:010}\r\n",
        HTML_HEADER_LEN, end_html, start_fragment, end_fragment
    );
    debug_assert_eq!(header.len(), HTML_HEADER_LEN);
    let mut out = Vec::with_capacity(end_html);
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(HTML_PREFIX.as_bytes());
    out.extend_from_slice(fragment.as_bytes());
    out.extend_from_slice(HTML_SUFFIX.as_bytes());
    out
}

/// Take the fragment back out.
///
/// The offsets are trusted only as far as they are inside the block: a
/// clipboard block is written by another application, some of which get the
/// arithmetic wrong, and a header claiming a fragment past the end of the data
/// must produce the whole block rather than a panic. Falling back to the whole
/// block is right rather than merely safe — a reader that shows too much HTML
/// is better than one that shows none.
pub fn html_unwrap(bytes: &[u8]) -> Vec<u8> {
    let head = &bytes[..bytes.len().min(1024)];
    let head = String::from_utf8_lossy(head);
    let field = |name: &str| -> Option<usize> {
        let at = head.find(name)?;
        let rest = &head[at + name.len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    };
    match (field("StartFragment:"), field("EndFragment:")) {
        (Some(s), Some(e)) if e >= s && e <= bytes.len() => bytes[s..e].to_vec(),
        _ => bytes.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// `sizeof(DROPFILES)`: a `DWORD` offset, a `POINT`, and two `BOOL`s.
const DROPFILES_LEN: usize = 20;

/// A `CF_HDROP` block as a `text/uri-list`.
///
/// # Why only this direction
///
/// A `CF_HDROP` names files **on the Windows machine**. In this project's
/// topology the Wayland client is on the other side of a hypervisor, so a
/// `file:///C:/Users/...` URI it is handed names something it cannot open
/// unless that path happens to be shared by other means. The conversion is
/// still worth doing — a paste into a text field gets the names, which is what
/// a user copying a path wants — and the limit is stated here rather than
/// discovered.
///
/// **The reverse is not implemented, and that is a decision.** A Linux client
/// offering `text/uri-list` names files in *its* filesystem; writing those into
/// a `CF_HDROP` would produce a clipboard that Explorer will happily try to
/// paste and always fail to, which is worse than a clipboard that does not
/// claim to hold files. So [`format_for`] maps `text/uri-list` to `HDrop` for
/// reading and the publish path drops it — see `crate::state`'s handler.
pub fn hdrop_to_uri_list(bytes: &[u8]) -> Option<String> {
    if bytes.len() < DROPFILES_LEN {
        return None;
    }
    let p_files = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    let wide = u32::from_le_bytes(bytes[16..20].try_into().ok()?) != 0;
    let list = bytes.get(p_files..)?;
    let names: Vec<String> = if wide {
        let units = utf16_units(list);
        split_double_nul_utf16(&units)
    } else {
        split_double_nul_ansi(list)
    };
    if names.is_empty() {
        return None;
    }
    let mut out = String::new();
    for name in names {
        out.push_str(&file_uri(&name));
        out.push_str("\r\n");
    }
    Some(out)
}

fn split_double_nul_utf16(units: &[u16]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, u) in units.iter().enumerate() {
        if *u != 0 {
            continue;
        }
        if i == start {
            break; // the empty string that terminates the list
        }
        out.push(String::from_utf16_lossy(&units[start..i]));
        start = i + 1;
    }
    out
}

fn split_double_nul_ansi(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b != 0 {
            continue;
        }
        if i == start {
            break;
        }
        out.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
        start = i + 1;
    }
    out
}

/// `C:\Users\me\a file.txt` -> `file:///C:/Users/me/a%20file.txt`.
///
/// Percent-encoding is deliberately conservative: everything outside the
/// unreserved set plus `/` and `:` is escaped, because a URI list is parsed by
/// somebody else's code and a space or a `#` left raw is the classic way to
/// deliver half a path.
fn file_uri(path: &str) -> String {
    let mut out = String::from("file:///");
    for b in path.replace('\\', "/").trim_start_matches('/').bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

/// The largest selection this compositor will hold, in bytes.
///
/// 4 MiB is a page of dense HTML with images inlined as data URIs and then
/// some. The limit exists because the bytes arrive from a client one chunk at
/// a time and a client that chunks forever and never publishes would otherwise
/// be a memory leak with a protocol in front of it — the same reasoning, and
/// the same shape of defence, as `crate::icon::MAX_IMAGE_BYTES`.
pub const MAX_SELECTION_BYTES: usize = 4 << 20;

/// How many mime types one selection may carry.
pub const MAX_MIMES: usize = 16;

/// How many bytes of payload go in one `chunk`, in either direction.
///
/// `wayland-backend`'s buffers are 4096 bytes (`rs/mod.rs`,
/// `DEFAULT_MAX_BUFFER_SIZE`, and `MAX_BYTES_OUT` on the sending side), and a
/// message that does not fit cannot be sent or read at all. 3072 leaves room
/// for the header, the token, the length word and the padding several times
/// over. The same number and the same reason as `wc-proxy`'s icon chunk
/// (docs/waycompose-design.md §16.5); it is a fact about the far end rather than a tuning
/// choice, which is why it is written down twice rather than shared.
pub const CHUNK: usize = 3072;

/// What a publisher has said so far, before it says to install it.
///
/// Split out of the protocol handler so the arithmetic is testable without a
/// `wl_display`, exactly as `crate::icon::Staging` is.
#[derive(Debug, Default)]
pub struct Staging {
    bytes: Vec<u8>,
    payloads: Vec<(String, Vec<u8>)>,
}

/// Why a staged selection was refused. Each maps to one protocol error.
#[derive(Debug, PartialEq, Eq)]
pub enum StageError {
    /// Past [`MAX_SELECTION_BYTES`].
    TooBig,
    /// Past [`MAX_MIMES`].
    TooMany,
}

impl Staging {
    pub fn chunk(&mut self, data: &[u8]) -> Result<(), StageError> {
        if self.staged() + data.len() > MAX_SELECTION_BYTES {
            return Err(StageError::TooBig);
        }
        self.bytes.extend_from_slice(data);
        Ok(())
    }

    /// Close off one mime type's payload.
    pub fn data(&mut self, mime: String) -> Result<(), StageError> {
        let bytes = std::mem::take(&mut self.bytes);
        // A repeat replaces rather than duplicating: a clipboard has one
        // payload per format and the last word is the client's.
        if let Some(slot) = self.payloads.iter_mut().find(|(m, _)| *m == mime) {
            slot.1 = bytes;
            return Ok(());
        }
        if self.payloads.len() >= MAX_MIMES {
            return Err(StageError::TooMany);
        }
        self.payloads.push((mime, bytes));
        Ok(())
    }

    /// How many bytes are held, staged and closed off together — which is the
    /// number the cap has to be applied to, not the length of the current
    /// chunk run.
    fn staged(&self) -> usize {
        self.bytes.len() + self.payloads.iter().map(|(_, b)| b.len()).sum::<usize>()
    }

    /// Take the selection and reset. Trailing chunked bytes that no `data`
    /// claimed are dropped, for the reason `crate::icon::Staging::take` gives.
    pub fn take(&mut self) -> Vec<(String, Vec<u8>)> {
        self.bytes.clear();
        std::mem::take(&mut self.payloads)
    }
}

/// Turn a Wayland selection into the set of system formats to install.
///
/// This is where the publish side's policy lives: pick the best text payload
/// among however many flavours the client offered, keep HTML if it sent any,
/// and drop everything else. Returning the pairs rather than setting them is
/// what makes the policy testable off Windows.
pub fn to_system(payloads: &[(String, Vec<u8>)]) -> Vec<(SystemFormat, Vec<u8>)> {
    let mut out: Vec<(SystemFormat, Vec<u8>)> = Vec::new();
    // Text, by the announcement order in `TEXT_MIMES`: the first flavour the
    // client offered that we recognise. They are supposed to be the same bytes,
    // and when they are not the client's own preference order is the one to
    // follow.
    let text = TEXT_MIMES
        .iter()
        .find_map(|want| payloads.iter().find(|(m, _)| m == want))
        .or_else(|| {
            payloads
                .iter()
                .find(|(m, _)| format_for(m) == Some(SystemFormat::UnicodeText))
        });
    if let Some((_, bytes)) = text {
        let s = String::from_utf8_lossy(bytes);
        out.push((SystemFormat::UnicodeText, utf16_bytes(&utf16_from_utf8(&s))));
    }
    if let Some((_, bytes)) = payloads
        .iter()
        .find(|(m, _)| format_for(m) == Some(SystemFormat::Html))
    {
        out.push((SystemFormat::Html, html_wrap(&String::from_utf8_lossy(bytes))));
    }
    // `text/uri-list` is deliberately absent. See `hdrop_to_uri_list`.
    out
}

/// Turn a system format's bytes into the payload for a mime type.
///
/// The inverse of [`to_system`], and the only place the fetch path converts.
pub fn from_system(mime: &str, format: SystemFormat, bytes: &[u8]) -> Option<Vec<u8>> {
    match (format_for(mime)?, format) {
        (SystemFormat::UnicodeText, SystemFormat::UnicodeText) => {
            Some(utf8_from_utf16(&utf16_units(bytes)).into_bytes())
        }
        (SystemFormat::Html, SystemFormat::Html) => Some(html_unwrap(bytes)),
        (SystemFormat::HDrop, SystemFormat::HDrop) => {
            Some(hdrop_to_uri_list(bytes)?.into_bytes())
        }
        // A file list asked for as text: the names, one per line. This is the
        // pairing `mimes_for` announces when the clipboard holds files and no
        // text, and it is the only cross-format conversion here.
        (SystemFormat::UnicodeText, SystemFormat::HDrop) => {
            Some(hdrop_to_uri_list(bytes)?.replace("\r\n", "\n").into_bytes())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mime_spellings_clients_actually_send_all_map_to_text() {
        for m in [
            "text/plain;charset=utf-8",
            "text/plain; charset=utf-8",
            "text/plain;charset=UTF-8",
            "TEXT/PLAIN",
            "UTF8_STRING",
            "STRING",
            "TEXT",
        ] {
            assert_eq!(format_for(m), Some(SystemFormat::UnicodeText), "{m}");
        }
        assert_eq!(format_for("text/html"), Some(SystemFormat::Html));
        assert_eq!(format_for("text/uri-list"), Some(SystemFormat::HDrop));
        assert_eq!(format_for("image/png"), None);
    }

    #[test]
    fn line_endings_cross_in_both_directions_without_doubling() {
        // The failure this catches is one long line in Notepad, and the other
        // one is a blank line between every line back on Linux.
        let units = utf16_from_utf8("a\nb\r\nc");
        let back = utf8_from_utf16(&units);
        assert_eq!(back, "a\nb\nc");
        assert_eq!(*units.last().unwrap(), 0, "CF_UNICODETEXT is NUL-terminated");
        let crlf = String::from_utf16_lossy(&units[..units.len() - 1]);
        assert_eq!(crlf, "a\r\nb\r\nc");
    }

    #[test]
    fn non_ascii_survives_the_round_trip() {
        // UTF-16 is the whole reason the text path is not a `to_vec`, and a
        // surrogate pair is the case a naive "one char, one u16" would lose.
        for s in ["grüße", "日本語", "emoji: \u{1F600}", ""] {
            let bytes = utf16_bytes(&utf16_from_utf8(s));
            assert_eq!(utf8_from_utf16(&utf16_units(&bytes)), s, "{s:?}");
        }
    }

    #[test]
    fn a_clipboard_block_longer_than_its_string_stops_at_the_nul() {
        // `GlobalSize` rounds up, so the block really is longer than the text
        // and the trailing bytes really are whatever was in the heap.
        let mut bytes = utf16_bytes(&utf16_from_utf8("hi"));
        bytes.extend_from_slice(&[0x41, 0x00, 0x42, 0x00]);
        assert_eq!(utf8_from_utf16(&utf16_units(&bytes)), "hi");
        // And an odd trailing byte is dropped rather than panicking.
        bytes.push(0x43);
        assert_eq!(utf8_from_utf16(&utf16_units(&bytes)), "hi");
    }

    #[test]
    fn the_html_header_offsets_point_at_the_fragment() {
        let wrapped = html_wrap("<b>hi</b>");
        let text = String::from_utf8(wrapped.clone()).unwrap();
        let num = |name: &str| -> usize {
            let at = text.find(name).unwrap() + name.len();
            text[at..at + 10].parse().unwrap()
        };
        assert_eq!(num("StartHTML:"), HTML_HEADER_LEN);
        assert_eq!(num("EndHTML:"), wrapped.len());
        assert_eq!(&text[num("StartFragment:")..num("EndFragment:")], "<b>hi</b>");
        // And the whole point: it comes back out.
        assert_eq!(html_unwrap(&wrapped), b"<b>hi</b>");
    }

    #[test]
    fn a_html_block_with_a_broken_header_yields_the_whole_block() {
        // Written by somebody else's application, which is the only kind there
        // is on the reading side.
        let bad = b"Version:0.9\r\nStartFragment:0000009999\r\nEndFragment:0000000001\r\n<p>x";
        assert_eq!(html_unwrap(bad), bad.to_vec());
        assert_eq!(html_unwrap(b"<p>no header at all"), b"<p>no header at all".to_vec());
    }

    #[test]
    fn a_dropfiles_block_becomes_percent_encoded_file_uris() {
        let mut block = Vec::new();
        block.extend_from_slice(&(DROPFILES_LEN as u32).to_le_bytes()); // pFiles
        block.extend_from_slice(&[0u8; 8]); // pt
        block.extend_from_slice(&0u32.to_le_bytes()); // fNC
        block.extend_from_slice(&1u32.to_le_bytes()); // fWide
        let mut units: Vec<u16> = Vec::new();
        for name in ["C:\\Users\\me\\a file.txt", "C:\\tmp\\x#y"] {
            units.extend(name.encode_utf16());
            units.push(0);
        }
        units.push(0);
        block.extend_from_slice(&utf16_bytes(&units));
        assert_eq!(
            hdrop_to_uri_list(&block).unwrap(),
            "file:///C:/Users/me/a%20file.txt\r\nfile:///C:/tmp/x%23y\r\n"
        );
        assert!(hdrop_to_uri_list(&[0u8; 4]).is_none(), "a runt block is None, not a panic");
    }

    #[test]
    fn staging_keeps_one_payload_per_mime_and_caps_the_total() {
        let mut s = Staging::default();
        s.chunk(b"hello").unwrap();
        s.data("text/plain".into()).unwrap();
        s.chunk(b"<b>hello</b>").unwrap();
        s.data("text/html".into()).unwrap();
        s.chunk(b"HELLO").unwrap();
        s.data("text/plain".into()).unwrap();
        let out = s.take();
        assert_eq!(out.len(), 2, "the repeat replaced rather than appended");
        assert_eq!(out[0], ("text/plain".to_owned(), b"HELLO".to_vec()));
        assert!(s.take().is_empty(), "take resets");

        let mut s = Staging::default();
        s.chunk(&vec![0u8; MAX_SELECTION_BYTES - 1]).unwrap();
        s.data("text/plain".into()).unwrap();
        // The cap counts what is already closed off, not just the current run.
        assert_eq!(s.chunk(&[0u8; 2]), Err(StageError::TooBig));
    }

    #[test]
    fn staging_refuses_more_mimes_than_a_clipboard_holds() {
        let mut s = Staging::default();
        for i in 0..MAX_MIMES {
            s.chunk(b"x").unwrap();
            s.data(format!("x/{i}")).unwrap();
        }
        s.chunk(b"x").unwrap();
        assert_eq!(s.data("one/too/many".into()), Err(StageError::TooMany));
    }

    #[test]
    fn a_selection_offering_six_flavours_of_text_installs_one() {
        // Which is what GTK sends: the same bytes under five names. A publish
        // that set CF_UNICODETEXT five times would be five clipboard writes and
        // the last one wins anyway.
        let payloads: Vec<(String, Vec<u8>)> = TEXT_MIMES
            .iter()
            .map(|m| ((*m).to_owned(), b"hi".to_vec()))
            .chain(std::iter::once((HTML_MIME.to_owned(), b"<i>hi</i>".to_vec())))
            .chain(std::iter::once(("image/png".to_owned(), vec![0x89, b'P'])))
            .collect();
        let system = to_system(&payloads);
        assert_eq!(system.len(), 2, "text and html, and the png dropped");
        assert_eq!(system[0].0, SystemFormat::UnicodeText);
        assert_eq!(utf8_from_utf16(&utf16_units(&system[0].1)), "hi");
        assert_eq!(system[1].0, SystemFormat::Html);
        assert_eq!(html_unwrap(&system[1].1), b"<i>hi</i>");
    }

    #[test]
    fn a_uri_list_is_never_published_as_files() {
        // The asymmetry argued in `hdrop_to_uri_list`: a Linux path in a
        // CF_HDROP is a clipboard Explorer will always fail to paste.
        let system = to_system(&[(URI_LIST_MIME.to_owned(), b"file:///home/me/x\r\n".to_vec())]);
        assert!(system.is_empty());
    }

    #[test]
    fn a_clipboard_with_files_and_no_text_still_offers_text() {
        assert_eq!(
            mimes_for(&[SystemFormat::HDrop]),
            [URI_LIST_MIME]
                .into_iter()
                .chain(TEXT_MIMES.iter().copied())
                .collect::<Vec<_>>()
        );
        // And when there is real text, the file names do not displace it.
        let both = mimes_for(&[SystemFormat::UnicodeText, SystemFormat::HDrop]);
        assert_eq!(both.iter().filter(|m| *m == "text/plain").count(), 1);
        assert_eq!(both.first().unwrap(), "text/plain;charset=utf-8");
    }

    #[test]
    fn the_fetch_conversion_refuses_a_pairing_it_cannot_make() {
        assert_eq!(
            from_system("text/plain", SystemFormat::UnicodeText, &utf16_bytes(&utf16_from_utf8("x\r\ny"))),
            Some(b"x\ny".to_vec())
        );
        assert_eq!(from_system("text/html", SystemFormat::UnicodeText, b"x"), None);
        assert_eq!(from_system("image/png", SystemFormat::UnicodeText, b"x"), None);
    }
}
