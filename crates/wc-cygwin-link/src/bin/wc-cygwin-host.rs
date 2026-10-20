//! A reverse Wayland byte bridge with an optional authenticated BAR arena.
//! Never use read() to silently discard compositor SCM_RIGHTS: fd-bearing
//! events are refused. BAR pool descriptors use the existing in-band dialect.
#[cfg(target_os = "linux")]
mod host {
    use std::io::{self, Write};
    use std::net::Shutdown;
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
            let streams = wc_handle::bar_stream::controls().ring_token(token, count);
            self.poke();
            rung || streams
        }
        fn rescan(&self, count: &AtomicU64) {
            self.control.rescan(count);
            wc_handle::bar_stream::controls().rescan(count);
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

    const QUEUE_BYTES: usize = 65536;

    struct Relay {
        channel: ByteChannel,
        socket: UnixStream,
        to_socket: Vec<u8>,
        to_ring: Vec<u8>,
        ring_eof: bool,
        socket_eof: bool,
        write_shutdown: bool,
        sent_eof: bool,
        requests: usize,
        replies: usize,
    }
    impl Relay {
        fn new(channel: ByteChannel, socket: UnixStream) -> io::Result<Self> {
            socket.set_nonblocking(true)?;
            Ok(Self {
                channel,
                socket,
                to_socket: Vec::new(),
                to_ring: Vec::new(),
                ring_eof: false,
                socket_eof: false,
                write_shutdown: false,
                sent_eof: false,
                requests: 0,
                replies: 0,
            })
        }
        fn service(
            &mut self,
            control: &Control,
            require_dead: bool,
        ) -> Result<bool, Box<dyn std::error::Error>> {
            let mut buf = [0u8; QUEUE_BYTES];
            // Stop reading at the queue bound. Ring credit backpressures this
            // client while the other connections continue independently.
            while !self.ring_eof && self.to_socket.len() < QUEUE_BYTES {
                let remaining = QUEUE_BYTES - self.to_socket.len();
                match self.channel.read(&mut buf[..remaining])? {
                    ReadOutcome::Bytes(n) => {
                        self.to_socket.extend_from_slice(&buf[..n]);
                        self.requests += n;
                    }
                    ReadOutcome::Empty => break,
                    ReadOutcome::Eof => {
                        self.ring_eof = true;
                        break;
                    }
                }
            }
            while !self.to_socket.is_empty() {
                match self.socket.write(&self.to_socket) {
                    Ok(0) => return Err("Linux compositor closed on write".into()),
                    Ok(n) => {
                        self.to_socket.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            if self.ring_eof && self.to_socket.is_empty() && !self.write_shutdown {
                self.socket.shutdown(Shutdown::Write)?;
                self.write_shutdown = true;
            }
            while !self.to_ring.is_empty() {
                let n = self.channel.write_available(&self.to_ring)?;
                if n == 0 {
                    break;
                }
                self.to_ring.drain(..n);
            }
            if self.to_ring.is_empty() && !self.socket_eof {
                match receive(&self.socket, &mut buf) {
                    Ok(0) => self.socket_eof = true,
                    Ok(n) => {
                        self.to_ring.extend_from_slice(&buf[..n]);
                        self.replies += n;
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            if self.socket_eof && self.to_ring.is_empty() && !self.sent_eof {
                self.sent_eof = self.channel.write_eof()?;
            }
            Ok(self.ring_eof
                && self.to_socket.is_empty()
                && self.sent_eof
                && (!require_dead
                    || control.ring_state(self.channel.slot())?
                        == sv_transport::control::RingState::Dead))
        }
        fn needs_credit(&self) -> bool {
            !self.to_ring.is_empty() || (self.socket_eof && !self.sent_eof)
        }
        fn ready(&self) -> Result<bool, Box<dyn std::error::Error>> {
            Ok((!self.ring_eof
                    && self.to_socket.len() < QUEUE_BYTES
                    && self.channel.readable()?)
                    || (!self.to_ring.is_empty() && self.channel.write_capacity()? > 0)
                    // An EOF needs only the aligned header. Payload capacity
                    // can still be zero after enough credit for EOF returns.
                    || (self.socket_eof && !self.sent_eof && self.to_ring.is_empty()
                        && self.channel.free_bytes()? >= sv_transport::ring::PACKET_HEADER_SIZE))
        }
        fn pollfd(&self) -> libc::pollfd {
            let events = if !self.socket_eof && self.to_ring.is_empty() {
                libc::POLLIN
            } else {
                0
            } | if !self.to_socket.is_empty() {
                libc::POLLOUT
            } else {
                0
            };
            libc::pollfd {
                fd: if events == 0 {
                    -1
                } else {
                    self.socket.as_raw_fd()
                },
                events,
                revents: 0,
            }
        }
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let mut args: Vec<_> = std::env::args().collect();
        let geometry = Geometry::for_wayland();
        let expected_clients = if let Some(index) = args.iter().position(|arg| arg == "--clients") {
            let count: usize = args
                .get(index + 1)
                .ok_or("--clients needs a count")?
                .parse()?;
            if count == 0 || count > geometry.ring_slot_count as usize {
                return Err("--clients is outside the available ring count".into());
            }
            args.drain(index..index + 2);
            count
        } else {
            1
        };
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
            return Err("usage: wc-cygwin-host [--clients COUNT] EXISTING_BAR_FILE WAYLAND_SOCKET WCBELL_SOCKET | [--clients COUNT] --qemu-bar QMP DEVICE SLOT ARENA RUN WAYLAND_SOCKET WCBELL_SOCKET".into());
        };
        let region = Arc::new(SharedRegion::map_fd(
            file.as_fd(),
            control_offset,
            geometry.control_size as usize,
        )?);
        let control = Arc::new(Control::create(
            region,
            geometry,
            ProtocolIds::new(
                if header.is_some() {
                    wc_handle::bar_lease::CONTROL_WIRE_VERSION
                } else {
                    1
                },
                sv_proto::PROTOCOL_HASH,
            ),
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
                wc_handle::ivshmem::write_leases_into(lease.0.base_ptr(), value);
            }
        }
        println!(
            "WC_CYGWIN_HOST_READY control_bytes={} descriptor_mode=refuse",
            geometry.control_size
        );
        io::stdout().flush()?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut relays: Vec<Relay> = Vec::new();
        let mut completed_slots = std::collections::BTreeSet::new();
        let mut all_dead = true;
        let mut accepted = 0usize;
        let mut completed = 0usize;
        let mut requests = 0usize;
        let mut replies = 0usize;
        loop {
            if Instant::now() >= deadline {
                return Err("bounded Wayland bridge proof timed out".into());
            }
            for slot in ByteChannel::live_connections(&control)? {
                if completed_slots.contains(&slot)
                    || relays.iter().any(|relay| relay.channel.slot() == slot)
                {
                    continue;
                }
                if accepted == expected_clients {
                    return Err("more Wayland clients than the configured proof bound".into());
                }
                let channel = ByteChannel::open(&control, slot, Endpoint::Peer)?;
                relays.push(Relay::new(channel, UnixStream::connect(socket_path)?)?);
                accepted += 1;
            }
            let mut index = 0;
            while index < relays.len() {
                if relays[index].service(&control, header.is_some())? {
                    let relay = relays.remove(index);
                    completed_slots.insert(relay.channel.slot());
                    all_dead &= control.ring_state(relay.channel.slot())?
                        == sv_transport::control::RingState::Dead;
                    requests += relay.requests;
                    replies += relay.replies;
                    completed += 1;
                } else {
                    index += 1;
                }
            }
            if completed == expected_clients {
                println!(
                    "WC_CYGWIN_HOST_DONE request_bytes={requests} reply_bytes={replies} fds_crossed=0 ring_dead={} clients={completed}",
                    u8::from(all_dead)
                );
                return Ok(());
            }
            // Park every lane before the final readiness check. Both incoming
            // bytes and released TX credit wake the shared eventfd. Never let
            // one blocked socket require periodic polling of another client.
            let mut parked = Vec::new();
            for relay in &relays {
                if !relay.ring_eof && relay.to_socket.len() < QUEUE_BYTES {
                    parked.push(relay.channel.rx_bell()?);
                }
                if relay.needs_credit() {
                    parked.push(relay.channel.tx_credit_bell()?);
                }
            }
            for bell in &parked {
                bell.parked
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            let ready = relays.iter().try_fold(false, |ready, relay| {
                relay.ready().map(|next| ready || next)
            });
            if matches!(ready, Ok(true)) {
                for bell in &parked {
                    bell.parked
                        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
                continue;
            }
            if let Err(error) = ready {
                for bell in &parked {
                    bell.parked
                        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
                return Err(error);
            }
            let mut polls = Vec::with_capacity(relays.len() + 1);
            polls.push(libc::pollfd {
                fd: wake.event.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            polls.extend(relays.iter().map(Relay::pollfd));
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32;
            // SAFETY: retained pollfd vector, no concurrent mutation.
            let status = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, timeout) };
            for bell in &parked {
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

        fn relay_pair(control: &Control, id: u64) -> (Relay, ByteChannel, UnixStream) {
            let owner = ByteChannel::claim(control, id).unwrap();
            let peer = ByteChannel::open(control, owner.slot(), Endpoint::Peer).unwrap();
            let (bridge, server) = UnixStream::pair().unwrap();
            server.set_nonblocking(true).unwrap();
            (Relay::new(peer, bridge).unwrap(), owner, server)
        }

        fn test_control() -> Control {
            let geometry = Geometry::for_wayland();
            let region = Arc::new(SharedRegion::heap(geometry.control_size as usize).unwrap());
            Control::create(region, geometry, ProtocolIds::new(1, 7), 1).unwrap()
        }

        #[test]
        fn concurrent_clients_keep_bytes_and_orderly_teardown_separate() {
            use std::io::Read;
            let control = test_control();
            let (mut first, mut first_owner, mut first_server) = relay_pair(&control, 11);
            let (mut second, mut second_owner, mut second_server) = relay_pair(&control, 12);
            first_owner.write(b"first request").unwrap();
            second_owner.write(b"second request").unwrap();
            assert!(!first.service(&control, true).unwrap());
            assert!(!second.service(&control, true).unwrap());
            let mut bytes = [0u8; 64];
            let n = first_server.read(&mut bytes).unwrap();
            assert_eq!(&bytes[..n], b"first request");
            let n = second_server.read(&mut bytes).unwrap();
            assert_eq!(&bytes[..n], b"second request");
            first_server.write_all(b"first reply").unwrap();
            second_server.write_all(b"second reply").unwrap();
            for relay in [&mut first, &mut second] {
                relay.service(&control, true).unwrap();
                relay.service(&control, true).unwrap();
            }
            let ReadOutcome::Bytes(n) = first_owner.read(&mut bytes).unwrap() else {
                panic!("missing first reply")
            };
            assert_eq!(&bytes[..n], b"first reply");
            let ReadOutcome::Bytes(n) = second_owner.read(&mut bytes).unwrap() else {
                panic!("missing second reply")
            };
            assert_eq!(&bytes[..n], b"second reply");
            first_owner.write_eof().unwrap();
            first.service(&control, true).unwrap();
            assert_eq!(
                first_server.read(&mut bytes).unwrap(),
                0,
                "EOF did not half-close Linux write side"
            );
            first_server.shutdown(Shutdown::Write).unwrap();
            assert!(
                !first.service(&control, true).unwrap(),
                "EOF was mistaken for a DEAD acknowledgement"
            );
            control.kill_ring(first_owner.slot()).unwrap();
            assert!(first.service(&control, true).unwrap());
            second_owner.write(b"still connected").unwrap();
            assert!(!second.service(&control, true).unwrap());
            let n = second_server.read(&mut bytes).unwrap();
            assert_eq!(&bytes[..n], b"still connected");
        }

        #[test]
        fn a_full_reply_lane_waits_for_credit_without_polling_the_socket() {
            let control = test_control();
            let (mut relay, mut owner, mut server) = relay_pair(&control, 13);
            let fill = [9u8; 4096];
            while relay.channel.write(&fill).unwrap() > 0 {}
            server.write_all(b"reply after pressure").unwrap();
            relay.service(&control, true).unwrap();
            assert!(relay.needs_credit());
            assert_eq!(
                relay.pollfd().fd,
                -1,
                "blocked queue left a readable socket in poll"
            );
            assert!(!relay.ready().unwrap());
            let mut bytes = [0u8; 65536];
            while matches!(owner.read(&mut bytes).unwrap(), ReadOutcome::Bytes(_)) {}
            assert!(
                relay.ready().unwrap(),
                "released credit did not make the relay runnable"
            );
            relay.service(&control, true).unwrap();
            let ReadOutcome::Bytes(n) = owner.read(&mut bytes).unwrap() else {
                panic!("missing queued reply")
            };
            assert_eq!(&bytes[..n], b"reply after pressure");
        }

        #[test]
        fn eof_readiness_accepts_header_only_credit() {
            let control = test_control();
            let (mut relay, _owner, _server) = relay_pair(&control, 14);
            relay.channel.write(&[1u8; 17]).unwrap();
            while relay.channel.free_bytes().unwrap() > sv_transport::ring::PACKET_HEADER_SIZE {
                assert_eq!(relay.channel.write(&[2u8; 1]).unwrap(), 1);
            }
            assert_eq!(
                relay.channel.free_bytes().unwrap(),
                sv_transport::ring::PACKET_HEADER_SIZE
            );
            assert_eq!(relay.channel.write_capacity().unwrap(), 0);
            relay.socket_eof = true;
            assert!(
                relay.ready().unwrap(),
                "EOF credit was mistaken for no writable payload"
            );
            relay.service(&control, true).unwrap();
            assert!(relay.sent_eof);
        }

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
