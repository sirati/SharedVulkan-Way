//! Walking and rebuilding a Wayland message body, in both dialects.
//!
//! # The two dialects, in one sentence each
//!
//! * **libwayland's**, which the client speaks: an `h` argument occupies
//!   **zero bytes** of the message body and the descriptor travels in a
//!   `SCM_RIGHTS` control message, matched positionally across the stream.
//! * **ours**, which the Windows compositor speaks: an `h` argument is encoded
//!   exactly like an `array` — a `u32` length, the bytes, padded to 4 — and
//!   there is no control message at all (docs/waycompose-design.md §2.3).
//!
//! Everything else about the two is identical, byte for byte, which is what
//! makes a translating proxy a matter of splicing rather than of re-encoding.
//! [`FdEncoding`] is the whole of the difference and it is a parameter rather
//! than a `cfg`, because this one process reads one dialect and writes the
//! other on every message it touches.
//!
//! # Native-endian, deliberately, and one exception
//!
//! The wire format is native-endian because a Unix socket cannot cross an
//! endianness boundary and the protocol takes advantage. The *descriptor*
//! spliced into it is little-endian, because it may cross a proxy — which is
//! precisely this program. docs/waycompose-design.md §2.3 predicted that this is where
//! that decision would be cashed in, and it is: `wc_handle::Descriptor::encode`
//! writes little-endian bytes and this module carries them without looking
//! inside.

use crate::spec::{Arg, ArgKind};

/// Whether `h` arguments occupy bytes in the body being walked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdEncoding {
    /// libwayland: zero bytes, the descriptor is out of band. What the local
    /// client writes and what it expects to read.
    OutOfBand,
    /// docs/waycompose-design.md §2.3: `array`-shaped, in the body. What the Windows
    /// compositor writes and what it expects to read.
    InBand,
}

/// A message header. `size` counts the header itself, as on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub object: u32,
    pub opcode: u16,
    pub size: usize,
}

pub const HEADER_LEN: usize = 8;

/// The smallest message the protocol can express. Anything shorter is a
/// desynchronised stream rather than a short read, and saying so early turns a
/// silent misparse into a named failure — which is the §2.4 failure mode this
/// whole program exists to keep out of the byte stream.
pub const MIN_MESSAGE: usize = HEADER_LEN;

impl Header {
    /// Read a header, if `buf` holds one.
    pub fn parse(buf: &[u8]) -> Option<Header> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        let object = u32::from_ne_bytes(buf[0..4].try_into().ok()?);
        let word = u32::from_ne_bytes(buf[4..8].try_into().ok()?);
        Some(Header { object, opcode: (word & 0xffff) as u16, size: (word >> 16) as usize })
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&self.object.to_ne_bytes());
        out[4..8]
            .copy_from_slice(&(((self.size as u32) << 16) | u32::from(self.opcode)).to_ne_bytes());
        out
    }
}

/// Where one argument lives in a body, and what kind it is.
///
/// `start == end` for an `h` in [`FdEncoding::OutOfBand`], which is not a
/// degenerate case to be tidied away: it is the position the descriptor has to
/// be spliced *into*, and it is the only reason the walk records positions for
/// arguments the proxy otherwise ignores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub kind: ArgKind,
    pub start: usize,
    pub end: usize,
}

/// Why a body could not be walked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalkError {
    /// The body ran out before the arguments did. On a stream this means the
    /// two sides disagree about the shape of a message, not that more bytes are
    /// coming — the header already told us how long the message is.
    Truncated { arg: usize },
    /// A length field claims more bytes than the body holds.
    BadLength { arg: usize, claimed: usize, available: usize },
    /// Bytes left over after the last argument.
    Trailing { left: usize },
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::Truncated { arg } => write!(f, "body ended before argument {arg}"),
            WalkError::BadLength { arg, claimed, available } => {
                write!(f, "argument {arg} claims {claimed} bytes, {available} available")
            }
            WalkError::Trailing { left } => write!(f, "{left} bytes after the last argument"),
        }
    }
}

impl std::error::Error for WalkError {}

