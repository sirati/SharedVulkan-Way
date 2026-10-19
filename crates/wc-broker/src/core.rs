//! The broker's state, with no operating system in it (§111.3).
//!
//! Everything the broker decides is decided here: who may hold a device, who
//! may map a region, what becomes of a dead process's devices, regions and
//! leases. The platform halves (`sys`) only find out who is asking, watch
//! processes, and move handles; they call into [`Core`] and carry out what it
//! returns. So the rules are the same on Linux, macOS and Windows, and they
//! are unit-tested without any of them.
//!
//! # The rules
//!
//! 1. **A user owns what its processes asked for.** A user is a uid or a SID,
//!    nothing more (no keys, no integrity levels). Only that user's processes,
//!    and the broker's own privileged clients, are given a handle to it.
//! 2. **A device has one holder.** A device (an ivshmem BAR, a virtio-serial
//!    port) can be opened once, so the broker opens it and *transfers* the
//!    handle: from then on it is the holder's, its data path is the holder's
//!    own, and the broker only records who holds it. It is free again when
//!    the holder's process exits, which the kernel tells the broker.
//! 3. **A region outlives its owner until its owner's pool has let go** (the
//!    keeper rule, §103.15 and §108.10). A region the broker made is held by
//!    the broker as well as by whoever maps it, so a crash of its owner
//!    leaves it held, an *orphan* of its pool. The next process of the same
//!    user that serves that pool adopts it. A region is destroyed only on its
//!    owner's `free`, or by the region's own release rule.
//! 4. **A lease is a holder's claim on one region's epoch** (the client role,
//!    §108.4). It ends with `close`, with the connection, or with the process.
//!    A retiring region whose last lease has ended is *drained*, which is what
//!    lets its substrate answer `UNMAPPED`.
//! 5. **A pool can be released as a whole** (§111.9, the macOS keeper). Where
//!    one region of a pool carries the host's word for the whole session (the
//!    boot page of a macOS session's ring window), the platform's probe of
//!    that region releases every orphan of the pool at once, and ends the
//!    processes *tied* to the pool (`tie POOL`: the session's GPU server,
//!    which the host must have stopped serving before it may go).

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// A connection to the broker, numbered by the platform half.
pub type ConnId = u64;
/// A process id. Process ids are reused, so the platform half watches a
/// process by a handle on it (pidfd, kqueue, a process handle), never by number.
pub type Pid = u32;
/// A region id, never reused within one broker run.
pub type ObjId = u64;

/// An operating-system user: `uid:1000`, `sid:S-1-5-21-…`. Compared as text.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct User(pub String);

impl User {
    pub fn uid(uid: u32) -> User {
        User(format!("uid:{uid}"))
    }
    pub fn sid(sid: &str) -> User {
        User(format!("sid:{sid}"))
    }
}

impl std::fmt::Display for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who is on the other end of a connection, as the kernel said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub pid: Pid,
    pub user: User,
    /// root, SYSTEM, or the broker's own account: may open any user's region.
    /// It is never given a device another user holds.
    pub admin: bool,
}

/// Which users may be lent a device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Allow {
    /// Any user, one at a time.
    AnyUser,
    /// Only these.
    Users(Vec<User>),
}

/// A device the broker may lend: its name in the protocol and who may have it.
/// Where it is (a PCI slot, a port name, a path) is the platform half's.
#[derive(Clone, Debug)]
pub struct DeviceSpec {
    pub name: String,
    pub allow: Allow,
}

/// When an orphaned region may be destroyed without its owner's `free`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// Only on `free` by its (adopting) owner. The Windows keeper's rule:
    /// never on a timeout (§108.10).
    OwnerFree,
    /// Also once the platform half's probe says so (a release word the host
    /// writes in the region, §103.15), or `after` this long as an orphan.
    Probe { after: Option<Duration> },
}

/// Why a request was refused. Sent as `err <text>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn refuse<T>(why: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal(why.into()))
}

/// A region as the protocol describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionInfo {
    pub id: ObjId,
    pub pool: String,
    pub len: u64,
    pub epoch: u64,
}

