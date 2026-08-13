//! The wire primitives: a bump encoder, a bounds-checked decoder, and the
//! packet headers.
//!
//! This is the hand-written floor the generated protocol stands on. It is small
//! on purpose — everything above it is generated, so a bug here is a bug in
//! 100,000 lines at once.
//!
//! # The format
//!
//! design.md §6.1 takes Venus's `VK_EXT_command_serialization` encoding rules;
//! §6.2 names three deliberate deviations. Both are implemented here, and both
//! are stated as executable facts rather than prose, because the failure mode of
//! getting them subtly wrong is silent corruption on both sides at once (R8).
//!
//! From Venus, verbatim (checked against the generated C in
//! `reference/venus/`, see `THIRD-PARTY.md` for provenance):
//!
//! | thing | wire |
//! |---|---|
//! | `u8`/`i8`/`u16`/`i16`/`u32`/`i32`/`VkBool32`/`VkFlags` | 4 bytes, little-endian |
//! | `u64`/`i64`/`f64`/`VkFlags64`/`size_t` | 8 bytes |
//! | `f32` | 4 bytes, IEEE bit pattern |
//! | 32-bit enum | 4 bytes, as `i32` |
//! | handle | 8 bytes, an object id; `VK_NULL_HANDLE` is 0 |
//! | array count | 8 bytes, and written *in addition to* the API's own `xxxCount` member |
//! | optional pointer | 8 bytes, 0 or 1 |
//! | array of elements ≥ 4 bytes | packed, no padding |
//! | array of elements < 4 bytes | packed, then padded once to a multiple of 4 |
//! | `char*` | count (`strlen + 1`) then the bytes *including* the NUL, padded to 4 |
//!
//! The stream is 4-byte granular and **not** 8-byte aligned: a `u64` routinely
//! lands on a 4-byte boundary. That is Venus's format and it costs us nothing,
//! because nothing here reads through a reference — every field is assembled
//! from bytes (design.md §7.3, "never transmute a struct out of shared memory").
//!
//! ## Where we differ from Venus, and why
//!
//! 1. **Padding bytes are written as zero.** Venus advances its cursor past pad
//!    bytes without writing them, so a `u8` on the wire is one byte of value and
//!    three bytes of whatever was in the buffer. In our transport that buffer is
//!    *shared memory the peer can read*, so stale pad bytes are a small
//!    information leak across the boundary and — worse for us — make the encoder
//!    non-deterministic, which defeats both the protocol hash and every
//!    byte-comparison test. We zero them. The layout is identical; only the
//!    value of bytes nobody is allowed to read differs.
//!
//! 2. **A `u32` byte length in every command header** (design.md §6.2, R13).
//!    The header is `{ cmd_type: u32, cmd_flags: u32, cmd_size: u32 }`, the
//!    first two fields being Venus's verbatim so a prefix comparison still
//!    works.
//!
//! 3. **Replies are a length-prefixed stream**, not a fixed mailbox
//!    (design.md §6.2, §4.4).
//!
//! `pNext` is the fourth difference and it does not live here: design.md D6
//! takes *gfxstream's* length-prefixed links rather than Venus's per-parent
//! switch, so chain encoding is generated, not primitive. [`PNEXT_TERMINATOR`]
//! and [`PNextLinkHeader`] are the shared vocabulary.
//!
//! # Safety
//!
//! `#![forbid(unsafe_code)]`. Every read is bounds-checked and returns a
//! [`WireError`] rather than reading past the end; every write is bounds-checked
//! against the reserved span. R7 says the generated marshalling must be
//! safe-by-construction, and it can only be if this is.

#![forbid(unsafe_code)]

use core::fmt;

/// The alignment granularity of the stream. Venus asserts `size % 4 == 0` on
/// every write; this is that constant, named.
pub const WIRE_GRANULARITY: usize = 4;

/// Round up to the stream granularity.
#[inline]
pub const fn pad4(n: usize) -> usize {
    (n + WIRE_GRANULARITY - 1) & !(WIRE_GRANULARITY - 1)
}

/// Bytes in a command header: `cmd_type`, `cmd_flags`, `cmd_size`.
pub const COMMAND_HEADER_SIZE: usize = 12;
/// Bytes in a reply header: `cmd_type`, `reply_size`.
pub const REPLY_HEADER_SIZE: usize = 8;

