//! ADDED BY THE WAYCOMPOSE FORK. Not upstream.
//!
//! One place where "what a file descriptor is" is decided, so that the rest of
//! the crate can say `crate::os::OwnedFd` and otherwise be left alone.
//!
//! The research report says the protocol machinery here "is genuinely
//! fd-type-agnostic already" (§1.4), and that turns out to be true: `Argument`
//! and `Message` are already generic over `Fd`, and the only coupling to Unix
//! is a set of `AsRawFd` bounds used for nothing but equality and `Display`.
//! This module is what that observation looks like once it is cashed in.
//!
//! # What an `Fd` is on each platform
//!
//! (This table is about the **socket** role. For the protocol role see
//! [`WireFd`], which is a disjunction rather than a platform choice.)
//!
//! | | `RawFd` | `OwnedFd` |
//! |---|---|---|
//! | Unix | `i32`, borrowed | `OwnedFd`, closes on drop |
//! | Windows | [`wc_handle::Handle`] | the same type |
//!
//! On Windows the raw/owned distinction collapses, and that is not laziness.
//! The distinction exists on Unix because an fd is an integer with no ownership
//! semantics of its own, so a separate type has to carry them. A
//! `wc_handle::Handle` is refcounted and already carries its own lifetime, so
//! "the borrowed form" and "the owned form" would be the same type with the
//! same behaviour. Upstream's `try_clone_to_owned()` becomes `.clone()`, which
//! is the honest analogue of `dup()`: both names refer to one object and the
//! object outlives whichever reference goes first.
//!
//! # Two jobs, and on Unix one name
//!
//! The names above answer "what is a file descriptor **here**", and this crate
//! asks that question for two unrelated reasons:
//!
//! * the **socket layer** — `rs/socket.rs`'s `in_fds`/`out_fds` queues, the
//!   `SCM_RIGHTS` control message, `rustix`'s `AncillaryIter`, and the `AsFd`
//!   bounds on `Socket`, `Client<D>` and the transport `Stream`. Every one of
//!   those is a real kernel descriptor and can never be anything else.
//! * the **protocol layer** — the `Fd` parameter of
//!   [`Argument`][crate::protocol::Argument] and
//!   [`Message`][crate::protocol::Message]. That payload is only ever compared,
//!   printed, and handed to whatever `rs/wire.rs` has decided an `h` argument
//!   looks like on the wire.
//!
//! On **Windows** those are already different types and nothing had to say so:
//! the socket is a `wc_transport::Stream` and an `h` is a `wc_handle::Handle`.
//! On **Unix** they are both `std::os::unix::io::OwnedFd`, so the difference has
//! no spelling — and changing what an `h` carries would silently drag the socket
//! layer with it. [`ProtoFd`] and its two companions give the protocol role its
//! own name so that it can move on its own. See
//! `docs/waycompose-design.md` §76.
//!
//! [`WireFd`] is what it moved **to**, in §79: an `h` that is either a name the
//! peer can open or a descriptor that is already open, chosen **per connection**
//! by [`FdEncoding`] rather than per platform. §77 introduced the type; §79 made
//! `rs/wire.rs` read and write both arms on both platforms, which is what
//! `docs/waycompose-design.md` §72.3 called for.

#[cfg(unix)]
pub use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};

/// What a wayland `h` argument carries, owned: the `Fd` parameter of a
/// [`Message`][crate::protocol::Message] that was **parsed** off the wire.
///
/// This is [`WireFd`]: an `h` that carries **either** a descriptor name (in
/// band, what a peer behind `wc-proxy` speaks) **or** an already-open fd (out of
/// band, `SCM_RIGHTS`), with `rs/wire.rs` choosing per connection rather than
/// per platform. `docs/waycompose-design.md` §79.
///
/// The socket layer stayed on [`OwnedFd`] when this changed, which is what §76
/// separated the two names for. Measured: swapping these three aliases for the
/// enums produced **two** compile errors in the whole of `wayland-backend` and
/// `wayland-server`, both in `rs/wire.rs`, and they were the write and parse
/// arms of `Argument::Fd`.
pub type ProtoFd = WireFd;

/// What a wayland `h` argument carries when it is about to be **written**: the
/// `Fd` parameter of the `Message` handed to `send_request` / `send_event`.
///
/// The send side is unowned on Unix — the caller keeps the descriptor and
/// `write_to_buffers` `dup()`s it — which is why the protocol has two of these
/// rather than one. See [`RawWireFd`].
pub type RawProtoFd = RawWireFd;