/// Something the platform half must do or tell, collected by [`Core::drain`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// Close the broker's own handle on region `id`: it is gone.
    Destroy(ObjId),
    /// A line for a subscribed connection (`!…`, §111.4).
    Push(ConnId, String),
    /// Every lease on retiring region `id` of epoch `epoch` has ended.
    Drained(ObjId, u64),
    /// End process `pid`: it tied itself to a pool that has been released
    /// (rule 5). The platform half ends it only while it still watches it.
    End(Pid),
}

struct Device {
    spec: DeviceSpec,
    /// The process it was lent to, and its user. Set when the lend is decided,
    /// so two requests in flight cannot both be given it.
    holder: Option<(Pid, User)>,
}

struct Region {
    info: RegionInfo,
    user: User,
    /// The connection that owns it; `None` with `orphan_since` set: an orphan.
    /// `None` without: published by the broker's own process.
    owner: Option<ConnId>,
    orphan_since: Option<Instant>,
    release: Release,
    leases: BTreeMap<ConnId, u32>,
    retiring: bool,
}

struct Conn {
    peer: Peer,
    role: String,
    subscribed: bool,
}

/// The broker's whole state.
pub struct Core {
    devices: Vec<Device>,
    regions: BTreeMap<ObjId, Region>,
    conns: BTreeMap<ConnId, Conn>,
    next_id: ObjId,
    notices: Vec<Notice>,
    /// What each process said it is (`hello`), kept until it exits: its
    /// connection may close before the kernel reports the exit.
    roles: BTreeMap<Pid, String>,
    /// Bytes of regions the broker made, for `stats` and a budget.
    made_bytes: u64,
    budget: Option<u64>,
    /// Processes that end with a pool (`tie`), and that pool: (user, pool).
    ties: BTreeMap<Pid, (User, String)>,
}

impl Core {
    pub fn new(devices: Vec<DeviceSpec>) -> Core {
        Core {
            devices: devices.into_iter().map(|spec| Device { spec, holder: None }).collect(),
            regions: BTreeMap::new(),
            conns: BTreeMap::new(),
            next_id: 1,
            notices: Vec::new(),
            roles: BTreeMap::new(),
            made_bytes: 0,
            budget: None,
            ties: BTreeMap::new(),
        }
    }

    /// The most bytes of regions the broker will hold at once (`alloc`).
    pub fn set_budget(&mut self, bytes: Option<u64>) {
        self.budget = bytes;
    }

