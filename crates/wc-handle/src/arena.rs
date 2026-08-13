//! Arenas: regions both sides mapped **before** any protocol traffic, so that
//! a buffer can cross by arithmetic instead of by opening anything.
//!
//! # Why this is a process-global registry and not a parameter
//!
//! An arena is not a per-message fact. It is attached once, out of band, by
//! whatever brought the two sides together — shared_vulkan's cold-path
//! handshake in the real deployment (its design.md §4.3), a command-line flag
//! in a test — and from then on every `Descriptor::ArenaSlice` in the stream
//! refers to it by a small integer.
//!
//! The alternative was to thread an arena table through
//! [`Handle::map`](crate::Handle::map). That fails for a reason worth writing
//! down, because it is the same reason the whole `h`-argument shim exists:
//! `Handle::map` is called from `vendor/smithay`'s `wayland/shm/pool.rs`, which
//! upstream calls with nothing but an fd, and giving it a context parameter
//! would mean carrying a diff through smithay's `ShmState`, its `Client` data
//! and its `wl_shm` dispatch — precisely the "spread through the commit path"
//! cost docs/design.md §8.4 says the seam exists to avoid. An arena is
//! genuinely process-wide state; modelling it as such is honest, and it keeps
//! the fork's diff at zero lines.
//!
//! # The lifetime rule, which is the opposite of a section's
//!
//! docs/design.md §2.5 says the *creator* of a section must hold it open until
//! the receiver has opened it. An arena inverts that: the receiver holds the
//! region open for the life of the process, and the sender's slices are pure
//! arithmetic that cannot dangle. Nothing has to be raced. That is the second
//! reason this is the right kind for the real deployment and not merely the
//! available one.
//!
//! # What is *not* here
//!
//! No allocator. Who owns which bytes of an arena is the sender's problem —
//! `sv_arena::alloc` on the shared_vulkan side, `wc-proxy`'s pool allocator on
//! this one. This module resolves `(arena, offset, len)` to memory and refuses
//! anything out of range. Refusing is the whole of its safety contribution and
//! it is worth being clear that it is not much: a peer that can name any offset
//! in the arena can read any of it, exactly as a peer that can name a section
//! can read all of that section. The boundary is between the two sides, not
//! within one arena.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, OnceLock, RwLock};

use crate::{Mapping, MappingInner, platform};

/// One attached region.
struct Attached {
    view: Arc<platform::MappedView>,
    len: usize,
    /// How the region was named, for logs. Never used to reach it again.
    origin: String,
}

type Registry = RwLock<HashMap<u32, Attached>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Attach the arena numbered `id`, whose bytes are the file at `path`.
///
/// Read-only, because the receiving side of an arena slice is a compositor
/// reading a client's pixels — the same default `Handle::map` has for a
/// section, and for the same reason (docs/design.md §7.4).
///
/// Idempotent per id only in the sense that re-attaching replaces: an arena is
/// identified by its number on the wire, and if the two sides ever disagree
/// about what number 0 means, no amount of care here can detect it. That
/// disagreement is what shared_vulkan's `generation` field exists to catch on
/// the real path (its design.md §4.3), and a test harness passing the same
/// flag to both processes is what catches it here.
pub fn attach(id: u32, path: &str, size: u64) -> io::Result<()> {
    let len = usize::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena too large"))?;
    let view = platform::map_file(path, len)?;
    let mut guard = registry()
        .write()
        .map_err(|_| io::Error::other("arena registry poisoned"))?;
    guard.insert(id, Attached { view: Arc::new(view), len, origin: path.to_owned() });
    Ok(())
}

/// Every attached arena, as `(id, length, origin)`, for logging.
pub fn attached() -> Vec<(u32, usize, String)> {
    let Ok(guard) = registry().read() else {
        return Vec::new();
    };
    let mut out: Vec<_> = guard
        .iter()
        .map(|(id, a)| (*id, a.len, a.origin.clone()))
        .collect();
    out.sort_by_key(|(id, _, _)| *id);
    out
}

