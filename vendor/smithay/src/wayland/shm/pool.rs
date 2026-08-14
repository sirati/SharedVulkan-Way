#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    num::NonZeroUsize,
    sync::{
        mpsc::{channel, Sender},
        LazyLock, RwLock,
    },
    thread,
};

// `ptr` is only reached by the fault guard and the `mmap` wrappers.
#[cfg(unix)]
use std::{cell::Cell, mem, ptr, sync::OnceLock};

use crate::utils::os::{Mapping, ProtoFd, WireFd};
#[cfg(unix)]
use crate::utils::os::AsFd;

#[cfg(unix)]
use rustix::mm;
use tracing::{debug, instrument, trace};

// Dropping Pool is actually pretty slow. Unmapping the memory can take 1-2 ms, but the real
// offender is closing the file descriptor, which I've seen take up to 6 ms. It's waiting on some
// spinlock in the kernel.
//
// Blocking the main thread for 6 ms is quite bad. In fact, 6 ms is almost the entire time budget
// for a 165 Hz frame. To make matters worse, some clients will cause repeated creation and
// dropping of shm pools, like Firefox during a focus-out animation. This results in dropped
// frames.
//
// To work around this problem, we spawn a separate thread whose sole purpose is dropping stuff we
// send it through a channel. Conveniently, Pool is already Send, so there's no problem doing this.
static DROP_THIS: LazyLock<Sender<InnerPool>> = LazyLock::new(|| {
    let (tx, rx) = channel();
    thread::Builder::new()
        .name("Shm dropping thread".to_owned())
        .spawn(move || {
            while let Ok(x) = rx.recv() {
                profiling::scope!("dropping Pool");
                drop(x);
            }
        })
        .unwrap();
    tx
});

// THE FAULT GUARD IS UNIX-ONLY, AND THAT IS A PROPERTY OF THE PLATFORM RATHER
// THAN AN OMISSION.
//
// It exists because a Linux client can `ftruncate` its pool *smaller* after the
// compositor has mapped it. Touching the truncated tail then raises `SIGBUS` in
// the compositor, so libwayland — and this file — install a process-wide
// handler, remember which mapping was being read, replace it with anonymous
// zero pages and report the access as failed.
//
// A Windows section object's size is fixed at `CreateFileMappingW` time and
// cannot be reduced afterwards. The event the handler defends against therefore
// cannot occur, so there is nothing for a vectored exception handler to catch
// and none is installed. docs/design.md §2.6 records this as the one place
// where the Windows model is *stronger* than the Unix one, and §5.4 item 1
// asked for the question to be settled before budgeting 120-180 lines for an
// `EXCEPTION_IN_PAGE_ERROR` reimplementation. It is settled: those lines are
// not needed.
//
// What remains on Windows is the `Err(())` return path, which is kept alive for
// `with_data_mut` (see below) and would be where a future fault check reported.
#[cfg(unix)]
thread_local!(static SIGBUS_GUARD: Cell<(*const MemMap, bool)> = const { Cell::new((ptr::null_mut(), false)) });

#[cfg(unix)]
static OLD_SIGBUS_HANDLER: OnceLock<libc::sigaction> = OnceLock::new();

#[derive(Debug)]
pub struct Pool {
    inner: Option<InnerPool>,
}

#[derive(Debug)]
struct InnerPool {
    map: RwLock<MemMap>,
    fd: ProtoFd,
}

// SAFETY: The memmap is owned by the pool and content is only accessible via a reference.
unsafe impl Send for InnerPool {}
// SAFETY: The memmap is guarded by a RwLock, meaning no writers may mutate the memmap when it is being read.
unsafe impl Sync for InnerPool {}

pub enum ResizeError {
    InvalidSize,
    MremapFailed,
}

impl InnerPool {
    #[instrument(level = "trace", skip_all, name = "wayland_shm")]
    pub fn new(fd: ProtoFd, size: NonZeroUsize) -> Result<InnerPool, ProtoFd> {
        let memmap = match MemMap::new(&fd, size) {
            Ok(memmap) => memmap,
            Err(_) => {
                return Err(fd);
            }
        };
        trace!(fd = ?fd, size = ?size, "Creating new shm pool");
        Ok(InnerPool {
            map: RwLock::new(memmap),
            fd,
        })
    }

