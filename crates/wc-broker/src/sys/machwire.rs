//! Fixed Mach packet ABI and kernel audit-trailer validation, testable offline.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use crate::core::{Peer, User};
use std::{io, mem::size_of};
type Port = u32;
pub(crate) const COMPLEX: u32 = 0x80000000;
pub(crate) const REQUEST: i32 = 0x57434d01;
pub(crate) const REPLY: i32 = REQUEST + 1;
pub(crate) const RECEIVE_OPTIONS: u32 = 2 | 0x100 | (3 << 24);
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Header {
    pub(crate) bits: u32,
    pub(crate) size: u32,
    pub(crate) remote: Port,
    pub(crate) local: Port,
    pub(crate) voucher: Port,
    pub(crate) id: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Descriptor {
    pub(crate) name: Port,
    pub(crate) pad: u32,
    pub(crate) pad2: u16,
    pub(crate) disposition: u8,
    pub(crate) kind: u8,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Request {
    pub(crate) h: Header,
    pub(crate) nonce: u64,
    pub(crate) region: u64,
    pub(crate) epoch: u64,
    pub(crate) index: u32,
    pub(crate) op: u32,
    pub(crate) target: u32,
    pub(crate) reserved: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Reply {
    pub(crate) h: Header,
    pub(crate) count: u32,
    pub(crate) right: Descriptor,
    pub(crate) nonce: u64,
    pub(crate) epoch: u64,
    pub(crate) offset: u64,
    pub(crate) len: u64,
    pub(crate) status: i32,
    pub(crate) reserved: u32,
}
const _: () =
    assert!(size_of::<Header>() == 24 && size_of::<Request>() == 64 && size_of::<Reply>() == 80);
pub(crate) fn audit(bytes: &[u8], message_size: u32) -> io::Result<Peer> {
    let start = (message_size as usize)
        .checked_add(3)
        .map(|v| v & !3)
        .ok_or_else(bad)?;
    let t = bytes
        .get(start..start.checked_add(52).ok_or_else(bad)?)
        .ok_or_else(bad)?;
    let word = |n: usize| u32::from_ne_bytes(t[n..n + 4].try_into().unwrap());
    if word(0) != 0
        || word(4) < 52
        || start
            .checked_add(word(4) as usize)
            .is_none_or(|end| end > bytes.len())
    {
        return Err(bad());
    }
    let uid = word(24);
    let pid = word(40);
    if pid == 0 {
        return Err(bad());
    }
    Ok(Peer {
        pid,
        user: User::uid(uid),
        admin: uid == 0,
    })
}
impl Request {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.h.size != size_of::<Self>() as u32
            || self.h.id != REQUEST
            || self.h.bits & COMPLEX != 0
            || self.h.remote == 0
            || self.h.bits & 0xff != 18
            || self.reserved != 0
            || self.op > 2
        {
            return Err(bad());
        }
        Ok(())
    }
}
pub(crate) fn bad() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "malformed Mach broker message or audit trailer",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_rejects_extra_descriptors_unknown_operations_and_wrong_rights() {
        let valid = Request {
            h: Header {
                size: 64,
                remote: 42,
                id: REQUEST,
                bits: 18,
                ..Header::default()
            },
            ..Request::default()
        };
        assert!(valid.validate().is_ok());
        for altered in [
            Request {
                h: Header {
                    bits: COMPLEX | 18,
                    ..valid.h
                },
                ..valid
            },
            Request {
                h: Header {
                    size: 63,
                    ..valid.h
                },
                ..valid
            },
            Request {
                h: Header {
                    bits: 17,
                    ..valid.h
                },
                ..valid
            },
            Request {
                h: Header {
                    remote: 0,
                    ..valid.h
                },
                ..valid
            },
            Request { op: 3, ..valid },
            Request {
                reserved: 1,
                ..valid
            },
        ] {
            assert!(altered.validate().is_err());
        }
    }
    #[test]
    fn audit_requires_complete_kernel_trailer_and_nonzero_process() {
        let mut bytes = [0u8; 256];
        let start = 64;
        let put = |bytes: &mut [u8], off: usize, value: u32| {
            bytes[start + off..start + off + 4].copy_from_slice(&value.to_ne_bytes())
        };
        put(&mut bytes, 4, 52);
        put(&mut bytes, 24, 501);
        put(&mut bytes, 40, 1234);
        assert_eq!(
            audit(&bytes, 64).unwrap(),
            Peer {
                pid: 1234,
                user: User::uid(501),
                admin: false
            }
        );
        assert!(audit(&bytes[..115], 64).is_err());
        assert!(audit(&bytes, u32::MAX).is_err());
        put(&mut bytes, 0, 1);
        assert!(audit(&bytes, 64).is_err());
        put(&mut bytes, 0, 0);
        put(&mut bytes, 4, 51);
        assert!(audit(&bytes, 64).is_err());
        put(&mut bytes, 4, 52);
        put(&mut bytes, 40, 0);
        assert!(audit(&bytes, 64).is_err());
        put(&mut bytes, 40, 1234);
        put(&mut bytes, 24, 0);
        assert!(audit(&bytes, 64).unwrap().admin);
    }
}

impl Reply {
    pub(crate) fn validate(
        &self,
        op: u32,
        nonce: u64,
        epoch: u64,
        offset: u64,
        len: u64,
    ) -> io::Result<()> {
        if self.h.size != size_of::<Self>() as u32
            || self.h.id != REPLY
            || self.h.bits & COMPLEX == 0
            || self.count != 1
            || self.nonce != nonce
            || self.epoch != epoch
            || (op != 2 && self.reserved != 0)
            || self.right.kind != 0
            || self.right.pad != 0
            || self.right.pad2 != 0
            || self.right.disposition != 17
        {
            return Err(bad());
        }
        if self.status != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Mach region export or lease refused",
            ));
        }
        if self.offset != offset || self.len != len || (op == 0) != (self.right.name != 0) {
            return Err(bad());
        }
        Ok(())
    }
}