/// What a wayland `h` argument carries in a generated request or event, borrowed
/// for `'a`.
///
/// This is the type `wayland-scanner` emits into `write_request` / `write_event`
/// and into the `Fd` field of every generated `Request<'a>`, which is why it has
/// a lifetime the other two do not. See [`BorrowedWireFd`].
pub type BorrowedProtoFd<'a> = BorrowedWireFd<'a>;

/// **What an `h` argument is once the dialect is a choice rather than a
/// platform.** Either a *name* the peer can open, or a descriptor that is
/// *already open*.
///
/// `docs/waycompose-design.md` §72.3 settled that both encodings exist on both
/// platforms and that the **transport** picks between them: a connection whose
/// peer is `wc-proxy` reads and writes the descriptor in band, a connection from
/// a stock libwayland client reads and writes `SCM_RIGHTS`. One binary serves
/// both, and the choice is per connection — which is what lets a single Linux
/// `wc-compositor` serve WLCS and the proxy without a second build.
///
/// # Why the second variant is not a `wc_handle::Descriptor`
///
/// [`wc_handle::Descriptor`] is deliberately not extended to cover this, and the
/// reason is the whole distinction the type draws: **`Descriptor` means "a name a
/// peer can open"**, and an fd that arrived over `SCM_RIGHTS` has no name. It was
/// never created under one, nothing can re-open it, and the only thing anybody
/// can do with it is use the descriptor they were handed. Adding an unnamed arm
/// to `Descriptor` would make `encode()`/`decode()` — a wire format — carry
/// something that cannot be written down, and every reader of that type would
/// have to learn that one variant is a lie.
///
/// So the disjunction lives here, in the one module that already decides what a
/// file descriptor is. `wc_handle::Descriptor::NtHandle` is the precedent on the
/// other side and it is precedent for the *shape*, not for reuse: it is Windows's
/// escape hatch for an object that cannot be named, and this is Unix's.
///
/// # Why there is no `Open` arm on Windows
///
/// Not an omission. `SCM_RIGHTS` has no Windows equivalent — there is no
/// ancillary channel on the socket to carry a descriptor out of band — so an
/// already-open unnamed fd cannot arrive there and a variant for it would be
/// uninhabited by construction. §72.3's "both encodings on both platforms" is
/// about the *in-band* form existing on Unix, which is the half that was
/// missing; it does not claim Windows grows an out-of-band one.
#[derive(Debug)]
pub enum WireFd {
    /// A name the peer can open: the **in-band** dialect. Carries a
    /// [`wc_handle::Handle`] rather than a bare
    /// [`Descriptor`][wc_handle::Descriptor] because the sender must keep the
    /// object alive until the peer has opened it — sending a name transfers
    /// nothing, unlike `SCM_RIGHTS`, and a `Handle` is what carries that
    /// keepalive.
    Named(wc_handle::Handle),

    /// A descriptor that is already open, received out of band over
    /// `SCM_RIGHTS`. Owned, and closed on drop, exactly as the `Fd` payload of a
    /// parsed `Message` is today.
    #[cfg(unix)]
    Open(OwnedFd),
}

impl WireFd {
    /// The name a peer would open, if this is the in-band form.
    ///
    /// `None` for [`WireFd::Open`], and that `None` is the honest answer rather
    /// than a failure: an `SCM_RIGHTS` fd has no name to give.
    pub fn descriptor(&self) -> Option<&wc_handle::Descriptor> {
        match self {
            Self::Named(handle) => Some(handle.descriptor()),
            #[cfg(unix)]
            Self::Open(_) => None,
        }
    }