    pub fn resize(&self, newsize: NonZeroUsize) -> Result<(), ResizeError> {
        let mut guard = self.map.write().unwrap();
        let oldsize = guard.size();

        if oldsize > usize::from(newsize) {
            return Err(ResizeError::InvalidSize);
        }

        trace!(fd = ?self.fd, oldsize = oldsize, newsize = ?newsize, "Resizing shm pool");
        guard.remap(&self.fd, newsize).map_err(|()| {
            debug!(fd = ?self.fd, oldsize = oldsize, newsize = ?newsize, "SHM pool resize failed");
            ResizeError::MremapFailed
        })
    }

    pub fn size(&self) -> usize {
        self.map.read().unwrap().size
    }

    #[instrument(level = "trace", skip_all, name = "wayland_shm")]
    pub fn with_data<T, F: FnOnce(*const u8, usize) -> T>(&self, f: F) -> Result<T, ()> {
        // Place the fault handler. No-op where the fault cannot happen.
        unsafe { place_sigbus_handler() };

        let pool_guard = self.map.read().unwrap();

        trace!(fd = ?self.fd, "Buffer access on shm pool");

        guard_enter(&pool_guard);
        let t = f(pool_guard.ptr as *const _, pool_guard.size);
        if guard_leave() {
            debug!(fd = ?self.fd, "SIGBUS caught on access on shm pool");
            Err(())
        } else {
            Ok(t)
        }
    }

    #[instrument(level = "trace", skip_all, name = "wayland_shm")]
    #[cfg(unix)]
    pub fn with_data_mut<T, F: FnOnce(*mut u8, usize) -> T>(&self, f: F) -> Result<T, ()> {
        // Place the sigbus handler
        unsafe { place_sigbus_handler() };

        // This is actually a write access.
        #[allow(clippy::readonly_write_lock)]
        let pool_guard = self.map.write().unwrap();

        trace!(fd = ?self.fd, "Mutable buffer access on shm pool");

        guard_enter(&pool_guard);
        let t = f(pool_guard.ptr, pool_guard.size);
        if guard_leave() {
            debug!(fd = ?self.fd, "SIGBUS caught on access on shm pool");
            Err(())
        } else {
            Ok(t)
        }
    }

    /// NOT IMPLEMENTED ON WINDOWS, and failing rather than pretending.
    ///
    /// The Windows mapping is created `FILE_MAP_READ`, so the pages are
    /// genuinely read-only and handing the caller a `*mut u8` into them would
    /// fault on the first store rather than misbehave visibly. That is a worse
    /// outcome than an error, so this returns `Err(())`.
    ///
    /// Read-only is `wc_handle`'s deliberate default: a compositor's job with a
    /// client's `wl_shm_pool` is to read it, and there is no `map_mut` to reach
    /// for. The callers that legitimately want to write are screen-capture
    /// paths handing a buffer *back* to a client, which this project does not
    /// have yet. Implementing it means a writable `wc_handle` mapping and a
    /// decision about which side owns the section — deferred with that stated,
    /// not overlooked.
    #[cfg(windows)]
    pub fn with_data_mut<T, F: FnOnce(*mut u8, usize) -> T>(&self, _f: F) -> Result<T, ()> {
        debug!(fd = ?self.fd, "with_data_mut is not available on Windows: shm mappings are read-only");
        Err(())
    }
}

/// Arm the fault guard around an access, on the platform that has one.
#[cfg(unix)]
fn guard_enter(map: &MemMap) {
    SIGBUS_GUARD.with(|guard| {
        let (p, _) = guard.get();
        if !p.is_null() {
            // Recursive call of this method is not supported
            panic!("Recursive access to a SHM pool content is not supported.");
        }
        guard.set((map as *const MemMap, false))
    });
}

/// Disarm the fault guard; returns whether a fault was caught during the access.
#[cfg(unix)]
fn guard_leave() -> bool {
    SIGBUS_GUARD.with(|guard| {
        let (_, triggered) = guard.get();
        guard.set((ptr::null_mut(), false));
        triggered
    })
}

