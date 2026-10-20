//! Cooperative clipboard streams over the existing BAR byte-ring protocol.
//!
//! Endpoint and transfer references are distinct from allocation references.
//! No timeout, EOF, EPIPE or client-writable word authorizes privileged broker
//! retirement. A crashed endpoint therefore remains retained in this mode.
use crate::bar_lease::{self, Consumer, Directory, Key, Owner, Role};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use sv_transport::control::{Control, Geometry, ProtocolIds};
use sv_transport::region::SharedRegion;
use sv_transport::ring::{Lane, RingCtrl};
use sv_transport::vdoorbell::ControlSet;
use sv_transport::wlbytes::{ByteChannel, Endpoint, ReadOutcome};

const HEADER: usize = 4096;
const CONTROL: usize = 80 * 1024;
const LENGTH: usize = HEADER + CONTROL;
const MAGIC: u64 = 0x314d5254534357;
const READER: usize = 1;
const WRITER: usize = 2;
const READER_PENDING: usize = 3;
const WRITER_PENDING: usize = 4;
const READER_CLOSED: usize = 5;
const WRITER_CLOSED: usize = 6;
const READ_GATE: usize = 7;
const WRITE_GATE: usize = 8;
const READ_CURSOR: usize = 9;
fn error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn transport(error: sv_transport::Error) -> io::Error {
    io::Error::other(error.to_string())
}
fn geometry() -> Geometry {
    Geometry {
        control_size: CONTROL as u64,
        arena_size: 0,
        arena_slab_size: 4096,
        ring_slot_count: 2,
        ring_data_bytes: 16 * 1024,
        timeline_count: 0,
    }
}
/// All live stream controls for the process's existing doorbell reader.
/// Membership is weak, so a doorbell cannot retain a finished allocation.
pub fn controls() -> Arc<ControlSet> {
    static CONTROLS: OnceLock<Arc<ControlSet>> = OnceLock::new();
    CONTROLS.get_or_init(ControlSet::new).clone()
}
fn ids() -> ProtocolIds {
    ProtocolIds::new(1, 0x5743_5354_524d_0001)
}
enum Allocation {
    Owner(Owner),
    Consumer(Consumer),
}
struct Backing {
    _directory: Directory,
    key: Key,
    address: usize,
    _allocation: Allocation,
}
// SAFETY: only atomic header accesses and the existing transport's ring accessors
// touch the coherent retained allocation. No shared Rust byte slices escape.
unsafe impl Send for Backing {}
unsafe impl Sync for Backing {}
impl Backing {
    fn word(&self, index: usize) -> &AtomicU64 {
        // SAFETY: the validated aligned allocation starts with a complete page.
        unsafe { &*(self.address as *const AtomicU64).add(index) }
    }
    fn count(writer: bool) -> usize {
        if writer { WRITER } else { READER }
    }
    fn pending(writer: bool) -> usize {
        if writer {
            WRITER_PENDING
        } else {
            READER_PENDING
        }
    }
    fn close_if_unused(&self, writer: bool) {
        if self.word(Self::count(writer)).load(Ordering::SeqCst) == 0
            && self.word(Self::pending(writer)).load(Ordering::SeqCst) == 0
        {
            self.word(if writer { WRITER_CLOSED } else { READER_CLOSED })
                .store(1, Ordering::Release);
        }
    }
    fn gate(&self, writer: bool) -> io::Result<Gate<'_>> {
        let word = self.word(if writer { WRITE_GATE } else { READ_GATE });
        word.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| io::Error::from(io::ErrorKind::WouldBlock))?;
        Ok(Gate(word))
    }
    fn publish(&self) -> io::Result<bar_lease::Transfer> {
        match &self._allocation {
            Allocation::Owner(owner) => owner.publish(),
            Allocation::Consumer(consumer) => consumer.publish(),
        }
    }
    fn control(self: &Arc<Self>, initialize: bool) -> io::Result<Arc<Registration>> {
        // SAFETY: the stream subpartition is shared writable, page aligned,
        // bounds checked, and retained by this exact allocation guard.
        let region = Arc::new(unsafe {
            SharedRegion::from_raw_owned(
                (self.address as *mut u8).add(HEADER),
                CONTROL,
                self.clone(),
            )
        });
        let control = if initialize {
            Control::create(region, geometry(), ids(), 1)
        } else {
            Control::attach(region, ids())
        }
        .map_err(transport)?;
        let control = Arc::new(control);
        controls().add(&control);
        Ok(Arc::new(Registration { control }))
    }
}
struct Gate<'a>(&'a AtomicU64);
impl Drop for Gate<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}
struct Registration {
    control: Arc<Control>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        controls().remove_and_wait(&self.control);
    }
}
impl std::ops::Deref for Registration {
    type Target = Control;
    fn deref(&self) -> &Control {
        &self.control
    }
}
struct State {
    backing: Arc<Backing>,
    writer: bool,
    channel: Mutex<ByteChannel>,
    _control: Arc<Registration>,
}
impl State {
    fn wake(&self, lane: Lane) {
        if let Ok(control) = RingCtrl::open(&self._control, 1)
            && let Ok(bell) = control.bell(lane)
        {
            bell.ring();
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        self.backing
            .word(Backing::count(self.writer))
            .fetch_sub(1, Ordering::SeqCst);
        self.backing.close_if_unused(self.writer);
        self.wake(Lane::Command);
    }
}
/// A local reference to exactly one direction. Clones share that local endpoint.
#[derive(Clone)]
pub struct Stream {
    state: Arc<State>,
}
impl Stream {
    pub fn writer(&self) -> bool {
        self.state.writer
    }
    pub fn key(&self) -> Key {
        self.state.backing.key
    }
    fn endpoint(
        backing: Arc<Backing>,
        writer: bool,
        control: Arc<Registration>,
    ) -> io::Result<Self> {
        let channel = ByteChannel::open(
            &control,
            1,
            if writer {
                Endpoint::Owner
            } else {
                Endpoint::Peer
            },
        )
        .map_err(transport)?;
        Ok(Self {
            state: Arc::new(State {
                backing,
                writer,
                channel: Mutex::new(channel),
                _control: control,
            }),
        })
    }
    pub(crate) fn pair(directory: Directory) -> io::Result<(Self, Self)> {
        let owner = directory.allocate(Role::Stream, LENGTH, LENGTH)?;
        let key = owner.key();
        let address = directory.stream_pointer(key)? as usize;
        let backing = Arc::new(Backing {
            _directory: directory,
            key,
            address,
            _allocation: Allocation::Owner(owner),
        });
        backing.word(READER).store(1, Ordering::Relaxed);
        backing.word(WRITER).store(1, Ordering::Relaxed);
        let control = backing.control(true)?;
        let channel = ByteChannel::claim(&control, 1).map_err(transport)?;
        if channel.slot() != 1 {
            return Err(error("BAR stream ring slot differs"));
        }
        drop(channel);
        backing.word(0).store(MAGIC, Ordering::Release);
        Ok((
            Self::endpoint(backing.clone(), false, control.clone())?,
            Self::endpoint(backing, true, control)?,
        ))
    }
    pub(crate) fn receive(directory: Directory, key: Key, writer: bool) -> io::Result<Self> {
        let (_, length, _) = directory.extent(key)?;
        if length != LENGTH {
            return Err(error("BAR stream extent length differs"));
        }
        let address = directory.stream_pointer(key)? as usize;
        // Accept reserves the allocation while inspecting the endpoint state.
        let consumer = directory.accept(key)?;
        let backing = Arc::new(Backing {
            _directory: directory,
            key,
            address,
            _allocation: Allocation::Consumer(consumer),
        });
        if backing.word(0).load(Ordering::Acquire) != MAGIC
            || backing
                .word(if writer { WRITER_CLOSED } else { READER_CLOSED })
                .load(Ordering::Acquire)
                != 0
        {
            return Err(error("BAR stream endpoint is not live"));
        }
        // Increment the endpoint reference before consuming its reservation.
        // The sender's drop can never observe both counts as zero in between.
        backing
            .word(Backing::count(writer))
            .fetch_add(1, Ordering::SeqCst);
        if backing
            .word(Backing::pending(writer))
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_sub(1)
            })
            .is_err()
        {
            backing
                .word(Backing::count(writer))
                .fetch_sub(1, Ordering::SeqCst);
            return Err(error("BAR stream endpoint has no pending transfer"));
        }
        let control = backing.control(false)?;
        Self::endpoint(backing, writer, control)
    }
    pub(crate) fn prepare(&self) -> io::Result<Transfer> {
        let allocation = self.state.backing.publish()?;
        self.state
            .backing
            .word(Backing::pending(self.writer()))
            .fetch_add(1, Ordering::SeqCst);
        Ok(Transfer {
            endpoint: self.clone(),
            allocation: Some(allocation),
            committed: false,
        })
    }
    /// Read bytes already published. `WouldBlock` is backpressure, zero is EOF.
    pub fn read(&self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.writer() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "write-only BAR stream",
            ));
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        // Acquire closure before inspecting the ring, so the last writer's
        // preceding publications are observed before reporting drained EOF.
        let _gate = self.state.backing.gate(false)?;
        let closed = self
            .state
            .backing
            .word(WRITER_CLOSED)
            .load(Ordering::Acquire)
            != 0;
        let outcome = self
            .state
            .channel
            .lock()
            .map_err(|_| error("BAR stream lock poisoned"))?
            .read_shared(bytes, self.state.backing.word(READ_CURSOR))
            .map_err(transport)?;
        drop(_gate);
        if matches!(outcome, ReadOutcome::Bytes(_)) {
            self.state.wake(Lane::Command);
        }
        match outcome {
            ReadOutcome::Bytes(count) => Ok(count),
            ReadOutcome::Eof => Ok(0),
            ReadOutcome::Empty if closed => Ok(0),
            ReadOutcome::Empty => Err(io::ErrorKind::WouldBlock.into()),
        }
    }
    pub fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        if !self.writer() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "read-only BAR stream",
            ));
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        if self
            .state
            .backing
            .word(READER_CLOSED)
            .load(Ordering::Acquire)
            != 0
        {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let _gate = self.state.backing.gate(true)?;
        let count = self
            .state
            .channel
            .lock()
            .map_err(|_| error("BAR stream lock poisoned"))?
            .write_available(bytes)
            .map_err(transport)?;
        drop(_gate);
        if count > 0 {
            self.state.wake(Lane::Command);
        }
        if count == 0 {
            Err(io::ErrorKind::WouldBlock.into())
        } else {
            Ok(count)
        }
    }
    pub fn peer_closed(&self) -> bool {
        self.state
            .backing
            .word(if self.writer() {
                READER_CLOSED
            } else {
                WRITER_CLOSED
            })
            .load(Ordering::Acquire)
            != 0
    }
    /// Watch one canonical lane for closure or requested I/O readiness.
    /// The epoch must precede the adapter's final stop/request checks.
    pub fn watch_since(
        &self,
        epoch: u32,
        want_io: bool,
        timeout: std::time::Duration,
    ) -> io::Result<u32> {
        let control = RingCtrl::open(&self.state._control, 1).map_err(transport)?;
        let bell = control.bell(Lane::Command).map_err(transport)?;
        let ready = || {
            if self.writer() {
                self.writable()
            } else {
                self.readable()
            }
        };
        bell.wait_while(epoch, timeout, || -> io::Result<bool> {
            Ok(!self.peer_closed() && !(want_io && ready()?))
        })?;
        Ok(u32::from(ready()?) | (u32::from(self.peer_closed()) << 1))
    }
    /// Record before the caller's final local queue/stop check.
    pub fn wake_epoch(&self) -> io::Result<u32> {
        let channel = self
            .state
            .channel
            .lock()
            .map_err(|_| error("BAR stream lock poisoned"))?;
        if self.writer() {
            channel.credit_epoch()
        } else {
            channel.wake_epoch()
        }
        .map_err(transport)
    }
    /// Wake an adapter waiting in this process, including an otherwise idle
    /// endpoint whose actual last local fd has just closed.
    pub fn interrupt(&self) {
        if let Ok(control) = RingCtrl::open(&self.state._control, 1)
            && let Ok(bell) = control.bell(Lane::Command)
        {
            bell.ring_local();
        }
    }
    pub fn writable(&self) -> io::Result<bool> {
        if !self.writer() {
            return Ok(false);
        }
        Ok(self
            .state
            .backing
            .word(READER_CLOSED)
            .load(Ordering::Acquire)
            != 0
            || self
                .state
                .channel
                .lock()
                .map_err(|_| error("BAR stream lock poisoned"))?
                .write_capacity()
                .map_err(transport)?
                > 0)
    }
    pub fn wait_since(&self, epoch: u32, timeout: std::time::Duration) -> io::Result<bool> {
        let channel = self
            .state
            .channel
            .lock()
            .map_err(|_| error("BAR stream lock poisoned"))?;
        if self.writer() {
            if self
                .state
                .backing
                .word(READER_CLOSED)
                .load(Ordering::Acquire)
                != 0
                || channel.write_capacity().map_err(transport)? > 0
            {
                return Ok(true);
            }
            channel
                .wait_writable_since(epoch, timeout)
                .map_err(transport)?;
            Ok(self
                .state
                .backing
                .word(READER_CLOSED)
                .load(Ordering::Acquire)
                != 0
                || channel.write_capacity().map_err(transport)? > 0)
        } else {
            if self
                .state
                .backing
                .word(WRITER_CLOSED)
                .load(Ordering::Acquire)
                != 0
                || channel.readable().map_err(transport)?
            {
                return Ok(true);
            }
            channel
                .wait_readable_since(epoch, timeout)
                .map_err(transport)?;
            Ok(self
                .state
                .backing
                .word(WRITER_CLOSED)
                .load(Ordering::Acquire)
                != 0
                || channel.readable().map_err(transport)?)
        }
    }
    pub fn readable(&self) -> io::Result<bool> {
        if self.writer() {
            return Ok(false);
        }
        Ok(self
            .state
            .backing
            .word(WRITER_CLOSED)
            .load(Ordering::Acquire)
            != 0
            || self
                .state
                .channel
                .lock()
                .map_err(|_| error("BAR stream lock poisoned"))?
                .readable()
                .map_err(transport)?)
    }
}
pub(crate) struct Transfer {
    endpoint: Stream,
    allocation: Option<bar_lease::Transfer>,
    committed: bool,
}
impl Transfer {
    pub(crate) fn commit(mut self) {
        self.committed = true;
        self.allocation.take().unwrap().commit();
    }
}
impl Drop for Transfer {
    fn drop(&mut self) {
        if !self.committed {
            let backing = &self.endpoint.state.backing;
            backing
                .word(Backing::pending(self.endpoint.writer()))
                .fetch_sub(1, Ordering::SeqCst);
            backing.close_if_unused(self.endpoint.writer());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Directory {
        let region = Arc::new(SharedRegion::heap(4 * 1024 * 1024).unwrap());
        // SAFETY: exclusive owned region, no peer exists during initialization.
        unsafe {
            let pointer = region.base_ptr();
            let directory = Directory::initialize(
                pointer,
                pointer,
                pointer.add(bar_lease::DIRECTORY_BYTES),
                Role::Guest,
                4 * 1024 * 1024,
                2 * 1024 * 1024,
                71,
                region.clone(),
            )
            .unwrap();
            let (start, _) = directory.stream_partition();
            directory.enable_streams(pointer.add(start)).unwrap();
            directory
        }
    }
    #[test]
    fn pending_writer_prevents_eof_and_allocation_reuse_until_acceptance() {
        let directory = fixture();
        let (reader, writer) = Stream::pair(directory.clone()).unwrap();
        let key = reader.key();
        let transfer = writer.prepare().unwrap();
        transfer.commit();
        drop(writer);
        assert_eq!(
            reader.read(&mut [0; 8]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let imported = Stream::receive(directory.clone(), key, true).unwrap();
        assert_eq!(imported.write(b"clipboard").unwrap(), 9);
        drop(imported);
        let mut bytes = [0; 16];
        assert_eq!(reader.read(&mut bytes).unwrap(), 9);
        assert_eq!(&bytes[..9], b"clipboard");
        assert_eq!(reader.read(&mut bytes).unwrap(), 0);
        assert!(directory.extent(key).is_ok());
        drop(reader);
        assert!(directory.extent(key).is_err());
    }
    #[test]
    fn short_reads_across_aliases_preserve_bytes_and_ring_credits() {
        let directory = fixture();
        let (reader, writer) = Stream::pair(directory.clone()).unwrap();
        let key = reader.key();
        reader.prepare().unwrap().commit();
        let alias = Stream::receive(directory.clone(), key, false).unwrap();
        let payload: Vec<u8> = (0..8192).map(|index| (index % 251) as u8).collect();
        assert_eq!(writer.write(&payload).unwrap(), payload.len());
        let mut actual = Vec::new();
        for index in 0..8192 {
            let mut byte = [0];
            assert_eq!(
                if index % 2 == 0 {
                    reader.read(&mut byte)
                } else {
                    alias.read(&mut byte)
                }
                .unwrap(),
                1
            );
            actual.push(byte[0]);
        }
        assert_eq!(actual, payload);
        drop(reader);
        assert_eq!(writer.write(b"still open").unwrap(), 10);
        drop(alias);
        assert_eq!(
            writer.write(b"closed").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        drop(writer);
        assert!(directory.extent(key).is_err());
    }
    #[test]
    fn unsent_transfer_rolls_back_and_full_ring_retains_bytes_until_drained() {
        let directory = fixture();
        let (reader, writer) = Stream::pair(directory.clone()).unwrap();
        let key = writer.key();
        drop(writer.prepare().unwrap());
        assert!(Stream::receive(directory.clone(), key, true).is_err());
        let payload = vec![0x5a; 8192];
        let mut sent = 0;
        loop {
            match writer.write(&payload) {
                Ok(count) => sent += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
            }
        }
        assert!(sent > 0);
        drop(writer);
        let mut received = 0;
        let mut bytes = [0; 997];
        loop {
            let count = reader.read(&mut bytes).unwrap();
            if count == 0 {
                break;
            }
            assert!(bytes[..count].iter().all(|byte| *byte == 0x5a));
            received += count;
        }
        assert_eq!(sent, received);
        drop(reader);
        assert!(directory.extent(key).is_err());
    }
    #[test]
    fn handle_descriptor_roundtrip_transfers_the_actual_endpoint_capability() {
        let directory = fixture();
        let arena = 0xf00f_0071;
        bar_lease::register(arena, directory.clone()).unwrap();
        let (reader, writer) = crate::Handle::create_bar_pipe(arena).unwrap();
        let key = reader.open_bar_stream().unwrap().key();
        let prepared = writer.prepare_transfer().unwrap();
        let descriptor = crate::Descriptor::decode(prepared.bytes()).unwrap();
        assert!(
            matches!(descriptor, crate::Descriptor::BarStream { arena: id, writer: true, .. } if id == arena)
        );
        prepared.commit();
        drop(writer);
        let remote = crate::Handle::receive_descriptor(descriptor.clone()).unwrap();
        assert!(
            crate::Handle::from_descriptor(descriptor)
                .open_bar_stream()
                .is_err()
        );
        assert!(remote.map().is_err());
        assert!(remote.map_mut().is_err());
        assert_eq!(
            remote
                .open_bar_stream()
                .unwrap()
                .write(b"actual capability")
                .unwrap(),
            17
        );
        drop(remote);
        let mut bytes = [0; 32];
        let stream = reader.open_bar_stream().unwrap();
        assert_eq!(stream.read(&mut bytes).unwrap(), 17);
        assert_eq!(&bytes[..17], b"actual capability");
        assert_eq!(stream.read(&mut bytes).unwrap(), 0);
        drop(stream);
        drop(reader);
        assert!(directory.extent(key).is_err());
    }
}