    /// Borrow this `h` in the form a request or event carries.
    ///
    /// The operation every sender that is **forwarding** an `h` needs: a
    /// clipboard pipe arrives on one connection and goes out on another, and
    /// neither end should have to know which arm it is.
    pub fn borrow(&self) -> BorrowedWireFd<'_> {
        match self {
            Self::Named(handle) => BorrowedWireFd::Named(handle),
            #[cfg(unix)]
            Self::Open(fd) => BorrowedWireFd::Open(fd.as_fd()),
        }
    }

    /// Take the open descriptor, or hand back the `h` unchanged if it is a name.
    ///
    /// For the callers that need a **real kernel descriptor** and cannot use a
    /// name: a `UnixListener` to accept on, an fd to `mmap` a virtual keyboard's
    /// keymap out of. `Err` is not a failure to convert, it is the statement
    /// that this peer sent something that cannot do that job, and the caller
    /// should raise a protocol error rather than guess.
    ///
    /// Unconditional, and on Windows it always returns `Err`: there is no
    /// unnamed descriptor there, so every caller of this is a caller that cannot
    /// be served on that platform. That is the same answer it has always given
    /// — it is now given by the type rather than by a `cfg` at each site.
    pub fn into_open(self) -> Result<OwnedFd, Self> {
        match self {
            #[cfg(unix)]
            Self::Open(fd) => Ok(fd),
            named => Err(named),
        }
    }

    /// The open descriptor, borrowed, if this is the out-of-band form.
    #[cfg(unix)]
    pub fn as_open(&self) -> Option<BorrowedFd<'_>> {
        match self {
            Self::Open(fd) => Some(fd.as_fd()),
            Self::Named(_) => None,
        }
    }
}

// Note that this is written out rather than left to the blanket
// `impl<T: AsRawFd> FdRepr for T` below: a `WireFd` deliberately has no
// `AsRawFd`, because half of its inhabitants have no raw fd to return and an
// impl that answered for them would have to invent one.
impl FdRepr for WireFd {
    /// Whether two `h` arguments refer to the same thing.
    ///
    /// The two arms answer with different strength and that is inherited rather
    /// than chosen: `Named` compares descriptors, so two handles to one section
    /// are equal; `Open` compares raw fd numbers, so two `dup()`s of one file are
    /// **not**. That is exactly the asymmetry this module's [`FdRepr`] docs
    /// already record between the platforms, now visible within one type. A
    /// named thing and an open thing are never equal — they might name the same
    /// bytes, but nothing here can know that.
    fn fd_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Named(a), Self::Named(b)) => a == b,
            #[cfg(unix)]
            (Self::Open(a), Self::Open(b)) => a.as_raw_fd() == b.as_raw_fd(),
            #[cfg(unix)]
            _ => false,
        }
    }

    fn fd_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Named(handle) => write!(f, "{:?}", handle.descriptor()),
            #[cfg(unix)]
            Self::Open(fd) => write!(f, "{}", fd.as_raw_fd()),
        }
    }
}

/// The send-side counterpart of [`WireFd`]: what `send_request` / `send_event`
/// carry.
///
/// Unowned in the `Open` arm, exactly as `RawFd` was — the caller keeps the
/// descriptor and `write_to_buffers` `dup()`s it into the `SCM_RIGHTS` batch.
/// The `Named` arm *is* owned, because a [`wc_handle::Handle`] is a refcounted
/// clone rather than a number, and cloning it is the honest analogue of the
/// `dup()` the other arm does later.
#[derive(Debug, Clone)]
pub enum RawWireFd {
    /// See [`WireFd::Named`].
    Named(wc_handle::Handle),
    /// A raw descriptor the caller still owns. See [`WireFd::Open`].
    #[cfg(unix)]
    Open(RawFd),
}

impl FdRepr for RawWireFd {
    fn fd_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Named(a), Self::Named(b)) => a == b,
            #[cfg(unix)]
            (Self::Open(a), Self::Open(b)) => a == b,
            #[cfg(unix)]
            _ => false,
        }
    }
    fn fd_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Named(handle) => write!(f, "{:?}", handle.descriptor()),
            #[cfg(unix)]
            Self::Open(raw) => write!(f, "{raw}"),
        }
    }
}