/// See the note on `SIGBUS_GUARD`: a Windows section cannot shrink, so there is
/// no fault to guard against and nothing to arm.
#[cfg(windows)]
fn guard_enter(_map: &MemMap) {}

/// See [`guard_enter`]. Always reports "no fault", because none is possible.
#[cfg(windows)]
fn guard_leave() -> bool {
    false
}

impl Pool {
    pub fn new(fd: ProtoFd, size: NonZeroUsize) -> Result<Self, ProtoFd> {
        InnerPool::new(fd, size).map(|p| Self { inner: Some(p) })
    }

    pub fn resize(&self, newsize: NonZeroUsize) -> Result<(), ResizeError> {
        self.inner.as_ref().unwrap().resize(newsize)
    }

    pub fn size(&self) -> usize {
        self.inner.as_ref().unwrap().size()
    }

    pub fn with_data<T, F: FnOnce(*const u8, usize) -> T>(&self, f: F) -> Result<T, ()> {
        self.inner.as_ref().unwrap().with_data(f)
    }

    pub fn with_data_mut<T, F: FnOnce(*mut u8, usize) -> T>(&self, f: F) -> Result<T, ()> {
        self.inner.as_ref().unwrap().with_data_mut(f)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let _ = DROP_THIS.send(self.inner.take().unwrap());
    }
}

/// A live mapping of a client's pool.
///
/// The **named** arm carries the mapping object itself, not only a pointer, and
/// the difference is load-bearing. `mmap` is an operation on the address space:
/// once it returns, the pointer stands on its own and the fd could be closed.
/// `wc_handle::Handle::map` returns a view whose lifetime is the mapping's, so
/// something has to hold it — and if nothing does, the pointer dangles the moment
/// the temporary is dropped, which is a silent use-after-free rather than a
/// compile error.
///
/// §79 made `_view` unconditional. It was `#[cfg(windows)]` while a named object
/// was a Windows-only thing; a Linux compositor behind `wc-proxy` now maps one
/// too, and whether the view has to be held is a property of **which kind of `h`
/// arrived**, not of which platform is running.
#[derive(Debug)]
struct MemMap {
    ptr: *mut u8,
    size: usize,
    /// Keeps the view alive for as long as `ptr` is used. Never read.
    ///
    /// `Some` exactly when the pool came from a [`WireFd::Named`], which is also
    /// what tells `Drop` not to `munmap`: the view owns the unmapping.
    _view: Option<Mapping>,
}

impl MemMap {
    fn new(fd: &ProtoFd, size: NonZeroUsize) -> Result<MemMap, ()> {
        let (ptr, view) = map(fd, size)?;
        Ok(MemMap { ptr, size: size.into(), _view: view })
    }

    fn remap(&mut self, fd: &ProtoFd, newsize: NonZeroUsize) -> Result<(), ()> {
        if self.ptr.is_null() {
            return Err(());
        }

        // §79: the two arms below are chosen by **what kind of `h` this pool came
        // from**, which used to be a platform question and is now a per-client
        // one. The bodies are unchanged from the `cfg`s they replace.
        match fd {
            // An open descriptor can be re-`mmap`ed larger, so the old mapping
            // goes first and a failure leaves the pool in the empty state
            // upstream defined.
            #[cfg(unix)]
            WireFd::Open(_) => {
                // memunmap cannot fail, as we are unmapping a pre-existing map
                let _ = unsafe { unmap(self.ptr, self.size) };
                match map(fd, newsize) {
                    Ok((ptr, view)) => {
                        self.ptr = ptr;
                        self.size = usize::from(newsize);
                        self._view = view;
                        Ok(())
                    }
                    Err(()) => {
                        self.ptr = ptr::null_mut();
                        self.size = 0;
                        Err(())
                    }
                }
            }

            // A named object's size is fixed when it is created — a Windows
            // section at `CreateFileMappingW` time — so there is no `mremap`. The
            // old view is dropped only after the new one succeeds, so a failed
            // grow leaves the pool exactly as it was rather than in the empty
            // state the other arm falls back to: a pool that is still the old
            // size is more useful to a client than a null one.
            //
            // `wl_shm_pool.resize` only ever grows, and a grow past the object's
            // creation size cannot be satisfied here. `map()` reports that, and
            // the caller turns it into `ResizeError::MremapFailed`, which is
            // already the protocol-visible outcome for a failed resize. The real
            // fix is `SEC_RESERVE` — reserve a large range at create time and
            // commit as the pool grows — which is docs/design.md §6’s "wl_shm
            // properly" item and is a `wc-handle` change, not a smithay one.
            WireFd::Named(_) => {
                let (ptr, view) = map(fd, newsize)?;
                self.ptr = ptr;
                self.size = usize::from(newsize);
                self._view = view;
                Ok(())
            }
        }
    }

