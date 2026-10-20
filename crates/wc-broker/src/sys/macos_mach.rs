//! Capability redemption for an existing Unix-broker lease. The kernel audit
//! trailer is the identity; region IDs and epochs are only lookup keys.
use super::unix::{Object, Unix};
use crate::{core::User, server::Broker};
use std::{ffi::CString, io, mem::size_of, ptr};
use sv_transport::mach::{self, Port, SendRight};

pub const SERVICE: &str = "org.waycompose.broker.memory";
use super::machwire::*;
unsafe extern "C" {
    static bootstrap_port: Port;
    fn bootstrap_check_in(port: Port, name: *const i8, service: &mut Port) -> i32;
    fn bootstrap_look_up(port: Port, name: *const i8, service: &mut Port) -> i32;
    fn mach_port_allocate(task: Port, kind: i32, name: &mut Port) -> i32;
    fn mach_port_destroy(task: Port, name: Port) -> i32;
    fn mach_msg(
        message: *mut Header,
        options: u32,
        send_size: u32,
        receive_size: u32,
        receive: Port,
        timeout: u32,
        notify: Port,
    ) -> i32;
    fn mach_msg_destroy(message: *mut Header);
}
struct ReceiveRight(Port);
impl Drop for ReceiveRight {
    fn drop(&mut self) {
        unsafe {
            mach_port_destroy(mach::task(), self.0);
        }
    }
}
#[repr(C, align(8))]
struct Buffer {
    bytes: [u8; 256],
    received: bool,
}
impl Drop for Buffer {
    fn drop(&mut self) {
        if self.received {
            unsafe {
                mach_msg_destroy(self.header());
            }
        }
    }
}
impl Buffer {
    fn header(&mut self) -> *mut Header {
        self.bytes.as_mut_ptr().cast()
    }
    fn audit(&self) -> io::Result<crate::core::Peer> {
        let h = unsafe { &*self.bytes.as_ptr().cast::<Header>() };
        super::machwire::audit(&self.bytes, h.size)
    }
}
fn send(packet: &mut Reply) -> io::Result<()> {
    let h = (packet as *mut Reply).cast::<Header>();
    // A fresh per-RPC reply port should have room immediately. A malicious
    // receiver cannot make a send sleep while the Core policy lock is held.
    let kr = unsafe { mach_msg(h, 1 | 0x10, (*h).size, 0, 0, 0, 0) };
    if kr != 0 {
        unsafe {
            mach_msg_destroy(h);
        }
    }
    mach::check(kr, "send Mach broker message")
}
fn receive(port: Port) -> io::Result<Buffer> {
    let mut buffer = Buffer {
        bytes: [0; 256],
        received: false,
    };
    let code = unsafe { mach_msg(buffer.header(), RECEIVE_OPTIONS, 0, 256, port, 2000, 0) };
    // Without MACH_RCV_LARGE the kernel discards an oversized message,
    // including its rights. It must not be allowed to disable the endpoint.
    if code == 0x10004004 {
        return Err(bad());
    }
    if code & !0x3e00 == 0x1000400c {
        // A body resource-shortage error can leave partially received rights.
        let size = unsafe { (*buffer.header()).size } as usize;
        if (size_of::<Header>()..=256).contains(&size) {
            buffer.received = true;
        }
        return Err(bad());
    }
    if code == 0x10004003 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Mach receive timed out",
        ));
    }
    mach::check(code, "receive Mach broker message")?;
    buffer.received = true;
    Ok(buffer)
}

