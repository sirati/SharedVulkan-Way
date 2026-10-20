//! Portable validation for the IOSurface Mach protocol. Ownership tokens name
//! a connection generation in the shared broker, not a caller-supplied PID.

use std::collections::{HashMap, HashSet};

pub const NAME_MAX: usize = 64;
pub const ADD: i32 = 0x5153_0003;
pub const GONE: i32 = 0x5153_0004;
pub const ACK: i32 = 0x5153_0005;
pub const COMPLEX: u32 = 0x8000_0000;
pub const ADD_SIZE: usize = 124;
pub const GONE_SIZE: usize = 108;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Owner { pub run: u64, pub connection: u64 }

#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Add { name: String, port: u32, owner: Owner },
    Gone { name: String, owner: Owner },
}
fn word(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}
fn wide(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(b.get(at..at.checked_add(8)?)?.try_into().ok()?))
}
pub fn valid_name(b: &[u8]) -> bool {
    b.len() > 4 && b.len() <= NAME_MAX && b.starts_with(b"ios.")
        && b.iter().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(c))
}
pub fn parse(bytes: &[u8]) -> Option<Message> {
    let bits = word(bytes, 0)?;
    let size = word(bytes, 4)? as usize;
    let id = word(bytes, 20)? as i32;
    if size > bytes.len() { return None; }
    let (name_at, owner_at) = match id {
        ADD if size == ADD_SIZE && bits & COMPLEX != 0 => {
            // A received port descriptor contains one MOVE_SEND send right,
            // descriptor type zero, and a live non-null port name.
            if bits & 0xff != 18 || word(bytes, 8)? == 0 || word(bytes, 24)? != 1
                || word(bytes, 32)? != 0 || word(bytes, 36)? != 17 << 16
                || matches!(word(bytes, 28)?, 0 | u32::MAX) { return None; }
            (40, 108)
        }
        GONE if size == GONE_SIZE && bits & COMPLEX == 0 => (24, 92),
        _ => return None,
    };
    let len = word(bytes, name_at)? as usize;
    let name = bytes.get(name_at + 4..name_at + 4 + NAME_MAX)?.get(..len)?;
    if !valid_name(name) { return None; }
    let owner = Owner { run: wide(bytes, owner_at)?, connection: wide(bytes, owner_at + 8)? };
    if owner.connection == 0 { return None; }
    let name = String::from_utf8(name.to_vec()).ok()?;
    Some(if id == ADD { Message::Add { name, port: word(bytes, 28)?, owner } }
        else { Message::Gone { name, owner } })
}

