//! Native Windows ring attachment for a Cygwin registry client.
//!
//! The C ABI uses fixed-width integers and size_t, which agree between
//! Cygwin LP64 and native Windows LLP64. It never passes a Cygwin fd to
//! Winsock or treats an SCM_RIGHTS payload as a cross-VM handle.
//! Only one connection per process is supported. Rendered wl_shm clients
//! use a bounded BAR-backed POSIX companion library and in-band descriptors.

#[cfg(any(windows, test))]
struct PoolWindow {
    len: usize,
    used: usize,
}
#[cfg(any(windows, test))]
impl PoolWindow {
    fn reserve(&mut self, len: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }
        let rounded = len.checked_add(65535)? & !65535;
        let end = self.used.checked_add(rounded)?;
        if end > self.len {
            return None;
        }
        let offset = self.used;
        self.used = end;
        Some(offset)
    }
}

#[cfg(windows)]
mod windows {
    use super::PoolWindow;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use sv_transport::control::{Control, Geometry, ProtocolIds};
    use sv_transport::region::SharedRegion;
    use sv_transport::vdoorbell::{self, Doorman, PortHandle};
    use sv_transport::wlbytes::{ByteChannel, Endpoint, Kicker, ReadOutcome};

    struct Connection {
        rx: Mutex<ByteChannel>,
        tx: Mutex<ByteChannel>,
        reader_kick: Kicker,
        closed: AtomicBool,
        pool: Mutex<PoolWindow>,
        pool_base: usize,
        arena: Option<u32>,
        header: Option<(usize, wc_handle::ivshmem::Header)>,
        _control: Arc<Control>,
        _door: Doorman,
    }
    static CONNECTION: OnceLock<Connection> = OnceLock::new();

    fn read_header(base: usize, len: usize) -> Result<wc_handle::ivshmem::Header, String> {
        use wc_handle::ivshmem::{ENCODED_LEN, Header};
        if len < ENCODED_LEN {
            return Err("Wayland BAR has no header".into());
        }
        let mut bytes = [0u8; ENCODED_LEN];
        // SAFETY: the caller retains the driver mapping and checked its length.
        for (offset, byte) in bytes.iter_mut().enumerate().take(8) {
            *byte = unsafe { (base as *const u8).add(offset).read_volatile() };
        }
        std::sync::atomic::fence(Ordering::Acquire);
        for (offset, byte) in bytes.iter_mut().enumerate().skip(8) {
            *byte = unsafe { (base as *const u8).add(offset).read_volatile() };
        }
        Header::decode(&bytes).map_err(|e| e.to_string())
    }
    impl Connection {
        fn valid(&self) -> bool {
            self.header.is_none_or(|(base, expected)| {
                read_header(base, expected.region_len as usize)
                    .is_ok_and(|actual| actual == expected)
            })
        }
    }

