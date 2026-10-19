//! The Mac's applications per type, from Launch Services: for every UTI of
//! `wc_hostint::mime::TABLE`, every application that opens it
//! (`LSCopyAllRoleHandlersForContentType`) and the default
//! (`LSCopyDefaultRoleHandlerForContentType`); for http, https and mailto,
//! the URL handlers (`LSCopyAllHandlersForURLScheme`,
//! `LSCopyDefaultHandlerForURLScheme`). These are deprecated since macOS 12
//! in favour of NSWorkspace's URL-based calls, and still answer on 14; they
//! are C, so no Objective-C runtime is needed here.
//!
//! An application's name is its bundle's file name, found through
//! `LSCopyApplicationURLsForBundleIdentifier`; a bundle id Launch Services
//! still lists but that is on no disk any more is dropped. Our own bundles
//! (`org.waycompose.*`) are dropped too (`plan::found`): offering Linux its
//! own applications "on the host" would loop. The bundle id is the
//! `HostHandler`'s `app`, which stays on the host; Linux gets the token.

use std::collections::HashMap;

use wc_hostint::host::HostHandler;

use super::cf;
use super::plan::{app_name_of_path, found, types, SCHEMES};
use crate::handlers::{collect, Found, Key};

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn LSCopyAllRoleHandlersForContentType(uti: cf::CFStringRef, roles: u32) -> cf::CFArrayRef;
    fn LSCopyDefaultRoleHandlerForContentType(uti: cf::CFStringRef, roles: u32) -> cf::CFStringRef;
    fn LSCopyAllHandlersForURLScheme(scheme: cf::CFStringRef) -> cf::CFArrayRef;
    fn LSCopyDefaultHandlerForURLScheme(scheme: cf::CFStringRef) -> cf::CFStringRef;
    fn LSCopyApplicationURLsForBundleIdentifier(bid: cf::CFStringRef, err: *mut cf::CFErrorRef) -> cf::CFArrayRef;
}

/// `kLSRolesViewer | kLSRolesEditor`: applications that open a document,
/// not shells (`kLSRolesShell`) or the rest.
const ROLES_OPEN: u32 = 0x2 | 0x4;

fn owned_strings(a: cf::CFArrayRef) -> Vec<String> {
    cf::Owned::new(a).map(|a| cf::string_array(a.get())).unwrap_or_default()
}

fn owned_string(s: cf::CFStringRef) -> Option<String> {
    cf::Owned::new(s).and_then(|s| cf::to_string(s.get()))
}

/// Every bundle that opens `uti`, and the default.
pub fn for_uti(uti: &str) -> (Vec<String>, Option<String>) {
    let s = cf::string(uti);
    // SAFETY: a live CFString; both results are +1, owned below.
    unsafe {
        (
            owned_strings(LSCopyAllRoleHandlersForContentType(s.get(), ROLES_OPEN)),
            owned_string(LSCopyDefaultRoleHandlerForContentType(s.get(), ROLES_OPEN)),
        )
    }
}

/// Every bundle that opens URLs of `scheme`, and the default.
pub fn for_scheme(scheme: &str) -> (Vec<String>, Option<String>) {
    let s = cf::string(scheme);
    // SAFETY: as in `for_uti`.
    unsafe { (owned_strings(LSCopyAllHandlersForURLScheme(s.get())), owned_string(LSCopyDefaultHandlerForURLScheme(s.get()))) }
}

/// Where the application with this bundle id is, if it is anywhere.
pub fn app_path(bid: &str) -> Option<String> {
    let s = cf::string(bid);
    // SAFETY: a live CFString; the array is +1 and owned; its URLs are
    // borrowed from it while it lives.
    let a = cf::Owned::new(unsafe { LSCopyApplicationURLsForBundleIdentifier(s.get(), std::ptr::null_mut()) })?;
    cf::array(a.get()).into_iter().find_map(cf::url_path)
}

/// What Launch Services says, per type and scheme, before the merge.
pub fn found_all() -> Vec<Found> {
    let mut names: HashMap<String, Option<String>> = HashMap::new();
    let mut name = |bid: &str| -> Option<String> {
        names.entry(bid.to_ascii_lowercase()).or_insert_with(|| app_path(bid).and_then(|p| app_name_of_path(&p))).clone()
    };
    let mut per_uti: HashMap<&str, (Vec<String>, Option<String>)> = HashMap::new();
    let mut out = Vec::new();
    for (mime, uti) in types() {
        let (all, default) = per_uti.entry(uti).or_insert_with(|| for_uti(uti)).clone();
        out.push(found(Key::Mime(mime.into()), &all, default.as_deref(), &mut name));
    }
    for scheme in SCHEMES {
        let (all, default) = for_scheme(scheme);
        out.push(found(Key::Scheme((*scheme).into()), &all, default.as_deref(), &mut name));
    }
    out
}

/// The host's handlers, as offered to Linux.
pub fn handlers() -> Vec<HostHandler> {
    collect(&found_all())
}