/// Set when the caller wants a reply. Venus's `VK_COMMAND_GENERATE_REPLY_BIT_EXT`,
/// same value, because there is no reason to renumber it.
pub const CMD_FLAG_GENERATE_REPLY: u32 = 0x1;

/// The `pNext` chain terminator: a zero link length.
pub const PNEXT_TERMINATOR: u32 = 0;

/// Anything that can go wrong reading or writing the wire.
///
/// Deliberately not `Vec`-allocating and deliberately `Copy`: this type appears
/// in the return of every generated read, so it has to be free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The stream ended in the middle of a field.
    Truncated { need: usize, have: usize },
    /// A reserved span was not exactly filled. This is a generator bug — the
    /// `size` pass and the `encode` pass disagreed — and it is worth catching
    /// loudly, because the two passes disagreeing is precisely how a stream
    /// desynchronises (design.md §7.2).
    SizeMismatch { reserved: usize, written: usize },
    /// A count read from the wire cannot be satisfied by the bytes remaining.
    /// Cheap, and the realistic thing it catches is a wrong `len=`, not an
    /// attack (`security-model.md` R2, R8).
    ImplausibleCount { count: u64, remaining: usize },
    /// A `u64` count did not fit in the `u32` the API uses.
    CountOverflow { count: u64 },
    /// A fixed-size array's length prefix disagreed with the registry's extent.
    ArrayLenMismatch { expected: u64, found: u64 },
    /// A command length field disagreed with what decoding actually consumed.
    /// This is the R13 detector: it fires on the *first* mismatched command
    /// rather than letting a version skew misinterpret everything downstream.
    CommandLengthMismatch { declared: u32, consumed: usize },
    /// A `pNext` link whose declared length does not match its content.
    PNextLengthMismatch { declared: u32, consumed: usize },
    /// A `pNext` link the decoder does not know. Fatal on the GPU side, skipped
    /// on the app side (design.md §6.1, gfxstream's asymmetric handling).
    UnknownPNext { s_type: i32 },
    /// A union tag with no member.
    UnknownUnionTag { tag: u32 },
    /// An opcode the dispatcher does not implement, or one the restricted
    /// sub-decoder is not allowed to accept (design.md §6.5).
    UnknownCommand { opcode: u32 },
    /// A string that was not NUL-terminated where the format promises one.
    UnterminatedString,
    /// A boolean that was neither 0 nor 1.
    InvalidBool { value: u32 },
    /// A `xxxCount` member disagreed with the array it is the `len=` of.
    ///
    /// This is the check that makes a wrong `len=` in the registry harmless
    /// (R8): the wire carries an explicit count for every array, so the
    /// registry's metadata is only ever used to *verify*, and a disagreement is
    /// reported instead of obeyed.
    LengthDisagreement { field: &'static str, declared: u64, actual: u64 },
    /// A `VkDeviceMemory` id the app side's allocator has never handed out, met
    /// while rewriting a `(memory, offset)` pair into arena coordinates
    /// (design.md §5.3, §7.2 pass 6).
    ///
    /// It is a *wire* error because the pass that raises it is the generated
    /// `MemoryTransform` walk, which speaks `sv_wire::Result` on both sides of
    /// the boundary. Caught here it costs the application one failed call;
    /// missed, the unrewritten id crosses and the GPU side takes a ring fatal
    /// on a command it cannot resolve — which is how this was found.
    NoSuchMemory { id: u64 },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Truncated { need, have } => {
                write!(f, "stream truncated: need {need} bytes, {have} remain")
            }
            WireError::SizeMismatch { reserved, written } => {
                write!(f, "reserved {reserved} bytes but wrote {written}")
            }
            WireError::ImplausibleCount { count, remaining } => {
                write!(f, "array count {count} cannot fit in {remaining} remaining bytes")
            }
            WireError::CountOverflow { count } => write!(f, "array count {count} exceeds u32"),
            WireError::ArrayLenMismatch { expected, found } => {
                write!(f, "fixed array length {found} where the registry says {expected}")
            }
            WireError::CommandLengthMismatch { declared, consumed } => {
                write!(f, "command declared {declared} bytes but decoding consumed {consumed}")
            }
            WireError::PNextLengthMismatch { declared, consumed } => {
                write!(f, "pNext link declared {declared} bytes but decoding consumed {consumed}")
            }
            WireError::UnknownPNext { s_type } => write!(f, "unknown pNext sType {s_type}"),
            WireError::UnknownUnionTag { tag } => write!(f, "union tag {tag} selects no member"),
            WireError::UnknownCommand { opcode } => write!(f, "unknown command opcode {opcode}"),
            WireError::UnterminatedString => write!(f, "string is not NUL-terminated"),
            WireError::InvalidBool { value } => write!(f, "VkBool32 has value {value}"),
            WireError::LengthDisagreement { field, declared, actual } => write!(
                f,
                "{field} says {declared} but the array it describes has {actual} elements"
            ),
            WireError::NoSuchMemory { id } => {
                write!(f, "no VkDeviceMemory with app-side id {id} to rewrite")
            }
        }
    }
}