/// Round a variable-length argument's payload up to the 4-byte boundary the
/// wire format pads to.
fn pad4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// Walk `body` according to `args`, returning where each argument is.
///
/// The walk is strict in both directions: a body that ends early *and* a body
/// with bytes to spare are both errors. Being strict about the second is what
/// catches a signature that is subtly wrong — a missing trailing argument
/// produces a message that parses perfectly and leaves four bytes over, and on
/// a stream that would otherwise present as corruption several messages later.
pub fn walk(body: &[u8], args: &[Arg], fds: FdEncoding) -> Result<Vec<Span>, WalkError> {
    let mut spans = Vec::with_capacity(args.len());
    let mut at = 0usize;

    let word = |at: &mut usize, i: usize| -> Result<u32, WalkError> {
        let end = at.checked_add(4).ok_or(WalkError::Truncated { arg: i })?;
        if end > body.len() {
            return Err(WalkError::Truncated { arg: i });
        }
        let v = u32::from_ne_bytes(body[*at..end].try_into().unwrap());
        *at = end;
        Ok(v)
    };

    for (i, arg) in args.iter().enumerate() {
        let start = at;
        match arg.kind {
            ArgKind::Word | ArgKind::Object | ArgKind::NewId => {
                word(&mut at, i)?;
            }
            ArgKind::Str | ArgKind::Array => {
                let len = word(&mut at, i)? as usize;
                let padded = pad4(len);
                let end = at.checked_add(padded).ok_or(WalkError::Truncated { arg: i })?;
                if end > body.len() {
                    return Err(WalkError::BadLength {
                        arg: i,
                        claimed: padded,
                        available: body.len() - at,
                    });
                }
                at = end;
            }
            ArgKind::NewIdUntyped => {
                // string, uint version, then the id. libwayland's own
                // special case, reproduced exactly.
                let len = word(&mut at, i)? as usize;
                let padded = pad4(len);
                let end = at.checked_add(padded).ok_or(WalkError::Truncated { arg: i })?;
                if end > body.len() {
                    return Err(WalkError::BadLength {
                        arg: i,
                        claimed: padded,
                        available: body.len() - at,
                    });
                }
                at = end;
                word(&mut at, i)?; // version
                word(&mut at, i)?; // id
            }
            ArgKind::Fd => match fds {
                FdEncoding::OutOfBand => {}
                FdEncoding::InBand => {
                    let len = word(&mut at, i)? as usize;
                    let padded = pad4(len);
                    let end = at.checked_add(padded).ok_or(WalkError::Truncated { arg: i })?;
                    if end > body.len() {
                        return Err(WalkError::BadLength {
                            arg: i,
                            claimed: padded,
                            available: body.len() - at,
                        });
                    }
                    at = end;
                }
            },
        }
        spans.push(Span { kind: arg.kind, start, end: at });
    }

    if at != body.len() {
        return Err(WalkError::Trailing { left: body.len() - at });
    }
    Ok(spans)
}

/// The 4-byte word at `span`'s start, for the fixed-width kinds.
pub fn word_at(body: &[u8], span: &Span) -> Option<u32> {
    body.get(span.start..span.start + 4)
        .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
}

/// The payload of a `string`, `array` or in-band `h`, without the length word
/// and without the padding. `None` for a null string.
pub fn bytes_at<'a>(body: &'a [u8], span: &Span) -> Option<&'a [u8]> {
    let len = word_at(body, span)? as usize;
    if len == 0 {
        return None;
    }
    body.get(span.start + 4..span.start + 4 + len)
}

/// A `string`'s contents, without the trailing NUL.
pub fn string_at(body: &[u8], span: &Span) -> Option<String> {
    let raw = bytes_at(body, span)?;
    let raw = raw.strip_suffix(b"\0").unwrap_or(raw);
    String::from_utf8(raw.to_vec()).ok()
}

/// The interface name and the new id from an untyped `new_id`
/// (`wl_registry.bind`).
pub fn untyped_new_id(body: &[u8], span: &Span) -> Option<(String, u32, u32)> {
    let name = string_at(body, span)?;
    let len = word_at(body, span)? as usize;
    let after = span.start + 4 + pad4(len);
    let version = u32::from_ne_bytes(body.get(after..after + 4)?.try_into().ok()?);
    let id = u32::from_ne_bytes(body.get(after + 4..after + 8)?.try_into().ok()?);
    Some((name, version, id))
}