/// The borrowed counterpart of [`WireFd`]: what `wayland-scanner` emits into
/// every generated `Request<'a>` and `write_request`/`write_event` signature.
///
/// A caller with a real descriptor writes `BorrowedWireFd::Open(fd.as_fd())`; a
/// caller with a named object writes `BorrowedWireFd::Named(&handle)`. Which one
/// is correct is not the caller's guess — it is what the peer's dialect demands,
/// and [`Client::fd_encoding`][crate::server::Client] is where that is recorded.
#[derive(Debug, Clone)]
pub enum BorrowedWireFd<'a> {
    /// See [`WireFd::Named`].
    Named(&'a wc_handle::Handle),
    /// See [`WireFd::Open`].
    #[cfg(unix)]
    Open(BorrowedFd<'a>),
}

impl BorrowedWireFd<'_> {
    /// Convert to the send-side form.
    ///
    /// Named `as_raw_fd` because that is what every generated `write_request`
    /// calls, and the name is inherited rather than chosen. It is not a lie in
    /// either arm: the `Open` arm really does produce the raw number, and the
    /// `Named` arm produces the refcounted clone that stands in for one.
    #[allow(clippy::wrong_self_convention)]
    pub fn as_raw_fd(&self) -> RawWireFd {
        match self {
            Self::Named(handle) => RawWireFd::Named((*handle).clone()),
            #[cfg(unix)]
            Self::Open(borrowed) => RawWireFd::Open(borrowed.as_raw_fd()),
        }
    }
}

impl FdRepr for BorrowedWireFd<'_> {
    fn fd_eq(&self, other: &Self) -> bool {
        self.as_raw_fd().fd_eq(&other.as_raw_fd())
    }
    fn fd_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_raw_fd().fd_fmt(f)
    }
}

/// **How a connection spells an `h` argument.** The thing that used to be a
/// `#[cfg]`.
///
/// Deliberately the same name and the same two variants as
/// `wc_proxy::wire::FdEncoding`, which has been a runtime parameter on the proxy
/// side since it was written — the proxy reads one dialect and writes the other
/// on every message it touches, so it never had the option of a `cfg`. The two
/// halves of this system now use one word for one concept.
///
/// `docs/waycompose-design.md` §72.3 for why the choice is by **transport**
/// rather than by platform, and §79 for what makes it per connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdEncoding {
    /// libwayland's: the descriptor rides a `SCM_RIGHTS` control message and the
    /// `h` occupies **zero bytes** of the message body.
    ///
    /// The default **on Unix**, because it is what an unannounced peer is: a
    /// stock client connecting to a stock socket. A peer that speaks the other
    /// dialect has to have arrived somewhere that says so.
    ///
    /// **Cannot be spoken on Windows at all** — there is no ancillary channel —
    /// which is why the default below is `cfg`-ed rather than derived.
    OutOfBand,
    /// The descriptor is encoded in the body exactly like an `array` — a `u32`
    /// length, the bytes, padded to four — and there is no control message.
    ///
    /// What a peer reached through `wc-proxy` speaks, on either platform, and
    /// the only dialect available on Windows because `SCM_RIGHTS` has no
    /// equivalent there — which is what makes it the default there.
    InBand,
    /// In-band descriptors restricted to one preauthenticated BAR arena.
    /// Other arenas and host pathname/section/pipe descriptors are refused
    /// before a protocol handler can open or map anything they name.
    InBandArena(u32),
    /// Authenticated original BAR allocation with consumer-completion leases.
    InBandLease(u32),
}

impl Default for FdEncoding {
    /// **What an unannounced peer on this platform is.**
    ///
    /// Not derived, because the honest answer differs: on Unix an unannounced
    /// peer is a stock libwayland client and speaks `SCM_RIGHTS`; on Windows
    /// there is no such thing as an out-of-band descriptor, so the only dialect
    /// a connection can possibly be speaking is the in-band one. Deriving
    /// `Default` on the first variant would have made every Windows connection
    /// start out claiming a mechanism the platform does not have.
    fn default() -> Self {
        #[cfg(unix)]
        {
            Self::OutOfBand
        }
        #[cfg(windows)]
        {
            Self::InBand
        }
    }
}

/// A file descriptor, borrowed. On Windows there is no such thing, so this is
/// the same refcounted handle as [`OwnedFd`].
#[cfg(windows)]
pub type RawFd = wc_handle::Handle;

/// A file descriptor, owned. On Windows, a typed handle to a named kernel
/// object — see the `wc-handle` crate docs for why it is named rather than
/// duplicated.
#[cfg(windows)]
pub type OwnedFd = wc_handle::Handle;

/// A file descriptor, borrowed for `'a`.
///
/// This is the type `wayland-scanner` emits into every generated protocol
/// crate's `write_event`/`write_request` signature, which is why it has to
/// exist on Windows with the same shape — a lifetime parameter and an
/// `as_raw_fd()` — even though nothing here is really borrowed. A
/// `wc_handle::Handle` is refcounted and carries its own lifetime, so the
/// lifetime parameter is inhabited by a `PhantomData` and exists to keep the
/// signature identical rather than to enforce anything.
///
/// Keeping the shape is what allows the scanner fork to be a five-line change
/// (`vendor/wayland-scanner`) instead of a rewrite of code generation.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct BorrowedFd<'a> {
    handle: wc_handle::Handle,
    _marker: std::marker::PhantomData<&'a ()>,
}