    fn connect() -> Result<Connection, String> {
        // Fail closed. Only the privileged broker opens raw devices.
        let device = wc_broker::client::lend::ivshmem("arena", "wayland-client")
            .ok_or("broker did not lend the Wayland arena")?;
        let (view, size, _) = wc_handle::map_ivshmem_device(device).map_err(|e| e.to_string())?;
        let owner = Arc::new(view);
        let len = usize::try_from(size).map_err(|_| "Wayland BAR exceeds address space")?;
        let expected_run = std::env::var("WC_CYGWIN_RUN").ok();
        let arena_text = std::env::var("WC_CYGWIN_ARENA_ID").ok();
        let (control_offset, control_len, pixel_len, pool_base, arena, header) =
            if let Some(run) = expected_run {
                let run: u64 = run
                    .parse()
                    .map_err(|_| "invalid pinned Wayland run identity")?;
                let id: u32 = arena_text
                    .ok_or("missing pinned Wayland arena id")?
                    .parse()
                    .map_err(|_| "invalid pinned Wayland arena id")?;
                let header = read_header(owner.ptr() as usize, len)?;
                if run == 0
                    || header.run_id != run
                    || header.arena_id != id
                    || header.region_len != size
                    || header.rings.1 != Geometry::for_wayland().control_size
                {
                    return Err("Wayland BAR run/arena/geometry identity differs".into());
                }
                (
                    header.rings.0 as usize,
                    header.rings.1 as usize,
                    header.arena_len() as usize,
                    owner.ptr() as usize + wc_handle::ivshmem::HEADER_BYTES as usize,
                    Some(id),
                    Some((owner.ptr() as usize, header)),
                )
            } else {
                if arena_text.is_some() {
                    return Err("pool allocation requires pinned run identity".into());
                }
                (0, len, 0, 0, None, None)
            };
        if control_offset
            .checked_add(control_len)
            .is_none_or(|end| end > len)
        {
            return Err("Wayland control window exceeds BAR".into());
        }
        // SAFETY: a validated in-BAR window; the actual driver owner is retained.
        let region = Arc::new(unsafe {
            SharedRegion::from_raw_owned(
                owner.ptr().cast_mut().add(control_offset),
                control_len,
                owner.clone(),
            )
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
        if control.layout().geometry != Geometry::for_wayland() {
            return Err("Wayland control geometry does not match its pool window".into());
        }
        let port = wc_broker::client::lend::port("org.sirati.wcbell.0", "wayland-client")
            .ok_or("broker did not lend the Wayland doorbell")?;
        let door = vdoorbell::attach(PortHandle::Pipe(Arc::new(port)), Arc::clone(&control))
            .map_err(|e| e.to_string())?;
        let tx = ByteChannel::claim(&control, u64::from(std::process::id()))
            .map_err(|e| e.to_string())?;
        let rx =
            ByteChannel::open(&control, tx.slot(), Endpoint::Owner).map_err(|e| e.to_string())?;
        let reader_kick = rx.kicker(&control).map_err(|e| e.to_string())?;
        if let Some(wake) = sv_transport::park::cross_wake() {
            wake.poke(sv_transport::park::RESCAN_TOKEN);
        }
        Ok(Connection {
            rx: Mutex::new(rx),
            tx: Mutex::new(tx),
            reader_kick,
            closed: AtomicBool::new(false),
            pool: Mutex::new(PoolWindow {
                len: pixel_len,
                used: 0,
            }),
            pool_base,
            arena,
            header,
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
            let Ok(epoch) = rx.wake_epoch() else {
                return -1;
            };
            if c.closed.load(Ordering::Acquire) || !c.valid() {
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
                    if rx
                        .wait_readable_since(
                            epoch,
                            deadline.saturating_duration_since(Instant::now()),
                        )
                        .is_err()
                    {
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
            if c.closed.load(Ordering::Acquire) || !c.valid() {
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

    /// Allocate one nonrecycled extent of the preassigned BAR pixel window.
    /// The caller's file token never owns an ordinary file-backed pixel map.
    ///
    /// # Safety
    /// All three output pointers must reference writable live output objects.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn wc_bridge_pool_alloc(
        len: usize,
        address: *mut usize,
        offset: *mut u64,
        arena: *mut u32,
    ) -> i32 {
        if address.is_null() || offset.is_null() || arena.is_null() {
            return -1;
        }
        let Some(c) = CONNECTION.get() else { return -1 };
        let Some(id) = c.arena else { return -1 };
        if c.closed.load(Ordering::Acquire) || !c.valid() {
            return -1;
        }
        let Ok(mut pool) = c.pool.lock() else {
            return -1;
        };
        let Some(start) = pool.reserve(len) else {
            return -1;
        };
        let Some(pointer) = c.pool_base.checked_add(start) else {
            return -1;
        };
        // Anonymous-file mappings begin zeroed. Initialize directly in the
        // newly reserved BAR extent; no ordinary file pixels are copied.
        // SAFETY: reserve checked len against the actual retained device window.
        unsafe {
            std::ptr::write_bytes(pointer as *mut u8, 0, len);
        }
        // SAFETY: caller guarantees these writable outputs; no pointers retained.
        unsafe {
            address.write(pointer);
            offset.write(start as u64);
            arena.write(id);
        }
        0
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn wc_bridge_close() {
        if let Some(c) = CONNECTION.get() {
            if c.closed.swap(true, Ordering::AcqRel) {
                return;
            }
            c.reader_kick.kick();
            if !c.valid() {
                c._door.stop();
                return;
            }
            let Ok(mut tx) = c.tx.lock() else { return };
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match tx.write_eof() {
                    Ok(true) => break,
                    Ok(false) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    _ => {
                        eprintln!("wc-cygwin-link: close could not publish EOF");
                        return;
                    }
                }
            }
            // The epoch-aware local kick ends a blocked read without an idle
            // polling interval. It leaves before DEAD is published.
            let Ok(_rx) = c.rx.lock() else { return };
            c._door.stop();
            if c.valid() {
                let _ = c._control.kill_ring(tx.slot());
                if let Some(wake) = sv_transport::park::cross_wake() {
                    wake.poke(sv_transport::park::RESCAN_TOKEN);
                }
                eprintln!("WC_CYGWIN_BAR_CONNECTION_CLOSED ring_dead=1");
            }
        }
        // The process-lifetime cell deliberately retains its mapping owner
        // while the doorbell thread can still reference any control word.
    }
}

#[cfg(test)]
mod tests {
    use super::PoolWindow;

    #[test]
    fn pool_extents_are_disjoint_bounded_and_never_recycled() {
        let mut pool = PoolWindow {
            len: 131072,
            used: 0,
        };
        assert_eq!(pool.reserve(1), Some(0));
        assert_eq!(pool.reserve(65536), Some(65536));
        assert_eq!(pool.reserve(1), None);
        assert_eq!(pool.used, 131072);
    }

    #[test]
    fn pool_rejects_zero_and_overflow_without_consuming_space() {
        let mut pool = PoolWindow {
            len: 65536,
            used: 0,
        };
        assert_eq!(pool.reserve(0), None);
        assert_eq!(pool.reserve(usize::MAX), None);
        assert_eq!(pool.used, 0);
        assert_eq!(pool.reserve(65536), Some(0));
    }
}
