//! What the GPU side was doing when it died (waycompose-design.md §86).
//!
//! On the real guest a GPU side serving Firefox died with a bare
//! `Segmentation fault` from MSYS and nothing else, twice in three runs. Running
//! it under `SV_TRACE=pre` changed the timing enough that the fault did not
//! happen. So this costs nothing on the hot path: each ring stores the opcode
//! and seqno it is about to dispatch in one relaxed atomic, and marks it done
//! afterwards. On Windows an unhandled-exception filter prints the exception,
//! the module the faulting address is in, and every ring's last command, then
//! lets the process die as it would have.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bit 63: the command finished. Bits 32..63: the opcode. Bits 0..32: the
/// seqno's low half.
const DONE: u64 = 1 << 63;
const SLOTS: usize = 16;

static IN_FLIGHT: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

/// Before dispatch.
#[inline]
pub fn begin(slot: u32, opcode: u32, seqno: u64) {
    if let Some(a) = IN_FLIGHT.get(slot as usize) {
        a.store(((opcode as u64 & 0x7fff_ffff) << 32) | (seqno & 0xffff_ffff), Ordering::Relaxed);
    }
}

/// After dispatch returned.
#[inline]
pub fn end(slot: u32) {
    if let Some(a) = IN_FLIGHT.get(slot as usize) {
        a.fetch_or(DONE, Ordering::Relaxed);
    }
}

/// One line per ring that has run anything: its last command, and whether
/// that command was still inside the driver.
pub fn report() -> String {
    let mut out = String::new();
    for (slot, a) in IN_FLIGHT.iter().enumerate() {
        let v = a.load(Ordering::Relaxed);
        if v == 0 {
            continue;
        }
        let op = ((v >> 32) & 0x7fff_ffff) as u32;
        let name = sv_proto::Opcode::from_u32(op).map_or_else(|| format!("opcode {op} (protocol, not Vulkan)"), |o| o.name().to_string());
        let state = if v & DONE != 0 { "returned" } else { "IN THE DRIVER" };
        out.push_str(&format!("  ring {slot}: seqno {} {name} ({state})\n", v & 0xffff_ffff));
    }
    out
}

