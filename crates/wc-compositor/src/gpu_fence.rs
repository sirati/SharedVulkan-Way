//! `wc_gpu_fence_v1`: hold a commit until the GPU has finished the frame
//! (docs/waycompose-design.md §104.11).
//!
//! A dma-buf `wl_buffer` from wc-proxy is a `wl_shm` pool on a section the
//! guest's GPU process writes. The client's driver no longer waits for the
//! GPU before it commits, so wc-proxy sends `wait(record, seq)` right before
//! the `wl_surface.commit` of such a buffer, and this module does not return
//! from that request until the GPU process's **fence record** — a 4 KiB
//! section it writes `completed` into — says `completed >= seq`. The commit
//! that follows on the same connection is therefore processed only after the
//! frame is complete, and nothing in the surface or blit code knows about it.
//!
//! The wait is on this thread, which is the cheap and blunt choice: the frame
//! was submitted before the client committed it, so by the time the commit
//! has crossed the proxy and the transport it is almost always done, and the
//! first check returns. When it is not, the wait is bounded ([`LIMIT`]); a
//! record that ran out its limit is not waited on again until it moves, so a
//! GPU process that died costs one wait, not one per frame.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New};

use crate::state::State;

/// The generated bindings for `protocols/wc-gpu-fence-v1.xml`. wc-proxy
/// writes the two requests by hand (`session::dmabuf::ops::FENCE_*`).
#[allow(missing_docs)]
pub mod proto {
    use smithay::reexports::wayland_server;

    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("protocols/wc-gpu-fence-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("protocols/wc-gpu-fence-v1.xml");
}

use proto::wc_gpu_fence_v1::{self, WcGpuFenceV1};

/// The longest a commit is held for one frame.
pub const LIMIT: Duration = Duration::from_millis(100);
/// A record's size, magic, and where `completed` is (`sv_gpu_server::fence`).
const RECORD_LEN: u64 = 4096;
const RECORD_MAGIC: &[u8; 8] = b"SVFENCE1";
const COMPLETED_OFFSET: usize = 8;
/// More records than this are never live at once (one per GPU session); the
/// cache is emptied rather than grown past it.
const MAX_RECORDS: usize = 64;

/// Create the global. Called once by `State::new`.
pub fn register(dh: &DisplayHandle) {
    let _ = dh.create_global::<State, WcGpuFenceV1, _>(1, ());
}

/// A record's name: a section or `/dev/shm` path whose basename starts with
/// `sv-fence-` (the rule of `sv_drm::token::record_name_ok`, restated because
/// this crate builds for Windows), so a client cannot make the compositor map
/// any other object through it.
pub fn record_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-./\\".contains(&b))
        && !name.contains("..")
        && name.rsplit(['/', '\\']).next().is_some_and(|b| b.starts_with("sv-fence-"))
}

struct Record {
    mapping: wc_handle::Mapping,
    /// The value it was stuck at when it last ran out [`LIMIT`].
    stuck_at: Option<u64>,
}

impl Record {
    fn completed(&self) -> Option<u64> {
        let s = self.mapping.as_slice();
        if s.len() < COMPLETED_OFFSET + 8 || &s[..8] != RECORD_MAGIC {
            return None;
        }
        // SAFETY: inside the mapping, 8-byte aligned (page base + 8); the GPU
        // process writes it with a release store from another process, so it
        // is read with a volatile load (an x86 load is acquire).
        let v = unsafe { std::ptr::read_volatile(s.as_ptr().add(COMPLETED_OFFSET) as *const u64) };
        Some(u64::from_le(v))
    }
}

static RECORDS: Mutex<Option<HashMap<String, Record>>> = Mutex::new(None);

fn open(name: &str) -> std::io::Result<Record> {
    let d = if name.starts_with('/') {
        wc_handle::Descriptor::SharedFile { path: name.to_string(), size: RECORD_LEN }
    } else {
        wc_handle::Descriptor::Section { name: name.to_string(), size: RECORD_LEN }
    };
    let mapping = wc_handle::Handle::from_descriptor(d).map()?;
    Ok(Record { mapping, stuck_at: None })
}