/// Append a variable-length payload: the length, the bytes, then padding.
pub fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_ne_bytes());
    out.extend_from_slice(bytes);
    out.extend(std::iter::repeat_n(0u8, pad4(bytes.len()) - bytes.len()));
}

/// Append a Wayland `string`: its length counting the terminating NUL, the
/// bytes and the NUL, padded to a word. §96's rewritten `set_app_id` is the
/// one string this proxy writes rather than copies.
pub fn push_string(out: &mut Vec<u8>, s: &str) {
    let mut v = Vec::with_capacity(s.len() + 1);
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    push_bytes(out, &v);
}

/// Rebuild `body` with every `h` argument re-encoded in the other dialect.
///
/// `replacements` supplies one payload per `h`, in order. Going
/// [`OutOfBand`](FdEncoding::OutOfBand) → [`InBand`](FdEncoding::InBand) that is
/// the encoded descriptor and the body grows; going the other way the payloads
/// are ignored, the bytes are dropped and the body shrinks.
///
/// Everything that is not an `h` is copied verbatim rather than re-encoded.
/// That is not an optimisation, it is the correctness argument: a proxy that
/// decoded and re-encoded every argument would have a second chance to get the
/// padding of every string in the protocol wrong, and the messages this program
/// must not corrupt are overwhelmingly the ones it has no business
/// understanding.
pub fn splice_fds(
    body: &[u8],
    spans: &[Span],
    from: FdEncoding,
    replacements: &[Vec<u8>],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 64);
    let mut copied = 0usize;
    let mut next = 0usize;
    for span in spans {
        if span.kind != ArgKind::Fd {
            continue;
        }
        out.extend_from_slice(&body[copied..span.start]);
        if from == FdEncoding::OutOfBand {
            // Growing: insert the descriptor where the zero-width argument was.
            let empty = Vec::new();
            push_bytes(&mut out, replacements.get(next).unwrap_or(&empty));
        }
        // Shrinking: skip the in-band bytes and emit nothing.
        copied = span.end;
        next += 1;
    }
    out.extend_from_slice(&body[copied..]);
    out
}

