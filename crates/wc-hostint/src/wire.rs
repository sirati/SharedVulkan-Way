//! The host-integration channel's wire format: one ASCII line per message on
//! the `OWNER_HOSTINT` ring slot, either side may send at any time.
//!
//! ```text
//! Linux -> host
//!   hello 1
//!   req N url B64URL              open a URL with the host's browser
//!   req N path B64HOSTPATH        open a host file with the host's default
//!   req N with TOKEN B64TARGET    open a URL or host file with one host handler
//! host -> Linux
//!   hello 1 PLATFORM              "windows" or "macos"
//!   rep N ok
//!   rep N err B64REASON
//!   audio B64STATE                the host's devices and defaults (audio.rs)
//!   handlers B64LIST              host applications per type (handlers.rs)
//!   paths B64MAP                  how host paths and Linux paths correspond
//! ```
//!
//! `N` is the requester's decimal sequence number. Free text crosses only as
//! base64 inside one word, so a hostile value cannot add a word or a line.
//! Inside a base64 payload, records are lines and fields are tab-separated;
//! a field never holds a tab or a control character ([`field`] replaces
//! them).
//!
//! Requests name things and never carry commands (§94, security-model R33):
//! a URL, a host path, or the opaque token the host gave a handler. The host
//! checks all of them before it opens anything.

use crate::audio::{Device, Dir, State};

pub const VERSION: u32 = 1;
/// The longest line either side accepts.
pub const MAX_LINE: usize = 256 * 1024;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64(data: &[u8]) -> String {
    let mut o = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                o.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                o.push('=');
            }
        }
    }
    o
}

pub fn unb64(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if s.len() % 4 != 0 {
        return None;
    }
    let val = |c: u8| B64.iter().position(|&b| b == c).map(|p| p as u32);
    let mut o = Vec::with_capacity(s.len() / 4 * 3);
    for (k, q) in s.chunks(4).enumerate() {
        let last = k == s.len() / 4 - 1;
        let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for (i, &c) in q.iter().enumerate() {
            let v = if i >= 4 - pad { 0 } else { val(c)? };
            n |= v << (18 - 6 * i);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        o.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(o)
}

fn unb64_str(s: &str) -> Option<String> {
    String::from_utf8(unb64(s)?).ok()
}

/// A field for a tab-separated record: control characters become spaces.
pub fn field(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// What Linux asks the host to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Url(String),
    HostPath(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Hello { version: u32, platform: Option<String> },
    /// Linux to host.
    Req { n: u64, req: Req },
    /// Host to Linux.
    Rep { n: u64, result: Result<(), String> },
    Audio(State),
    Handlers(Vec<Handler>),
    Paths(PathMap),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Req {
    /// Open with the host's browser.
    Url(String),
    /// Open a host file with the host's default for its type.
    Path(String),
    /// Open with one host handler, by the token the host gave it.
    With { token: String, target: Target },
}

/// One host application that opens some types, as the host offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handler {
    /// Opaque, `[a-z0-9]{1,32}`, chosen by the host; the host maps it back to
    /// its own application and never to anything Linux sent.
    pub token: String,
    /// "Notepad", "TextEdit", "Google Chrome".
    pub name: String,
    pub mimes: Vec<String>,
    /// The types this handler is the host's default for.
    pub default_for: Vec<String>,
    /// URL schemes it opens (http, https, mailto, ...).
    pub schemes: Vec<String>,
}

pub fn token_ok(t: &str) -> bool {
    !t.is_empty() && t.len() <= 32 && t.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Path correspondence. `shares` are directories both sides see: a host
/// path under `host` is the Linux path under `linux`. `drop` is the share
/// directory Linux copies a Linux-only file into before asking the host to
/// open it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PathMap {
    pub shares: Vec<(String, String)>,
    pub drop: Option<(String, String)>,
}

impl Msg {
    pub fn encode(&self) -> String {
        match self {
            Msg::Hello { version, platform: None } => format!("hello {version}"),
            Msg::Hello { version, platform: Some(p) } => format!("hello {version} {p}"),
            Msg::Req { n, req } => match req {
                Req::Url(u) => format!("req {n} url {}", b64(u.as_bytes())),
                Req::Path(p) => format!("req {n} path {}", b64(p.as_bytes())),
                Req::With { token, target } => {
                    let t = match target {
                        Target::Url(u) => format!("u{u}"),
                        Target::HostPath(p) => format!("p{p}"),
                    };
                    format!("req {n} with {token} {}", b64(t.as_bytes()))
                }
            },
            Msg::Rep { n, result: Ok(()) } => format!("rep {n} ok"),
            Msg::Rep { n, result: Err(e) } => format!("rep {n} err {}", b64(e.as_bytes())),
            Msg::Audio(s) => format!("audio {}", b64(encode_state(s).as_bytes())),
            Msg::Handlers(h) => format!("handlers {}", b64(encode_handlers(h).as_bytes())),
            Msg::Paths(p) => format!("paths {}", b64(encode_paths(p).as_bytes())),
        }
    }

    pub fn decode(line: &str) -> Result<Msg, String> {
        if line.len() > MAX_LINE {
            return Err("line too long".into());
        }
        let w: Vec<&str> = line.split(' ').collect();
        let bad = || format!("malformed: {:?}", line.chars().take(80).collect::<String>());
        let num = |s: &str| s.parse::<u64>().map_err(|_| bad());
        let text = |s: &str| unb64_str(s).ok_or_else(bad);
        Ok(match w.as_slice() {
            ["hello", v] => Msg::Hello { version: v.parse().map_err(|_| bad())?, platform: None },
            ["hello", v, p] if p.chars().all(|c| c.is_ascii_lowercase()) => {
                Msg::Hello { version: v.parse().map_err(|_| bad())?, platform: Some((*p).into()) }
            }
            ["req", n, "url", u] => Msg::Req { n: num(n)?, req: Req::Url(text(u)?) },
            ["req", n, "path", p] => Msg::Req { n: num(n)?, req: Req::Path(text(p)?) },
            ["req", n, "with", t, x] if token_ok(t) => {
                let x = text(x)?;
                let target = if let Some(u) = x.strip_prefix('u') {
                    Target::Url(u.into())
                } else if let Some(p) = x.strip_prefix('p') {
                    Target::HostPath(p.into())
                } else {
                    return Err(bad());
                };
                Msg::Req { n: num(n)?, req: Req::With { token: (*t).into(), target } }
            }
            ["rep", n, "ok"] => Msg::Rep { n: num(n)?, result: Ok(()) },
            ["rep", n, "err", e] => Msg::Rep { n: num(n)?, result: Err(text(e)?) },
            ["audio", s] => Msg::Audio(decode_state(&text(s)?).ok_or_else(bad)?),
            ["handlers", h] => Msg::Handlers(decode_handlers(&text(h)?)),
            ["paths", p] => Msg::Paths(decode_paths(&text(p)?)),
            _ => return Err(bad()),
        })
    }
}

fn encode_state(s: &State) -> String {
    let mut o = String::new();
    for d in &s.devices {
        o.push_str(&format!("{}\t{}\t{}\t{}\t{}\n", d.dir.wire(), field(&d.id), field(&d.description), d.channels, d.rate));
    }
    if let Some(d) = &s.default_output {
        o.push_str(&format!("default-out\t{}\n", field(d)));
    }
    if let Some(d) = &s.default_input {
        o.push_str(&format!("default-in\t{}\n", field(d)));
    }
    o
}

fn decode_state(t: &str) -> Option<State> {
    let mut s = State::default();
    for l in t.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        match f.as_slice() {
            ["default-out", id] => s.default_output = Some((*id).into()),
            ["default-in", id] => s.default_input = Some((*id).into()),
            [dir, id, desc, ch, rate] => s.devices.push(Device {
                dir: Dir::from_wire(dir)?,
                id: (*id).into(),
                description: (*desc).into(),
                channels: ch.parse().ok()?,
                rate: rate.parse().ok()?,
            }),
            _ => return None,
        }
    }
    // A host with hundreds of devices is not one this protocol serves.
    (s.devices.len() <= 64).then_some(s)
}

fn encode_handlers(hs: &[Handler]) -> String {
    let mut o = String::new();
    for h in hs {
        o.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            h.token,
            field(&h.name),
            h.mimes.join(";"),
            h.default_for.join(";"),
            h.schemes.join(";")
        ));
    }
    o
}