    /// What the platform half must carry out since the last call.
    pub fn drain(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    // ------------------------------------------------------------ connections

    pub fn connect(&mut self, conn: ConnId, peer: Peer) {
        self.conns.insert(conn, Conn { peer, role: String::from("?"), subscribed: false });
    }

    pub fn peer(&self, conn: ConnId) -> Option<&Peer> {
        self.conns.get(&conn).map(|c| &c.peer)
    }

    /// `hello ROLE`: what the process is, for logs and `!exited` lines.
    pub fn hello(&mut self, conn: ConnId, role: &str) -> Result<(), Refusal> {
        let Some(c) = self.conns.get_mut(&conn) else { return refuse("not connected") };
        c.role = role.to_string();
        let pid = c.peer.pid;
        self.roles.insert(pid, role.to_string());
        Ok(())
    }

    /// `subscribe`: `!` lines for this user's processes and leases from now on.
    pub fn subscribe(&mut self, conn: ConnId) -> Result<(), Refusal> {
        let Some(c) = self.conns.get_mut(&conn) else { return refuse("not connected") };
        c.subscribed = true;
        Ok(())
    }

    /// The connection is gone (closed, or its process exited): its leases end
    /// and the regions it owned become orphans of their pools.
    pub fn conn_closed(&mut self, conn: ConnId, now: Instant) {
        if self.conns.remove(&conn).is_none() {
            return;
        }
        let mut drained = Vec::new();
        for r in self.regions.values_mut() {
            if r.owner == Some(conn) {
                r.owner = None;
                r.orphan_since = Some(now);
            }
            if r.leases.remove(&conn).is_some() && r.retiring && r.leases.is_empty() {
                drained.push((r.info.id, r.info.epoch));
            }
        }
        for (id, epoch) in drained {
            self.notices.push(Notice::Drained(id, epoch));
        }
    }

    /// The kernel says process `pid` has exited: every device it held is free,
    /// and each of its connections is closed (§111.2).
    pub fn process_exited(&mut self, pid: Pid, now: Instant) {
        let mut freed = Vec::new();
        for d in &mut self.devices {
            if d.holder.as_ref().is_some_and(|(p, _)| *p == pid) {
                let (_, user) = d.holder.take().unwrap_or((pid, User(String::new())));
                freed.push((d.spec.name.clone(), user));
            }
        }
        let gone: Vec<(ConnId, User, String)> = self
            .conns
            .iter()
            .filter(|(_, c)| c.peer.pid == pid)
            .map(|(id, c)| (*id, c.peer.user.clone(), c.role.clone()))
            .collect();
        let users: BTreeSet<User> = gone.iter().map(|g| g.1.clone()).chain(freed.iter().map(|f| f.1.clone())).collect();
        let role = self.roles.remove(&pid).or_else(|| gone.first().map(|g| g.2.clone())).unwrap_or_else(|| "?".into());
        self.ties.remove(&pid);
        for (conn, _, _) in &gone {
            self.conn_closed(*conn, now);
        }
        for user in users {
            self.push_to(&user, &format!("!exited {pid} {role}"));
        }
        for (name, user) in freed {
            self.push_to(&user, &format!("!device-free {name}"));
        }
    }

    /// What process `pid` said it is, while it lives.
    pub fn role(&self, pid: Pid) -> Option<&str> {
        self.roles.get(&pid).map(String::as_str)
    }

    fn push_to(&mut self, user: &User, line: &str) {
        for (id, c) in &self.conns {
            if c.subscribed && (c.peer.user == *user || c.peer.admin) {
                self.notices.push(Notice::Push(*id, line.to_string()));
            }
        }
    }

    // ---------------------------------------------------------------- devices

    /// `device NAME`: may this connection's process be lent the device? On
    /// `Ok` the device is recorded as its, and the platform half opens it and
    /// transfers the handle; if that fails it calls [`Core::device_failed`].
    pub fn device(&mut self, conn: ConnId, name: &str) -> Result<(), Refusal> {
        let Some(c) = self.conns.get(&conn) else { return refuse("not connected") };
        let peer = c.peer.clone();
        let Some(d) = self.devices.iter_mut().find(|d| d.spec.name == name) else {
            return refuse(format!("no device {name} here"));
        };
        if let Allow::Users(users) = &d.spec.allow
            && !users.contains(&peer.user)
        {
            return refuse(format!("device {name} is not for {}", peer.user));
        }
        match &d.holder {
            Some((pid, user)) if *user != peer.user => {
                refuse(format!("device {name} is held by another user's process; one user at a time"))
            }
            Some((pid, _)) if *pid == peer.pid => refuse(format!("device {name} is already this process's")),
            Some((pid, _)) => refuse(format!("device {name} is held by process {pid} of the same user")),
            None => {
                d.holder = Some((peer.pid, peer.user));
                Ok(())
            }
        }
    }

    /// The open or the transfer failed: the device was never the process's.
    pub fn device_failed(&mut self, name: &str, pid: Pid) {
        if let Some(d) = self.devices.iter_mut().find(|d| d.spec.name == name)
            && d.holder.as_ref().is_some_and(|(p, _)| *p == pid)
        {
            d.holder = None;
        }
    }

    /// Who holds device `name`.
    pub fn device_holder(&self, name: &str) -> Option<(Pid, &User)> {
        self.devices.iter().find(|d| d.spec.name == name)?.holder.as_ref().map(|(p, u)| (*p, u))
    }

    // ---------------------------------------------------------------- regions

    /// `alloc POOL BYTES`: a new region of this user's pool, owned by this
    /// connection, held by the broker. The platform half makes the object and
    /// hands it out, or calls [`Core::alloc_failed`].
    pub fn alloc(&mut self, conn: ConnId, pool: &str, len: u64, release: Release) -> Result<RegionInfo, Refusal> {
        let Some(c) = self.conns.get(&conn) else { return refuse("not connected") };
        if len == 0 {
            return refuse("a region of 0 bytes");
        }
        if let Some(b) = self.budget
            && self.made_bytes.saturating_add(len) > b
        {
            return refuse(format!(
                "budget: {} of {} MiB held, {} MiB more refused",
                self.made_bytes >> 20,
                b >> 20,
                len >> 20
            ));
        }
        let user = c.peer.user.clone();
        let id = self.next_id;
        self.next_id += 1;
        let info = RegionInfo { id, pool: pool.to_string(), len, epoch: 1 };
        self.made_bytes += len;
        self.regions.insert(
            id,
            Region {
                info: info.clone(),
                user,
                owner: Some(conn),
                orphan_since: None,
                release,
                leases: BTreeMap::from([(conn, 1)]),
                retiring: false,
            },
        );
        Ok(info)
    }

    pub fn alloc_failed(&mut self, id: ObjId) {
        if let Some(r) = self.regions.remove(&id) {
            self.made_bytes = self.made_bytes.saturating_sub(r.info.len);
        }
    }

    /// A region the broker's own process made (the host's substrate,
    /// §108.4): `user` may open it. It has no owner connection and is never an
    /// orphan; it ends with [`Core::unpublish`].
    pub fn publish(&mut self, user: User, pool: &str, len: u64, epoch: u64) -> RegionInfo {
        let id = self.next_id;
        self.next_id += 1;
        let info = RegionInfo { id, pool: pool.to_string(), len, epoch };
        self.regions.insert(
            id,
            Region {
                info: info.clone(),
                user,
                owner: None,
                orphan_since: None,
                release: Release::OwnerFree,
                leases: BTreeMap::new(),
                retiring: false,
            },
        );
        info
    }

    pub fn unpublish(&mut self, id: ObjId) {
        if self.regions.remove(&id).is_some() {
            self.notices.push(Notice::Destroy(id));
        }
    }

    /// The region is to go: every lease holder that subscribed is told
    /// `!retiring ID EPOCH`; once the last lease has ended, [`Notice::Drained`].
    pub fn retire(&mut self, id: ObjId) {
        let Some(r) = self.regions.get_mut(&id) else { return };
        r.retiring = true;
        let (epoch, holders): (u64, Vec<ConnId>) = (r.info.epoch, r.leases.keys().copied().collect());
        if holders.is_empty() {
            self.notices.push(Notice::Drained(id, epoch));
            return;
        }
        for h in holders {
            if self.conns.get(&h).is_some_and(|c| c.subscribed) {
                self.notices.push(Notice::Push(h, format!("!retiring {id} {epoch}")));
            }
        }
    }

    /// `open ID`: a lease on a region of this connection's user.
    pub fn open(&mut self, conn: ConnId, id: ObjId) -> Result<RegionInfo, Refusal> {
        let Some(c) = self.conns.get(&conn) else { return refuse("not connected") };
        let peer = c.peer.clone();
        let Some(r) = self.regions.get_mut(&id) else { return refuse(format!("no region {id}")) };
        if r.user != peer.user && !peer.admin {
            return refuse(format!("region {id} is another user's"));
        }
        if r.retiring {
            return refuse(format!("region {id} is retiring"));
        }
        *r.leases.entry(conn).or_insert(0) += 1;
        Ok(r.info.clone())
    }

    /// `close ID`: one lease ends.
    pub fn close(&mut self, conn: ConnId, id: ObjId) -> Result<(), Refusal> {
        let Some(r) = self.regions.get_mut(&id) else { return refuse(format!("no region {id}")) };
        let Some(n) = r.leases.get_mut(&conn) else { return refuse(format!("no lease on region {id}")) };
        *n -= 1;
        if *n == 0 {
            r.leases.remove(&conn);
            if r.retiring && r.leases.is_empty() {
                self.notices.push(Notice::Drained(id, r.info.epoch));
            }
        }
        Ok(())
    }

    /// `free ID`: the owner says its pool has let go (for a guest region:
    /// after the host's `UNMAPPED`). The broker's handle is closed.
    pub fn free(&mut self, conn: ConnId, id: ObjId) -> Result<(), Refusal> {
        let Some(r) = self.regions.get(&id) else { return refuse(format!("no region {id}")) };
        if r.owner != Some(conn) {
            return refuse(format!("region {id} is not this connection's to free (adopt it first)"));
        }
        self.destroy(id);
        Ok(())
    }

    fn destroy(&mut self, id: ObjId) {
        if let Some(r) = self.regions.remove(&id) {
            if r.owner.is_some() || r.orphan_since.is_some() {
                self.made_bytes = self.made_bytes.saturating_sub(r.info.len);
            }
            self.notices.push(Notice::Destroy(id));
        }
    }

    /// `adopt POOL`: this connection takes over the orphans its user left in
    /// that pool. They stay held; it frees each once its pool has let go.
    pub fn adopt(&mut self, conn: ConnId, pool: &str) -> Result<Vec<RegionInfo>, Refusal> {
        let Some(c) = self.conns.get(&conn) else { return refuse("not connected") };
        let user = c.peer.user.clone();
        let mut out = Vec::new();
        for r in self.regions.values_mut() {
            if r.orphan_since.is_some() && r.user == user && r.info.pool == pool {
                r.owner = Some(conn);
                r.orphan_since = None;
                out.push(r.info.clone());
            }
        }
        Ok(out)
    }

    /// Orphans whose release rule wants the platform half's probe.
    pub fn probe_candidates(&self) -> Vec<ObjId> {
        self.regions
            .values()
            .filter(|r| r.orphan_since.is_some() && matches!(r.release, Release::Probe { .. }))
            .map(|r| r.info.id)
            .collect()
    }

    /// The probe found region `id` released.
    pub fn released(&mut self, id: ObjId) {
        if self.regions.get(&id).is_some_and(|r| r.orphan_since.is_some() && matches!(r.release, Release::Probe { .. })) {
            self.destroy(id);
        }
    }

    /// `tie POOL` (rule 5): this connection's process ends when its user's
    /// pool `pool` is released. A process can only ask that of itself.
    pub fn tie(&mut self, conn: ConnId, pool: &str) -> Result<(), Refusal> {
        let Some(c) = self.conns.get(&conn) else { return refuse("not connected") };
        self.ties.insert(c.peer.pid, (c.peer.user.clone(), pool.to_string()));
        Ok(())
    }

    /// The pool process `pid` is tied to, if any.
    pub fn tied(&self, pid: Pid) -> Option<&str> {
        self.ties.get(&pid).map(|t| t.1.as_str())
    }

    /// The probe found the word orphan `id` carries for its whole pool set
    /// (rule 5): every orphan of that user's pool goes, and the processes
    /// tied to it are ended. A region whose owner lives is not a probe's.
    pub fn pool_released(&mut self, id: ObjId) {
        let Some(r) = self.regions.get(&id) else { return };
        if r.orphan_since.is_none() || !matches!(r.release, Release::Probe { .. }) {
            return;
        }
        let (user, pool) = (r.user.clone(), r.info.pool.clone());
        let gone: Vec<ObjId> = self
            .regions
            .values()
            .filter(|r| r.orphan_since.is_some() && r.user == user && r.info.pool == pool)
            .map(|r| r.info.id)
            .collect();
        for id in gone {
            self.destroy(id);
        }
        self.end_tied(&user, &pool);
    }

    fn end_tied(&mut self, user: &User, pool: &str) {
        let pids: Vec<Pid> = self.ties.iter().filter(|(_, t)| t.0 == *user && t.1 == pool).map(|(p, _)| *p).collect();
        for pid in pids {
            self.ties.remove(&pid);
            self.notices.push(Notice::End(pid));
        }
    }

    /// Once in a while: orphans past their release rule's time go. A pool
    /// left with no region by that ends its tied processes too, as the
    /// macOS keeper's own time limit did (§103.15).
    pub fn tick(&mut self, now: Instant) {
        let due: Vec<(ObjId, User, String)> = self
            .regions
            .values()
            .filter(|r| match (r.orphan_since, r.release) {
                (Some(t), Release::Probe { after: Some(a) }) => now.duration_since(t) >= a,
                _ => false,
            })
            .map(|r| (r.info.id, r.user.clone(), r.info.pool.clone()))
            .collect();
        let mut pools = BTreeSet::new();
        for (id, user, pool) in due {
            self.destroy(id);
            pools.insert((user, pool));
        }
        for (user, pool) in pools {
            if !self.regions.values().any(|r| r.user == user && r.info.pool == pool) {
                self.end_tied(&user, &pool);
            }
        }
    }

    pub fn region(&self, id: ObjId) -> Option<&RegionInfo> {
        self.regions.get(&id).map(|r| &r.info)
    }

    /// `stats`.
    pub fn stats(&self) -> Stats {
        let orphans: Vec<&Region> = self.regions.values().filter(|r| r.orphan_since.is_some()).collect();
        Stats {
            connections: self.conns.len(),
            devices_held: self.devices.iter().filter(|d| d.holder.is_some()).count(),
            regions: self.regions.len(),
            region_bytes: self.made_bytes,
            orphans: orphans.len(),
            orphan_bytes: orphans.iter().map(|r| r.info.len).sum(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stats {
    pub connections: usize,
    pub devices_held: usize,
    pub regions: usize,
    pub region_bytes: u64,
    pub orphans: usize,
    pub orphan_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(pid: Pid, uid: u32) -> Peer {
        Peer { pid, user: User::uid(uid), admin: false }
    }

    fn core() -> Core {
        Core::new(vec![
            DeviceSpec { name: "arena".into(), allow: Allow::AnyUser },
            DeviceSpec { name: "wcbell".into(), allow: Allow::Users(vec![User::uid(1000)]) },
        ])
    }

    /// Rule 2: one holder, one user at a time; free when the holder exits.
    #[test]
    fn a_device_has_one_holder_and_comes_back_when_it_exits() {
        let mut c = core();
        let t = Instant::now();
        c.connect(1, peer(100, 1000));
        c.connect(2, peer(200, 1001));
        c.connect(3, peer(101, 1000));
        assert_eq!(c.device(1, "arena"), Ok(()));
        let e = c.device(2, "arena").unwrap_err().0;
        assert!(e.contains("another user"), "{e}");
        let e = c.device(3, "arena").unwrap_err().0;
        assert!(e.contains("process 100 of the same user"), "{e}");
        // The holder closes its connection: the device is still its (the
        // handle is in its process), until the process exits.
        c.conn_closed(1, t);
        assert!(c.device(2, "arena").is_err());
        c.process_exited(100, t);
        assert_eq!(c.device_holder("arena"), None);
        assert_eq!(c.device(2, "arena"), Ok(()));
    }

    #[test]
    fn a_device_for_named_users_refuses_the_others_and_a_failed_open_is_undone() {
        let mut c = core();
        c.connect(1, peer(100, 1001));
        assert!(c.device(1, "wcbell").unwrap_err().0.contains("not for uid:1001"));
        assert!(c.device(1, "nope").unwrap_err().0.contains("no device"));
        c.connect(2, peer(200, 1000));
        c.device(2, "wcbell").unwrap();
        c.device_failed("wcbell", 200);
        assert_eq!(c.device_holder("wcbell"), None);
    }

    /// Rule 1: another user's process may not open a region; root may.
    #[test]
    fn a_region_is_its_users_only() {
        let mut c = core();
        c.connect(1, peer(100, 1000));
        c.connect(2, peer(200, 1001));
        c.connect(3, Peer { pid: 1, user: User::uid(0), admin: true });
        c.connect(4, peer(101, 1000));
        let r = c.alloc(1, "gpu", 64 << 20, Release::OwnerFree).unwrap();
        assert!(c.open(2, r.id).unwrap_err().0.contains("another user's"));
        assert_eq!(c.open(4, r.id).unwrap(), r);
        assert!(c.open(3, r.id).is_ok());
        assert!(c.free(4, r.id).unwrap_err().0.contains("not this connection's"));
        assert!(c.free(2, r.id).is_err());
    }

    /// Rule 3: the keeper rule. An owner that dies leaves an orphan, held;
    /// only the same user's next provider of the pool adopts it, and only an
    /// owner's free destroys it.
    #[test]
    fn an_owners_death_leaves_an_orphan_that_only_its_user_adopts() {
        let mut c = core();
        let t = Instant::now();
        c.connect(1, peer(100, 1000));
        let a = c.alloc(1, "gpu", 32 << 20, Release::OwnerFree).unwrap();
        let b = c.alloc(1, "wayland", 16 << 20, Release::OwnerFree).unwrap();
        c.process_exited(100, t);
        assert!(c.drain().iter().all(|n| !matches!(n, Notice::Destroy(_))), "nothing is destroyed by a death");
        assert_eq!(c.stats().orphans, 2);
        // Never on a timeout.
        c.tick(t + Duration::from_secs(36_000));
        assert_eq!(c.stats().orphans, 2);
        c.connect(2, peer(200, 1001));
        assert!(c.adopt(2, "gpu").unwrap().is_empty(), "another user adopts nothing");
        c.connect(3, peer(300, 1000));
        assert_eq!(c.adopt(3, "gpu").unwrap(), vec![a.clone()]);
        assert_eq!(c.stats().orphans, 1, "the wayland pool's orphan is not the gpu pool's");
        c.free(3, a.id).unwrap();
        assert_eq!(c.drain(), vec![Notice::Destroy(a.id)]);
        assert!(c.region(b.id).is_some());
    }

    /// The macOS keeper's rule (§103.15): an orphan goes when the host's
    /// release word is seen, or after a time.
    #[test]
    fn a_probe_released_orphan_goes_when_released_or_late() {
        let mut c = core();
        let t = Instant::now();
        c.connect(1, peer(100, 501));
        let rule = Release::Probe { after: Some(Duration::from_secs(600)) };
        let a = c.alloc(1, "ring", 4 << 20, rule).unwrap();
        let b = c.alloc(1, "arena", 64 << 20, rule).unwrap();
        assert!(c.probe_candidates().is_empty(), "not an orphan yet");
        c.released(a.id);
        assert!(c.region(a.id).is_some(), "a live owner's region is not released by a probe");
        c.conn_closed(1, t);
        assert_eq!(c.probe_candidates(), vec![a.id, b.id]);
        c.released(a.id);
        c.tick(t + Duration::from_secs(599));
        assert!(c.region(b.id).is_some());
        c.tick(t + Duration::from_secs(600));
        assert_eq!(c.drain(), vec![Notice::Destroy(a.id), Notice::Destroy(b.id)]);
        assert_eq!(c.stats().region_bytes, 0);
    }

    /// Rule 5, the macOS keeper folded into the broker (§111.9): the boot
    /// region's probe releases its whole pool, never another pool or another
    /// user's, never a live owner's, and ends the processes tied to it.
    #[test]
    fn a_released_boot_region_frees_its_whole_pool_and_ends_its_ties() {
        let mut c = core();
        let t = Instant::now();
        let rule = Release::Probe { after: Some(Duration::from_secs(600)) };
        c.connect(1, peer(100, 501)); // the main process of session s1
        c.connect(2, peer(200, 501)); // its GPU server
        c.connect(3, peer(300, 501)); // the same user's main process of session s2
        c.connect(4, peer(400, 502)); // another user's, with the same pool name
        let arena = c.alloc(1, "s1", 64 << 20, rule).unwrap();
        let rings = c.alloc(1, "s1", 6 << 20, rule).unwrap();
        let other = c.alloc(3, "s2", 6 << 20, rule).unwrap();
        let theirs = c.alloc(4, "s1", 6 << 20, rule).unwrap();
        c.tie(2, "s1").unwrap();
        c.tie(4, "s1").unwrap();
        assert_eq!(c.tied(200), Some("s1"));
        // While the owner lives, the word changes nothing.
        c.pool_released(rings.id);
        assert!(c.region(arena.id).is_some() && c.drain().is_empty());
        // SIGKILL of the main process: its regions stay held.
        c.process_exited(100, t);
        c.conn_closed(4, t);
        assert_eq!(c.stats().orphans, 3);
        assert!(c.drain().iter().all(|n| !matches!(n, Notice::Destroy(_) | Notice::End(_))));
        c.pool_released(rings.id);
        let n = c.drain();
        assert_eq!(n, vec![Notice::Destroy(arena.id), Notice::Destroy(rings.id), Notice::End(200)]);
        assert!(c.region(other.id).is_some(), "another pool of the same user stays");
        assert!(c.region(theirs.id).is_some(), "another user's pool of the same name stays");
        assert_eq!(c.tied(400), Some("s1"), "another user's tie stays");
        assert_eq!(c.stats().region_bytes, 12 << 20);
        // Its exit clears what is left of it.
        c.process_exited(400, t);
        assert_eq!(c.tied(400), None);
    }

    /// The keeper's time limit, at the pool level: the last orphan of a pool
    /// going by time ends the pool's tied processes, once; a tied process
    /// that exited on its own is not ended later.
    #[test]
    fn a_pool_that_times_out_ends_its_ties_once() {
        let mut c = core();
        let t = Instant::now();
        let rule = Release::Probe { after: Some(Duration::from_secs(600)) };
        c.connect(1, peer(100, 501));
        c.connect(2, peer(200, 501));
        c.connect(3, peer(300, 501));
        c.hello(2, "gpu").unwrap();
        assert_eq!(c.role(200), Some("gpu"));
        c.alloc(1, "s1", 4 << 20, rule).unwrap();
        c.tie(2, "s1").unwrap();
        c.tie(3, "s1").unwrap();
        c.process_exited(300, t);
        c.process_exited(100, t);
        c.drain();
        c.tick(t + Duration::from_secs(599));
        assert!(c.drain().is_empty());
        c.tick(t + Duration::from_secs(600));
        let ends: Vec<Notice> = c.drain().into_iter().filter(|n| matches!(n, Notice::End(_))).collect();
        assert_eq!(ends, vec![Notice::End(200)]);
        c.tick(t + Duration::from_secs(1200));
        assert!(c.drain().is_empty());
    }

    /// Rule 4: leases on a published region; a retiring region drains when
    /// the last lease ends, by close, by connection, or by process.
    #[test]
    fn a_retiring_region_drains_when_its_last_lease_ends() {
        let mut c = core();
        let t = Instant::now();
        let r = c.publish(User::uid(1000), "pool-1", 128 << 20, 7);
        c.connect(1, peer(100, 1000));
        c.connect(2, peer(200, 1000));
        c.connect(3, peer(300, 1000));
        c.subscribe(1).unwrap();
        c.open(1, r.id).unwrap();
        c.open(1, r.id).unwrap();
        c.open(2, r.id).unwrap();
        c.open(3, r.id).unwrap();
        c.retire(r.id);
        assert_eq!(c.drain(), vec![Notice::Push(1, format!("!retiring {} 7", r.id))], "only the subscriber is told");
        assert!(c.open(2, r.id).is_err(), "no new lease on a retiring region");
        c.close(1, r.id).unwrap();
        c.close(1, r.id).unwrap();
        c.conn_closed(2, t);
        assert!(c.drain().is_empty());
        c.process_exited(300, t);
        let n = c.drain();
        assert!(n.contains(&Notice::Drained(r.id, 7)), "{n:?}");
        assert!(n.contains(&Notice::Push(1, "!exited 300 ?".into())), "the subscriber hears of the exit too");
        c.unpublish(r.id);
        assert_eq!(c.drain(), vec![Notice::Destroy(r.id)]);
    }

    #[test]
    fn a_region_nobody_leases_is_drained_at_once() {
        let mut c = core();
        let r = c.publish(User::uid(1000), "pool-1", 4096, 1);
        c.retire(r.id);
        assert_eq!(c.drain(), vec![Notice::Drained(r.id, 1)]);
    }

    /// Exits are told to the user's subscribers and to admins, nobody else.
    #[test]
    fn an_exit_is_told_to_its_users_subscribers() {
        let mut c = core();
        let t = Instant::now();
        c.connect(1, peer(100, 1000));
        c.hello(1, "gpu").unwrap();
        c.device(1, "arena").unwrap();
        c.connect(2, peer(200, 1000));
        c.subscribe(2).unwrap();
        c.connect(3, peer(300, 1001));
        c.subscribe(3).unwrap();
        c.connect(4, Peer { pid: 4, user: User::sid("S-1-5-18"), admin: true });
        c.subscribe(4).unwrap();
        c.process_exited(100, t);
        let n = c.drain();
        assert_eq!(
            n,
            vec![
                Notice::Push(2, "!exited 100 gpu".into()),
                Notice::Push(4, "!exited 100 gpu".into()),
                Notice::Push(2, "!device-free arena".into()),
                Notice::Push(4, "!device-free arena".into()),
            ]
        );
    }

    #[test]
    fn a_budget_refuses_and_a_free_gives_it_back() {
        let mut c = core();
        c.set_budget(Some(100 << 20));
        c.connect(1, peer(100, 1000));
        let a = c.alloc(1, "gpu", 64 << 20, Release::OwnerFree).unwrap();
        assert!(c.alloc(1, "gpu", 64 << 20, Release::OwnerFree).unwrap_err().0.starts_with("budget"));
        c.free(1, a.id).unwrap();
        assert!(c.alloc(1, "gpu", 64 << 20, Release::OwnerFree).is_ok());
        let f = c.alloc(1, "gpu", 1 << 20, Release::OwnerFree).unwrap();
        c.alloc_failed(f.id);
        assert_eq!(c.stats().region_bytes, 64 << 20);
    }
}