impl core::error::Error for WireError {}

pub type Result<T> = core::result::Result<T, WireError>;

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// A bump cursor over a span the caller has already reserved.
///
/// design.md §7.2 keeps the `size` and `encode` passes separate precisely so a
/// contiguous span can be reserved in the ring or the arena blob before any
/// bytes are written, and encoding then never allocates and never copies. The
/// encoder therefore borrows a slice it does not own and cannot grow.
pub struct Encoder<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Encoder<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Encoder { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Check that encoding filled the reservation exactly.
    pub fn finish(self) -> Result<usize> {
        if self.pos != self.buf.len() {
            return Err(WireError::SizeMismatch { reserved: self.buf.len(), written: self.pos });
        }
        Ok(self.pos)
    }

    /// Write `bytes`, then zero-fill to the next 4-byte boundary.
    ///
    /// The zero fill is the deviation from Venus documented at the top of this
    /// file: it costs a few stores and buys determinism.
    #[inline]
    pub fn write_padded(&mut self, bytes: &[u8]) -> Result<()> {
        let total = pad4(bytes.len());
        if self.remaining() < total {
            return Err(WireError::Truncated { need: total, have: self.remaining() });
        }
        self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        for b in &mut self.buf[self.pos + bytes.len()..self.pos + total] {
            *b = 0;
        }
        self.pos += total;
        Ok(())
    }

    /// Write exactly `bytes`, which the caller guarantees is a multiple of 4.
    #[inline]
    fn write_exact(&mut self, bytes: &[u8]) -> Result<()> {
        debug_assert_eq!(bytes.len() % WIRE_GRANULARITY, 0);
        if self.remaining() < bytes.len() {
            return Err(WireError::Truncated { need: bytes.len(), have: self.remaining() });
        }
        self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        self.pos += bytes.len();
        Ok(())
    }

    // -- primitives ---------------------------------------------------------

    #[inline]
    pub fn u8(&mut self, v: u8) -> Result<()> {
        self.write_padded(&[v])
    }
    #[inline]
    pub fn i8(&mut self, v: i8) -> Result<()> {
        self.write_padded(&v.to_le_bytes())
    }
    #[inline]
    pub fn u16(&mut self, v: u16) -> Result<()> {
        self.write_padded(&v.to_le_bytes())
    }
    #[inline]
    pub fn i16(&mut self, v: i16) -> Result<()> {
        self.write_padded(&v.to_le_bytes())
    }
    #[inline]
    pub fn u32(&mut self, v: u32) -> Result<()> {
        self.write_exact(&v.to_le_bytes())
    }
    #[inline]
    pub fn i32(&mut self, v: i32) -> Result<()> {
        self.write_exact(&v.to_le_bytes())
    }
    #[inline]
    pub fn u64(&mut self, v: u64) -> Result<()> {
        self.write_exact(&v.to_le_bytes())
    }
    #[inline]
    pub fn i64(&mut self, v: i64) -> Result<()> {
        self.write_exact(&v.to_le_bytes())
    }
    #[inline]
    pub fn f32(&mut self, v: f32) -> Result<()> {
        self.write_exact(&v.to_bits().to_le_bytes())
    }
    #[inline]
    pub fn f64(&mut self, v: f64) -> Result<()> {
        self.write_exact(&v.to_bits().to_le_bytes())
    }
    /// `size_t`, widened to 64 bits because the two sides need not agree on the
    /// width of a C `size_t` and the wire must not care.
    #[inline]
    pub fn usize_as_u64(&mut self, v: usize) -> Result<()> {
        self.u64(v as u64)
    }
    /// A `VkBool32`. Normalised to 0/1 rather than passed through, so that two
    /// values the application considers equal encode identically.
    #[inline]
    pub fn bool32(&mut self, v: bool) -> Result<()> {
        self.u32(u32::from(v))
    }
    /// An object id. `VK_NULL_HANDLE` is 0 and needs no special case.
    #[inline]
    pub fn handle(&mut self, id: u64) -> Result<()> {
        self.u64(id)
    }
    /// An array element count, or the length of a string including its NUL.
    #[inline]
    pub fn array_size(&mut self, n: u64) -> Result<()> {
        self.u64(n)
    }
    /// A one-or-zero presence flag for an optional pointer.
    ///
    /// Eight bytes, because that is what Venus writes (`vn_encode_simple_pointer`
    /// is an `array_size` of 0 or 1). design.md §6.1 describes this as "a
    /// one-word boolean … smaller [than] gfxstream's 8-byte … token"; it is the
    /// same eight bytes, and the real difference — the one that matters — is
    /// that it carries no address.
    #[inline]
    pub fn presence(&mut self, present: bool) -> Result<()> {
        self.u64(u64::from(present))
    }