fn decode_handlers(t: &str) -> Vec<Handler> {
    let list = |s: &str, ok: &dyn Fn(&str) -> bool| -> Vec<String> {
        s.split(';').filter(|x| !x.is_empty() && ok(x)).map(String::from).collect()
    };
    let scheme_ok = |s: &str| crate::mime::scheme_of(&format!("x-scheme-handler/{s}")).is_some();
    t.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let [token, name, mimes, defs, schemes] = f.as_slice() else { return None };
            token_ok(token).then(|| Handler {
                token: (*token).into(),
                name: name.chars().take(128).collect(),
                mimes: list(mimes, &crate::mime::is_mime),
                default_for: list(defs, &crate::mime::is_mime),
                schemes: list(schemes, &scheme_ok),
            })
        })
        .take(256)
        .collect()
}

fn encode_paths(p: &PathMap) -> String {
    let mut o = String::new();
    for (h, l) in &p.shares {
        o.push_str(&format!("share\t{}\t{}\n", field(h), field(l)));
    }
    if let Some((h, l)) = &p.drop {
        o.push_str(&format!("drop\t{}\t{}\n", field(h), field(l)));
    }
    o
}

fn decode_paths(t: &str) -> PathMap {
    let mut p = PathMap::default();
    for l in t.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        match f.as_slice() {
            ["share", h, l] => p.shares.push(((*h).into(), (*l).into())),
            ["drop", h, l] => p.drop = Some(((*h).into(), (*l).into())),
            _ => {}
        }
    }
    p
}

/// Bytes from a ring slot, cut into lines. A line longer than [`MAX_LINE`]
/// is dropped whole (up to its newline) rather than buffered without bound,
/// since the peer may be hostile.
#[derive(Default)]
pub struct LineBuf {
    buf: Vec<u8>,
    skipping: bool,
}

