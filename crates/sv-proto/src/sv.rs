//! docs/design.md §6.3's non-Vulkan commands — "hand-written, not generated, and …
//! the only place the protocol is ours rather than Vulkan's."
//!
//! # Why they are opcodes and not a second framing
//!
//! A ring packet is already `{header, payload}` with a `u32 cmd_type` at the
//! front of the payload, and the ring cross-checks the framing length against
//! that header's `cmd_size` (§6.2 deviation 1). Giving these commands their own
//! packet *kind* would put protocol semantics in the transport, which is the one
//! layering rule §4.4 keeps: the ring carries descriptors and does not know what
//! is in them. Giving them opcodes above the generated range costs nothing and
//! keeps every framing check that already exists.
//!
//! The generated `Opcode::from_u32` returns `None` for these, which is exactly
//! right: `sv_proto::dispatch` is the *Vulkan* dispatcher and one of these
//! reaching it is a bug. The consumer looks at the opcode before dispatching and
//! routes on [`is_protocol`].
//!
//! # Numbering
//!
//! The profile's opcodes are dense from zero — 239 of them today — and the
//! numbering is "not stable against the profile gaining or losing a command",
//! which is safe because the profile is in the protocol hash. Ours are at
//! `0x8000_0000` and up, chosen so that no plausible profile ever reaches them
//! and so that a stray Vulkan opcode read as one of ours is impossible rather
//! than unlikely.

use sv_wire::{CommandHeader, Decoder, Encoder, Result, WireError};

/// The first opcode that is ours rather than Vulkan's.
pub const PROTOCOL_BASE: u32 = 0x8000_0000;

/// docs/design.md §6.3: "Hand a recorded `vkCmd*` blob at `(offset,len)` to the
/// sub-decoder."
pub const SV_EXECUTE_COMMAND_BLOB: u32 = PROTOCOL_BASE;

/// docs/design.md §6.3: "Slab import, with the memory-type reply."
pub const SV_IMPORT_ARENA_SLAB: u32 = PROTOCOL_BASE + 1;

/// docs/design.md §0.18: "which Vulkan version is behind you?", asked once at connect.
///
/// # Why this is a ring command and not a field of the cold-path `Attachment`
///
/// The obvious place for a fact the GPU side knows and the app side needs is the
/// handshake message, and it was tried there first. It does not work, for two
/// reasons that are both about *ordering* rather than about taste:
///
/// - On the cross-VM path the guest sends its `Hello` **before it opens the
///   driver**, deliberately — "bound before the driver is opened so that a host
///   waiting on the port sees it come up promptly" (`sv_gpu_server::entry`).
///   At `Hello` time there is no `apiVersion` to report.
/// - On the `sv-hostd` path the daemon that sends the `Attachment` is not the
///   side holding the driver at all. In `Mode::Local` the GPU side is one of its
///   *clients*, so the daemon has nothing to relay.
///
/// A ring command has neither problem: by the time any ring is served the GPU
/// side has a `Driver`, and it is the same side that answers every other
/// question about the device. It is the pattern
/// `sv_icd::state::SharedVulkan::ask_device_extensions` already uses, for the
/// same reason — the answer belongs to whoever holds the driver.
pub const SV_DRIVER_VERSIONS: u32 = PROTOCOL_BASE + 2;

/// Is this opcode ours?
pub fn is_protocol(cmd_type: u32) -> bool {
    cmd_type >= PROTOCOL_BASE
}

/// Peek the opcode of an encoded command without consuming the decoder.
///
/// The consumer needs it before it can decide between [`is_protocol`] and
/// `sv_proto::dispatch`, and a peek is cheaper and less error-prone than
/// decoding a header and putting it back.
pub fn peek_opcode(payload: &[u8]) -> Option<u32> {
    payload.get(0..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Did the sender ask for a reply?
///
/// The GPU side's dispatcher now *always* produces one, so that a fire-and-
/// forget command's `VkResult` can still be looked at, and this is what decides
/// whether it is sent back. It is a peek at `cmd_flags` for the same reason
/// [`peek_opcode`] is a peek: the consumer has to know before it decodes.
pub fn peek_flags(payload: &[u8]) -> Option<u32> {
    payload.get(4..8).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Does this packet want its reply sent back?
pub fn wants_reply(payload: &[u8]) -> bool {
    peek_flags(payload).is_some_and(|f| f & sv_wire::CMD_FLAG_GENERATE_REPLY != 0)
}

/// `svExecuteCommandBlob`: replay a recorded command buffer out of the arena.
///
/// `command_buffer` is the app-side id; `arena_offset` and `len` name the bytes
/// the app side recorded into. The blob's own contents are a sequence of
/// `sv-wire` commands and are decoded by the restricted sub-decoder, which
/// accepts only the opcodes recording is allowed to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExecuteCommandBlob {
    pub command_buffer: u64,
    pub arena_offset: u64,
    pub len: u64,
}

/// `svImportArenaSlab`: ask the GPU side to import one slab (docs/design.md §5.2).
///
/// > Slabs are **imported** lazily. When the app side needs a slab it picks a
/// > free region of the arena, writes a slab-directory entry, and sends
/// > `svImportArenaSlab`. The GPU side imports it and replies.
///
/// It generates a reply because the memory type is the GPU side's answer and the
/// app side cannot proceed without it — this is the one place the arena costs a
/// round trip, and it is once per 64 MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportArenaSlab {
    pub slab: u32,
    pub arena_offset: u64,
    pub len: u64,
}

/// What the GPU side answers an [`ImportArenaSlab`] with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportArenaSlabReply {
    /// `VK_SUCCESS`, or the `VkResult` the import failed with.
    pub result: i32,
    pub memory_type: u32,
}