    // -- aggregates ---------------------------------------------------------

    /// A packed array of a type whose elements are at least 4 bytes wide, so no
    /// padding is involved.
    #[inline]
    pub fn packed_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.write_exact(bytes)
    }

    /// A blob: `size` bytes, then padding to a multiple of 4.
    #[inline]
    pub fn blob(&mut self, bytes: &[u8]) -> Result<()> {
        self.write_padded(bytes)
    }

    /// An array of `u16`, packed two bytes each and padded once at the end.
    ///
    /// Exists because the obvious loop would be wrong: a *scalar* `u16` occupies
    /// four bytes on the wire, but an *array element* occupies two. Getting that
    /// distinction wrong is symmetric — both sides would agree — and would
    /// silently halve or double every such array, which is precisely the R8
    /// failure mode. It is worth a named function rather than an open-coded
    /// loop in generated code.
    pub fn packed_u16(&mut self, vals: &[u16]) -> Result<()> {
        let total = pad4(vals.len() * 2);
        if self.remaining() < total {
            return Err(WireError::Truncated { need: total, have: self.remaining() });
        }
        let start = self.pos;
        for (i, v) in vals.iter().enumerate() {
            self.buf[start + i * 2..start + i * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
        for b in &mut self.buf[start + vals.len() * 2..start + total] {
            *b = 0;
        }
        self.pos += total;
        Ok(())
    }

    /// A NUL-terminated string: the length including the NUL, then the bytes.
    pub fn c_string(&mut self, s: &[u8]) -> Result<()> {
        // The caller passes the bytes *without* the terminator; the terminator
        // is part of the format, not of the value.
        let total = s.len() + 1;
        self.array_size(total as u64)?;
        let padded = pad4(total);
        if self.remaining() < padded {
            return Err(WireError::Truncated { need: padded, have: self.remaining() });
        }
        self.buf[self.pos..self.pos + s.len()].copy_from_slice(s);
        for b in &mut self.buf[self.pos + s.len()..self.pos + padded] {
            *b = 0;
        }
        self.pos += padded;
        Ok(())
    }

    /// Reserve four bytes and return a handle for writing them later.
    ///
    /// Used for the command header's `cmd_size` and for `pNext` link lengths:
    /// both are "how long is the thing I am about to write", and the `size`
    /// pass already knows, but patching keeps the two answers from being able
    /// to disagree silently.
    pub fn reserve_u32(&mut self) -> Result<Patch> {
        let at = self.pos;
        self.u32(0)?;
        Ok(Patch { at })
    }

    /// Fill in a previously reserved `u32`.
    pub fn patch_u32(&mut self, patch: Patch, value: u32) {
        self.buf[patch.at..patch.at + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Bytes written since a mark, which is what a length patch needs.
    pub fn since(&self, mark: usize) -> usize {
        self.pos - mark
    }
}

/// A reserved four-byte slot, to be filled in once its length is known.
#[derive(Debug, Clone, Copy)]
pub struct Patch {
    at: usize,
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// A bounds-checked cursor over bytes the peer wrote.
///
/// Every accessor returns `Result`. design.md §7.3: "One comparison per field,
/// which is noise next to the work being done." The alternative — a decoder
/// that reads first and checks later, or not at all — is how a wrong `len=`
/// becomes memory corruption instead of an error.
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    unknown_pnext: UnknownPNextPolicy,
}

impl<'a> Decoder<'a> {
    /// A decoder that treats an unknown `pNext` link as fatal — the GPU side's
    /// behaviour, and the safe default. The app side asks for
    /// [`Decoder::lenient`] explicitly, so that leniency is always a decision
    /// somebody made rather than one that was inherited.
    pub fn new(buf: &'a [u8]) -> Self {
        Decoder { buf, pos: 0, unknown_pnext: UnknownPNextPolicy::Fatal }
    }

    /// The app side's decoder: unknown `pNext` links are kept as raw bytes and
    /// the chain walk continues (design.md §6.1).
    pub fn lenient(buf: &'a [u8]) -> Self {
        Decoder { buf, pos: 0, unknown_pnext: UnknownPNextPolicy::Skip }
    }

    pub fn unknown_pnext_policy(&self) -> UnknownPNextPolicy {
        self.unknown_pnext
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub fn since(&self, mark: usize) -> usize {
        self.pos - mark
    }

    /// A decoder over the next `len` bytes, advancing this one past them.
    ///
    /// This is how an unknown `pNext` link is skipped and how a known one is
    /// confined to its declared length: a sub-decoder cannot read past the link
    /// even if the generated body is wrong about the shape.
    pub fn sub(&mut self, len: usize) -> Result<Decoder<'a>> {
        let bytes = self.take(len)?;
        Ok(Decoder { buf: bytes, pos: 0, unknown_pnext: self.unknown_pnext })
    }

    #[inline]
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(WireError::Truncated { need: n, have: self.remaining() });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Read `n` bytes of value, then skip the padding to a 4-byte boundary.
    #[inline]
    fn take_padded(&mut self, n: usize) -> Result<&'a [u8]> {
        let total = pad4(n);
        if self.remaining() < total {
            return Err(WireError::Truncated { need: total, have: self.remaining() });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += total;
        Ok(out)
    }

    // -- primitives ---------------------------------------------------------

    #[inline]
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take_padded(1)?[0])
    }
    #[inline]
    pub fn i8(&mut self) -> Result<i8> {
        Ok(self.take_padded(1)?[0] as i8)
    }
    #[inline]
    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take_padded(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    #[inline]
    pub fn i16(&mut self) -> Result<i16> {
        Ok(self.u16()? as i16)
    }
    #[inline]
    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    #[inline]
    pub fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }
    #[inline]
    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }
    #[inline]
    pub fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }
    #[inline]
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }
    #[inline]
    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.u64()?))
    }
    /// `size_t`, narrowed from the wire's 64 bits.
    ///
    /// On a 32-bit side this can genuinely not fit, and the honest answer is an
    /// error rather than a truncation nobody notices.
    #[inline]
    pub fn u64_as_usize(&mut self) -> Result<usize> {
        let v = self.u64()?;
        usize::try_from(v).map_err(|_| WireError::CountOverflow { count: v })
    }
    /// A `VkBool32`. A value other than 0 or 1 is not a boolean the application
    /// could have produced through our ICD, so it is an error rather than a
    /// truthiness test.
    #[inline]
    pub fn bool32(&mut self) -> Result<bool> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(WireError::InvalidBool { value }),
        }
    }
    #[inline]
    pub fn handle(&mut self) -> Result<u64> {
        self.u64()
    }
    #[inline]
    pub fn presence(&mut self) -> Result<bool> {
        Ok(self.u64()? != 0)
    }

    // -- aggregates ---------------------------------------------------------

    /// An array count, checked against what is actually left.
    ///
    /// `elem_size` is the *wire* size of one element, so the check is exact for
    /// packed arrays and conservative (never over-permissive) for arrays of
    /// variable-size structs, where an element is at least `elem_size` bytes.
    /// Count-then-check-then-loop, never slice-from-peer-length
    /// (design.md §7.3).
    pub fn array_size(&mut self, min_elem_size: usize) -> Result<usize> {
        let count = self.u64()?;
        let n = usize::try_from(count).map_err(|_| WireError::CountOverflow { count })?;
        let need = n.checked_mul(min_elem_size).ok_or(WireError::CountOverflow { count })?;
        if need > self.remaining() {
            return Err(WireError::ImplausibleCount { count, remaining: self.remaining() });
        }
        Ok(n)
    }

    /// An array count that is additionally bounded by a value already read from
    /// the stream — the array's `len=` sibling.
    ///
    /// [`Decoder::array_size`] bounds a count by the bytes remaining, which is
    /// enough whenever an element occupies at least one byte. It is *not* enough
    /// for the count-then-fill idiom: `vkEnumerateInstanceLayerProperties` sends
    /// its `pProperties` array in the partial form, where a `VkLayerProperties`
    /// is entirely implementation-filled and therefore encodes to **zero
    /// bytes**. A count of a billion zero-byte elements then passes a
    /// byte-based check and asks for a billion allocations.
    ///
    /// The count still comes from the wire — the registry's `len=` does not size
    /// anything — but it is now bounded above by the capacity the caller
    /// declared, which is the same invariant Venus relies on when it decodes an
    /// array reply against `*pCount`.
    pub fn array_size_bounded(&mut self, min_elem_size: usize, max: usize) -> Result<usize> {
        let count = self.u64()?;
        let n = usize::try_from(count).map_err(|_| WireError::CountOverflow { count })?;
        if n > max {
            return Err(WireError::ImplausibleCount { count, remaining: max });
        }
        let need = n.checked_mul(min_elem_size).ok_or(WireError::CountOverflow { count })?;
        if need > self.remaining() {
            return Err(WireError::ImplausibleCount { count, remaining: self.remaining() });
        }
        Ok(n)
    }

    /// A fixed-extent array's length prefix, which the registry already knows.
    pub fn fixed_array_size(&mut self, expected: u64) -> Result<()> {
        let found = self.u64()?;
        if found != expected {
            return Err(WireError::ArrayLenMismatch { expected, found });
        }
        Ok(())
    }

    /// `n` bytes of packed array data.
    pub fn packed_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }

    /// `n` bytes, then padding to a multiple of 4.
    pub fn blob(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take_padded(n)
    }

    /// `n` packed `u16`s, then padding. The counterpart of
    /// [`Encoder::packed_u16`].
    pub fn packed_u16(&mut self, n: usize) -> Result<Vec<u16>> {
        let bytes = self.take_padded(n * 2)?;
        Ok(bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
    }

    /// A NUL-terminated string. Returns the bytes *without* the terminator.
    pub fn c_string(&mut self) -> Result<&'a [u8]> {
        let total = self.array_size(1)?;
        if total == 0 {
            return Err(WireError::UnterminatedString);
        }
        let bytes = self.take_padded(total)?;
        match bytes.last() {
            Some(0) => Ok(&bytes[..total - 1]),
            _ => Err(WireError::UnterminatedString),
        }
    }
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

