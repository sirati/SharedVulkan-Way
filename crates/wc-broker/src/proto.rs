//! The broker's protocol (§111.4): one line each way, on a local socket
//! (Unix) or pipe (Windows). Text, because it is a control plane that sees a
//! handful of requests per session, and a line is what every existing
//! service of this project speaks (§105's control ring, §107's user socket,
//! §108's keeper).
//!
//! ```text
//! hello ROLE                    -> ok hello RUN USER PID
//! device NAME                   -> ok device NAME HANDLE
//! alloc POOL BYTES [large]      -> ok region ID POOL LEN EPOCH PIECES
//! open ID                       -> ok region ID POOL LEN EPOCH PIECES
//! find POOL                     -> ok region ...   (the live region of that pool, e.g. gpu.3)
//! close ID                      -> ok
//! free ID                       -> ok
//! adopt POOL                    -> ok adopted [ID:LEN ...]
//! connect NAME                  -> ok connect NAME HANDLE   (a fresh connection to a local service)
//! sign INSTALL SESSION INDEX ARUN APAGES OUTBOUND RRUN RPAGES GPU SEQ
//!                               -> ok signed SEQ MAC IDENTITY...
//! subscribe                     -> ok
//! stats                        -> ok stats K=V ...
//! anything refused              -> err WHY
//! ```
//!
//! `HANDLE` and `PIECES` say where the handles are. On Unix they travel as
//! `SCM_RIGHTS` with the reply, in order, and the text says `fd`. On Windows
//! the broker has already put them into the caller's process with
//! `DuplicateHandle`, and the text is their values. `PIECES` is
//! `HANDLE@OFFSET+LEN,...`: one piece for a region the broker made, several
//! for a region made of dma-bufs (§108.4's host side).
//!
//! A line the broker pushes to a subscribed connection starts with `!`
//! (`!exited PID ROLE`, `!device-free NAME`, `!retiring ID EPOCH`), so a
//! client reading a reply puts such lines aside and reads on.

/// A request, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Hello { role: String },
    Device { name: String },
    Alloc { pool: String, len: u64, large: bool },
    Open { id: u64 },
    Find { pool: String },
    Close { id: u64 },
    Free { id: u64 },
    Adopt { pool: String },
    Connect { name: String },
    Sign(SignReq),
    Subscribe,
    Stats,
}

/// `sign`: the boot record a compositor is about to publish (§111.6). The
/// identity is never in the request: the broker reads it from the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignReq {
    pub install: String,
    pub session: String,
    pub index: u8,
    pub arena_run: u64,
    pub arena_pages: u32,
    pub outbound: u64,
    pub rings_run: u64,
    pub rings_pages: u32,
    pub gpu: bool,
    pub seq: u64,
}

/// The longest line either side accepts.
pub const MAX_LINE: usize = 4096;

fn word_ok(w: &str) -> bool {
    !w.is_empty() && w.len() <= 64 && w.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
}

impl Request {
    pub fn parse(line: &str) -> Result<Request, String> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.len() > MAX_LINE {
            return Err("line too long".into());
        }
        let w: Vec<&str> = line.split(' ').collect();
        let name = |i: usize| -> Result<String, String> {
            let s = w.get(i).copied().unwrap_or("");
            if word_ok(s) { Ok(s.to_string()) } else { Err(format!("bad word {s:?}")) }
        };
        let num = |i: usize| -> Result<u64, String> {
            w.get(i).and_then(|s| s.parse().ok()).ok_or_else(|| format!("bad number {:?}", w.get(i).copied().unwrap_or("")))
        };
        let n = w.len();
        let arity = |want: usize| if n == want { Ok(()) } else { Err(format!("{} takes {} word(s)", w[0], want - 1)) };
        Ok(match w[0] {
            "hello" => {
                arity(2)?;
                Request::Hello { role: name(1)? }
            }
            "device" => {
                arity(2)?;
                Request::Device { name: name(1)? }
            }
            "alloc" => {
                let large = match n {
                    3 => false,
                    4 if w[3] == "large" => true,
                    _ => return Err("alloc POOL BYTES [large]".into()),
                };
                Request::Alloc { pool: name(1)?, len: num(2)?, large }
            }
            "open" => {
                arity(2)?;
                Request::Open { id: num(1)? }
            }
            "find" => {
                arity(2)?;
                Request::Find { pool: name(1)? }
            }
            "close" => {
                arity(2)?;
                Request::Close { id: num(1)? }
            }
            "free" => {
                arity(2)?;
                Request::Free { id: num(1)? }
            }
            "adopt" => {
                arity(2)?;
                Request::Adopt { pool: name(1)? }
            }
            "connect" => {
                arity(2)?;
                Request::Connect { name: name(1)? }
            }
            "sign" => {
                arity(11)?;
                let small = |i: usize| -> Result<u32, String> {
                    u32::try_from(num(i)?).map_err(|_| format!("{} is too large", w[i]))
                };
                Request::Sign(SignReq {
                    install: name(1)?,
                    session: name(2)?,
                    index: u8::try_from(num(3)?).map_err(|_| "the index is 0..=255".to_string())?,
                    arena_run: num(4)?,
                    arena_pages: small(5)?,
                    outbound: num(6)?,
                    rings_run: num(7)?,
                    rings_pages: small(8)?,
                    gpu: match num(9)? {
                        0 => false,
                        1 => true,
                        _ => return Err("gpu is 0 or 1".into()),
                    },
                    seq: num(10)?,
                })
            }
            "subscribe" => {
                arity(1)?;
                Request::Subscribe
            }
            "stats" => {
                arity(1)?;
                Request::Stats
            }
            other => return Err(format!("unknown request {other:?}")),
        })
    }

    pub fn line(&self) -> String {
        match self {
            Request::Hello { role } => format!("hello {role}"),
            Request::Device { name } => format!("device {name}"),
            Request::Alloc { pool, len, large } => format!("alloc {pool} {len}{}", if *large { " large" } else { "" }),
            Request::Open { id } => format!("open {id}"),
            Request::Find { pool } => format!("find {pool}"),
            Request::Close { id } => format!("close {id}"),
            Request::Free { id } => format!("free {id}"),
            Request::Adopt { pool } => format!("adopt {pool}"),
            Request::Connect { name } => format!("connect {name}"),
            Request::Sign(s) => format!(
                "sign {} {} {} {} {} {} {} {} {} {}",
                s.install,
                s.session,
                s.index,
                s.arena_run,
                s.arena_pages,
                s.outbound,
                s.rings_run,
                s.rings_pages,
                u8::from(s.gpu),
                s.seq
            ),
            Request::Subscribe => "subscribe".into(),
            Request::Stats => "stats".into(),
        }
    }
}