#[cfg(test)]
mod reply_tests {
    use super::*;
    #[test]
    fn reply_checks_capability_type_and_exact_geometry_before_adoption() {
        let valid = Reply {
            h: Header {
                bits: COMPLEX,
                size: 80,
                id: REPLY,
                ..Header::default()
            },
            count: 1,
            right: Descriptor {
                name: 42,
                disposition: 17,
                ..Descriptor::default()
            },
            nonce: 9,
            epoch: 7,
            offset: 4096,
            len: 8192,
            ..Reply::default()
        };
        assert!(valid.validate(0, 9, 7, 4096, 8192).is_ok());
        for altered in [
            Reply {
                h: Header { bits: 0, ..valid.h },
                ..valid
            },
            Reply { count: 2, ..valid },
            Reply {
                right: Descriptor {
                    kind: 1,
                    ..valid.right
                },
                ..valid
            },
            Reply {
                right: Descriptor {
                    disposition: 18,
                    ..valid.right
                },
                ..valid
            },
            Reply {
                right: Descriptor {
                    name: 0,
                    ..valid.right
                },
                ..valid
            },
            Reply { len: 4096, ..valid },
            Reply { offset: 0, ..valid },
            Reply { epoch: 8, ..valid },
            Reply { nonce: 10, ..valid },
        ] {
            assert!(altered.validate(0, 9, 7, 4096, 8192).is_err());
        }
        assert!(valid.validate(1, 9, 7, 4096, 8192).is_err());
        let refusal = Reply { status: 1, ..valid };
        assert_eq!(
            refusal.validate(0, 9, 7, 4096, 8192).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let event = Reply {
            right: Descriptor {
                name: 0,
                ..valid.right
            },
            reserved: 17,
            ..valid
        };
        assert!(event.validate(2, 9, 7, 4096, 8192).is_ok());
        assert!(event.validate(1, 9, 7, 4096, 8192).is_err());
    }
}