/// A command header. Venus's two dwords plus our length (design.md §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandHeader {
    pub cmd_type: u32,
    pub cmd_flags: u32,
    /// Total bytes of the command, header included.
    pub cmd_size: u32,
}

impl CommandHeader {
    pub fn wants_reply(&self) -> bool {
        self.cmd_flags & CMD_FLAG_GENERATE_REPLY != 0
    }

    pub fn encode(&self, enc: &mut Encoder<'_>) -> Result<()> {
        enc.u32(self.cmd_type)?;
        enc.u32(self.cmd_flags)?;
        enc.u32(self.cmd_size)
    }

    pub fn decode(dec: &mut Decoder<'_>) -> Result<CommandHeader> {
        Ok(CommandHeader { cmd_type: dec.u32()?, cmd_flags: dec.u32()?, cmd_size: dec.u32()? })
    }
}

/// A reply header: which command this answers, and how long the answer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyHeader {
    pub cmd_type: u32,
    /// Total bytes of the reply, header included.
    pub reply_size: u32,
}

impl ReplyHeader {
    pub fn encode(&self, enc: &mut Encoder<'_>) -> Result<()> {
        enc.u32(self.cmd_type)?;
        enc.u32(self.reply_size)
    }
    pub fn decode(dec: &mut Decoder<'_>) -> Result<ReplyHeader> {
        Ok(ReplyHeader { cmd_type: dec.u32()?, reply_size: dec.u32()? })
    }
}

