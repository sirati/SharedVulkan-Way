//! Process-selected shared-memory allocation supplied by a transport.
//! Installing a provider replaces section creation without changing clients.
//! Provider failure is final: falling back to guest-local memory would make
//! the same descriptor mean inaccessible pixels to a cross-VM compositor.
use crate::Handle;
use std::io;
use std::sync::OnceLock;

type Provider = dyn Fn(u64) -> io::Result<Handle> + Send + Sync;
static PROVIDER: OnceLock<Box<Provider>> = OnceLock::new();

/// Select the original backing allocator once, before clients allocate pools.
/// The provider must retain each allocation's owner through its Handle/maps.
pub fn install(provider: impl Fn(u64) -> io::Result<Handle> + Send + Sync + 'static) -> io::Result<()> {
    PROVIDER.set(Box::new(provider)).map_err(|_| io::Error::new(io::ErrorKind::AlreadyExists, "shared section provider already selected"))
}

pub(crate) fn allocate(size: u64) -> Option<io::Result<Handle>> {
    PROVIDER.get().map(|provider| provider(size))
}