impl LineBuf {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete line, without its newline. Invalid UTF-8 becomes
    /// U+FFFD, which no message decodes.
    pub fn next_line(&mut self) -> Option<String> {
        loop {
            match self.buf.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    let line: Vec<u8> = self.buf.drain(..=i).collect();
                    if std::mem::take(&mut self.skipping) {
                        continue;
                    }
                    return Some(String::from_utf8_lossy(&line[..i]).into_owned());
                }
                None => {
                    if self.buf.len() > MAX_LINE {
                        self.buf.clear();
                        self.skipping = true;
                    }
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_cut_and_bounded() {
        let mut l = LineBuf::default();
        l.push(b"hello 1\nrep 2 o");
        assert_eq!(l.next_line().as_deref(), Some("hello 1"));
        assert_eq!(l.next_line(), None);
        l.push(b"k\n");
        assert_eq!(l.next_line().as_deref(), Some("rep 2 ok"));
        l.push(&vec![b'x'; MAX_LINE + 10]);
        assert_eq!(l.next_line(), None);
        l.push(b"tail of the long one\nnext\n");
        assert_eq!(l.next_line().as_deref(), Some("next"), "the overlong line is dropped whole");
    }

    #[test]
    fn base64_round_trip_and_rejects() {
        for s in ["", "a", "ab", "abc", "abcd", "https://example.org/ä?x=1", "\0\u{ff}"] {
            assert_eq!(unb64(&b64(s.as_bytes())).unwrap(), s.as_bytes(), "{s:?}");
        }
        assert_eq!(b64(b"Man"), "TWFu");
        assert_eq!(b64(b"Ma"), "TWE=");
        assert!(unb64("TWE").is_none());
        assert!(unb64("T=Fu").is_none());
        assert!(unb64("TW=uTWFu").is_none());
        assert!(unb64("TW$u").is_none());
    }

    fn rt(m: Msg) {
        let line = m.encode();
        assert!(!line.contains('\n'), "{line}");
        assert_eq!(Msg::decode(&line).unwrap(), m, "{line}");
    }

    #[test]
    fn messages_round_trip() {
        rt(Msg::Hello { version: 1, platform: None });
        rt(Msg::Hello { version: 1, platform: Some("windows".into()) });
        rt(Msg::Req { n: 7, req: Req::Url("https://example.org/a b\n".into()) });
        rt(Msg::Req { n: 8, req: Req::Path("C:\\Users\\A\\x.txt".into()) });
        rt(Msg::Req { n: 9, req: Req::With { token: "h3".into(), target: Target::HostPath("/Users/a/b.txt".into()) } });
        rt(Msg::Req { n: 9, req: Req::With { token: "h3".into(), target: Target::Url("https://x/".into()) } });
        rt(Msg::Rep { n: 7, result: Ok(()) });
        rt(Msg::Rep { n: 7, result: Err("scheme 'file' is not allowed".into()) });
        rt(Msg::Audio(State {
            devices: vec![Device { dir: Dir::Output, id: "{0.0.0.00000000}.{x}".into(), description: "Speakers (Realtek(R) Audio)".into(), channels: 2, rate: 48000 }],
            default_output: Some("{0.0.0.00000000}.{x}".into()),
            default_input: None,
        }));
        rt(Msg::Handlers(vec![Handler {
            token: "n0".into(),
            name: "Notepad".into(),
            mimes: vec!["text/plain".into()],
            default_for: vec!["text/plain".into()],
            schemes: vec![],
        }]));
        rt(Msg::Paths(PathMap { shares: vec![("Z:\\".into(), "/srv/winshare/".into())], drop: Some(("Z:\\wc-open".into(), "/srv/winshare/wc-open".into())) }));
    }

    #[test]
    fn hostile_lines_are_refused_or_cleaned() {
        assert!(Msg::decode("req 1 url not-base64!").is_err());
        assert!(Msg::decode("req x url aGk=").is_err());
        assert!(Msg::decode("req 1 with ../x aGk=").is_err());
        assert!(Msg::decode("req 1 with h1 eGFiYw==").is_err(), "a target must say u or p");
        assert!(Msg::decode("run firefox").is_err());
        assert!(Msg::decode(&"x".repeat(MAX_LINE + 1)).is_err());
        // A tab or newline inside a name cannot become a new field or record.
        let h = Handler { token: "a".into(), name: "x\ty\nz".into(), mimes: vec![], default_for: vec![], schemes: vec![] };
        let Msg::Handlers(back) = Msg::decode(&Msg::Handlers(vec![h]).encode()).unwrap() else { panic!() };
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, "x y z");
        // Types that are not types are dropped.
        let line = format!("handlers {}", b64(b"t\tN\ttext/plain;../..;a b\t\thttp;1x\n"));
        let Msg::Handlers(back) = Msg::decode(&line).unwrap() else { panic!() };
        assert_eq!(back[0].mimes, ["text/plain"]);
        assert_eq!(back[0].schemes, ["http"]);
    }
}