/// Resolve `(arena, offset, len)` to a read-only view.
///
/// Every arithmetic step is checked. An offset or length that runs off the end
/// of the region is refused rather than clamped: a clamped window would hand
/// the compositor a short buffer that `BufferSpec::required_len` would then
/// reject somewhere much further away, and the message would be about a stride.
pub(crate) fn window(arena: u32, offset: u64, len: u64) -> io::Result<Mapping> {
    let guard = registry()
        .read()
        .map_err(|_| io::Error::other("arena registry poisoned"))?;
    let Some(a) = guard.get(&arena) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "arena {arena} is not attached (attached: {:?})",
                guard.keys().collect::<Vec<_>>()
            ),
        ));
    };
    let offset = usize::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena offset too large"))?;
    let len = usize::try_from(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "arena slice too large"))?;
    let end = offset
        .checked_add(len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "arena slice overflows"))?;
    if end > a.len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("arena slice {offset}..{end} runs past the end of arena {arena} ({})", a.len),
        ));
    }
    Ok(Mapping { inner: MappingInner::Window(a.view.clone()), offset, len })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Descriptor, Handle};

    /// Arena ids are process-global and `cargo test` runs these threads
    /// concurrently, so **every test must own its id**. Sharing one produced a
    /// real failure the first time these were written: two tests attached
    /// different files as the same arena and the second zeroed the first's
    /// bytes underneath it. That is the registry behaving exactly as documented
    /// — re-attaching replaces — and it is worth leaving this note rather than
    /// the memory of an afternoon.
    const A: u32 = 9001;
    const B: u32 = 9002;
    const C: u32 = 9003;

    fn make_arena(id: u32, size: u64, fill: impl Fn(&mut [u8])) -> Handle {
        let path = format!("{}/wc-arena-test-{id}", std::env::temp_dir().display());
        let _ = std::fs::remove_file(&path);
        let owner = Handle::create_shared_file(&path, &path, size).unwrap();
        {
            let mut view = owner.map_mut().unwrap();
            fill(view.as_mut_slice());
        }
        attach(id, &path, size).unwrap();
        owner
    }

    /// The claim: only three integers cross, and the bytes are there.
    #[test]
    fn a_slice_of_an_attached_arena_carries_the_memory() {
        let _keep = make_arena(A, 8192, |b| {
            b[0..4].copy_from_slice(b"HEAD");
            b[4096..4100].copy_from_slice(b"MIDL");
            b[8188..8192].copy_from_slice(b"TAIL");
        });

        // These bytes, and nothing else, are what a peer would receive.
        let wire = Descriptor::ArenaSlice { arena: A, offset: 4096, len: 4096 }.encode();
        let peer = Handle::from_descriptor(Descriptor::decode(&wire).unwrap());

        let map = peer.map().unwrap();
        assert_eq!(map.len(), 4096);
        assert_eq!(&map.as_slice()[0..4], b"MIDL");
        assert_eq!(&map.as_slice()[4092..4096], b"TAIL");
    }

    /// A window is a window, not a mapping: the region survives it.
    #[test]
    fn many_windows_coexist_and_do_not_unmap_the_region() {
        let _keep = make_arena(B, 4096, |b| b[100] = 0x5a);
        let mk = |off: u64, len: u64| {
            Handle::from_descriptor(Descriptor::ArenaSlice { arena: B, offset: off, len })
        };
        let first = mk(0, 4096).map().unwrap();
        {
            // Dropped while `first` is still live. If a window unmapped the
            // region on drop, the read below would fault rather than fail a
            // comparison, which is exactly why this test exists.
            let second = mk(64, 128).map().unwrap();
            assert_eq!(second.len(), 128);
        }
        assert_eq!(first.as_slice()[100], 0x5a);
    }

    #[test]
    fn out_of_range_windows_are_refused_rather_than_clamped() {
        let _keep = make_arena(C, 4096, |_| {});
        for (off, len) in [(4096u64, 1u64), (0, 4097), (4000, 100), (u64::MAX, 1)] {
            let h = Handle::from_descriptor(Descriptor::ArenaSlice { arena: C, offset: off, len });
            assert!(h.map().is_err(), "window {off}+{len} should have been refused");
        }
    }

    #[test]
    fn an_unattached_arena_is_an_error_and_not_a_panic() {
        let h = Handle::from_descriptor(Descriptor::ArenaSlice {
            arena: 0xdead_beef,
            offset: 0,
            len: 16,
        });
        assert_eq!(h.map().unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