#[cfg(windows)]
impl<'a> BorrowedFd<'a> {
    /// Borrow a handle for `'a`.
    pub fn borrow(handle: &'a wc_handle::Handle) -> Self {
        Self { handle: handle.clone(), _marker: std::marker::PhantomData }
    }

    /// Convert to the owned form.
    ///
    /// Named `as_raw_fd` so that the call sites generated by `wayland-scanner`
    /// and the ones written by hand in `wayland-server` do not need a `cfg`.
    /// It is not a lie: on Unix `as_raw_fd()` also produces the [`RawProtoFd`]
    /// that `Message<_, RawProtoFd>` wants, and on Windows `RawFd` *is* the
    /// handle.
    #[allow(clippy::wrong_self_convention)]
    pub fn as_raw_fd(&self) -> RawFd {
        self.handle.clone()
    }
}

#[cfg(windows)]
impl<'a> From<&'a wc_handle::Handle> for BorrowedFd<'a> {
    fn from(handle: &'a wc_handle::Handle) -> Self {
        Self::borrow(handle)
    }
}

#[cfg(windows)]
impl FdRepr for BorrowedFd<'_> {
    fn fd_eq(&self, other: &Self) -> bool {
        self.handle == other.handle
    }
    fn fd_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.handle.descriptor())
    }
}

/// "This thing can lend out a descriptor", the Windows counterpart of
/// `std::os::fd::AsFd`.
///
/// ADDED FOR STAGE 2. Upstream `wayland-backend` never needed this because on
/// Unix `AsFd` comes from `std`. It exists here because smithay's port needs
/// one spelling that works on both platforms: `wayland/shm/pool.rs`,
/// `utils/sealed_file.rs`, `input/keyboard/keymap_file.rs` and the clipboard
/// code all take an `impl AsFd` for what is, on this platform, a named kernel
/// object rather than a descriptor.
///
/// Note what is deliberately *not* here: no `AsRawFd`, and no blanket impl over
/// `Deref`. On Unix `AsFd` and `AsRawFd` differ in whether the caller gets an
/// ownership guarantee; on Windows the handle is refcounted, so the two would
/// collapse into the same method and the distinction would silently stop
/// meaning anything. A call site that genuinely wants the raw form should say
/// so with a `cfg` and be visible in the diff.
#[cfg(windows)]
pub trait AsFd {
    /// Borrow the descriptor.
    fn as_fd(&self) -> BorrowedFd<'_>;
}

#[cfg(windows)]
impl AsFd for wc_handle::Handle {
    fn as_fd(&self) -> BorrowedFd<'_> {
        BorrowedFd::borrow(self)
    }
}

#[cfg(windows)]
impl AsFd for BorrowedFd<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        BorrowedFd { handle: self.handle.clone(), _marker: std::marker::PhantomData }
    }
}

#[cfg(windows)]
impl<T: AsFd + ?Sized> AsFd for &T {
    fn as_fd(&self) -> BorrowedFd<'_> {
        T::as_fd(self)
    }
}

/// A live, read-only view of a descriptor's memory.
///
/// ADDED FOR STAGE 2. On Unix a compositor maps a `wl_shm_pool` with `mmap` and
/// holds nothing but a pointer, because the fd it mapped is a separate object
/// it already owns. On Windows the mapping *is* the object reference — the
/// `MapViewOfFile` result has to stay alive for the pointer to stay valid — so
/// the type has to be nameable by whoever holds the pointer. Re-exported here
/// so that a consumer (smithay's `wayland/shm/pool.rs`) can name it without
/// depending on `wc-handle` directly.
///
/// There is deliberately no writable counterpart; see `wc_handle::Handle::map`
/// for why a compositor reading client buffers should not have one by default.
///
/// §79 made this unconditional. It was Windows-only while a named object was a
/// Windows-only thing; a Linux compositor behind `wc-proxy` now receives
/// [`WireFd::Named`] and maps it exactly the same way.
pub use wc_handle::Mapping;