/// `svDriverVersions`: ask the GPU side what Vulkan its driver actually is.
///
/// No body. The GPU side has exactly one driver and one physical device
/// (docs/design.md §8), so there is nothing to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DriverVersions;

/// What the GPU side answers a [`DriverVersions`] with.
///
/// Both numbers are the driver's own, unclamped and un-negotiated: **this
/// message reports, it does not decide**. Clamping them to what the wire can
/// carry is the app side's job and happens in `sv_icd::state::negotiate`, which
/// is where the decision can be printed once instead of being made silently in
/// two places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DriverVersionsReply {
    /// `vkEnumerateInstanceVersion` on the GPU side's loader — what the driver
    /// stack over there can offer an *instance*.
    pub instance_api_version: u32,
    /// `VkPhysicalDeviceProperties::apiVersion` of the physical device the GPU
    /// side opened — the number `vulkaninfo` prints per device.
    pub device_api_version: u32,
}

/// Header, the `u32` that follows it, and three `u64`s. The `u32` is not
/// padding for its own sake: `COMMAND_HEADER_SIZE` is twelve, so without it
/// every `u64` in every one of these bodies would be four-byte aligned.
const BLOB_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4 + 24;
const IMPORT_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4 + 24;
const IMPORT_REPLY_SIZE: usize = sv_wire::REPLY_HEADER_SIZE + 8;
/// Header and the `u32` that keeps the body four-byte aligned; there is no body.
const VERSIONS_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4;
const VERSIONS_REPLY_SIZE: usize = sv_wire::REPLY_HEADER_SIZE + 8;

impl ExecuteCommandBlob {
    pub fn encoded_len() -> usize {
        BLOB_SIZE
    }

    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        out.resize(BLOB_SIZE, 0);
        let mut enc = Encoder::new(out);
        CommandHeader {
            cmd_type: SV_EXECUTE_COMMAND_BLOB,
            cmd_flags: 0,
            cmd_size: BLOB_SIZE as u32,
        }
        .encode(&mut enc)?;
        enc.u32(0)?; // pad, so the three u64s are eight-byte aligned
        enc.u64(self.command_buffer)?;
        enc.u64(self.arena_offset)?;
        enc.u64(self.len)?;
        enc.finish()?;
        Ok(())
    }

    /// Decode a whole packet payload, header included.
    pub fn decode(payload: &[u8]) -> Result<ExecuteCommandBlob> {
        let mut dec = Decoder::new(payload);
        let h = CommandHeader::decode(&mut dec)?;
        if h.cmd_type != SV_EXECUTE_COMMAND_BLOB {
            return Err(WireError::UnknownCommand { opcode: h.cmd_type });
        }
        let _pad = dec.u32()?;
        Ok(ExecuteCommandBlob {
            command_buffer: dec.u64()?,
            arena_offset: dec.u64()?,
            len: dec.u64()?,
        })
    }
}

impl ImportArenaSlab {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        out.resize(IMPORT_SIZE, 0);
        let mut enc = Encoder::new(out);
        CommandHeader {
            cmd_type: SV_IMPORT_ARENA_SLAB,
            cmd_flags: sv_wire::CMD_FLAG_GENERATE_REPLY,
            cmd_size: IMPORT_SIZE as u32,
        }
        .encode(&mut enc)?;
        enc.u32(self.slab)?;
        enc.u64(self.arena_offset)?;
        enc.u64(self.len)?;
        enc.u64(0)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<ImportArenaSlab> {
        let mut dec = Decoder::new(payload);
        let h = CommandHeader::decode(&mut dec)?;
        if h.cmd_type != SV_IMPORT_ARENA_SLAB {
            return Err(WireError::UnknownCommand { opcode: h.cmd_type });
        }
        let slab = dec.u32()?;
        let arena_offset = dec.u64()?;
        let len = dec.u64()?;
        let _pad = dec.u64()?;
        Ok(ImportArenaSlab { slab, arena_offset, len })
    }
}

impl ImportArenaSlabReply {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        out.resize(IMPORT_REPLY_SIZE, 0);
        let mut enc = Encoder::new(out);
        sv_wire::ReplyHeader {
            cmd_type: SV_IMPORT_ARENA_SLAB,
            reply_size: IMPORT_REPLY_SIZE as u32,
        }
        .encode(&mut enc)?;
        enc.i32(self.result)?;
        enc.u32(self.memory_type)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<ImportArenaSlabReply> {
        let mut dec = Decoder::new(payload);
        let h = sv_wire::ReplyHeader::decode(&mut dec)?;
        if h.cmd_type != SV_IMPORT_ARENA_SLAB {
            return Err(WireError::UnknownCommand { opcode: h.cmd_type });
        }
        Ok(ImportArenaSlabReply { result: dec.i32()?, memory_type: dec.u32()? })
    }
}

