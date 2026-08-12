//! design.md §6.3's non-Vulkan commands — "hand-written, not generated, and …
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

/// design.md §6.3: "Hand a recorded `vkCmd*` blob at `(offset,len)` to the
/// sub-decoder."
pub const SV_EXECUTE_COMMAND_BLOB: u32 = PROTOCOL_BASE;

/// design.md §6.3: "Slab import, with the memory-type reply."
pub const SV_IMPORT_ARENA_SLAB: u32 = PROTOCOL_BASE + 1;

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

/// `svImportArenaSlab`: ask the GPU side to import one slab (design.md §5.2).
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

/// Header, the `u32` that follows it, and three `u64`s. The `u32` is not
/// padding for its own sake: `COMMAND_HEADER_SIZE` is twelve, so without it
/// every `u64` in every one of these bodies would be four-byte aligned.
const BLOB_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4 + 24;
const IMPORT_SIZE: usize = sv_wire::COMMAND_HEADER_SIZE + 4 + 24;
const IMPORT_REPLY_SIZE: usize = sv_wire::REPLY_HEADER_SIZE + 8;

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

    /// The one property that makes the two namespaces safe to share a `u32`.
    #[test]
    fn no_generated_opcode_reaches_the_protocol_range() {
        assert!((crate::Opcode::COUNT as u32) < PROTOCOL_BASE);
        assert!(crate::Opcode::from_u32(SV_EXECUTE_COMMAND_BLOB).is_none());
        assert!(crate::Opcode::from_u32(SV_IMPORT_ARENA_SLAB).is_none());
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