/// One piece of a region: a handle (`fd` on Unix, a value on Windows), and
/// the range of the region it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    pub handle: String,
    pub offset: u64,
    pub len: u64,
}

pub fn pieces_text(p: &[Piece]) -> String {
    p.iter().map(|p| format!("{}@{}+{}", p.handle, p.offset, p.len)).collect::<Vec<_>>().join(",")
}

pub fn parse_pieces(s: &str) -> Option<Vec<Piece>> {
    s.split(',')
        .map(|one| {
            let (h, rest) = one.split_once('@')?;
            let (o, l) = rest.split_once('+')?;
            Some(Piece { handle: h.to_string(), offset: o.parse().ok()?, len: l.parse().ok()? })
        })
        .collect()
}

/// A region as a client reads it from an `ok region` reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionReply {
    pub id: u64,
    pub pool: String,
    pub len: u64,
    pub epoch: u64,
    pub pieces: Vec<Piece>,
}

pub fn region_line(r: &crate::core::RegionInfo, pieces: &[Piece]) -> String {
    format!("ok region {} {} {} {} {}", r.id, r.pool, r.len, r.epoch, pieces_text(pieces))
}

pub fn parse_region(line: &str) -> Result<RegionReply, String> {
    let w: Vec<&str> = line.split(' ').collect();
    match w.as_slice() {
        ["ok", "region", id, pool, len, epoch, pieces] => Ok(RegionReply {
            id: id.parse().map_err(|_| "id")?,
            pool: pool.to_string(),
            len: len.parse().map_err(|_| "len")?,
            epoch: epoch.parse().map_err(|_| "epoch")?,
            pieces: parse_pieces(pieces).ok_or("pieces")?,
        }),
        _ => Err(reply_error(line)),
    }
}

/// The error text of a reply that is not what was asked for.
pub fn reply_error(line: &str) -> String {
    line.strip_prefix("err ").map_or_else(|| format!("unexpected reply {line:?}"), str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_survives_its_own_line() {
        for r in [
            Request::Hello { role: "compositor".into() },
            Request::Device { name: "wcbell".into() },
            Request::Alloc { pool: "gpu".into(), len: 64 << 20, large: true },
            Request::Alloc { pool: "ring".into(), len: 4096, large: false },
            Request::Open { id: 7 },
            Request::Find { pool: "gpu.3".into() },
            Request::Close { id: 7 },
            Request::Free { id: 7 },
            Request::Adopt { pool: "gpu".into() },
            Request::Connect { name: "wl-hub".into() },
            Request::Sign(SignReq {
                install: "wcivs".into(),
                session: "wcivs-77".into(),
                index: 3,
                arena_run: u64::MAX,
                arena_pages: 32768,
                outbound: 1 << 20,
                rings_run: 9,
                rings_pages: 1536,
                gpu: true,
                seq: 12,
            }),
            Request::Subscribe,
            Request::Stats,
        ] {
            assert_eq!(Request::parse(&r.line()), Ok(r.clone()), "{}", r.line());
        }
    }

    #[test]
    fn a_malformed_request_is_refused_by_name() {
        for bad in ["", "hello", "hello a b", "device ../x", "alloc gpu", "alloc gpu 12 small", "open x", "fly", "device a\tb"] {
            assert!(Request::parse(bad).is_err(), "{bad:?}");
        }
        assert!(Request::parse(&"x".repeat(MAX_LINE + 1)).is_err());
    }

    #[test]
    fn a_region_reply_round_trips_with_several_pieces() {
        let info = crate::core::RegionInfo { id: 3, pool: "pool-1".into(), len: 128 << 20, epoch: 2 };
        let pieces = vec![
            Piece { handle: "fd".into(), offset: 0, len: 64 << 20 },
            Piece { handle: "fd".into(), offset: 64 << 20, len: 64 << 20 },
        ];
        let r = parse_region(&region_line(&info, &pieces)).unwrap();
        assert_eq!((r.id, r.pool.as_str(), r.len, r.epoch), (3, "pool-1", 128 << 20, 2));
        assert_eq!(r.pieces, pieces);
        assert_eq!(parse_region("err no region 9").unwrap_err(), "no region 9");
    }
}