/// Starts only against a root LaunchDaemon's configured MachServices entry.
/// No bootstrap registration fallback or separate policy state is created.
pub fn start(broker: Broker<Unix>) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Mach lender must run as root",
        ));
    }
    let owner = super::pci_owner::load()?;
    let name = CString::new(SERVICE).unwrap();
    let mut port = 0;
    mach::check(
        unsafe { bootstrap_check_in(bootstrap_port, name.as_ptr(), &mut port) },
        "bootstrap_check_in",
    )?;
    let right = ReceiveRight(port);
    let workers = super::machworkers::Workers::new(move |mut buffer: Buffer| {
        let _ = serve_one(&broker, &owner, &mut buffer);
    })?;
    std::thread::Builder::new()
        .name("wc-mach-lender".into())
        .spawn(move || {
            let _right = right;
            loop {
                let buffer = match receive(port) {
                    Ok(buffer) => buffer,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::InvalidData
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => {
                        eprintln!("[wc-broker] Mach lender stopped: {error}");
                        break;
                    }
                };
                // Classify only structurally valid requests. Each worker still
                // authenticates the kernel audit identity and checks Core.
                let request = unsafe { ptr::read(buffer.bytes.as_ptr().cast::<Request>()) };
                if request.validate().is_ok() {
                    let _ = workers.submit(request.op, buffer);
                }
            }
        })?;
    Ok(())
}
fn serve_one(
    broker: &Broker<Unix>,
    owner: &crate::raw_device::VmPciOwner,
    buffer: &mut Buffer,
) -> io::Result<()> {
    let peer = buffer.audit()?;
    owner
        .authorize(&peer.user)
        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
    let req = unsafe { ptr::read(buffer.bytes.as_ptr().cast::<Request>()) };
    req.validate()?;
    let mut reply = Reply {
        h: Header {
            bits: 18 | COMPLEX,
            size: size_of::<Reply>() as u32,
            remote: req.h.remote,
            id: REPLY,
            ..Header::default()
        },
        count: 1,
        right: Descriptor {
            disposition: 17,
            ..Descriptor::default()
        },
        nonce: req.nonce,
        epoch: req.epoch,
        status: 1,
        ..Reply::default()
    };
    // Move the reply send-once right out of the received message.
    unsafe {
        (*buffer.header()).remote = 0;
    }
    // A wait keeps the driver mapping alive but never holds the policy lock
    // while sleeping. Recheck that same lease before transferring its reply.
    let sequence = if req.op == 2 {
        let mapping = broker.with_region_lease(
            &peer,
            req.region,
            req.epoch,
            req.index as usize,
            |obj, _, _| match obj {
                Object::MachRegion(mapping) => Ok(std::sync::Arc::clone(mapping)),
                _ => Err(bad()),
            },
        );
        match mapping.and_then(|mapping| mapping.wait_irq(req.target)) {
            Ok(sequence) => Some(sequence),
            Err(error) => {
                send(&mut reply)?;
                return Err(error);
            }
        }
    } else {
        None
    };
    let transfer = broker.with_region_lease(
        &peer,
        req.region,
        req.epoch,
        req.index as usize,
        |obj, offset, len| {
            let Object::MachRegion(mapping) = obj else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "region is not PCI memory",
                ));
            };
            if mapping.len() != len {
                return Err(bad());
            }
            reply.offset = offset;
            reply.len = len;
            if req.op == 0 {
                if req.target != 0 {
                    return Err(bad());
                }
                let right = mapping.memory_entry()?;
                reply.right = Descriptor {
                    name: right.into_raw(),
                    disposition: 17,
                    ..Descriptor::default()
                };
            } else if req.op == 1 {
                mapping.ring((req.target >> 16) as u16, req.target as u16)?;
            }
            if let Some(sequence) = sequence {
                reply.reserved = sequence;
            }
            reply.status = 0;
            send(&mut reply)
        },
    );
    if transfer.is_err() && reply.status != 0 {
        send(&mut reply)?;
    }
    transfer
}

/// A mapping request must be made while its ordinary Unix lease is retained.
/// The reply's root kernel identity prevents user bootstrap namespace spoofing.
pub fn redeem(
    region: u64,
    epoch: u64,
    index: u32,
    offset: u64,
    len: u64,
) -> io::Result<mach::Mapping> {
    let right = rpc(region, epoch, index, 0, 0, offset, len)?
        .0
        .ok_or_else(bad)?;
    mach::Mapping::new(right, len)
}
pub fn ring(
    region: u64,
    epoch: u64,
    index: u32,
    peer: u16,
    vector: u16,
    offset: u64,
    len: u64,
) -> io::Result<()> {
    rpc(
        region,
        epoch,
        index,
        1,
        (u32::from(peer) << 16) | u32::from(vector),
        offset,
        len,
    )
    .map(|_| ())
}
pub fn wait_irq(
    region: u64,
    epoch: u64,
    index: u32,
    seen: u32,
    offset: u64,
    len: u64,
) -> io::Result<u32> {
    rpc(region, epoch, index, 2, seen, offset, len).map(|reply| reply.1)
}
fn rpc(
    region: u64,
    epoch: u64,
    index: u32,
    op: u32,
    target: u32,
    offset: u64,
    len: u64,
) -> io::Result<(Option<SendRight>, u32)> {
    let name = CString::new(SERVICE).unwrap();
    let mut service = 0;
    mach::check(
        unsafe { bootstrap_look_up(bootstrap_port, name.as_ptr(), &mut service) },
        "bootstrap_look_up",
    )?;
    let service = unsafe { SendRight::from_raw(service) };
    let mut receive_port = 0;
    mach::check(
        unsafe { mach_port_allocate(mach::task(), 1, &mut receive_port) },
        "allocate reply port",
    )?;
    let receive_right = ReceiveRight(receive_port);
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut request = Request {
        h: Header {
            bits: 19 | (21 << 8),
            size: size_of::<Request>() as u32,
            remote: service.as_raw(),
            local: receive_right.0,
            id: REQUEST,
            ..Header::default()
        },
        nonce,
        region,
        epoch,
        index,
        op,
        target,
        reserved: 0,
    };
    // COPY_SEND leaves the service reference owned by `service`.
    mach::check(
        unsafe {
            mach_msg(
                &mut request.h,
                1 | 0x10,
                size_of::<Request>() as u32,
                0,
                0,
                2000,
                0,
            )
        },
        "send Mach broker request",
    )?;
    let mut buffer = receive(receive_right.0)?;
    let peer = buffer.audit()?;
    let reply = unsafe { ptr::read(buffer.bytes.as_ptr().cast::<Reply>()) };
    if peer.user != User::uid(0) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Mach reply did not come from root",
        ));
    }
    reply.validate(op, nonce, epoch, offset, len)?;
    if op != 0 {
        return Ok((None, reply.reserved));
    }
    let port = reply.right.name;
    unsafe {
        (*buffer.bytes.as_mut_ptr().cast::<Reply>()).right.name = 0;
    }
    Ok((Some(unsafe { SendRight::from_raw(port) }), 0))
}
