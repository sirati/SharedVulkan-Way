//! The host's applications for each type, as offered to Linux: one
//! [`HostHandler`] per application, gathered from what the platform said per
//! type. The platform part asks (`win::handlers`, the macOS one); this part
//! merges, tokenises and orders, and is tested here.

use wc_hostint::host::HostHandler;
use wc_hostint::wire::{field, Handler};

use crate::token::token;

/// What the platform found for one type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    /// A MIME type of `wc_hostint::mime::TABLE`.
    Mime(String),
    /// A URL scheme: `http`, `https`, `mailto`.
    Scheme(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// The host's identity for it: an executable's absolute path, a bundle
    /// id. Stays on the host.
    pub id: String,
    /// What a person calls it: "Notepad".
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub key: Key,
    pub apps: Vec<App>,
    /// The host's default for the type, by identity.
    pub default: Option<String>,
}

/// At most this many handlers are offered: the line that carries them is
/// bounded, and nobody picks from a longer list.
pub const MAX_HANDLERS: usize = 64;

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Merge per-type findings into one handler per application, ordered by
/// name, then identity (so the offer is the same however the host
/// enumerated). An application that is the default for a type it was not
/// listed for gets the type too: Windows lists only the "recommended"
/// handlers, and the default is always one.
pub fn collect(found: &[Found]) -> Vec<HostHandler> {
    let mut out: Vec<HostHandler> = Vec::new();
    let entry = |out: &mut Vec<HostHandler>, app: &App| -> usize {
        // One application, also when the shell knows it by two paths
        // (Notepad is both `C:\Windows\notepad.exe` and the one in System32):
        // the same name and the same file name are the same application.
        let base = |p: &str| p.rsplit(['\\', '/']).next().unwrap_or(p).to_ascii_lowercase();
        let name = field(app.name.trim());
        if let Some(i) = out.iter().position(|h| {
            same(&h.app, &app.id) || (!name.is_empty() && h.offer.name == name && base(&h.app) == base(&app.id))
        }) {
            return i;
        }
        out.push(HostHandler {
            offer: Handler {
                token: token(&app.id),
                name: if name.is_empty() { "application".into() } else { name },
                mimes: vec![],
                default_for: vec![],
                schemes: vec![],
            },
            app: app.id.clone(),
        });
        out.len() - 1
    };
    let add = |v: &mut Vec<String>, s: &str| {
        if !v.iter().any(|x| x == s) {
            v.push(s.to_string());
        }
    };
    for f in found {
        let mut apps: Vec<&App> = f.apps.iter().collect();
        let dflt_app;
        if let Some(d) = &f.default
            && !apps.iter().any(|a| same(&a.id, d))
        {
            // The default named only by identity: its name is the file's.
            let name = d.rsplit(['\\', '/']).next().unwrap_or(d).trim_end_matches(".exe").to_string();
            dflt_app = App { id: d.clone(), name };
            apps.push(&dflt_app);
        }
        for a in apps {
            let i = entry(&mut out, a);
            let o = &mut out[i].offer;
            let is_default = f.default.as_deref().is_some_and(|d| same(d, &a.id));
            match &f.key {
                Key::Mime(m) => {
                    add(&mut o.mimes, m);
                    if is_default {
                        add(&mut o.default_for, m);
                    }
                }
                Key::Scheme(s) => {
                    add(&mut o.schemes, s);
                    if is_default {
                        add(&mut o.default_for, &format!("x-scheme-handler/{s}"));
                    }
                }
            }
        }
    }
    out.sort_by_key(|h| (h.offer.name.to_lowercase(), h.app.to_lowercase()));
    out.truncate(MAX_HANDLERS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str) -> App {
        App { id: id.into(), name: name.into() }
    }

    #[test]
    fn one_handler_per_application_with_its_types_and_defaults() {
        let np = "C:\\Windows\\System32\\notepad.exe";
        let ff = "C:\\Program Files\\Mozilla Firefox\\firefox.exe";
        let found = vec![
            Found { key: Key::Mime("text/plain".into()), apps: vec![app(np, "Notepad")], default: Some(np.into()) },
            Found {
                key: Key::Mime("text/html".into()),
                apps: vec![app(ff, "Firefox"), app(&np.to_uppercase(), "Notepad")],
                default: Some(ff.into()),
            },
            Found { key: Key::Scheme("https".into()), apps: vec![app(ff, "Firefox")], default: Some(ff.into()) },
            // Notepad again, by its other path: the same application.
            Found {
                key: Key::Mime("text/css".into()),
                apps: vec![app("C:\\Windows\\NOTEPAD.EXE", "Notepad")],
                default: Some("C:\\Windows\\NOTEPAD.EXE".into()),
            },
            // The default is not among the recommended: it is added.
            Found { key: Key::Scheme("mailto".into()), apps: vec![], default: Some("C:\\Mail\\mail.exe".into()) },
        ];
        let hs = collect(&found);
        let names: Vec<&str> = hs.iter().map(|h| h.offer.name.as_str()).collect();
        assert_eq!(names, ["Firefox", "mail", "Notepad"]);
        let f = &hs[0].offer;
        assert_eq!(f.mimes, ["text/html"]);
        assert_eq!(f.schemes, ["https"]);
        assert_eq!(f.default_for, ["text/html", "x-scheme-handler/https"]);
        let n = &hs[2];
        assert_eq!(n.offer.mimes, ["text/plain", "text/html", "text/css"], "one Notepad, by either path");
        assert_eq!(n.offer.default_for, ["text/plain", "text/css"]);
        assert_eq!(n.app, np);
        assert_eq!(hs[1].offer.default_for, ["x-scheme-handler/mailto"]);
        // The identity never crosses: the offer has the token only.
        let line = wc_hostint::wire::Msg::Handlers(hs.iter().map(|h| h.offer.clone()).collect()).encode();
        let decoded = wc_hostint::wire::Msg::decode(&line).unwrap();
        assert!(!format!("{decoded:?}").contains("System32"));
        assert!(hs.iter().all(|h| wc_hostint::wire::token_ok(&h.offer.token)));
    }
}
