//! The applications this Windows offers for each type Linux knows
//! (`wc_hostint::mime::TABLE`) and for `http`, `https` and `mailto`.
//!
//! Per extension: `SHAssocEnumHandlers(".ext", ASSOC_FILTER_RECOMMENDED)`,
//! the list Explorer's "Open with" shows first, and the default from
//! `AssocQueryStringW(ASSOCSTR_EXECUTABLE)`. Per scheme:
//! `SHAssocEnumHandlersForProtocolByApplication` and the same query with
//! `ASSOCF_IS_PROTOCOL`. Only desktop applications are offered, named by an
//! existing `.exe`: a packaged (UWP) application has no executable that
//! `CreateProcess` could start with a file, so it cannot be an "open with"
//! target here. The executable path stays on this side; Linux gets a token
//! ([`crate::token`]).

use std::ffi::c_void;

use windows_sys::Win32::UI::Shell::{
    ASSOC_FILTER_RECOMMENDED, ASSOCF_IS_PROTOCOL, ASSOCF_NONE, ASSOCSTR_EXECUTABLE, AssocQueryStringW,
    SHAssocEnumHandlers, SHAssocEnumHandlersForProtocolByApplication,
};

use wc_hostint::host::HostHandler;
use wc_hostint::mime::TABLE;

use super::com::*;
use super::open::is_exe;
use crate::handlers::{App, Found, Key, collect};

/// The schemes whose handlers are offered.
pub const SCHEMES: &[&str] = &["http", "https", "mailto"];

fn apps_of(e: Com<EnumAssocHandlersVtbl>) -> Vec<App> {
    let mut out = Vec::new();
    for _ in 0..64 {
        let mut h: *mut c_void = std::ptr::null_mut();
        let mut got = 0u32;
        // SAFETY: one element at a time into `h`.
        let hr = unsafe { (e.v().next)(e.as_raw(), 1, &mut h, &mut got) };
        if hr != 0 || got == 0 {
            break;
        }
        // SAFETY: an IAssocHandler with one reference, released on drop.
        let Some(h) = (unsafe { Com::<AssocHandlerVtbl>::from_raw(h) }) else { break };
        let (mut name, mut ui) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: both strings are CoTaskMemAlloc'ed and taken below.
        let (id, label) = unsafe {
            (h.v().get_name)(h.as_raw(), &mut name);
            (h.v().get_ui_name)(h.as_raw(), &mut ui);
            (take_co_string(name), take_co_string(ui))
        };
        if is_exe(&id) {
            out.push(App { id, name: label });
        }
    }
    out
}

fn query_exe(assoc: &str, protocol: bool) -> Option<String> {
    let a = wide(assoc);
    let verb = wide("open");
    let mut buf = vec![0u16; 1024];
    let mut n = buf.len() as u32;
    let flags = if protocol { ASSOCF_IS_PROTOCOL } else { ASSOCF_NONE };
    // SAFETY: NUL-terminated inputs and a buffer of `n` characters.
    let hr = unsafe { AssocQueryStringW(flags, ASSOCSTR_EXECUTABLE, a.as_ptr(), verb.as_ptr(), buf.as_mut_ptr(), &mut n) };
    if hr < 0 {
        return None;
    }
    let s = from_wide(&buf);
    is_exe(&s).then_some(s)
}

fn for_extension(ext: &str) -> (Vec<App>, Option<String>) {
    let w = wide(&format!(".{ext}"));
    // SAFETY: an IEnumAssocHandlers comes back.
    let apps = unsafe {
        out::<EnumAssocHandlersVtbl>("SHAssocEnumHandlers", |p| SHAssocEnumHandlers(w.as_ptr(), ASSOC_FILTER_RECOMMENDED, p))
    }
    .map(apps_of)
    .unwrap_or_default();
    (apps, query_exe(&format!(".{ext}"), false))
}

fn for_scheme(s: &str) -> (Vec<App>, Option<String>) {
    let w = wide(s);
    // SAFETY: an IEnumAssocHandlers comes back.
    let apps = unsafe {
        out::<EnumAssocHandlersVtbl>("SHAssocEnumHandlersForProtocolByApplication", |p| {
            SHAssocEnumHandlersForProtocolByApplication(w.as_ptr(), &IID_ENUM_ASSOC_HANDLERS, p)
        })
    }
    .map(apps_of)
    .unwrap_or_default();
    (apps, query_exe(s, true))
}

/// What the shell says now, per type. Exposed for the probe.
pub fn found() -> Vec<Found> {
    let _com = Apartment::sta();
    let mut out = Vec::new();
    for (mime, _, exts) in TABLE {
        let mut apps: Vec<App> = Vec::new();
        let mut default = None;
        for e in exts.iter() {
            let (a, d) = for_extension(e);
            for x in a {
                if !apps.iter().any(|y| y.id.eq_ignore_ascii_case(&x.id)) {
                    apps.push(x);
                }
            }
            // The first extension with a default (the canonical one first).
            default = default.or(d);
        }
        out.push(Found { key: Key::Mime((*mime).into()), apps, default });
    }
    for s in SCHEMES {
        let (apps, default) = for_scheme(s);
        out.push(Found { key: Key::Scheme((*s).into()), apps, default });
    }
    out
}

/// The handlers to offer Linux.
pub fn handlers() -> Vec<HostHandler> {
    collect(&found())
}