impl DriverVersions {
    pub fn encoded_len() -> usize {
        VERSIONS_SIZE
    }

    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        out.resize(VERSIONS_SIZE, 0);
        let mut enc = Encoder::new(out);
        CommandHeader {
            cmd_type: SV_DRIVER_VERSIONS,
            cmd_flags: sv_wire::CMD_FLAG_GENERATE_REPLY,
            cmd_size: VERSIONS_SIZE as u32,
        }
        .encode(&mut enc)?;
        enc.u32(0)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<DriverVersions> {
        let mut dec = Decoder::new(payload);
        let h = CommandHeader::decode(&mut dec)?;
        if h.cmd_type != SV_DRIVER_VERSIONS {
            return Err(WireError::UnknownCommand { opcode: h.cmd_type });
        }
        let _pad = dec.u32()?;
        Ok(DriverVersions)
    }
}

impl DriverVersionsReply {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        out.resize(VERSIONS_REPLY_SIZE, 0);
        let mut enc = Encoder::new(out);
        sv_wire::ReplyHeader {
            cmd_type: SV_DRIVER_VERSIONS,
            reply_size: VERSIONS_REPLY_SIZE as u32,
        }
        .encode(&mut enc)?;
        enc.u32(self.instance_api_version)?;
        enc.u32(self.device_api_version)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<DriverVersionsReply> {
        let mut dec = Decoder::new(payload);
        let h = sv_wire::ReplyHeader::decode(&mut dec)?;
        if h.cmd_type != SV_DRIVER_VERSIONS {
            return Err(WireError::UnknownCommand { opcode: h.cmd_type });
        }
        Ok(DriverVersionsReply {
            instance_api_version: dec.u32()?,
            device_api_version: dec.u32()?,
        })
    }
}


/// `svArenaInfo`: how this session's arena is provided (waycompose-design.md
/// §97). Asked once, at attach, on the instance ring.
///
/// The answer is either **fixed** — the arena is one window the app side was
/// handed at attach and allocates from with `svImportArenaSlab`, as every path
/// did before §97 — or **leased**: the arena is the GPU side's pool, and the
/// app side asks for ranges of it with [`SV_LEASE_ARENA`] and gives them back
/// with [`SV_RELEASE_ARENA`]. The question carries the application's name,
/// which is only ever used in the GPU side's log of who holds what.
pub const SV_ARENA_INFO: u32 = PROTOCOL_BASE + 3;

/// `svLeaseArena`: lease a range of the pool, imported for this session's
/// device, with the memory type it was imported as (§97).
pub const SV_LEASE_ARENA: u32 = PROTOCOL_BASE + 4;

/// `svReleaseArena`: give a lease back (§97). The GPU side frees its import
/// first, so a range is never in another session's hands while this
/// session's device still has it.
pub const SV_RELEASE_ARENA: u32 = PROTOCOL_BASE + 5;

/// The application name an [`ArenaInfo`] carries: 32 bytes, NUL-padded.
pub const ARENA_NAME_LEN: usize = 32;

/// [`ArenaInfoReply::mode`]: the arena is a fixed window.
pub const ARENA_MODE_FIXED: u32 = 0;
/// [`ArenaInfoReply::mode`]: the arena is leased from the GPU side's pool.
pub const ARENA_MODE_LEASED: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArenaInfo {
    pub name: [u8; ARENA_NAME_LEN],
}

impl ArenaInfo {
    /// The name, cut to fit.
    pub fn named(name: &str) -> ArenaInfo {
        let mut n = [0u8; ARENA_NAME_LEN];
        let b = name.as_bytes();
        let len = b.len().min(ARENA_NAME_LEN);
        n[..len].copy_from_slice(&b[..len]);
        ArenaInfo { name: n }
    }

