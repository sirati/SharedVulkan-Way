//! A reverse Wayland byte bridge with an optional authenticated BAR arena.
//! Never use read() to silently discard compositor SCM_RIGHTS: fd-bearing
//! events are refused. BAR pool descriptors use the existing in-band dialect.
#[cfg(target_os = "linux")]
mod host {
    use std::io::{self, Write};
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, Instant};
    use sv_transport::control::{Control, Geometry, ProtocolIds};
    use sv_transport::region::SharedRegion;
    use sv_transport::vdoorbell::{self, BellTarget};
    use sv_transport::wlbytes::{ByteChannel, Endpoint, ReadOutcome};

    struct HeaderLease(Arc<SharedRegion>);
    impl Drop for HeaderLease {
        fn drop(&mut self) {
            // SAFETY: this host owns publication for the whole retained page.
            let _ = unsafe { wc_handle::ivshmem::invalidate(self.0.base_ptr()) };
        }
    }

    struct Wake {
        control: Arc<Control>,
        event: OwnedFd,
    }
    impl Wake {
        fn poke(&self) {
            let one = 1u64;
            // SAFETY: owned nonblocking eventfd, live eight-byte value.
            unsafe {
                libc::write(self.event.as_raw_fd(), (&one as *const u64).cast(), 8);
            }
        }
        fn drain(&self) {
            let mut value = 0u64;
            // SAFETY: owned eventfd and live eight-byte output.
            unsafe {
                libc::read(self.event.as_raw_fd(), (&mut value as *mut u64).cast(), 8);
            }
        }
    }
    impl BellTarget for Wake {
        fn ring_token(&self, token: u8, count: &AtomicU64) -> bool {
            let rung = self.control.ring_token(token, count);
            self.poke();
            rung
        }
        fn rescan(&self, count: &AtomicU64) {
            self.control.rescan(count);
            self.poke();
        }
    }

    fn receive(socket: &UnixStream, buf: &mut [u8]) -> io::Result<usize> {
        // Align ancillary storage for cmsghdr, and close every delivered fd
        // before refusing the message. No host descriptor crosses the VM.
        let mut ancillary = [0u64; 64];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        // SAFETY: zero is valid for msghdr before filling its buffer pointers.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = ancillary.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&ancillary);
        // SAFETY: all pointers refer to writable live buffers for this call.
        let n = unsafe {
            libc::recvmsg(
                socket.as_raw_fd(),
                &mut msg,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut has_fds = msg.msg_flags & libc::MSG_CTRUNC != 0;
        // SAFETY: recvmsg initialized the ancillary prefix described by msg.
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(&msg);
            while !header.is_null() {
                if (*header).cmsg_level == libc::SOL_SOCKET
                    && (*header).cmsg_type == libc::SCM_RIGHTS
                {
                    has_fds = true;
                    let length = (*header)
                        .cmsg_len
                        .saturating_sub(libc::CMSG_LEN(0) as usize);
                    for index in 0..length / std::mem::size_of::<i32>() {
                        let fd = std::ptr::read_unaligned(
                            libc::CMSG_DATA(header).cast::<i32>().add(index),
                        );
                        libc::close(fd);
                    }
                }
                header = libc::CMSG_NXTHDR(&msg, header);
            }
        }
        if has_fds {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "descriptor event needs a BAR/fd adapter; registry-only bridge refused it",
            ));
        }
        Ok(n as usize)
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args: Vec<_> = std::env::args().collect();
        let geometry = Geometry::for_wayland();
        let (file, socket_path, bell_path, control_offset, header, _bar_guard) = if args.len() == 9
            && args[1] == "--qemu-bar"
        {
            let slot: u8 = args[4].parse()?;
            let arena_id: u32 = args[5].parse()?;
            let run_id: u64 = args[6].parse()?;
            if run_id == 0 {
                return Err("zero BAR run identity".into());
            }
            let bar = sv_hostd::qemu_bar::QemuBar::open(
                std::path::Path::new(&args[2]),
                &args[3],
                slot,
                true,
            )?;
            let rings_offset = bar
                .len
                .checked_sub(geometry.control_size)
                .ok_or("BAR is smaller than rings")?;
            if rings_offset <= wc_handle::ivshmem::HEADER_BYTES
                || !rings_offset.is_multiple_of(4096)
            {
                return Err("BAR has no aligned pixel window".into());
            }
            let file = bar.file.try_clone()?;
            // One bounded session per backend lifetime. Even an invalidated
            // prior header forbids reuse while a Windows process could retain
            // its BAR mapping; create a fresh VM backend after guest teardown.
            use std::os::unix::fs::FileExt;
            let mut previous = [0u8; wc_handle::ivshmem::ENCODED_LEN];
            file.read_exact_at(&mut previous, 0)?;
            if previous.iter().any(|byte| *byte != 0) {
                return Err(
                    "Wayland BAR backend was already initialized; refusing session/extent reuse"
                        .into(),
                );
            }
            let page = Arc::new(SharedRegion::map_fd(
                file.as_fd(),
                0,
                wc_handle::ivshmem::HEADER_BYTES as usize,
            )?);
            // SAFETY: retained writable header page, sole fixture publisher.
            let generation = unsafe { wc_handle::ivshmem::invalidate(page.base_ptr()) }
                .unwrap_or(0)
                .checked_add(1)
                .ok_or("BAR generation overflow")?;
            let arena_len = rings_offset - wc_handle::ivshmem::HEADER_BYTES;
            let host_start = arena_len
                .checked_sub(4 * 1024 * 1024)
                .ok_or("BAR has no outbound partition")?
                & !65535;
            if host_start <= wc_handle::bar_lease::DIRECTORY_BYTES as u64 {
                return Err("BAR has no leased pixel capacity".into());
            }
            let allocation_view = Arc::new(SharedRegion::map_fd(
                file.as_fd(),
                wc_handle::ivshmem::HEADER_BYTES,
                arena_len as usize,
            )?);
            // SAFETY: sole fresh backend initializer, before publishing Header.
            unsafe {
                wc_handle::bar_lease::Directory::initialize(
                    allocation_view.base_ptr(),
                    allocation_view.base_ptr(),
                    allocation_view.base_ptr().add(host_start as usize),
                    wc_handle::bar_lease::Role::Host,
                    arena_len as usize,
                    host_start as usize,
                    run_id,
                    allocation_view.clone(),
                )
            }?;
            let header = wc_handle::ivshmem::Header {
                run_id,
                generation,
                region_len: bar.len,
                arena_id,
                outbound: (host_start, arena_len - host_start),
                rings: (rings_offset, geometry.control_size),
            };
            println!(
                "WC_CYGWIN_BAR pid={} device={} backend={} bytes={} run={} arena={}",
                bar.pid, bar.device, bar.backend, bar.len, run_id, arena_id
            );
            (
                file,
                &args[7],
                &args[8],
                rings_offset,
                Some((HeaderLease(page), header)),
                Some(bar),
            )
        } else if args.len() == 4 {
            // Existing descriptor-free registry fixture. Never create or resize.
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&args[1])?;
            if file.metadata()?.len() < geometry.control_size {
                return Err("Wayland BAR is smaller than control geometry".into());
            }
            (file, &args[2], &args[3], 0, None, None)
        } else {
            return Err("usage: wc-cygwin-host EXISTING_BAR_FILE WAYLAND_SOCKET WCBELL_SOCKET | --qemu-bar QMP DEVICE SLOT ARENA RUN WAYLAND_SOCKET WCBELL_SOCKET".into());
        };
        let region = Arc::new(SharedRegion::map_fd(
            file.as_fd(),
            control_offset,
            geometry.control_size as usize,
        )?);
        let control = Arc::new(Control::create(
            region,
            geometry,
            ProtocolIds::new(1, sv_proto::PROTOCOL_HASH),
            1,
        )?);
        // SAFETY: eventfd returns a fresh owned fd, checked before ownership.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let event = unsafe { OwnedFd::from_raw_fd(raw) };
        let wake = Arc::new(Wake {
            control: Arc::clone(&control),
            event,
        });
        let (_door, connected) = vdoorbell::attach_host_reconnecting(bell_path, Arc::clone(&wake))?;
        if !connected {
            return Err("Wayland doorbell is not connected".into());
        }
        if let Some((lease, value)) = &header {
            // SAFETY: initialization and doorbell attachment completed before publication.
            unsafe {
                wc_handle::ivshmem::write_into(lease.0.base_ptr(), value);
            }
        }
        println!(
            "WC_CYGWIN_HOST_READY control_bytes={} descriptor_mode=refuse",
            geometry.control_size
        );
        io::stdout().flush()?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut channel = None;
        let mut socket = None;
        let mut to_socket = Vec::new();
        let mut to_ring = Vec::new();
        let mut eof = false;
        let mut buf = [0u8; 65536];
        let mut requests = 0usize;
        let mut replies = 0usize;
        loop {
            if Instant::now() >= deadline {
                return Err("bounded registry proof timed out".into());
            }
            if channel.is_none() {
                let slots = ByteChannel::live_connections(&control)?;
                if slots.len() > 1 {
                    return Err("registry fixture supports exactly one connection".into());
                }
                if let Some(slot) = slots.first() {
                    channel = Some(ByteChannel::open(&control, *slot, Endpoint::Peer)?);
                    let s = UnixStream::connect(socket_path)?;
                    s.set_nonblocking(true)?;
                    socket = Some(s);
                }
            }
            if let (Some(ch), Some(s)) = (channel.as_mut(), socket.as_mut()) {
                loop {
                    match ch.read(&mut buf)? {
                        ReadOutcome::Bytes(n) => {
                            if to_socket.len() + n > 65536 {
                                return Err("bounded bridge request queue overflow".into());
                            }
                            to_socket.extend_from_slice(&buf[..n]);
                            requests += n;
                        }
                        ReadOutcome::Empty => break,
                        ReadOutcome::Eof => {
                            eof = true;
                            break;
                        }
                    }
                }
                while !to_socket.is_empty() {
                    match s.write(&to_socket) {
                        Ok(0) => return Err("Linux compositor closed on write".into()),
                        Ok(n) => {
                            to_socket.drain(..n);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e.into()),
                    }
                }
                while !to_ring.is_empty() {
                    let n = ch.write(&to_ring)?;
                    if n == 0 {
                        break;
                    }
                    to_ring.drain(..n);
                }
                if to_ring.is_empty() && !eof {
                    match receive(s, &mut buf) {
                        Ok(0) => {
                            eof = true;
                        }
                        Ok(n) => {
                            to_ring.extend_from_slice(&buf[..n]);
                            replies += n;
                            continue;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                if eof && to_socket.is_empty() && to_ring.is_empty() {
                    let _ = ch.write_eof();
                    if header.is_none()
                        || control.ring_state(ch.slot())? == sv_transport::control::RingState::Dead
                    {
                        println!(
                            "WC_CYGWIN_HOST_DONE request_bytes={requests} reply_bytes={replies} fds_crossed=0 ring_dead={}",
                            u8::from(
                                control.ring_state(ch.slot())?
                                    == sv_transport::control::RingState::Dead
                            )
                        );
                        return Ok(());
                    }
                }
                if to_ring.len() > 65536 || to_socket.len() > 65536 {
                    return Err("bounded bridge queue overflow".into());
                }
            }
            // Park the command lane before checking again. The port reader
            // signals eventfd, so poll never needs a periodic idle slice.
            let parked = channel.as_ref().and_then(|c| c.rx_bell().ok());
            if let Some(bell) = &parked {
                bell.parked
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            if !eof
                && channel
                    .as_ref()
                    .is_some_and(|c| c.readable().unwrap_or(true))
            {
                if let Some(bell) = &parked {
                    bell.parked
                        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
                continue;
            }
            let mut polls = [
                libc::pollfd {
                    fd: wake.event.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if eof {
                        -1
                    } else {
                        socket.as_ref().map_or(-1, AsRawFd::as_raw_fd)
                    },
                    events: libc::POLLIN
                        | if to_socket.is_empty() {
                            0
                        } else {
                            libc::POLLOUT
                        },
                    revents: 0,
                },
            ];
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32;
            // SAFETY: a live array of two pollfd entries.
            let status = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, timeout) };
            if let Some(bell) = &parked {
                bell.parked
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
            wake.drain();
            if status < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error().into());
            }
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ordinary_wayland_bytes_survive_the_socket_boundary() {
            let (mut writer, reader) = UnixStream::pair().unwrap();
            writer.write_all(b"wayland").unwrap();
            let mut data = [0u8; 32];
            assert_eq!(receive(&reader, &mut data).unwrap(), 7);
            assert_eq!(&data[..7], b"wayland");
        }

        #[test]
        fn a_real_ancillary_descriptor_is_refused() {
            let (writer, reader) = UnixStream::pair().unwrap();
            let file = std::fs::File::open("/dev/null").unwrap();
            let byte = [1u8];
            let mut iov = libc::iovec {
                iov_base: byte.as_ptr().cast_mut().cast(),
                iov_len: 1,
            };
            let mut storage = [0u64; 8];
            // SAFETY: zeroed msghdr, then live aligned control/data buffers.
            let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
            message.msg_iov = &mut iov;
            message.msg_iovlen = 1;
            message.msg_control = storage.as_mut_ptr().cast();
            // SAFETY: enough aligned storage for one SCM_RIGHTS descriptor.
            unsafe {
                message.msg_controllen = libc::CMSG_SPACE(4) as usize;
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(4) as usize;
                std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<i32>(), file.as_raw_fd());
                assert_eq!(libc::sendmsg(writer.as_raw_fd(), &message, 0), 1);
            }
            let error = receive(&reader, &mut [0u8; 8]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    if let Err(error) = host::run() {
        eprintln!("wc-cygwin-host: {error}");
        std::process::exit(1);
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("wc-cygwin-host runs on Linux");
        std::process::exit(1);
    }
}
