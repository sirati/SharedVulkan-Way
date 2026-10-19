//! The few CoreFoundation calls the macOS half needs, declared by hand (as
//! `audio_mac.rs` and the prototype's `coreaudio.rs` do): strings, arrays,
//! URLs and bundles, and an owner that releases what a `Copy`/`Create`
//! function returned.

use std::ffi::{c_char, c_void, CStr};

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFURLRef = *const c_void;
pub type CFBundleRef = *const c_void;
pub type CFErrorRef = *const c_void;
pub type CFIndex = isize;
pub type Boolean = u8;

const UTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub fn CFRelease(cf: CFTypeRef);
    fn CFStringCreateWithBytes(alloc: CFTypeRef, bytes: *const u8, len: CFIndex, encoding: u32, external: Boolean) -> CFStringRef;
    fn CFStringGetLength(s: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(len: CFIndex, encoding: u32) -> CFIndex;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> Boolean;
    fn CFArrayGetCount(a: CFArrayRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(a: CFArrayRef, i: CFIndex) -> *const c_void;
    fn CFURLGetFileSystemRepresentation(url: CFURLRef, resolve: Boolean, buf: *mut u8, max: CFIndex) -> Boolean;
    fn CFURLCreateFromFileSystemRepresentation(alloc: CFTypeRef, buf: *const u8, len: CFIndex, is_dir: Boolean) -> CFURLRef;
    fn CFURLCreateWithBytes(alloc: CFTypeRef, bytes: *const u8, len: CFIndex, encoding: u32, base: CFURLRef) -> CFURLRef;
    fn CFBundleGetMainBundle() -> CFBundleRef;
    fn CFBundleGetIdentifier(b: CFBundleRef) -> CFStringRef;
    fn CFBundleCreate(alloc: CFTypeRef, url: CFURLRef) -> CFBundleRef;
}

/// A CoreFoundation object this code owns (+1), released on drop.
pub struct Owned(pub CFTypeRef);

impl Owned {
    /// `None` for a null result.
    pub fn new(r: CFTypeRef) -> Option<Owned> {
        (!r.is_null()).then_some(Owned(r))
    }
    pub fn get(&self) -> CFTypeRef {
        self.0
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: a +1 reference this value owns, released once.
        unsafe { CFRelease(self.0) }
    }
}

// SAFETY: CoreFoundation's immutable objects (strings, URLs, arrays) may be
// released on any thread.
unsafe impl Send for Owned {}

/// A new CFString.
pub fn string(s: &str) -> Owned {
    // SAFETY: valid UTF-8 bytes and their length; the result is +1.
    let r = unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as CFIndex, UTF8, 0) };
    Owned::new(r).expect("CFStringCreateWithBytes")
}

/// A CFString's text. `s` is borrowed.
pub fn to_string(s: CFStringRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    // SAFETY: `s` is a live CFString; the buffer is as large as CF says the
    // UTF-8 form can be, plus the terminator.
    unsafe {
        let max = CFStringGetMaximumSizeForEncoding(CFStringGetLength(s), UTF8) + 1;
        let mut buf = vec![0 as c_char; usize::try_from(max).ok()?];
        if CFStringGetCString(s, buf.as_mut_ptr(), max, UTF8) == 0 {
            return None;
        }
        Some(CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

/// The elements of a CFArray, borrowed.
pub fn array(a: CFArrayRef) -> Vec<*const c_void> {
    if a.is_null() {
        return Vec::new();
    }
    // SAFETY: `a` is a live CFArray; indices are below its count.
    unsafe { (0..CFArrayGetCount(a)).map(|i| CFArrayGetValueAtIndex(a, i)).collect() }
}

/// A CFArray of CFStrings, as strings.
pub fn string_array(a: CFArrayRef) -> Vec<String> {
    array(a).into_iter().filter_map(to_string).collect()
}

/// A file URL's path.
pub fn url_path(u: CFURLRef) -> Option<String> {
    if u.is_null() {
        return None;
    }
    let mut buf = vec![0u8; 4096];
    // SAFETY: a live CFURL and a buffer of the size passed.
    let ok = unsafe { CFURLGetFileSystemRepresentation(u, 1, buf.as_mut_ptr(), buf.len() as CFIndex) };
    if ok == 0 {
        return None;
    }
    let n = buf.iter().position(|&b| b == 0)?;
    String::from_utf8(buf[..n].to_vec()).ok()
}

/// A file URL for a path.
pub fn file_url(path: &str, is_dir: bool) -> Option<Owned> {
    // SAFETY: the path's bytes and length; the result is +1.
    Owned::new(unsafe {
        CFURLCreateFromFileSystemRepresentation(std::ptr::null(), path.as_ptr(), path.len() as CFIndex, u8::from(is_dir))
    })
}

/// A URL from its text (an `https:` URL, say).
pub fn url(text: &str) -> Option<Owned> {
    // SAFETY: the text's bytes and length; the result is +1.
    Owned::new(unsafe {
        CFURLCreateWithBytes(std::ptr::null(), text.as_ptr(), text.len() as CFIndex, UTF8, std::ptr::null())
    })
}

/// The bundle identifier of the bundle at `path` (an `.app`).
pub fn bundle_id_at(path: &str) -> Option<String> {
    let u = file_url(path, true)?;
    // SAFETY: a live URL; the bundle is +1 and released by `Owned`; the
    // identifier is borrowed from it and copied out before it goes.
    unsafe {
        let b = Owned::new(CFBundleCreate(std::ptr::null(), u.get()))?;
        to_string(CFBundleGetIdentifier(b.get()))
    }
}

/// This process's own bundle identifier: the bundle its executable is in,
/// or `None` for a bare executable.
pub fn main_bundle_id() -> Option<String> {
    // SAFETY: the main bundle and its identifier are borrowed (Get rule).
    unsafe {
        let b = CFBundleGetMainBundle();
        if b.is_null() {
            return None;
        }
        to_string(CFBundleGetIdentifier(b))
    }
}