    /// The name as text, lossily, without its padding.
    pub fn name_str(&self) -> String {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(ARENA_NAME_LEN);
        String::from_utf8_lossy(&self.name[..end]).into_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArenaInfoReply {
    /// [`ARENA_MODE_FIXED`] or [`ARENA_MODE_LEASED`].
    pub mode: u32,
    /// Every lease's length and alignment is a multiple of this.
    pub lease_granule: u64,
    /// The most this session may lease, control region excepted.
    pub session_cap: u64,
    /// The pool's size now, and the most it can grow to.
    pub capacity: u64,
    pub max_capacity: u64,
}

/// What a lease is for; [`LeaseArena::purpose`]. The same numbers as
/// `sv_arena::pool::Purpose`.
pub const LEASE_BLOBS: u32 = 1;
pub const LEASE_CHUNK: u32 = 2;
pub const LEASE_DEDICATED: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LeaseArena {
    pub len: u64,
    pub purpose: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LeaseArenaReply {
    /// `VK_SUCCESS`, or `VK_ERROR_OUT_OF_DEVICE_MEMORY` (-2) when the pool
    /// cannot serve it, even after growing.
    pub result: i32,
    pub memory_type: u32,
    /// The pool offset of the lease.
    pub offset: u64,
    pub len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReleaseArena {
    pub offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReleaseArenaReply {
    pub result: i32,
}

const INFO_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4 + ARENA_NAME_LEN;
const INFO_REPLY_SIZE: usize = sv_wire::REPLY_HEADER_SIZE + 8 + 32;
const LEASE_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4 + 16;
const LEASE_REPLY_SIZE: usize = sv_wire::REPLY_HEADER_SIZE + 8 + 16;
const RELEASE_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4 + 8;
const RELEASE_REPLY_SIZE: usize = sv_wire::REPLY_HEADER_SIZE + 8;

fn command(out: &mut Vec<u8>, cmd_type: u32, size: usize) -> Result<Encoder<'_>> {
    out.clear();
    out.resize(size, 0);
    let mut enc = Encoder::new(out);
    CommandHeader { cmd_type, cmd_flags: sv_wire::CMD_FLAG_GENERATE_REPLY, cmd_size: size as u32 }
        .encode(&mut enc)?;
    Ok(enc)
}

fn reply(out: &mut Vec<u8>, cmd_type: u32, size: usize) -> Result<Encoder<'_>> {
    out.clear();
    out.resize(size, 0);
    let mut enc = Encoder::new(out);
    sv_wire::ReplyHeader { cmd_type, reply_size: size as u32 }.encode(&mut enc)?;
    Ok(enc)
}

fn expect_command<'a>(payload: &'a [u8], cmd_type: u32) -> Result<Decoder<'a>> {
    let mut dec = Decoder::new(payload);
    let h = CommandHeader::decode(&mut dec)?;
    if h.cmd_type != cmd_type {
        return Err(WireError::UnknownCommand { opcode: h.cmd_type });
    }
    Ok(dec)
}

fn expect_reply<'a>(payload: &'a [u8], cmd_type: u32) -> Result<Decoder<'a>> {
    let mut dec = Decoder::new(payload);
    let h = sv_wire::ReplyHeader::decode(&mut dec)?;
    if h.cmd_type != cmd_type {
        return Err(WireError::UnknownCommand { opcode: h.cmd_type });
    }
    Ok(dec)
}

impl ArenaInfo {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = command(out, SV_ARENA_INFO, INFO_SIZE)?;
        enc.u32(0)?;
        for w in self.name.chunks_exact(4) {
            enc.u32(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))?;
        }
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<ArenaInfo> {
        let mut dec = expect_command(payload, SV_ARENA_INFO)?;
        let _pad = dec.u32()?;
        let mut name = [0u8; ARENA_NAME_LEN];
        for w in name.chunks_exact_mut(4) {
            w.copy_from_slice(&dec.u32()?.to_le_bytes());
        }
        Ok(ArenaInfo { name })
    }
}

impl ArenaInfoReply {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = reply(out, SV_ARENA_INFO, INFO_REPLY_SIZE)?;
        enc.u32(self.mode)?;
        enc.u32(0)?;
        enc.u64(self.lease_granule)?;
        enc.u64(self.session_cap)?;
        enc.u64(self.capacity)?;
        enc.u64(self.max_capacity)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<ArenaInfoReply> {
        let mut dec = expect_reply(payload, SV_ARENA_INFO)?;
        let mode = dec.u32()?;
        let _pad = dec.u32()?;
        Ok(ArenaInfoReply {
            mode,
            lease_granule: dec.u64()?,
            session_cap: dec.u64()?,
            capacity: dec.u64()?,
            max_capacity: dec.u64()?,
        })
    }
}

impl LeaseArena {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = command(out, SV_LEASE_ARENA, LEASE_SIZE)?;
        enc.u32(self.purpose)?;
        enc.u64(self.len)?;
        enc.u64(0)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<LeaseArena> {
        let mut dec = expect_command(payload, SV_LEASE_ARENA)?;
        let purpose = dec.u32()?;
        let len = dec.u64()?;
        let _pad = dec.u64()?;
        Ok(LeaseArena { len, purpose })
    }
}

impl LeaseArenaReply {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = reply(out, SV_LEASE_ARENA, LEASE_REPLY_SIZE)?;
        enc.i32(self.result)?;
        enc.u32(self.memory_type)?;
        enc.u64(self.offset)?;
        enc.u64(self.len)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<LeaseArenaReply> {
        let mut dec = expect_reply(payload, SV_LEASE_ARENA)?;
        Ok(LeaseArenaReply { result: dec.i32()?, memory_type: dec.u32()?, offset: dec.u64()?, len: dec.u64()? })
    }
}

impl ReleaseArena {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = command(out, SV_RELEASE_ARENA, RELEASE_SIZE)?;
        enc.u32(0)?;
        enc.u64(self.offset)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<ReleaseArena> {
        let mut dec = expect_command(payload, SV_RELEASE_ARENA)?;
        let _pad = dec.u32()?;
        Ok(ReleaseArena { offset: dec.u64()? })
    }
}

impl ReleaseArenaReply {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = reply(out, SV_RELEASE_ARENA, RELEASE_REPLY_SIZE)?;
        enc.i32(self.result)?;
        enc.u32(0)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<ReleaseArenaReply> {
        let mut dec = expect_reply(payload, SV_RELEASE_ARENA)?;
        Ok(ReleaseArenaReply { result: dec.i32()? })
    }
}

/// `svAllocShared`: a `VkDeviceMemory` whose pages are a named shared section
/// the GPU side creates, maps and imports as a host pointer (the dma-buf path
/// of waycompose-design.md §104). The app-side memory id is chosen by the app
/// side, as for every object (§6.4); the GPU side registers its import under it.
pub const SV_ALLOC_SHARED: u32 = PROTOCOL_BASE + 6;