    fn size(&self) -> usize {
        self.size
    }

    #[cfg_attr(windows, allow(dead_code))]
    fn contains(&self, ptr: *mut u8) -> bool {
        ptr >= self.ptr && ptr < unsafe { self.ptr.add(self.size) }
    }

    #[cfg(unix)]
    fn nullify(&self) -> Result<(), ()> {
        unsafe { nullify_map(self.ptr, self.size) }
    }
}

impl Drop for MemMap {
    fn drop(&mut self) {
        // §79: `_view` is `Some` exactly for a named mapping, and dropping it is
        // what unmaps that one. `munmap` is for the other kind and would be
        // wrong here — the address did not come from `mmap`.
        #[cfg(unix)]
        if self._view.is_none() && !self.ptr.is_null() {
            let _ = unsafe { unmap(self.ptr, self.size) };
        }
    }
}

/// Map a client's pool, whichever kind of `h` it arrived as.
///
/// §79 folded two `#[cfg]` functions into one `match`. The bodies are unchanged;
/// what changed is that **both are reachable on both platforms**, because which
/// one applies is now a property of the connection the pool came in on rather
/// than of the kernel this is running under.
///
/// Returns the keepalive as `Option`: `mmap` needs none (the pointer stands on
/// its own once it returns), and a named mapping needs one (the view owns the
/// address range). That `Option` is also what `Drop` reads to decide whether to
/// `munmap`.
fn map(fd: &ProtoFd, size: NonZeroUsize) -> Result<(*mut u8, Option<Mapping>), ()> {
    match fd {
        // A simple wrapper with some default arguments for `nix::mman::mmap`.
        #[cfg(unix)]
        WireFd::Open(owned) => {
            let ret = unsafe {
                mm::mmap(
                    ptr::null_mut(),
                    size.into(),
                    mm::ProtFlags::READ | mm::ProtFlags::WRITE,
                    mm::MapFlags::SHARED,
                    owned.as_fd(),
                    0,
                )
            };
            ret.map(|p| (p as *mut u8, None)).map_err(|_| ())
        }

        // Map a client's named object, read-only, and hand back both the pointer
        // and the object that keeps it valid.
        //
        // Not `unsafe`, unlike the arm above: there is no raw address, length or
        // flag combination for the caller to get wrong. `wc_handle` opens the
        // named object by name and maps the whole of it, so the only failure
        // modes are "the object is gone" and "it is not a section", both of
        // which come back as `Err`.
        //
        // `size` is checked rather than requested. The pool's size is a protocol
        // argument the client chose; the object's size is fixed at creation. A
        // client asking for more than it created is a protocol error the caller
        // turns into a failed map, which is the same outcome an over-large
        // `mmap` offset gives on the other arm.
        WireFd::Named(handle) => {
            // `eprintln!` and not `debug!`, and that was learned rather than
            // chosen. Failing to map a client's pool is protocol-fatal: the
            // client is disconnected and told only "Failed to mmap fd", because
            // the caller in `shm/handlers.rs` has the handle and not the error.
            // This binary installs no `tracing` subscriber, so the one line that
            // says *why* went nowhere, and stage 4 spent an hour guessing at a
            // `MapViewOfFile` failure whose cause was already known here.
            let mapping = handle.map().map_err(|err| {
                eprintln!("wc-compositor: could not map {:?}: {err}", handle.descriptor());
            })?;
            if mapping.len() < usize::from(size) {
                eprintln!(
                    "wc-compositor: pool wants {} bytes, {:?} has {}",
                    usize::from(size),
                    handle.descriptor(),
                    mapping.len()
                );
                return Err(());
            }
            let ptr = mapping.as_slice().as_ptr() as *mut u8;
            Ok((ptr, Some(mapping)))
        }
    }
}