/// The named kernel object a [`WireFd::Named`] carries.
///
/// Re-exported so that a consumer building one — smithay's `SealedFile`, which
/// has to mint a keymap an in-band peer can open — can name the type without
/// depending on `wc-handle` directly. §80.
pub use wc_handle::Handle;

/// The socket type the transport is built on.
pub use wc_transport::Stream as WlStream;

/// What the server backend waits on.
///
/// Upstream has exactly two, chosen by `cfg`: an epoll fd on Linux and a kqueue
/// fd on the BSDs, with **no third path** — the research (§1.1) calls this out
/// as one of the blockers. This adds the third.
///
/// `polling` is the right choice for it rather than raw `CreateIoCompletionPort`
/// for a reason beyond convenience: `calloop` — which the research identifies
/// (§1.2) as the one crate in the smithay stack that already builds for Windows
/// — is itself built on `polling`. Using the same crate here means the
/// compositor's event loop and the backend's dispatcher agree about what a
/// readiness source is, instead of the backend inventing a second answer that
/// would have to be bridged later.
///
/// wepoll supports level-triggered mode (`supports_level()` is unconditionally
/// `true` in `polling`'s IOCP backend), which is what makes this a faithful
/// substitution: upstream registers clients with `EventFlags::IN`, level
/// triggered, and re-arming semantics would have changed the dispatch loop.
#[cfg(unix)]
pub(crate) type PollHandle = OwnedFd;

/// See the Unix definition above.
#[cfg(windows)]
pub(crate) type PollHandle = polling::Poller;

/// The handful of `errno` values this crate raises by name.
///
/// Upstream writes `rustix::io::Errno::PIPE.into()` and friends. Those are
/// preserved exactly on Unix — the resulting `io::Error` carries the real
/// `raw_os_error()`, and something downstream may be matching on it — and
/// mapped to the equivalent `ErrorKind` on Windows, where `rustix` provides no
/// `Errno` at all.
pub(crate) mod errors {
    use std::io::Error;
    #[cfg(windows)]
    use std::io::ErrorKind;

    /// The peer hung up.
    pub(crate) fn broken_pipe() -> Error {
        #[cfg(unix)]
        {
            rustix::io::Errno::PIPE.into()
        }
        #[cfg(windows)]
        {
            Error::from(ErrorKind::BrokenPipe)
        }
    }

    /// The peer sent something that is not a well-formed message.
    pub(crate) fn protocol() -> Error {
        #[cfg(unix)]
        {
            rustix::io::Errno::PROTO.into()
        }
        #[cfg(windows)]
        {
            Error::from(ErrorKind::InvalidData)
        }
    }

    /// A single message will not fit in a buffer of any size we will grow to.
    pub(crate) fn too_big() -> Error {
        #[cfg(unix)]
        {
            rustix::io::Errno::TOOBIG.into()
        }
        #[cfg(windows)]
        {
            Error::from(ErrorKind::InvalidInput)
        }
    }
}

/// What the protocol layer needs from whatever stands in for a file descriptor.
///
/// This replaces upstream's `Fd: AsRawFd` bounds on `PartialEq`, `Eq` and
/// `Display` for [`Argument`][crate::protocol::Argument] and
/// [`Message`][crate::protocol::Message]. Those bounds were never about file
/// descriptors as such — they exist so that two messages can be compared and so
/// that `WAYLAND_DEBUG=1` can print something. Naming that requirement directly
/// is what makes the type parameter genuinely portable instead of merely
/// looking portable.
pub trait FdRepr {
    /// Whether two handles refer to the same underlying object.
    ///
    /// Note that the Unix and Windows answers differ in strength, and the
    /// Windows one is the stronger. On Unix this compares raw fd *numbers*, so
    /// two `dup()`s of one file compare unequal. On Windows it compares the
    /// descriptor, so two handles to one section compare equal. Nothing in the
    /// crate depends on the weaker behaviour; it is called out because a test
    /// that passes on one platform and fails on the other would otherwise be a
    /// mystery.
    fn fd_eq(&self, other: &Self) -> bool;

    /// How this handle appears in a `WAYLAND_DEBUG` trace.
    fn fd_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
}

#[cfg(unix)]
impl<T: AsRawFd> FdRepr for T {
    fn fd_eq(&self, other: &Self) -> bool {
        self.as_raw_fd() == other.as_raw_fd()
    }
    fn fd_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_raw_fd())
    }
}

