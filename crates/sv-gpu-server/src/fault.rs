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
        msg.push_str(&format!("{}\n", std::backtrace::Backtrace::force_capture()));
        // Written in one call so that two dying threads do not interleave.
        use std::io::Write;
        let _ = std::io::stderr().write_all(msg.as_bytes());
        CONTINUE_SEARCH
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