/// One `pNext` link's header, in gfxstream's shape (design.md D6).
///
/// `link_size` counts the bytes after the length word: the `sType` and the
/// link's own body. A reader that does not recognise `s_type` can therefore skip
/// exactly `link_size` bytes and carry on down the chain without consulting a
/// type table — which is the whole point, and the reason Venus's smaller
/// per-parent switch was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PNextLinkHeader {
    pub link_size: u32,
    pub s_type: i32,
}

impl PNextLinkHeader {
    /// Bytes of the header itself.
    pub const SIZE: usize = 8;
}

/// How a side reacts to a `pNext` link it does not know.
///
/// design.md §6.1: "the app side silently drops unknown links and continues
/// down `pNext`; the GPU side treats one as fatal", because a fatal abort turns
/// a silent protocol mismatch into a loud one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownPNextPolicy {
    /// App side.
    Skip,
    /// GPU side.
    Fatal,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T: PartialEq + fmt::Debug>(
        size: usize,
        write: impl FnOnce(&mut Encoder<'_>) -> Result<()>,
        read: impl FnOnce(&mut Decoder<'_>) -> Result<T>,
        expect: T,
    ) {
        let mut buf = vec![0xAAu8; size];
        let mut enc = Encoder::new(&mut buf);
        write(&mut enc).unwrap();
        assert_eq!(enc.finish().unwrap(), size);
        let mut dec = Decoder::new(&buf);
        assert_eq!(read(&mut dec).unwrap(), expect);
        assert_eq!(dec.remaining(), 0);
    }

    #[test]
    fn narrow_integers_occupy_four_bytes() {
        roundtrip(4, |e| e.u8(0x7f), |d| d.u8(), 0x7f);
        roundtrip(4, |e| e.u16(0xbeef), |d| d.u16(), 0xbeef);
        roundtrip(4, |e| e.i8(-1), |d| d.i8(), -1);
        roundtrip(4, |e| e.i16(-2), |d| d.i16(), -2);
    }

    #[test]
    fn wide_values_occupy_eight() {
        roundtrip(8, |e| e.u64(u64::MAX), |d| d.u64(), u64::MAX);
        roundtrip(8, |e| e.f64(-0.5), |d| d.f64(), -0.5);
        roundtrip(8, |e| e.handle(0), |d| d.handle(), 0);
        roundtrip(8, |e| e.presence(true), |d| d.presence(), true);
    }

    /// The deviation from Venus stated in the module docs: padding is zero, not
    /// whatever the buffer held. Without this the encoder is not a function of
    /// its input, and the determinism the generator promises is unenforceable.
    #[test]
    fn padding_is_zeroed_not_left_stale() {
        let mut buf = vec![0xAAu8; 4];
        let mut enc = Encoder::new(&mut buf);
        enc.u8(1).unwrap();
        enc.finish().unwrap();
        assert_eq!(buf, [1, 0, 0, 0]);
    }

    #[test]
    fn floats_are_bit_patterns_not_decimal() {
        let mut buf = [0u8; 4];
        Encoder::new(&mut buf).f32(1.0).unwrap();
        assert_eq!(buf, 1.0f32.to_bits().to_le_bytes());
    }

    #[test]
    fn strings_carry_their_nul_and_pad_to_four() {
        // "vk" is 2 bytes + NUL = 3, padded to 4, after an 8-byte count.
        let mut buf = vec![0xAAu8; 12];
        let mut enc = Encoder::new(&mut buf);
        enc.c_string(b"vk").unwrap();
        enc.finish().unwrap();
        assert_eq!(&buf[0..8], &3u64.to_le_bytes());
        assert_eq!(&buf[8..12], b"vk\0\0");

        let mut dec = Decoder::new(&buf);
        assert_eq!(dec.c_string().unwrap(), b"vk");
        assert_eq!(dec.remaining(), 0);
    }

    #[test]
    fn empty_string_is_just_its_nul() {
        let mut buf = vec![0u8; 12];
        let mut enc = Encoder::new(&mut buf);
        enc.c_string(b"").unwrap();
        enc.finish().unwrap();
        let mut dec = Decoder::new(&buf);
        assert_eq!(dec.c_string().unwrap(), b"");
    }

    #[test]
    fn a_string_without_its_terminator_is_rejected() {
        let mut buf = vec![0u8; 12];
        buf[0..8].copy_from_slice(&3u64.to_le_bytes());
        buf[8..11].copy_from_slice(b"abc");
        let mut dec = Decoder::new(&buf);
        assert_eq!(dec.c_string(), Err(WireError::UnterminatedString));
    }

    /// The whole point of the decoder: a length the peer chose can never make
    /// us read past the end.
    #[test]
    fn an_implausible_count_is_an_error_not_a_read() {
        let mut buf = vec![0u8; 16];
        buf[0..8].copy_from_slice(&u64::MAX.to_le_bytes());
        let mut dec = Decoder::new(&buf);
        // `u64::MAX` elements overflows the multiplication before it can be
        // compared, which is a different error and equally not a read.
        assert!(matches!(dec.array_size(4), Err(WireError::CountOverflow { .. })));

        buf[0..8].copy_from_slice(&1000u64.to_le_bytes());
        let mut dec = Decoder::new(&buf);
        assert!(matches!(dec.array_size(4), Err(WireError::ImplausibleCount { .. })));
    }

    #[test]
    fn reading_past_the_end_is_an_error() {
        let buf = [0u8; 4];
        let mut dec = Decoder::new(&buf);
        assert!(dec.u32().is_ok());
        assert_eq!(dec.u32(), Err(WireError::Truncated { need: 4, have: 0 }));
    }

    #[test]
    fn an_under_filled_reservation_is_caught() {
        let mut buf = [0u8; 8];
        let mut enc = Encoder::new(&mut buf);
        enc.u32(1).unwrap();
        assert_eq!(enc.finish(), Err(WireError::SizeMismatch { reserved: 8, written: 4 }));
    }

    #[test]
    fn a_bool32_that_is_not_a_bool_is_rejected() {
        let mut buf = [0u8; 4];
        Encoder::new(&mut buf).u32(2).unwrap();
        assert_eq!(Decoder::new(&buf).bool32(), Err(WireError::InvalidBool { value: 2 }));
    }

    #[test]
    fn command_header_round_trips_and_is_twelve_bytes() {
        let h = CommandHeader { cmd_type: 50, cmd_flags: CMD_FLAG_GENERATE_REPLY, cmd_size: 92 };
        let mut buf = [0u8; COMMAND_HEADER_SIZE];
        h.encode(&mut Encoder::new(&mut buf)).unwrap();
        assert_eq!(CommandHeader::decode(&mut Decoder::new(&buf)).unwrap(), h);
        assert!(h.wants_reply());
        // Venus's first two dwords, unchanged, so a prefix comparison against
        // its generated C is still meaningful.
        assert_eq!(&buf[0..4], &50u32.to_le_bytes());
        assert_eq!(&buf[4..8], &1u32.to_le_bytes());
    }

    #[test]
    fn a_patched_length_can_be_filled_in_afterwards() {
        let mut buf = [0u8; 12];
        let mut enc = Encoder::new(&mut buf);
        let p = enc.reserve_u32().unwrap();
        let mark = enc.position();
        enc.u32(7).unwrap();
        enc.u32(8).unwrap();
        let n = enc.since(mark) as u32;
        enc.patch_u32(p, n);
        enc.finish().unwrap();
        assert_eq!(&buf[0..4], &8u32.to_le_bytes());
    }

    #[test]
    fn a_sub_decoder_cannot_read_past_its_link() {
        let mut buf = [0u8; 12];
        let mut enc = Encoder::new(&mut buf);
        enc.u32(1).unwrap();
        enc.u32(2).unwrap();
        enc.u32(3).unwrap();
        enc.finish().unwrap();

        let mut dec = Decoder::new(&buf);
        let mut sub = dec.sub(4).unwrap();
        assert_eq!(sub.u32().unwrap(), 1);
        assert!(sub.u32().is_err());
        // The outer decoder resumed exactly after the sub-span.
        assert_eq!(dec.u32().unwrap(), 2);
    }

    #[test]
    fn pad4_is_the_granularity() {
        assert_eq!(pad4(0), 0);
        assert_eq!(pad4(1), 4);
        assert_eq!(pad4(4), 4);
        assert_eq!(pad4(5), 8);
    }
}
