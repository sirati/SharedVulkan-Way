//! Host integration for waycompose (docs/waycompose-design.md §106): every
//! host audio device as its own PipeWire node in Linux with the defaults
//! following the host, and opening links and files across the boundary in
//! both directions, by type.
//!
//! Everything here is pure and is tested on the build machine. Nothing in
//! this crate opens a socket: the channel is the `OWNER_HOSTINT` ring slot
//! of the session's shared memory (sv-transport `wlbytes`), and the PCM is
//! on the sound ring. The model is the real-Mac prototype's
//! (nix-darwin-subsystem-linux, `agent/`), whose pure functions and tests
//! were adopted where they fit.

pub mod assoc;
pub mod audio;
pub mod host;
pub mod linux;
pub mod mime;
pub mod openurl;
pub mod wire;