#[cfg(windows)]
pub use win::install;

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    #[repr(C)]
    struct ExceptionRecord {
        code: u32,
        flags: u32,
        record: *mut ExceptionRecord,
        address: *mut c_void,
        nparams: u32,
        info: [usize; 15],
    }

    #[repr(C)]
    struct ExceptionPointers {
        record: *mut ExceptionRecord,
        context: *mut c_void,
    }

    type Filter = unsafe extern "system" fn(*mut ExceptionPointers) -> i32;

    unsafe extern "system" {
        fn SetUnhandledExceptionFilter(f: Option<Filter>) -> Option<Filter>;
        fn GetModuleHandleExW(flags: u32, addr: *const u16, out: *mut *mut c_void) -> i32;
        fn GetModuleFileNameW(h: *mut c_void, buf: *mut u16, n: u32) -> u32;
    }

    const FROM_ADDRESS: u32 = 0x4;
    const UNCHANGED_REFCOUNT: u32 = 0x2;
    const CONTINUE_SEARCH: i32 = 0;

    fn module_of(addr: *mut c_void) -> String {
        let mut h: *mut c_void = std::ptr::null_mut();
        // SAFETY: FROM_ADDRESS takes the address as the "name"; the handle is
        // not reference-counted with UNCHANGED_REFCOUNT, so nothing to free.
        let ok = unsafe { GetModuleHandleExW(FROM_ADDRESS | UNCHANGED_REFCOUNT, addr as *const u16, &mut h) };
        if ok == 0 || h.is_null() {
            return "<no module>".into();
        }
        let mut buf = [0u16; 520];
        // SAFETY: the buffer is as long as the count says.
        let n = unsafe { GetModuleFileNameW(h, buf.as_mut_ptr(), buf.len() as u32) } as usize;
        format!("{}+0x{:x}", String::from_utf16_lossy(&buf[..n]), addr as usize - h as usize)
    }

    unsafe extern "system" fn filter(p: *mut ExceptionPointers) -> i32 {
        // SAFETY: the system passes a valid EXCEPTION_POINTERS to the filter.
        let r = unsafe { &*(*p).record };
        let mut msg = format!(
            "[gpu] FAULT: exception 0x{:08x} at {:p} in {}\n",
            r.code,
            r.address,
            module_of(r.address)
        );
        if r.code == 0xC000_0005 && r.nparams >= 2 {
            let kind = match r.info[0] {
                0 => "read",
                1 => "write",
                8 => "execute",
                _ => "access",
            };
            msg.push_str(&format!("[gpu] FAULT: {kind} of 0x{:x}\n", r.info[1]));
        }
        msg.push_str("[gpu] FAULT: the last command on each ring:\n");
        msg.push_str(&super::report());
        // docs/video-decode.md §7.10: the ring slots above are shared by every
        // session of the process, so the video log's own last line is what
        // names the call that was inside a decode driver.
        msg.push_str(&crate::video_log::fault_summary());
        msg.push_str(&format!("{}\n", std::backtrace::Backtrace::force_capture()));
        // Written in one call so that two dying threads do not interleave.
        use std::io::Write;
        let _ = std::io::stderr().write_all(msg.as_bytes());
        // Also into the video log's file, if it has one: that file is what
        // the guest run collects.
        crate::video_log::emit_raw(&msg);
        // SAFETY: `p` is the filter's own argument, valid for this call.
        unsafe { minidump(p) };
        CONTINUE_SEARCH
    }

    #[repr(C, packed(4))]
    struct MinidumpExceptionInformation {
        thread_id: u32,
        exception_pointers: *mut ExceptionPointers,
        client_pointers: i32,
    }

    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            disposition: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
        fn CloseHandle(h: *mut c_void) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
        fn GetCurrentProcessId() -> u32;
        fn GetCurrentThreadId() -> u32;
    }

    type MiniDumpWriteDump = unsafe extern "system" fn(
        process: *mut c_void,
        pid: u32,
        file: *mut c_void,
        dump_type: u32,
        exception: *const MinidumpExceptionInformation,
        user_stream: *const c_void,
        callback: *const c_void,
    ) -> i32;

    /// `SV_MINIDUMP=<file>`: a minidump of this process at the fault, with
    /// every thread's stack and the data segments, for the guest run of
    /// docs/video-decode.md §7.10. Written from inside the dying process — the
    /// documented caveat of doing so is that a corrupted loader lock can stop
    /// it, in which case the text report above has already been written.
    unsafe fn minidump(p: *mut ExceptionPointers) {
        let Some(path) = std::env::var_os("SV_MINIDUMP") else { return };
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.encode_wide().chain([0]).collect();
        let lib: Vec<u16> = "dbghelp.dll".encode_utf16().chain([0]).collect();
        // SAFETY: plain Win32 calls with NUL-terminated strings; every handle
        // is checked before use and closed after.
        unsafe {
            let m = LoadLibraryW(lib.as_ptr());
            if m.is_null() {
                eprintln!("[gpu] FAULT: no dbghelp.dll, no minidump");
                return;
            }
            let f = GetProcAddress(m, c"MiniDumpWriteDump".as_ptr().cast());
            if f.is_null() {
                return;
            }
            let write: MiniDumpWriteDump = std::mem::transmute(f);
            const GENERIC_WRITE: u32 = 0x4000_0000;
            const CREATE_ALWAYS: u32 = 2;
            const INVALID: isize = -1;
            let h = CreateFileW(wide.as_ptr(), GENERIC_WRITE, 0, std::ptr::null_mut(), CREATE_ALWAYS, 0x80, std::ptr::null_mut());
            if h as isize == INVALID || h.is_null() {
                eprintln!("[gpu] FAULT: cannot create the minidump file");
                return;
            }
            let info = MinidumpExceptionInformation {
                thread_id: GetCurrentThreadId(),
                exception_pointers: p,
                client_pointers: 0,
            };
            // MiniDumpWithDataSegs | MiniDumpWithHandleData |
            // MiniDumpWithThreadInfo | MiniDumpWithIndirectlyReferencedMemory:
            // the faulting frame's pointers are what is wanted, and the
            // indirectly referenced memory is what holds the structures they
            // point at.
            let kind = 0x1 | 0x4 | 0x40 | 0x1000;
            let ok = write(GetCurrentProcess(), GetCurrentProcessId(), h, kind, &info, std::ptr::null(), std::ptr::null());
            CloseHandle(h);
            eprintln!("[gpu] FAULT: minidump {} ({})", path.to_string_lossy(), if ok != 0 { "written" } else { "failed" });
        }
    }

    /// Install the filter. Once per process; later calls replace it with itself.
    pub fn install() {
        // SAFETY: a plain function pointer with the documented signature.
        unsafe {
            SetUnhandledExceptionFilter(Some(filter));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_names_the_command_and_whether_it_returned() {
        let op = sv_proto::Opcode::VkCreatePipelineLayout as u32;
        begin(9, op, 41);
        assert!(report().contains("ring 9: seqno 41 vkCreatePipelineLayout (IN THE DRIVER)"), "{}", report());
        end(9);
        assert!(report().contains("ring 9: seqno 41 vkCreatePipelineLayout (returned)"), "{}", report());
    }
}
