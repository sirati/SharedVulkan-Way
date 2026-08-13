//! The per-operating-system half of the handle shim.
//!
//! Both implementations satisfy the same contract, and the contract is the
//! interesting part:
//!
//! * `create_section(name, size)` makes an object that a **different process**
//!   can reach knowing only `name`.
//! * the object lives exactly as long as the `OwnedObject` returned, and a peer
//!   that has not opened it by then loses the race.
//! * `map_section(name, len)` opens it by name, read-only.
//!
//! That contract is satisfiable on Windows without `DuplicateHandle` and
//! therefore without holding any rights on the peer process, which is the whole
//! point. It is also satisfiable on Unix, which is what lets the tests in
//! `lib.rs` run on the machine that builds them rather than only under Wine.
//!
//! # Stage 4 adds a second pair, and the split is by *namespace*
//!
//! * `create_file_object(path, size)` / `map_file(path, len)` name the object
//!   by a **filesystem path** instead of by an entry in the operating system's
//!   own object namespace.
//!
//! On Unix these are the same functions as the pair above, because the Unix
//! analogue of a named section was always a file at a path. On Windows they are
//! genuinely different calls — `CreateFileW` and `CreateFileMappingW(hFile, …)`
//! rather than `CreateFileMappingW(INVALID_HANDLE_VALUE, …)` and
//! `OpenFileMappingW` — and the difference is the whole point: an NT object
//! name is unreachable from a Linux process and a path is not. See
//! `crate::Descriptor::SharedFile`.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