/// Assemble a complete message.
pub fn message(object: u32, opcode: u16, body: &[u8]) -> Vec<u8> {
    let size = HEADER_LEN + body.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&Header { object, opcode, size }.encode());
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Spec;

    fn args(iface: &str, opcode: u16, spec: &Spec) -> Vec<Arg> {
        spec.request(iface, opcode).unwrap().args.clone()
    }

    #[test]
    fn a_header_round_trips() {
        let h = Header { object: 7, opcode: 3, size: 24 };
        assert_eq!(Header::parse(&h.encode()).unwrap(), h);
    }

    #[test]
    fn a_short_buffer_has_no_header() {
        for n in 0..HEADER_LEN {
            assert!(Header::parse(&vec![0u8; n]).is_none());
        }
    }

    /// `wl_shm.create_pool` in the client's dialect: new_id, a zero-width fd,
    /// and a size. Eight bytes of body for a three-argument message, which is
    /// the fact that makes the whole thing work and is worth an assertion.
    #[test]
    fn create_pool_out_of_band_is_eight_bytes_of_body() {
        let spec = Spec::builtin().unwrap();
        let a = args("wl_shm", 0, &spec);
        let mut body = Vec::new();
        body.extend_from_slice(&10u32.to_ne_bytes()); // new pool id
        body.extend_from_slice(&4096u32.to_ne_bytes()); // size
        let spans = walk(&body, &a, FdEncoding::OutOfBand).unwrap();
        assert_eq!(body.len(), 8);
        assert_eq!(spans[1].kind, ArgKind::Fd);
        assert_eq!(spans[1].start, 4);
        assert_eq!(spans[1].end, 4, "an out-of-band h occupies no bytes");
        assert_eq!(word_at(&body, &spans[0]), Some(10));
        assert_eq!(word_at(&body, &spans[2]), Some(4096));
    }

    /// The round trip the proxy performs on every pool, both ways, with the
    /// real descriptor type rather than a stand-in.
    #[test]
    fn a_descriptor_survives_the_splice_in_both_directions() {
        let spec = Spec::builtin().unwrap();
        let a = args("wl_shm", 0, &spec);
        let desc = wc_handle::Descriptor::SharedFile {
            path: "Z:\\tmp\\wc-pool-0123456789abcdef".into(),
            size: 4096,
        };

        let mut client_body = Vec::new();
        client_body.extend_from_slice(&10u32.to_ne_bytes());
        client_body.extend_from_slice(&4096u32.to_ne_bytes());

        // Client dialect -> ours.
        let spans = walk(&client_body, &a, FdEncoding::OutOfBand).unwrap();
        let ours = splice_fds(&client_body, &spans, FdEncoding::OutOfBand, &[desc.encode()]);
        assert!(ours.len() > client_body.len());
        assert_eq!(ours.len() % 4, 0, "the wire format is 4-byte aligned throughout");

        // Ours -> back, and the descriptor is intact and the body identical.
        let spans2 = walk(&ours, &a, FdEncoding::InBand).unwrap();
        assert_eq!(spans2[1].kind, ArgKind::Fd);
        let carried = bytes_at(&ours, &spans2[1]).unwrap();
        assert_eq!(wc_handle::Descriptor::decode(carried).unwrap(), desc);
        let back = splice_fds(&ours, &spans2, FdEncoding::InBand, &[]);
        assert_eq!(back, client_body);
        // And the words either side of the spliced argument did not move.
        assert_eq!(word_at(&ours, &spans2[0]), Some(10));
        assert_eq!(word_at(&ours, &spans2[2]), Some(4096));
    }

    /// The class of bug docs/waycompose-design.md §8.9 names: a length that is not a
    /// multiple of 4 must still land the next argument on a boundary. Every
    /// length mod 4, as `wc-shmclient`'s tests do for the same reason.
    #[test]
    fn every_payload_length_mod_four_lands_on_a_boundary() {
        let spec = Spec::builtin().unwrap();
        let a = args("wl_shm", 0, &spec);
        for len in 0..24usize {
            let desc = wc_handle::Descriptor::SharedFile {
                path: "x".repeat(len),
                size: len as u64,
            };
            let mut body = Vec::new();
            body.extend_from_slice(&10u32.to_ne_bytes());
            body.extend_from_slice(&4096u32.to_ne_bytes());
            let spans = walk(&body, &a, FdEncoding::OutOfBand).unwrap();
            let ours = splice_fds(&body, &spans, FdEncoding::OutOfBand, &[desc.encode()]);
            assert_eq!(ours.len() % 4, 0, "path length {len} left the body unaligned");
            // The walk in the other dialect must consume it exactly — the
            // `Trailing` arm of `walk` is what makes this an assertion about
            // the boundary and not merely about the length.
            let spans2 = walk(&ours, &a, FdEncoding::InBand).unwrap();
            assert_eq!(
                wc_handle::Descriptor::decode(bytes_at(&ours, &spans2[1]).unwrap()).unwrap(),
                desc
            );
        }
    }

    #[test]
    fn strings_of_every_length_walk() {
        let spec = Spec::builtin().unwrap();
        let a = args("xdg_toplevel", 2, &spec); // set_title(string)
        for len in 0..20usize {
            let mut title = "t".repeat(len).into_bytes();
            title.push(0);
            let mut body = Vec::new();
            push_bytes(&mut body, &title);
            let spans = walk(&body, &a, FdEncoding::OutOfBand).unwrap();
            assert_eq!(string_at(&body, &spans[0]).unwrap(), "t".repeat(len));
        }
    }

    #[test]
    fn a_null_string_is_a_bare_length_word() {
        let spec = Spec::builtin().unwrap();
        let a = args("xdg_toplevel", 2, &spec);
        let body = 0u32.to_ne_bytes().to_vec();
        let spans = walk(&body, &a, FdEncoding::OutOfBand).unwrap();
        assert_eq!(spans[0], Span { kind: ArgKind::Str, start: 0, end: 4 });
        assert!(string_at(&body, &spans[0]).is_none());
    }

    #[test]
    fn registry_bind_walks_as_three_fields() {
        let spec = Spec::builtin().unwrap();
        let a = args("wl_registry", 0, &spec);
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_ne_bytes()); // name
        push_bytes(&mut body, b"wl_compositor\0");
        body.extend_from_slice(&6u32.to_ne_bytes()); // version
        body.extend_from_slice(&4u32.to_ne_bytes()); // new id
        let spans = walk(&body, &a, FdEncoding::OutOfBand).unwrap();
        assert_eq!(untyped_new_id(&body, &spans[1]).unwrap(), ("wl_compositor".into(), 6, 4));
    }

    #[test]
    fn a_truncated_body_is_an_error_and_not_a_panic() {
        let spec = Spec::builtin().unwrap();
        let a = args("wl_registry", 0, &spec);
        let mut full = Vec::new();
        full.extend_from_slice(&1u32.to_ne_bytes());
        push_bytes(&mut full, b"wl_compositor\0");
        full.extend_from_slice(&6u32.to_ne_bytes());
        full.extend_from_slice(&4u32.to_ne_bytes());
        for n in 0..full.len() {
            assert!(
                walk(&full[..n], &a, FdEncoding::OutOfBand).is_err(),
                "a {n}-byte prefix walked when it should not have"
            );
        }
    }

    #[test]
    fn a_hostile_length_is_refused_without_allocating() {
        let spec = Spec::builtin().unwrap();
        let a = args("xdg_toplevel", 2, &spec);
        let body = u32::MAX.to_ne_bytes().to_vec();
        assert!(matches!(
            walk(&body, &a, FdEncoding::OutOfBand),
            Err(WalkError::BadLength { .. })
        ));
    }

    #[test]
    fn trailing_bytes_are_an_error() {
        let spec = Spec::builtin().unwrap();
        let a = args("wl_surface", 6, &spec); // commit(), no arguments
        assert_eq!(walk(&[], &a, FdEncoding::OutOfBand).unwrap().len(), 0);
        assert_eq!(
            walk(&[0, 0, 0, 0], &a, FdEncoding::OutOfBand),
            Err(WalkError::Trailing { left: 4 })
        );
    }

    /// Every message in the vendored protocols, encoded from plausible values
    /// and walked back. This is the check that the table and the walker agree
    /// about the whole protocol and not only about the parts stage 4 uses —
    /// which matters because the proxy forwards messages it does not
    /// understand, and a walk that is wrong about one of *those* corrupts the
    /// stream for everything after it.
    #[test]
    fn every_message_in_every_protocol_round_trips() {
        let spec = Spec::builtin().unwrap();
        let mut checked = 0;
        for name in spec.names() {
            let iface = spec.interface(name).unwrap();
            for msgs in [&iface.requests, &iface.events] {
                for m in msgs {
                    for enc in [FdEncoding::OutOfBand, FdEncoding::InBand] {
                        let mut body = Vec::new();
                        for a in &m.args {
                            match a.kind {
                                ArgKind::Word | ArgKind::Object | ArgKind::NewId => {
                                    body.extend_from_slice(&7u32.to_ne_bytes())
                                }
                                ArgKind::Str => push_bytes(&mut body, b"abc\0"),
                                ArgKind::Array => push_bytes(&mut body, &[1, 2, 3, 4, 5]),
                                ArgKind::NewIdUntyped => {
                                    push_bytes(&mut body, b"wl_seat\0");
                                    body.extend_from_slice(&2u32.to_ne_bytes());
                                    body.extend_from_slice(&9u32.to_ne_bytes());
                                }
                                ArgKind::Fd => {
                                    if enc == FdEncoding::InBand {
                                        push_bytes(&mut body, &[0xaa; 9]);
                                    }
                                }
                            }
                        }
                        let spans = walk(&body, &m.args, enc).unwrap_or_else(|e| {
                            panic!("{name}.{} ({enc:?}) did not walk: {e}", m.name)
                        });
                        assert_eq!(spans.len(), m.args.len());
                        checked += 1;
                    }
                }
            }
        }
        // 193 messages across the five vendored protocols, walked in both
        // dialects. An exact figure rather than a floor: adding a protocol
        // should be a visible change to this line, because the set of messages
        // the proxy can walk is exactly the set of clients it can carry.
        //
        // It moved from 354 to 376 in stage 9, when the two decoration
        // protocols were added — eleven messages between them, and the reason
        // is docs/waycompose-design.md §13.7: the compositor advertises both to find out
        // which one Firefox uses, and a global that is advertised can be bound.
        //
        // 376 to 426 in stage 21, for four protocols and the same reason
        // stated more sharply (§25.1): `xdg-toplevel-icon` had been advertised
        // by the compositor for two stages while this table did not know it,
        // and the price of that disagreement is a killed connection rather
        // than a missing feature.
        //
        // 426 to 436 in stage 12, when `wp_cursor_shape_v1` was added
        // (docs/waycompose-design.md §23): five requests — `destroy`, `get_pointer` and
        // `get_tablet_tool_v2` on the manager, `destroy` and `set_shape` on the
        // device — and no events at all, which is what a protocol that only
        // ever tells the compositor something looks like.
        //
        // 436 to 468 in §96, for `zwp_pointer_constraints_v1` (three interfaces,
        // eight requests and four events) and `zwp_relative_pointer_manager_v1`
        // (two interfaces, three requests and one event): sixteen messages,
        // each walked in both directions, which a game's mouse-look needs.
        //
        // 468 to 494 in §102: `wp_viewporter` (five requests), the
        // single-pixel buffer manager (two) and `wp_presentation` (two
        // requests, four events), thirteen messages a nested KWin sends or
        // is sent.
        //
        // 494 to 534 in §104: `linux-dmabuf-v1`, nine requests and eleven
        // events over three interfaces, twenty messages.
        assert_eq!(checked, 534, "the vendored protocols changed shape");
    }
}