/// Authoritative allocation bounds for a non-planar pixel surface.
pub fn geometry(width: usize, height: usize, bpp: usize, pitch: usize, allocation: usize) -> Option<usize> {
    if width == 0 || height == 0 || bpp == 0 || allocation == 0 || allocation > isize::MAX as usize { return None; }
    let row = width.checked_mul(bpp)?;
    if pitch < row { return None; }
    let end = pitch.checked_mul(height - 1)?.checked_add(row)?;
    (end <= allocation).then_some(allocation)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sender { pub uid: u32, pub pid: u32, pub pid_version: u32 }
/// Parse the kernel audit trailer requested with MACH_RCV_TRAILER_AUDIT.
pub fn audit(bytes: &[u8]) -> Option<Sender> {
    let size = word(bytes, 4)? as usize;
    let at = size.checked_add(3)? & !3;
    let trailer_size = word(bytes, at + 4)? as usize;
    if word(bytes, at)? != 0 || trailer_size < 52 || at.checked_add(trailer_size)? > bytes.len() { return None; }
    let pid = word(bytes, at + 20 + 5 * 4)?;
    if pid == 0 { return None; }
    Some(Sender { uid: word(bytes, at + 20 + 4)?, pid, pid_version: word(bytes, at + 20 + 7 * 4)? })
}

/// Only the broker observer authorizes adding an owner. Revocation removes
/// discoverable names; independent retained consumers are not invalidated.
pub struct Registry<T> {
    owners: HashSet<Owner>,
    names: HashMap<String, (Owner, T)>,
}
impl<T> Default for Registry<T> {
    fn default() -> Self { Self { owners: HashSet::new(), names: HashMap::new() } }
}
impl<T> Registry<T> {
    pub fn has_owner(&self, owner: Owner) -> bool { self.owners.contains(&owner) }
    pub fn add_owner(&mut self, owner: Owner) { self.owners.insert(owner); }
    pub fn insert(&mut self, owner: Owner, name: String, value: T) -> bool {
        if !self.has_owner(owner) || self.names.contains_key(&name) { return false; }
        self.names.insert(name, (owner, value)); true
    }
    pub fn get(&self, name: &str) -> Option<&T> { self.names.get(name).map(|(_, value)| value) }
    pub fn remove(&mut self, owner: Owner, name: &str) {
        if self.names.get(name).is_some_and(|(o, _)| *o == owner) { self.names.remove(name); }
    }
    pub fn revoke(&mut self, owner: Owner) {
        self.owners.remove(&owner);
        self.names.retain(|_, (o, _)| *o != owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn put(b: &mut [u8], at: usize, n: u32) { b[at..at+4].copy_from_slice(&n.to_ne_bytes()); }
    fn add() -> Vec<u8> {
        let mut b = vec![0; ADD_SIZE];
        put(&mut b, 0, COMPLEX | 18); put(&mut b, 4, ADD_SIZE as u32);
        put(&mut b, 8, 71); put(&mut b, 20, ADD as u32); put(&mut b, 24, 1);
        put(&mut b, 28, 91); put(&mut b, 36, 17 << 16); put(&mut b, 40, 5);
        b[44..49].copy_from_slice(b"ios.x"); put(&mut b, 116, 9); b
    }
    #[test] fn accepts_exact_received_shape_and_owner_generation() {
        assert_eq!(parse(&add()), Some(Message::Add { name: "ios.x".into(), port: 91,
            owner: Owner { run: 0, connection: 9 } }));
    }
    #[test] fn rejects_wrong_count_type_disposition_and_null_port() {
        for (at, value) in [(24,0),(24,2),(32,1),(36,1<<24),(36,19<<16),(28,0),(28,u32::MAX)] {
            let mut b=add(); put(&mut b,at,value); assert_eq!(parse(&b),None,"{at} {value}");
        }
    }
    #[test] fn rejects_truncation_overlong_name_and_ambiguous_shapes() {
        let b=add(); for n in 0..ADD_SIZE { assert_eq!(parse(&b[..n]),None); }
        for (at,value) in [(4,ADD_SIZE as u32+4),(0,18),(40,65),(40,0),(8,0)] {
            let mut b=add();put(&mut b,at,value);assert_eq!(parse(&b),None);
        }
        let mut b=add();b[48]=0;assert_eq!(parse(&b),None);
    }
    #[test] fn gone_requires_exact_noncomplex_shape() {
        let mut b = vec![0; GONE_SIZE];
        put(&mut b, 4, GONE_SIZE as u32); put(&mut b, 20, GONE as u32);
        put(&mut b, 24, 5); b[28..33].copy_from_slice(b"ios.x"); put(&mut b, 100, 7);
        assert_eq!(parse(&b), Some(Message::Gone { name: "ios.x".into(), owner: Owner { run: 0, connection: 7 } }));
        put(&mut b, 0, COMPLEX); assert_eq!(parse(&b), None);
    }
    #[test] fn repeated_crashes_leave_no_discoverable_names() {
        let mut r = Registry::default();
        for connection in 1..100 {
            let owner = Owner { run: 1, connection };
            r.add_owner(owner); assert!(r.insert(owner, "ios.x".into(), connection));
            r.revoke(owner); assert!(r.get("ios.x").is_none());
        }
        assert!(r.owners.is_empty()); assert!(r.names.is_empty());
    }
    #[test] fn geometry_never_expands_the_allocation_or_wraps() {
        assert_eq!(geometry(0,3,4,128,284),None);
        assert_eq!(geometry(7,0,4,128,284),None);
        assert_eq!(geometry(7,3,4,128,284),Some(284));
        assert_eq!(geometry(7,3,4,128,283),None);
        assert_eq!(geometry(usize::MAX,2,4,128,1024),None);
        assert_eq!(geometry(1,usize::MAX,4,128,1024),None);
        assert_eq!(geometry(7,3,4,27,1000),None);
    }
    #[test] fn revocation_removes_names_but_keeps_independent_consumers_alive() {
        use std::sync::Arc;
        let owner=Owner{run:1,connection:7};let mut r=Registry::default();r.add_owner(owner);
        let value=Arc::new(vec![17]);let weak=Arc::downgrade(&value);
        assert!(r.insert(owner,"ios.x".into(),value));
        let consumer=Arc::clone(r.get("ios.x").unwrap());r.revoke(owner);
        assert!(r.get("ios.x").is_none());assert_eq!(weak.upgrade().as_deref(),Some(&vec![17]));
        assert!(!r.insert(owner,"ios.y".into(),Arc::new(vec![])));
        drop(consumer);assert!(weak.upgrade().is_none());
    }
    #[test] fn old_broker_run_or_connection_cannot_revoke_new_generation() {
        let old=Owner{run:1,connection:7};let new=Owner{run:2,connection:7};
        let mut r=Registry::default();r.add_owner(new);assert!(r.insert(new,"ios.x".into(),42));
        r.revoke(old);r.remove(old,"ios.x");assert_eq!(r.get("ios.x"),Some(&42));
    }
    #[test] fn audit_trailer_is_bounded_and_uses_kernel_pid_and_uid_fields() {
        let mut b=add();b.resize(ADD_SIZE+52,0);put(&mut b,ADD_SIZE+4,52);
        put(&mut b,ADD_SIZE+24,501);put(&mut b,ADD_SIZE+40,123);put(&mut b,ADD_SIZE+48,77);
        assert_eq!(audit(&b),Some(Sender{uid:501,pid:123,pid_version:77}));
        b.pop();assert_eq!(audit(&b),None);
    }
}
