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
    ASSOC_FILTER_RECOMMENDED, ASSOCF_IS_PROTOCOL, ASSOCF_NONE, ASSOCSTR, ASSOCSTR_EXECUTABLE, ASSOCSTR_FRIENDLYAPPNAME,
    AssocQueryStringW,
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
        if is_exe(&id) && !is_open_with_dialog(&id) {
            out.push(App { id, name: label });
        }
    }
    out
}

fn query(assoc: &str, protocol: bool, what: ASSOCSTR) -> Option<String> {
    let a = wide(assoc);
    let verb = wide("open");
    let mut buf = vec![0u16; 1024];
    let mut n = buf.len() as u32;
    let flags = if protocol { ASSOCF_IS_PROTOCOL } else { ASSOCF_NONE };
    // SAFETY: NUL-terminated inputs and a buffer of `n` characters.
    let hr = unsafe { AssocQueryStringW(flags, what, a.as_ptr(), verb.as_ptr(), buf.as_mut_ptr(), &mut n) };
    (hr >= 0).then(|| from_wide(&buf))
}

/// The shell's "pick an application" dialog, which is what a type with no
/// association resolves to. Never an application to offer.
fn is_open_with_dialog(exe: &str) -> bool {
    exe.rsplit(['\\', '/']).next().is_some_and(|b| b.eq_ignore_ascii_case("OpenWith.exe"))
}

/// The default application for `assoc`, with the shell's name for it.
fn query_default(assoc: &str, protocol: bool) -> Option<App> {
    let id = query(assoc, protocol, ASSOCSTR_EXECUTABLE).filter(|s| is_exe(s) && !is_open_with_dialog(s))?;
    let name = query(assoc, protocol, ASSOCSTR_FRIENDLYAPPNAME).filter(|n| !n.trim().is_empty()).unwrap_or_default();
    Some(App { id, name })
}

fn for_extension(ext: &str) -> (Vec<App>, Option<App>) {
    let w = wide(&format!(".{ext}"));
    // SAFETY: an IEnumAssocHandlers comes back.
    let apps = unsafe {
        out::<EnumAssocHandlersVtbl>("SHAssocEnumHandlers", |p| SHAssocEnumHandlers(w.as_ptr(), ASSOC_FILTER_RECOMMENDED, p))
    }
    .map(apps_of)
    .unwrap_or_default();
    (apps, query_default(&format!(".{ext}"), false))
}

fn for_scheme(s: &str) -> (Vec<App>, Option<App>) {
    let w = wide(s);
    // SAFETY: an IEnumAssocHandlers comes back.
    let apps = unsafe {
        out::<EnumAssocHandlersVtbl>("SHAssocEnumHandlersForProtocolByApplication", |p| {
            SHAssocEnumHandlersForProtocolByApplication(w.as_ptr(), &IID_ENUM_ASSOC_HANDLERS, p)
        })
    }
    .map(apps_of)
    .unwrap_or_default();
    (apps, query_default(s, true))
}

/// What the shell says now, per type. Exposed for the probe.
pub fn found() -> Vec<Found> {
    let _com = Apartment::sta();
    let mut out = Vec::new();
    for (mime, _, exts) in TABLE {
        let mut apps: Vec<App> = Vec::new();
        let mut default = None;
        for e in exts.iter() {
            let (mut a, d) = for_extension(e);
            // The default is offered even when it is not "recommended".
            a.extend(d.clone());
            for x in a {
                if !apps.iter().any(|y| y.id.eq_ignore_ascii_case(&x.id)) {
                    apps.push(x);
                }
            }
            // The first extension with a default (the canonical one first).
            default = default.or(d.map(|d| d.id));
        }
        out.push(Found { key: Key::Mime((*mime).into()), apps, default });
    }
    for s in SCHEMES {
        let (mut apps, default) = for_scheme(s);
        if let Some(d) = &default
            && !apps.iter().any(|a| a.id.eq_ignore_ascii_case(&d.id))
        {
            apps.push(d.clone());
        }
        out.push(Found { key: Key::Scheme((*s).into()), apps, default: default.map(|d| d.id) });
    }
    out
}

/// The handlers to offer Linux.
pub fn handlers() -> Vec<HostHandler> {
    collect(&found())
}