/// A simple wrapper for `nix::mman::munmap`.
#[cfg(unix)]
#[profiling::function]
unsafe fn unmap(ptr: *mut u8, size: usize) -> Result<(), ()> {
    let ret = unsafe { mm::munmap(ptr as *mut _, size) };
    ret.map_err(|_| ())
}

#[cfg(unix)]
unsafe fn nullify_map(ptr: *mut u8, size: usize) -> Result<(), ()> {
    let ret = unsafe {
        mm::mmap_anonymous(
            ptr as *mut std::ffi::c_void,
            size,
            mm::ProtFlags::READ | mm::ProtFlags::WRITE,
            mm::MapFlags::PRIVATE | mm::MapFlags::FIXED,
        )
    };
    ret.map(|_| ()).map_err(|_| ())
}

/// See the note on `SIGBUS_GUARD`: the fault this guards against cannot occur
/// on a Windows section, so there is no handler to place.
#[cfg(windows)]
unsafe fn place_sigbus_handler() {}

/// The sigbus handler will be placed only once
#[cfg(unix)]
unsafe fn place_sigbus_handler() {
    let _ = OLD_SIGBUS_HANDLER.get_or_init(|| {
        // create our sigbus handler
        unsafe {
            // We use `mem::zeroed()` because regular struct init as well as struct update syntax require all fields to be public
            // and libc does not guarantee that for all targets
            let mut action: libc::sigaction = mem::zeroed();
            // Cast through the function-pointer type rather than straight to
            // an integer: `fn_item as usize` triggers `fn_ptr_addr_cast`
            // because a function *item* is zero-sized and the coercion is
            // implicit. Naming the pointer type makes the coercion explicit and
            // the value identical.
            action.sa_sigaction =
                (sigbus_handler as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void))
                    as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;

            let mut old_action = mem::zeroed();
            if libc::sigaction(libc::SIGBUS, &action, &mut old_action) == -1 {
                let e = rustix::io::Errno::from_raw_os_error(errno::errno().0);
                panic!("sigaction failed for SIGBUS handler: {:?}", e);
            }

            old_action
        }
    });
}

#[cfg(unix)]
unsafe fn reraise_sigbus() {
    // reset the old sigaction
    unsafe {
        libc::sigaction(libc::SIGBUS, OLD_SIGBUS_HANDLER.get().unwrap(), ptr::null_mut());
        libc::raise(libc::SIGBUS);
    }
}

#[cfg(unix)]
extern "C" fn sigbus_handler(_signum: libc::c_int, info: *mut libc::siginfo_t, _context: *mut libc::c_void) {
    let faulty_ptr = unsafe { siginfo_si_addr(info) } as *mut u8;
    SIGBUS_GUARD.with(|guard| {
        let (memmap, _) = guard.get();
        match unsafe { memmap.as_ref() }.map(|m| (m, m.contains(faulty_ptr))) {
            Some((m, true)) => {
                // we are in a faulty memory pool !
                // remember that it was faulty
                guard.set((memmap, true));
                // nullify the pool
                if m.nullify().is_err() {
                    // something terrible occurred !
                    unsafe { reraise_sigbus() }
                }
            }
            _ => {
                // something else occurred, let's die honorably
                unsafe { reraise_sigbus() }
            }
        }
    });
}

/// This was shamelessly stolen from rustc's source
/// so I expect it to work whenever rust works
/// I guess it's good enough?
///
/// SAFETY:
/// The returned pointer points to a struct. Make sure that you use it
/// appropriately.
#[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
unsafe fn siginfo_si_addr(info: *mut libc::siginfo_t) -> *mut libc::c_void {
    #[repr(C)]
    #[allow(non_camel_case_types)]
    struct siginfo_t {
        a: [libc::c_int; 3], // si_signo, si_errno, si_code
        si_addr: *mut libc::c_void,
    }

    unsafe { (*(info as *const siginfo_t)).si_addr }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
unsafe fn siginfo_si_addr(info: *mut libc::siginfo_t) -> *mut libc::c_void {
    unsafe { (*info).si_addr as _ }
}