/// `svImportShared`: import a section this GPU process created earlier, named
/// by a token's name and nonce, under a new app-side memory id.
pub const SV_IMPORT_SHARED: u32 = PROTOCOL_BASE + 7;

/// The longest section name either command carries; `sv_drm::token::MAX_NAME`.
pub const SHARED_NAME_LEN: usize = 200;

/// A section name on the wire: its length, then [`SHARED_NAME_LEN`] bytes,
/// NUL-padded, as `u32` words (the [`ArenaInfo::name`] precedent).
fn put_name(enc: &mut Encoder<'_>, name: &str) -> Result<()> {
    let b = name.as_bytes();
    if b.len() > SHARED_NAME_LEN {
        return Err(WireError::Truncated { need: b.len(), have: SHARED_NAME_LEN });
    }
    let mut buf = [0u8; SHARED_NAME_LEN];
    buf[..b.len()].copy_from_slice(b);
    enc.u32(b.len() as u32)?;
    for w in buf.chunks_exact(4) {
        enc.u32(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))?;
    }
    Ok(())
}

fn get_name(dec: &mut Decoder<'_>) -> Result<String> {
    let len = dec.u32()? as usize;
    let mut buf = [0u8; SHARED_NAME_LEN];
    for w in buf.chunks_exact_mut(4) {
        w.copy_from_slice(&dec.u32()?.to_le_bytes());
    }
    if len > SHARED_NAME_LEN {
        return Err(WireError::Truncated { need: len, have: SHARED_NAME_LEN });
    }
    // A name that is not UTF-8 is refused by whoever checks names; carrying it
    // lossily would make two different names compare equal.
    String::from_utf8(buf[..len].to_vec()).map_err(|_| WireError::Truncated { need: len, have: 0 })
}

fn put_nonce(enc: &mut Encoder<'_>, n: &[u8; 16]) -> Result<()> {
    for w in n.chunks_exact(4) {
        enc.u32(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))?;
    }
    Ok(())
}