#[cfg(windows)]
impl FdRepr for wc_handle::Handle {
    fn fd_eq(&self, other: &Self) -> bool {
        self == other
    }
    fn fd_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.descriptor())
    }
}

/// The three libwayland C ABI structs that appear in this crate's public API.
///
/// `protocol.rs` re-exports `wl_argument`, `wl_interface` and `wl_message`, and
/// [`Interface`][crate::protocol::Interface] carries a
/// `c_ptr: Option<&'static wl_interface>` so that a generated protocol crate can
/// hand the same interface description to either backend. That is public API and
/// every generated crate in the ecosystem mentions it, so it has to keep
/// existing on Windows even though nothing on Windows can consume it.
///
/// On Unix these come from `wayland-sys`. On Windows they are declared here
/// instead, for a reason that is worth recording because it is smaller than it
/// looks: `wayland-sys` is only *linked* when its `client`/`server` features are
/// on — with no features its build script does nothing at all. The single thing
/// that stops it compiling for Windows is `use std::os::unix::io::RawFd` at the
/// top of its `common.rs`, for the `h` member of a union that the rust backend
/// never reads. Twenty lines of struct definitions is a smaller and more honest
/// dependency than a second fork.
pub mod c_abi {
    #[cfg(unix)]
    pub use wayland_sys::common::{wl_argument, wl_interface, wl_message};

    #[cfg(windows)]
    mod defs {
        use std::os::raw::{c_char, c_int, c_void};

        /// libwayland's description of one request or event.
        #[repr(C)]
        #[derive(Debug)]
        pub struct wl_message {
            /// Interface or message name, NUL-terminated.
            pub name: *const c_char,
            /// libwayland signature string, e.g. `"uo"`.
            pub signature: *const c_char,
            /// Interfaces of the object-typed arguments.
            pub types: *const *const wl_interface,
        }

        /// libwayland's description of one interface.
        #[repr(C)]
        pub struct wl_interface {
            /// Interface or message name, NUL-terminated.
            pub name: *const c_char,
            /// Latest supported version of this interface.
            pub version: c_int,
            /// Number of entries in `requests`.
            pub request_count: c_int,
            /// Request descriptions, indexed by opcode.
            pub requests: *const wl_message,
            /// Number of entries in `events`.
            pub event_count: c_int,
            /// Event descriptions, indexed by opcode.
            pub events: *const wl_message,
        }

        impl std::fmt::Debug for wl_interface {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "wl_interface@{self:p}")
            }
        }

        // SAFETY: identical to the reasoning in `wayland-sys`: these are
        // immutable statics generated at compile time and never mutated.
        unsafe impl Send for wl_interface {}
        unsafe impl Sync for wl_interface {}

        /// libwayland's tagged-union argument slot.
        ///
        /// `h` is `i32` here where `wayland-sys` writes `RawFd`. They are the
        /// same type on Unix; on Windows there is no `RawFd`, and since nothing
        /// in the rust backend ever reads this union — it exists so the layout
        /// matches what C would produce — the width is all that matters.
        #[repr(C)]
        pub union wl_argument {
            /// `i` — signed integer.
            pub i: i32,
            /// `u` — unsigned integer.
            pub u: u32,
            /// `f` — 24.8 fixed point.
            pub f: i32,
            /// `s` — NUL-terminated string.
            pub s: *const c_char,
            /// `o` — object.
            pub o: *const c_void,
            /// `n` — new id.
            pub n: u32,
            /// `a` — byte array.
            pub a: *const c_void,
            /// `h` — file descriptor. Never populated on Windows.
            pub h: i32,
        }

        impl std::fmt::Debug for wl_argument {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                // A union has no discriminant, so there is nothing truthful to
                // print. Printing one arm and calling it the value would be a
                // lie in a `Debug` impl.
                f.write_str("wl_argument")
            }
        }
    }

    #[cfg(windows)]
    pub use defs::{wl_argument, wl_interface, wl_message};
}

