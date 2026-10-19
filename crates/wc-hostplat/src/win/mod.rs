//! Windows: the endpoints and their streams (WASAPI), opening (the shell),
//! the applications per type (the shell's association handlers) and the
//! per-user registration of Linux applications (HKCU).

pub mod audio;
pub mod com;
pub mod handlers;
pub mod open;
pub mod policy;
pub mod registry;
pub mod stream;