/// What does the per-message dialect rewrite actually cost?
///
/// The user's rule is that efficiency decides between a negotiated,
/// side-specialised ring format and one standard protocol translated at the
/// boundary. Both readings are defensible from the armchair, so this measures
/// the quantity they disagree about: **the per-message cost of `walk` +
/// `splice_fds`**, which is the entire rewrite a standard protocol pays and a
/// negotiated one avoids.
///
/// Ignored, because it is a stopwatch. Run it deliberately:
/// `cargo test -p wc-proxy --lib rewrite -- --ignored --nocapture`
#[cfg(test)]
mod rewrite_cost {
    use super::*;
    use crate::spec::{Arg, ArgKind};

    fn arg(kind: ArgKind) -> Arg {
        Arg { kind, interface: None }
    }

    /// `wl_surface.attach(object, int, int)` — three words, no `h`. This is the
    /// shape of nearly every message a session carries, so it is the one whose
    /// cost multiplies.
    fn attach() -> (Vec<u8>, Vec<Arg>) {
        let mut b = Vec::new();
        b.extend_from_slice(&7u32.to_ne_bytes());
        b.extend_from_slice(&0u32.to_ne_bytes());
        b.extend_from_slice(&0u32.to_ne_bytes());
        (b, vec![arg(ArgKind::Object), arg(ArgKind::Word), arg(ArgKind::Word)])
    }