/// Tests for [`WireFd`], which exists before anything constructs it.
///
/// **The point of these is the first one.** `docs/waycompose-design.md` §72.3
/// describes the remaining work as "a cfg becoming a runtime choice rather than
/// a redesign", and that sentence is only true if the type the choice produces
/// is already a legal `Argument`/`Message` payload. `wire_fd_is_already_a_legal_message_payload`
/// is that claim compiled: if it builds, `ProtoFd = WireFd` is a substitution.
/// If `WireFd` had to grow a bound the protocol layer could not supply, this is
/// where it would be found — before the wire format depends on it, rather than
/// halfway through changing it.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Argument, Message};
    use wc_handle::{Descriptor, Handle};

    fn section(name: &str) -> Handle {
        Handle::from_descriptor(Descriptor::Section { name: name.into(), size: 4096 })
    }

    #[test]
    fn wire_fd_is_already_a_legal_message_payload() {
        // Everything the protocol layer asks of an `Fd`: it goes in an
        // `Argument`, the `Argument` goes in a `Message`, and two of them can be
        // compared and printed. That is the whole of `FdRepr`'s contract.
        let msg: Message<u32, WireFd> = Message {
            sender_id: 3,
            opcode: 0,
            args: smallvec::smallvec![
                Argument::Uint(7),
                Argument::Fd(WireFd::Named(section("wc-pool-a"))),
            ],
        };
        let same: Message<u32, WireFd> = Message {
            sender_id: 3,
            opcode: 0,
            args: smallvec::smallvec![
                Argument::Uint(7),
                Argument::Fd(WireFd::Named(section("wc-pool-a"))),
            ],
        };
        assert_eq!(msg, same);
        assert!(format!("{}", msg.args[1]).contains("wc-pool-a"));
    }

    #[test]
    fn two_handles_to_one_section_are_equal() {
        // The strong answer: `Named` compares the name, so two references to one
        // section are the same `h` argument even though they are two objects.
        assert!(WireFd::Named(section("wc-pool-b")).fd_eq(&WireFd::Named(section("wc-pool-b"))));
        assert!(!WireFd::Named(section("wc-pool-b")).fd_eq(&WireFd::Named(section("wc-pool-c"))));
    }

    #[cfg(unix)]
    #[test]
    fn two_dups_of_one_file_are_not_equal_and_that_is_upstream_s_answer() {
        // The weak answer, and it is inherited rather than chosen: upstream
        // compares `as_raw_fd()` values, so a `dup()` is a different `h`. Held
        // here so that the asymmetry between the two arms is asserted somewhere
        // rather than only described.
        let a = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        let b = a.try_clone().unwrap();
        assert_ne!(a.as_raw_fd(), b.as_raw_fd());
        assert!(!WireFd::Open(a).fd_eq(&WireFd::Open(b)));
    }

    #[cfg(unix)]
    #[test]
    fn a_name_and_an_open_descriptor_are_never_equal() {
        let named = WireFd::Named(section("wc-pool-d"));
        let open = WireFd::Open(OwnedFd::from(std::fs::File::open("/dev/null").unwrap()));
        assert!(!named.fd_eq(&open));
        assert!(!open.fd_eq(&named));
    }

    #[test]
    fn a_named_h_has_a_descriptor_to_give() {
        let named = WireFd::Named(section("wc-pool-e"));
        assert!(matches!(named.descriptor(), Some(Descriptor::Section { name, .. }) if name == "wc-pool-e"));
    }

    #[cfg(unix)]
    #[test]
    fn an_open_h_has_no_name_and_says_so() {
        // Not a failure and not a placeholder: an `SCM_RIGHTS` fd was never
        // created under a name, so `None` is the true answer. This is the whole
        // reason the disjunction is here and not a sixth `Descriptor` variant.
        let open = WireFd::Open(OwnedFd::from(std::fs::File::open("/dev/null").unwrap()));
        assert!(open.descriptor().is_none());
        assert!(open.as_open().is_some());
        assert!(WireFd::Named(section("wc-pool-f")).as_open().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_trace_can_tell_the_two_arms_apart() {
        // `WAYLAND_DEBUG=1` prints these, and a trace in which both dialects
        // rendered the same way would be worse than no trace: the encoding is
        // precisely what one would be reading it to find out.
        let open = WireFd::Open(OwnedFd::from(std::fs::File::open("/dev/null").unwrap()));
        let raw = open.as_open().unwrap().as_raw_fd();
        assert_eq!(format!("{}", Argument::<u32, WireFd>::Fd(open)), format!("{raw}"));
        let named = format!("{}", Argument::<u32, WireFd>::Fd(WireFd::Named(section("wc-pool-g"))));
        assert!(named.contains("Section"), "a named h should print its descriptor, got {named}");
    }
}