fn get_nonce(dec: &mut Decoder<'_>) -> Result<[u8; 16]> {
    let mut n = [0u8; 16];
    for w in n.chunks_exact_mut(4) {
        w.copy_from_slice(&dec.u32()?.to_le_bytes());
    }
    Ok(n)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AllocShared {
    /// The app-side `VkDeviceMemory` id the import is registered under.
    pub memory: u64,
    /// `allocationSize`; the GPU side rounds it up to what an import needs.
    pub size: u64,
    /// The app-side `VkImage` the allocation is dedicated to, or 0: its
    /// requirements narrow the memory type and bound the size.
    pub image: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AllocSharedReply {
    /// `VK_SUCCESS`, or the `VkResult` the allocation failed with.
    pub result: i32,
    /// The driver memory type of the import, for logs.
    pub memory_type: u32,
    /// The section's size, a multiple of the import alignment.
    pub size: u64,
    pub nonce: [u8; 16],
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportShared {
    pub memory: u64,
    pub size: u64,
    pub image: u64,
    pub nonce: [u8; 16],
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportSharedReply {
    pub result: i32,
    pub memory_type: u32,
    pub size: u64,
}

const ALLOC_SHARED_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4 + 24;
const ALLOC_SHARED_REPLY_SIZE: usize = sv_wire::REPLY_HEADER_SIZE + 8 + 8 + 16 + 4 + SHARED_NAME_LEN + 4;
const IMPORT_SHARED_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4 + 24 + 16 + 4 + SHARED_NAME_LEN;
const IMPORT_SHARED_REPLY_SIZE: usize = sv_wire::REPLY_HEADER_SIZE + 8 + 8;

impl AllocShared {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = command(out, SV_ALLOC_SHARED, ALLOC_SHARED_SIZE)?;
        enc.u32(0)?;
        enc.u64(self.memory)?;
        enc.u64(self.size)?;
        enc.u64(self.image)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<AllocShared> {
        let mut dec = expect_command(payload, SV_ALLOC_SHARED)?;
        let _pad = dec.u32()?;
        Ok(AllocShared { memory: dec.u64()?, size: dec.u64()?, image: dec.u64()? })
    }
}

impl AllocSharedReply {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = reply(out, SV_ALLOC_SHARED, ALLOC_SHARED_REPLY_SIZE)?;
        enc.i32(self.result)?;
        enc.u32(self.memory_type)?;
        enc.u64(self.size)?;
        put_nonce(&mut enc, &self.nonce)?;
        put_name(&mut enc, &self.name)?;
        enc.u32(0)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<AllocSharedReply> {
        let mut dec = expect_reply(payload, SV_ALLOC_SHARED)?;
        let result = dec.i32()?;
        let memory_type = dec.u32()?;
        let size = dec.u64()?;
        let nonce = get_nonce(&mut dec)?;
        let name = get_name(&mut dec)?;
        let _pad = dec.u32()?;
        Ok(AllocSharedReply { result, memory_type, size, nonce, name })
    }
}

impl ImportShared {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = command(out, SV_IMPORT_SHARED, IMPORT_SHARED_SIZE)?;
        enc.u32(0)?;
        enc.u64(self.memory)?;
        enc.u64(self.size)?;
        enc.u64(self.image)?;
        put_nonce(&mut enc, &self.nonce)?;
        put_name(&mut enc, &self.name)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<ImportShared> {
        let mut dec = expect_command(payload, SV_IMPORT_SHARED)?;
        let _pad = dec.u32()?;
        let memory = dec.u64()?;
        let size = dec.u64()?;
        let image = dec.u64()?;
        let nonce = get_nonce(&mut dec)?;
        let name = get_name(&mut dec)?;
        Ok(ImportShared { memory, size, image, nonce, name })
    }
}

impl ImportSharedReply {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = reply(out, SV_IMPORT_SHARED, IMPORT_SHARED_REPLY_SIZE)?;
        enc.i32(self.result)?;
        enc.u32(self.memory_type)?;
        enc.u64(self.size)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<ImportSharedReply> {
        let mut dec = expect_reply(payload, SV_IMPORT_SHARED)?;
        Ok(ImportSharedReply { result: dec.i32()?, memory_type: dec.u32()?, size: dec.u64()? })
    }
}

/// `svFenceRecord` (§104.11): the name of this session's fence record, a
/// small section the GPU side creates on first use and writes "every marked
/// piece of work up to `n` has completed" into. Answered; asked once.
pub const SV_FENCE_RECORD: u32 = PROTOCOL_BASE + 8;

/// `svFenceMark` (§104.11): GPU work numbered `seq` is the work that last
/// signalled `semaphore` on `queue`; once it has completed, the record says
/// `completed >= seq`. `memory` is the shared allocation the work touched, or
/// 0. Sent on the queue ring after the submit, never answered: the export of a
/// sync fd returns at once.
pub const SV_FENCE_MARK: u32 = PROTOCOL_BASE + 9;

/// `svFenceWait` (§104.11): before the next command on this ring, wait until
/// the work that wrote the shared allocation `memory` has completed: the work
/// numbered `seq` in fence record `record` (another session's), and every
/// marked work of any other session on the same section that the GPU side has
/// already seen. Sent on the queue ring before a submit, never answered; the
/// wait is the GPU side's, on its own thread.
pub const SV_FENCE_WAIT: u32 = PROTOCOL_BASE + 10;

/// A command whose sender does not wait for a reply.
fn unanswered(out: &mut Vec<u8>, cmd_type: u32, size: usize) -> Result<Encoder<'_>> {
    out.clear();
    out.resize(size, 0);
    let mut enc = Encoder::new(out);
    CommandHeader { cmd_type, cmd_flags: 0, cmd_size: size as u32 }.encode(&mut enc)?;
    Ok(enc)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FenceRecord;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FenceRecordReply {
    /// `VK_SUCCESS`, or why there is no record (the dma-buf sync then falls
    /// back to waiting on the app side).
    pub result: i32,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FenceMark {
    pub queue: u64,
    pub semaphore: u64,
    pub seq: u64,
    pub memory: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FenceWait {
    pub memory: u64,
    pub seq: u64,
    /// Empty: no other session's number to wait for, only the marks already
    /// seen on the section.
    pub record: String,
}

const FENCE_RECORD_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4;
const FENCE_RECORD_REPLY_SIZE: usize = sv_wire::REPLY_HEADER_SIZE + 4 + 4 + SHARED_NAME_LEN;
const FENCE_MARK_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4 + 32;
const FENCE_WAIT_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4 + 16 + 4 + SHARED_NAME_LEN;

impl FenceRecord {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = command(out, SV_FENCE_RECORD, FENCE_RECORD_SIZE)?;
        enc.u32(0)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<FenceRecord> {
        let mut dec = expect_command(payload, SV_FENCE_RECORD)?;
        let _pad = dec.u32()?;
        Ok(FenceRecord)
    }
}

impl FenceRecordReply {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = reply(out, SV_FENCE_RECORD, FENCE_RECORD_REPLY_SIZE)?;
        enc.i32(self.result)?;
        put_name(&mut enc, &self.name)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<FenceRecordReply> {
        let mut dec = expect_reply(payload, SV_FENCE_RECORD)?;
        let result = dec.i32()?;
        let name = get_name(&mut dec)?;
        Ok(FenceRecordReply { result, name })
    }
}

impl FenceMark {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = unanswered(out, SV_FENCE_MARK, FENCE_MARK_SIZE)?;
        enc.u32(0)?;
        enc.u64(self.queue)?;
        enc.u64(self.semaphore)?;
        enc.u64(self.seq)?;
        enc.u64(self.memory)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<FenceMark> {
        let mut dec = expect_command(payload, SV_FENCE_MARK)?;
        let _pad = dec.u32()?;
        Ok(FenceMark { queue: dec.u64()?, semaphore: dec.u64()?, seq: dec.u64()?, memory: dec.u64()? })
    }
}

impl FenceWait {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut enc = unanswered(out, SV_FENCE_WAIT, FENCE_WAIT_SIZE)?;
        enc.u32(0)?;
        enc.u64(self.memory)?;
        enc.u64(self.seq)?;
        put_name(&mut enc, &self.record)?;
        enc.finish()?;
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<FenceWait> {
        let mut dec = expect_command(payload, SV_FENCE_WAIT)?;
        let _pad = dec.u32()?;
        let memory = dec.u64()?;
        let seq = dec.u64()?;
        let record = get_name(&mut dec)?;
        Ok(FenceWait { memory, seq, record })
    }
}

/// A printable name for any opcode on a ring, ours or Vulkan's, for
/// instruments (`WC_FRAME_TRACE`, docs/waycompose-design.md §88).
pub fn opcode_name(op: u32) -> std::borrow::Cow<'static, str> {
    match op {
        SV_FENCE_RECORD => "svFenceRecord".into(),
        SV_FENCE_MARK => "svFenceMark".into(),
        SV_FENCE_WAIT => "svFenceWait".into(),
        SV_ALLOC_SHARED => "svAllocShared".into(),
        SV_IMPORT_SHARED => "svImportShared".into(),
        SV_EXECUTE_COMMAND_BLOB => "svExecuteCommandBlob".into(),
        SV_IMPORT_ARENA_SLAB => "svImportArenaSlab".into(),
        SV_DRIVER_VERSIONS => "svDriverVersions".into(),
        SV_ARENA_INFO => "svArenaInfo".into(),
        SV_LEASE_ARENA => "svLeaseArena".into(),
        SV_RELEASE_ARENA => "svReleaseArena".into(),
        _ => match crate::Opcode::from_u32(op) {
            Some(o) => o.name().into(),
            None => format!("op{op:#x}").into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blob_descriptor_round_trips() {
        let want = ExecuteCommandBlob {
            command_buffer: 0x1234_5678_9abc_def0,
            arena_offset: 4096,
            len: 65536,
        };
        let mut buf = Vec::new();
        want.encode(&mut buf).unwrap();
        assert_eq!(buf.len(), ExecuteCommandBlob::encoded_len());
        assert_eq!(ExecuteCommandBlob::decode(&buf).unwrap(), want);
    }

    #[test]
    fn a_slab_import_and_its_reply_round_trip() {
        let want = ImportArenaSlab { slab: 3, arena_offset: 64 << 20, len: 64 << 20 };
        let mut buf = Vec::new();
        want.encode(&mut buf).unwrap();
        assert_eq!(ImportArenaSlab::decode(&buf).unwrap(), want);

        let reply = ImportArenaSlabReply { result: 0, memory_type: 1 };
        reply.encode(&mut buf).unwrap();
        assert_eq!(ImportArenaSlabReply::decode(&buf).unwrap(), reply);
    }

    #[test]
    fn a_driver_version_question_and_its_answer_round_trip() {
        let mut buf = Vec::new();
        DriverVersions.encode(&mut buf).unwrap();
        assert_eq!(buf.len(), DriverVersions::encoded_len());
        assert_eq!(DriverVersions::decode(&buf).unwrap(), DriverVersions);
        // It must ask for a reply, or the app side waits for one that the GPU
        // side's dispatcher never sends back.
        let flags = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        assert_ne!(flags & sv_wire::CMD_FLAG_GENERATE_REPLY, 0);

        // 1.4.323 is the testhost guest's Intel UHD (docs/video-decode.md) and
        // the number this whole command exists to carry.
        let reply = DriverVersionsReply {
            instance_api_version: (1 << 22) | (4 << 12) | 323,
            device_api_version: (1 << 22) | (4 << 12) | 323,
        };
        reply.encode(&mut buf).unwrap();
        assert_eq!(DriverVersionsReply::decode(&buf).unwrap(), reply);
        // The patch digits must survive: they are the only part of the number
        // that distinguishes two drivers of the same minor version.
        assert_eq!(DriverVersionsReply::decode(&buf).unwrap().device_api_version & 0xfff, 323);
    }

    /// §97's three commands, each with its reply. Every one asks for a reply,
    /// and every one puts its length where the ring looks for it.
    #[test]
    fn the_lease_commands_and_their_replies_round_trip() {
        let mut buf = Vec::new();
        let info = ArenaInfo::named("firefox-bin-with-a-name-longer-than-32");
        info.encode(&mut buf).unwrap();
        assert_eq!(ArenaInfo::decode(&buf).unwrap(), info);
        assert_eq!(info.name_str(), "firefox-bin-with-a-name-longer-t");
        assert!(wants_reply(&buf));
        assert_eq!(u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize, buf.len());
        let r = ArenaInfoReply {
            mode: ARENA_MODE_LEASED,
            lease_granule: 64 << 10,
            session_cap: 720 << 20,
            capacity: 256 << 20,
            max_capacity: 768 << 20,
        };
        r.encode(&mut buf).unwrap();
        assert_eq!(ArenaInfoReply::decode(&buf).unwrap(), r);

        let l = LeaseArena { len: 8 << 20, purpose: LEASE_CHUNK };
        l.encode(&mut buf).unwrap();
        assert_eq!(LeaseArena::decode(&buf).unwrap(), l);
        assert!(wants_reply(&buf));
        assert_eq!(u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize, buf.len());
        let r = LeaseArenaReply { result: 0, memory_type: 1, offset: (1 << 32) | (24 << 20), len: 8 << 20 };
        r.encode(&mut buf).unwrap();
        assert_eq!(LeaseArenaReply::decode(&buf).unwrap(), r);

        let x = ReleaseArena { offset: (1 << 32) | (24 << 20) };
        x.encode(&mut buf).unwrap();
        assert_eq!(ReleaseArena::decode(&buf).unwrap(), x);
        assert!(wants_reply(&buf));
        let r = ReleaseArenaReply { result: -2 };
        r.encode(&mut buf).unwrap();
        assert_eq!(ReleaseArenaReply::decode(&buf).unwrap(), r);
        // One command's reply is not another's.
        assert!(LeaseArenaReply::decode(&buf).is_err());
        for op in [SV_ARENA_INFO, SV_LEASE_ARENA, SV_RELEASE_ARENA] {
            assert!(crate::Opcode::from_u32(op).is_none());
            assert!(is_protocol(op));
        }
    }

    /// §104's two commands: the name and the nonce survive both directions,
    /// the ring finds each length where it looks, and a name too long for the
    /// wire is refused at encode rather than cut.
    #[test]
    fn the_shared_section_commands_round_trip() {
        let mut buf = Vec::new();
        let a = AllocShared { memory: 77, size: 4 << 20, image: 76 };
        a.encode(&mut buf).unwrap();
        assert_eq!(AllocShared::decode(&buf).unwrap(), a);
        assert!(wants_reply(&buf));
        assert_eq!(u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize, buf.len());

        let name = r"Local\sv-shared-412-3-a1b2c3d4e5f60718".to_string();
        let r = AllocSharedReply { result: 0, memory_type: 2, size: 4 << 20, nonce: [9; 16], name: name.clone() };
        r.encode(&mut buf).unwrap();
        assert_eq!(AllocSharedReply::decode(&buf).unwrap(), r);
        assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize, buf.len());

        let i = ImportShared { memory: 90, size: 4 << 20, image: 89, nonce: [3; 16], name: "/dev/shm/sv-shared-1-2-0011223344556677".into() };
        i.encode(&mut buf).unwrap();
        assert_eq!(ImportShared::decode(&buf).unwrap(), i);
        assert!(wants_reply(&buf));
        assert_eq!(u32::from_le_bytes(buf[8..12].try_into().unwrap()) as usize, buf.len());
        let r = ImportSharedReply { result: -1_000_072_003, memory_type: 1, size: 4 << 20 };
        r.encode(&mut buf).unwrap();
        assert_eq!(ImportSharedReply::decode(&buf).unwrap(), r);
        assert!(AllocSharedReply::decode(&buf).is_err());

        let long = ImportShared { name: "a".repeat(SHARED_NAME_LEN + 1), ..Default::default() };
        assert!(long.encode(&mut buf).is_err());
        let full = ImportShared { name: "b".repeat(SHARED_NAME_LEN), ..Default::default() };
        full.encode(&mut buf).unwrap();
        assert_eq!(ImportShared::decode(&buf).unwrap().name.len(), SHARED_NAME_LEN);
        for op in [SV_ALLOC_SHARED, SV_IMPORT_SHARED, SV_FENCE_RECORD, SV_FENCE_MARK, SV_FENCE_WAIT] {
            assert!(crate::Opcode::from_u32(op).is_none());
            assert!(is_protocol(op));
        }
    }

    #[test]
    fn the_fence_commands_round_trip_and_two_of_them_want_no_reply() {
        let mut buf = Vec::new();
        FenceRecord.encode(&mut buf).unwrap();
        assert_eq!(FenceRecord::decode(&buf).unwrap(), FenceRecord);
        assert!(wants_reply(&buf));
        let r = FenceRecordReply { result: 0, name: "/dev/shm/sv-fence-1-0-00ff".into() };
        r.encode(&mut buf).unwrap();
        assert_eq!(FenceRecordReply::decode(&buf).unwrap(), r);

        let m = FenceMark { queue: 7, semaphore: 9, seq: 1 << 40, memory: 11 };
        m.encode(&mut buf).unwrap();
        assert_eq!(FenceMark::decode(&buf).unwrap(), m);
        assert_eq!(peek_opcode(&buf), Some(SV_FENCE_MARK));
        assert!(!wants_reply(&buf));

        let w = FenceWait { memory: 3, seq: 12, record: "Local\\sv-fence-412-1-ab".into() };
        w.encode(&mut buf).unwrap();
        assert_eq!(FenceWait::decode(&buf).unwrap(), w);
        assert!(!wants_reply(&buf));
        assert!(FenceMark::decode(&buf).is_err());
    }

    /// The one property that makes the two namespaces safe to share a `u32`.
    #[test]
    fn no_generated_opcode_reaches_the_protocol_range() {
        assert!((crate::Opcode::COUNT as u32) < PROTOCOL_BASE);
        assert!(crate::Opcode::from_u32(SV_EXECUTE_COMMAND_BLOB).is_none());
        assert!(crate::Opcode::from_u32(SV_IMPORT_ARENA_SLAB).is_none());
        assert!(crate::Opcode::from_u32(SV_DRIVER_VERSIONS).is_none());
        assert!(!is_protocol(crate::Opcode::COUNT as u32 - 1));
        assert!(is_protocol(SV_EXECUTE_COMMAND_BLOB));
    }

    /// The framing cross-check the ring performs reads `cmd_size` at byte 8 of
    /// the payload. If one of these commands ever stopped putting it there the
    /// symptom would be `FramingDisagreement` on a healthy ring, which is a
    /// confusing thing to debug from.
    #[test]
    fn the_ring_finds_the_length_where_it_looks_for_it() {
        let mut buf = Vec::new();
        ExecuteCommandBlob::default().encode(&mut buf).unwrap();
        let declared = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(declared as usize, buf.len());
    }
}