    /// `wl_shm.create_pool(new_id, h, int)` — the message the whole question is
    /// about, in its out-of-band form.
    fn create_pool() -> (Vec<u8>, Vec<Arg>) {
        let mut b = Vec::new();
        b.extend_from_slice(&9u32.to_ne_bytes());
        b.extend_from_slice(&(4096u32).to_ne_bytes());
        (b, vec![arg(ArgKind::NewId), arg(ArgKind::Fd), arg(ArgKind::Word)])
    }

    fn time(name: &str, body: &[u8], args: &[Arg], repl: &[Vec<u8>]) {
        // Warm, then measure. The rewrite is `walk` to find the spans and
        // `splice_fds` to rebuild the body — exactly what the boundary does per
        // message today.
        for _ in 0..1000 {
            let s = walk(body, args, FdEncoding::OutOfBand).unwrap();
            std::hint::black_box(splice_fds(body, &s, FdEncoding::OutOfBand, repl));
        }
        let n = 200_000;
        let t0 = std::time::Instant::now();
        for _ in 0..n {
            let s = walk(body, args, FdEncoding::OutOfBand).unwrap();
            std::hint::black_box(splice_fds(body, &s, FdEncoding::OutOfBand, repl));
        }
        let per = t0.elapsed().as_secs_f64() / n as f64;
        println!("  {name:<14} {:>8.0} ns/message", per * 1e9);
    }

    #[test]
    #[ignore = "a stopwatch: reports the per-message rewrite cost"]
    fn what_the_dialect_rewrite_costs_per_message() {
        println!("per-message cost of walk + splice_fds (the boundary rewrite):");
        let (b, a) = attach();
        time("attach", &b, &a, &[]);
        let (b, a) = create_pool();
        time("create_pool", &b, &a, &[vec![0u8; 8]]);
    }
}
