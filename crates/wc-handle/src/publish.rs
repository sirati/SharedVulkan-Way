//! Where a **server-created** shared object goes when the peer is on the other
//! side of the boundary.
//!
//! # The direction that stage 4 did not need, and stage 6 does
//!
//! Stage 4 moved a `wl_shm` pool, which is a client-created object: the proxy
//! made the region on Linux and the compositor opened it on Windows. Every
//! transport decision lived in `wc-proxy`, because the proxy was the creator.
//!
//! `wl_keyboard.keymap` runs the other way. **The compositor creates the
//! object** and the client opens it, so the creator is now the Windows side and
//! the opener is a Linux process. [`Handle::create_section`] is exactly wrong
//! for that: it mints `Local\wc-…`, a name only the NT object manager
//! understands, which a Linux process can neither open nor spell. That is
//! docs/design.md §9.10 item 5 — "`wl_keyboard.keymap`: implemented,
//! unexercised" — and the reason it was unexercised is that exercising it would
//! have failed.
//!
//! [`Handle::create_section`]: crate::Handle::create_section
//!
//! # Why this is a process-global registry, exactly as [`arena`] is
//!
//! [`arena`]: crate::arena
//!
//! The same argument, and it is worth not paraphrasing it: threading "where do
//! objects I create for my peer go" through the call site would mean a diff
//! through smithay's `SealedFile`, its `KeymapFile`, its `KeyboardHandle`, its
//! `Seat` and its `SeatState` — a parameter carried through five layers so that
//! one leaf can spell a path. Every one of those is upstream code we intend to
//! send patches back to (docs/design.md §5.3), and a five-layer parameter is
//! not a patch anybody merges.
//!
//! It is also honest: like an arena, this is not a per-message fact. It is
//! settled once, out of band, by whatever brought the two sides together —
//! `--shared-dir` on the compositor's command line here, and in the real
//! deployment whatever mounts the guest's view of the host filesystem. Nothing
//! about it varies per keymap.
//!
//! # What is *not* decided here
//!
//! Whether a given call site should publish this way at all. `wc_handle` offers
//! [`Handle::create_shared`], which is "make something my peer can open"; the
//! caller says what it wants and this module says where it lands. A caller that
//! genuinely wants a same-machine section — nothing does yet — still calls
//! `create_section` and is unaffected by anything set here.
//!
//! [`Handle::create_shared`]: crate::Handle::create_shared
//!
//! # The two paths, and why the caller must supply both
//!
//! Identical to [`Descriptor::SharedFile`](crate::Descriptor::SharedFile) and
//! for the same reason: the same bytes are `Z:\tmp\wc-keymaps\wc-3f…` to the
//! compositor and `/tmp/wc-keymaps/wc-3f…` to the client, and nothing in this
//! crate can derive one spelling from the other. A crate that guessed would be
//! wrong on the deployment that matters. Here the *creator* is the Windows
//! side, so `local` is the Windows spelling and `peer` is the Linux one — the
//! mirror image of stage 4, which is exactly the trap: a descriptor built with
//! the two the wrong way round is perfectly well-formed and cannot be opened.

use std::io;
use std::sync::{OnceLock, RwLock};

/// Where objects created for a peer are put, and how the peer spells that
/// place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dir {
    /// The directory as *this* process spells it. Objects are created here.
    pub local: String,
    /// The prefix the peer prepends to a file name to reach the same bytes.
    /// Includes its own separator, because only the caller knows which one the
    /// peer uses.
    pub peer_prefix: String,
}

fn slot() -> &'static RwLock<Option<Dir>> {
    static SLOT: OnceLock<RwLock<Option<Dir>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Publish server-created objects into `local`, telling peers to open
/// `peer_prefix` + the file's name.
///
/// Creates the directory if it is missing, so that a wrong path fails here —
/// at startup, with the path in the message — rather than at the first keymap,
/// where it would present as a client that connects and then dies.
pub fn set(local: &str, peer_prefix: &str) -> io::Result<()> {
    std::fs::create_dir_all(local)?;
    let mut guard = slot()
        .write()
        .map_err(|_| io::Error::other("publish registry poisoned"))?;
    *guard = Some(Dir { local: local.to_owned(), peer_prefix: peer_prefix.to_owned() });
    Ok(())
}

/// What [`set`] was last called with, if it ever was.
pub fn get() -> Option<Dir> {
    slot().read().ok().and_then(|g| g.clone())
}

/// A fresh, unguessable file name inside the published directory, as both
/// sides spell it: `(local, peer)`.
pub(crate) fn mint() -> Option<(String, String)> {
    let dir = get()?;
    let token = crate::platform::random_token();
    // The separator on the local side is the one the local platform uses; the
    // peer's is baked into `peer_prefix`, because the peer may not agree about
    // which character separates path components — which is the entire reason
    // this is a *prefix* rather than a directory.
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let local = if dir.local.ends_with(['\\', '/']) {
        format!("{}{token}", dir.local)
    } else {
        format!("{}{sep}{token}", dir.local)
    };
    Some((local, format!("{}{token}", dir.peer_prefix)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-global, so the tests that touch it must not run
    /// concurrently with each other. One test, several assertions.
    #[test]
    fn a_published_directory_mints_two_spellings_of_one_name() {
        assert!(mint().is_none(), "nothing may be minted before `set`");

        let dir = std::env::temp_dir().join(format!("wc-publish-{}", std::process::id()));
        set(dir.to_str().unwrap(), "Z:\\peer\\").unwrap();
        assert!(dir.is_dir(), "`set` must create the directory it names");

        let (local, peer) = mint().unwrap();
        assert!(local.starts_with(dir.to_str().unwrap()), "local: {local}");
        assert!(peer.starts_with("Z:\\peer\\"), "peer: {peer}");

        // The same token on both sides is the whole contract: the compositor
        // creates `local` and the client opens `peer`, and if the tails ever
        // differed the descriptor would be well-formed and unopenable — the
        // failure mode `Descriptor::SharedFile`'s own documentation warns
        // about.
        let tail = |s: &str| s.rsplit(['\\', '/']).next().unwrap().to_owned();
        assert_eq!(tail(&local), tail(&peer));

        // Two mints never collide.
        let (l2, _) = mint().unwrap();
        assert_ne!(local, l2);

        let _ = std::fs::remove_dir_all(&dir);
        *slot().write().unwrap() = None;
    }
}
