//! Native Windows ring attachment for a Cygwin registry client.
//!
//! The C ABI uses fixed-width integers and size_t, which agree between
//! Cygwin LP64 and native Windows LLP64. It never passes a Cygwin fd to
//! Winsock or treats an SCM_RIGHTS payload as a cross-VM handle.
//! Only one connection per process is supported. Rendered wl_shm clients
//! still need a BAR-backed mmap/descriptor adapter and are not supported.

#[cfg(windows)]
mod windows {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use sv_transport::control::{Control, ProtocolIds};
    use sv_transport::region::SharedRegion;
    use sv_transport::vdoorbell::{self, Doorman, PortHandle};
    use sv_transport::wlbytes::{ByteChannel, Endpoint, ReadOutcome};

    struct Connection {
        rx: Mutex<ByteChannel>,
        tx: Mutex<ByteChannel>,
        closed: AtomicBool,
        _control: Arc<Control>,
        _door: Doorman,
    }
    static CONNECTION: OnceLock<Connection> = OnceLock::new();

    fn connect() -> Result<Connection, String> {
        // Fail closed. Only the privileged broker opens raw devices.
        let device = wc_broker::client::lend::ivshmem("arena", "wayland-client")
            .ok_or("broker did not lend the Wayland arena")?;
        let (view, size, _) = wc_handle::map_ivshmem_device(device).map_err(|e| e.to_string())?;
        let owner = Arc::new(view);
        let len = usize::try_from(size).map_err(|_| "Wayland BAR exceeds address space")?;
        // SAFETY: the driver returned a read/write mapping of size bytes;
        // every control/ring reference retains its actual mapping/device owner.
        let region = Arc::new(unsafe {
            SharedRegion::from_raw_owned(owner.ptr().cast_mut(), len, owner.clone())
        });
        let ids = ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
        let deadline = Instant::now() + Duration::from_secs(10);
        let control = loop {
            match Control::attach(Arc::clone(&region), ids) {
                Ok(control) => break Arc::new(control),
                Err(e) if Instant::now() >= deadline => return Err(e.to_string()),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        let port = wc_broker::client::lend::port("org.sirati.wcbell.0", "wayland-client")
            .ok_or("broker did not lend the Wayland doorbell")?;
        let door = vdoorbell::attach(PortHandle::Pipe(Arc::new(port)), Arc::clone(&control))
            .map_err(|e| e.to_string())?;
        let tx = ByteChannel::claim(&control, u64::from(std::process::id()))
            .map_err(|e| e.to_string())?;
        let rx =
            ByteChannel::open(&control, tx.slot(), Endpoint::Owner).map_err(|e| e.to_string())?;
        if let Some(wake) = sv_transport::park::cross_wake() {
            wake.poke(sv_transport::park::RESCAN_TOKEN);
        }
        Ok(Connection {
            rx: Mutex::new(rx),
            tx: Mutex::new(tx),
            closed: AtomicBool::new(false),
            _control: control,
            _door: door,
        })
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn wc_bridge_open() -> i32 {
        if CONNECTION.get().is_some() {
            return -1;
        }
        match connect().and_then(|c| {
            CONNECTION
                .set(c)
                .map_err(|_| "connection already initialized".into())
        }) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("wc-cygwin-link: {e}");
                -1
            }
        }
    }

    /// # Safety
    /// `data` must point to a writable buffer of `len` bytes for this call.
    /// The caller must have called wc_bridge_open successfully.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn wc_bridge_read(data: *mut u8, len: usize) -> isize {
        if data.is_null() || len == 0 || len > isize::MAX as usize {
            return -1;
        }
        let Some(c) = CONNECTION.get() else { return -1 };
        // SAFETY: caller's buffer contract above, no retained borrow.
        let buf = unsafe { std::slice::from_raw_parts_mut(data, len) };
        let Ok(mut rx) = c.rx.lock() else { return -1 };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if c.closed.load(Ordering::Acquire) {
                return 0;
            }
            match rx.read(buf) {
                Ok(ReadOutcome::Bytes(n)) => {
                    if let Some(w) = sv_transport::park::cross_wake() {
                        w.poke(sv_transport::park::RESCAN_TOKEN);
                    }
                    return n as isize;
                }
                Ok(ReadOutcome::Eof) => return 0,
                Ok(ReadOutcome::Empty) if Instant::now() < deadline => {
                    if rx.wait_readable(Duration::from_secs(1)).is_err() {
                        return -1;
                    }
                }
                _ => return -1,
            }
        }
    }

    /// # Safety
    /// `data` must point to `len` initialized readable bytes for this call.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn wc_bridge_write(data: *const u8, len: usize) -> isize {
        if data.is_null() || len == 0 || len > isize::MAX as usize {
            return -1;
        }
        let Some(c) = CONNECTION.get() else { return -1 };
        // SAFETY: caller's buffer contract above, no retained borrow.
        let buf = unsafe { std::slice::from_raw_parts(data, len) };
        let Ok(mut tx) = c.tx.lock() else { return -1 };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if c.closed.load(Ordering::Acquire) {
                return -1;
            }
            match tx.write(buf) {
                Ok(0) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
                Ok(n) => return n as isize,
                Err(e) => {
                    eprintln!("wc-cygwin-link: write: {e}");
                    return -1;
                }
            }
        }
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn wc_bridge_close() {
        if let Some(c) = CONNECTION.get() {
            if let Ok(mut tx) = c.tx.lock() {
                let _ = tx.write_eof();
            }
            c.closed.store(true, Ordering::Release);
        }
        // The process-lifetime cell deliberately retains its mapping owner
        // while the doorbell thread can still reference any control word.
    }
}
