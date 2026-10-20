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

    /// A region a previous run of the broker made, found again (§111.8: the
    /// Windows holder kept it): back as an orphan of its user's pool, under
    /// its old id, so the next provider of that pool adopts it as if this
    /// broker had never restarted.
    pub fn restore_orphan(&mut self, info: &RegionInfo, user: User, release: Release, now: Instant) {
        self.next_id = self.next_id.max(info.id + 1);
        self.made_bytes += info.len;
        self.regions.insert(
            info.id,
            Region {
                info: info.clone(),
                user,
                owner: None,
                orphan_since: Some(now),
                release,
                leases: BTreeMap::new(),
                retiring: false,
            },
        );
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

    /// `find POOL`: a lease on the live region of this user's pool `POOL`,
    /// for a client that knows a region by its place (`gpu.<k>`, §108) and
    /// not by the broker's id. A retiring region is never found, so after a
    /// pool's region is replaced the next `find` gets the new one.
    pub fn find(&mut self, conn: ConnId, pool: &str) -> Result<RegionInfo, Refusal> {
        let Some(c) = self.conns.get(&conn) else { return refuse("not connected") };
        let peer = c.peer.clone();
        let id = self
            .regions
            .values()
            .filter(|r| r.info.pool == pool && !r.retiring && (r.user == peer.user || peer.admin))
            .map(|r| r.info.id)
            .max();
        match id {
            Some(id) => self.open(conn, id),
            None => refuse(format!("no live region of pool {pool} for {}", peer.user)),
        }
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

    /// Once in a while: orphans past their release rule's time go.
    pub fn tick(&mut self, now: Instant) {
        let due: Vec<ObjId> = self
            .regions
            .values()
            .filter(|r| match (r.orphan_since, r.release) {
                (Some(t), Release::Probe { after: Some(a) }) => now.duration_since(t) >= a,
                _ => false,
            })
            .map(|r| r.info.id)
            .collect();
        for id in due {
            self.destroy(id);
        }
    }

    /// The processes with a lease on region `id`, each once.
    pub fn holders(&self, id: ObjId) -> Vec<Pid> {
        let Some(r) = self.regions.get(&id) else { return Vec::new() };
        let pids: BTreeSet<Pid> = r.leases.keys().filter_map(|c| self.conns.get(c)).map(|c| c.peer.pid).collect();
        pids.into_iter().collect()
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

    /// `find` names a region by its pool: only the caller's user's, never a
    /// retiring one, the newest when there are two.
    #[test]
    fn a_region_is_found_by_its_pool_for_its_user_only() {
        let mut c = core();
        let t = Instant::now();
        let a = c.publish(User::uid(1000), "gpu.3", 64 << 20, 1);
        c.publish(User::uid(1001), "gpu.3", 64 << 20, 1);
        c.connect(1, peer(100, 1000));
        c.connect(2, peer(200, 1002));
        assert_eq!(c.find(1, "gpu.3").unwrap(), a);
        assert_eq!(c.holders(a.id), vec![100]);
        assert!(c.find(2, "gpu.3").unwrap_err().0.contains("no live region"));
        assert!(c.find(1, "gpu.4").is_err());
        c.retire(a.id);
        assert!(c.find(1, "gpu.3").is_err(), "a retiring region is not found");
        let b = c.publish(User::uid(1000), "gpu.3", 128 << 20, 2);
        assert_eq!(c.find(1, "gpu.3").unwrap(), b);
        c.conn_closed(1, t);
        assert!(c.drain().contains(&Notice::Drained(a.id, 1)), "the find was a lease, and its end drains");
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

    /// §111.8: a region a restarted broker found again is an orphan of its
    /// user's pool under its old id; only that user adopts it, ids go on
    /// above it, and its bytes count.
    #[test]
    fn a_restored_region_is_an_orphan_its_user_adopts() {
        let mut c = core();
        let t = Instant::now();
        let info = RegionInfo { id: 41, pool: "gpu".into(), len: 2 << 20, epoch: 1 };
        c.restore_orphan(&info, User::sid("S-1-5-21-1-2-3-1001"), Release::OwnerFree, t);
        assert_eq!((c.stats().orphans, c.stats().region_bytes), (1, 2 << 20));
        c.connect(1, Peer { pid: 10, user: User::sid("S-1-5-21-1-2-3-1002"), admin: false });
        assert!(c.adopt(1, "gpu").unwrap().is_empty());
        c.connect(2, Peer { pid: 11, user: User::sid("S-1-5-21-1-2-3-1001"), admin: false });
        assert_eq!(c.adopt(2, "gpu").unwrap(), vec![info]);
        assert_eq!(c.alloc(2, "gpu", 4096, Release::OwnerFree).unwrap().id, 42);
        c.free(2, 41).unwrap();
        assert_eq!(c.stats().region_bytes, 4096);
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