/// What [`wait`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Waited {
    /// Already there, or got there in this long.
    Done(Duration),
    /// Gave up after [`LIMIT`], or did not wait at all for a record already
    /// known to be stuck.
    Late,
    /// Not a record: bad name, no such section, wrong magic.
    Refused,
}

/// Wait until record `name` says `completed >= seq`, at most [`LIMIT`].
pub fn wait(name: &str, seq: u64) -> Waited {
    wait_with(name, seq, LIMIT)
}

fn wait_with(name: &str, seq: u64, limit: Duration) -> Waited {
    if !record_name_ok(name) {
        return Waited::Refused;
    }
    let mut g = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    if !map.contains_key(name) {
        if map.len() >= MAX_RECORDS {
            map.clear();
        }
        match open(name) {
            Ok(r) => {
                map.insert(name.to_string(), r);
            }
            Err(_) => return Waited::Refused,
        }
    }
    let r = map.get_mut(name).expect("just inserted");
    let t = Instant::now();
    let mut spins = 0u32;
    loop {
        let Some(c) = r.completed() else { return Waited::Refused };
        if c >= seq {
            r.stuck_at = None;
            return Waited::Done(t.elapsed());
        }
        if r.stuck_at == Some(c) || t.elapsed() >= limit {
            r.stuck_at = Some(c);
            return Waited::Late;
        }
        // A frame is usually a few hundred microseconds from done: yield
        // first, then sleep (a Windows sleep is a millisecond at best).
        spins += 1;
        if spins < 200 {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl GlobalDispatch<WcGpuFenceV1, ()> for State {
    fn bind(
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<WcGpuFenceV1>,
        _data: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WcGpuFenceV1, ()> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _resource: &WcGpuFenceV1,
        request: wc_gpu_fence_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        if let wc_gpu_fence_v1::Request::Wait { record, seq_hi, seq_lo } = request {
            let seq = (u64::from(seq_hi) << 32) | u64::from(seq_lo);
            match wait(&record, seq) {
                Waited::Done(d) if d >= Duration::from_millis(2) => {
                    eprintln!("wc-compositor: a GPU frame ({record} >= {seq}) took {d:.1?} more after its commit");
                }
                Waited::Done(_) => {}
                Waited::Late => eprintln!("wc-compositor: {record} did not reach {seq}; the commit goes on"),
                Waited::Refused => eprintln!("wc-compositor: wc_gpu_fence_v1.wait on {record:?}: not a fence record"),
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn only_fence_records_are_opened() {
        for bad in ["", "/etc/passwd", "/dev/shm/sv-shared-1-2-00", "Local\\other", "/dev/shm/../x/sv-fence-1"] {
            assert!(!record_name_ok(bad), "{bad}");
            assert_eq!(wait(bad, 1), Waited::Refused);
        }
        assert!(record_name_ok("Local\\sv-fence-412-0-0011223344556677"));
    }

    #[test]
    fn a_wait_returns_when_the_record_gets_there_and_gives_up_once_when_it_does_not() {
        let path = std::env::temp_dir().join(format!("sv-fence-{}-0-0011223344556677", std::process::id()));
        let name = path.to_str().unwrap().to_string();
        let mut page = vec![0u8; RECORD_LEN as usize];
        page[..8].copy_from_slice(RECORD_MAGIC);
        page[8..16].copy_from_slice(&3u64.to_le_bytes());
        std::fs::write(&path, &page).unwrap();
        assert!(matches!(wait(&name, 3), Waited::Done(_)));
        assert_eq!(wait_with(&name, 4, Duration::from_millis(20)), Waited::Late);
        // Stuck at 3: the next wait does not wait.
        let t = Instant::now();
        assert_eq!(wait_with(&name, 5, Duration::from_secs(5)), Waited::Late);
        assert!(t.elapsed() < Duration::from_secs(1));
        // It moves: waits work again.
        let p2 = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            use std::os::unix::fs::FileExt;
            let f = std::fs::OpenOptions::new().write(true).open(&p2).unwrap();
            f.write_all_at(&6u64.to_le_bytes(), 8).unwrap();
        });
        writer.join().unwrap();
        assert!(matches!(wait(&name, 6), Waited::Done(_)));
        std::fs::remove_file(&path).unwrap();
    }
}
