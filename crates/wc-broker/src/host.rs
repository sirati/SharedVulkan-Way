//! The client role's daemon, as a component embedded in it sees it (§111.10).
//!
//! The session service on the Linux host (`wc-arena-host sessiond --broker`)
//! runs the broker in its own process and starts each guest session as the
//! Linux user users.conf maps it to. A component that publishes regions of
//! its own (the guest-RAM substrate, §108: `sv_ram::brokered`) runs in the
//! same process and needs two things from it: the [`Broker`] handle, and who
//! owns each live session, so that it publishes a session's regions for that
//! session's user and nobody else. This module is that interface:
//!
//! * [`Embedded::attach`] is called once, at the daemon's start, after the
//!   broker serves, with the broker and a [`SessionsView`];
//! * [`Embedded::session_up`] after a session has started as its user,
//!   [`Embedded::session_down`] after it has ended (its regions the daemon
//!   published are already unpublished then);
//! * [`SessionsView`] answers "whose is session ID / session index N" at any
//!   time, from any thread.
//!
//! The daemon's list of components is `embedded()` in wc-arena-host's
//! `sessiond_ivshmem.rs`; a component is added there.

use crate::core::User;
use crate::server::Broker;
use crate::sys::unix::Unix;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// One live session, as the daemon started it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSession {
    /// The boot record's session id.
    pub id: String,
    pub install: String,
    /// §107: its index in the guest's shared compositor (0: one session).
    pub index: u8,
    /// The compositor's run id (the arena header's).
    pub run: u64,
    /// The Linux user it runs as: what a region of this session is published for.
    pub user: User,
    pub uid: u32,
    /// The account name, when users.conf mapped it (`None`: the daemon's own user).
    pub linux_user: Option<String>,
}

/// The live sessions, shared between the daemon and its components.
#[derive(Clone, Default)]
pub struct SessionsView {
    inner: Arc<Mutex<BTreeMap<String, HostSession>>>,
}

impl SessionsView {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, HostSession>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Session `id`, if it is live.
    pub fn get(&self, id: &str) -> Option<HostSession> {
        self.lock().get(id).cloned()
    }

    /// The live sessions of run `run` at index `index` (normally one).
    pub fn by_index(&self, run: u64, index: u8) -> Vec<HostSession> {
        self.lock().values().filter(|s| s.run == run && s.index == index).cloned().collect()
    }

    /// Who owns session `id`: the user its regions are published for.
    pub fn owner(&self, id: &str) -> Option<User> {
        self.lock().get(id).map(|s| s.user.clone())
    }

    pub fn all(&self) -> Vec<HostSession> {
        self.lock().values().cloned().collect()
    }

    /// The daemon's: a session has started.
    pub fn insert(&self, s: HostSession) {
        self.lock().insert(s.id.clone(), s);
    }

    /// The daemon's: a session has ended.
    pub fn remove(&self, id: &str) -> Option<HostSession> {
        self.lock().remove(id)
    }
}

/// Where the guest's RAM is, for the guest-RAM substrate (§108).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QemuRam {
    /// `--guest-ram-qemu-pid PID`: QEMU's process, whose RAM memfd is found
    /// in its fds.
    Pid(u32),
    /// `--guest-ram-qemu-pidfile FILE`: the same, the pid read from a file.
    PidFile(PathBuf),
    /// `--guest-ram FILE`: the RAM's backing file itself.
    File(PathBuf),
}

/// Whose the guest-RAM regions are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuestRamUser {
    /// `--guest-ram-user UID`.
    Uid(u32),
    /// `--guest-ram-user mapped`: the Linux user users.conf maps the guest
    /// GPU process's user to ([`GuestRam::map_user`]).
    Mapped,
}

/// Maps a server-side identity (os, user, id) to the uid of the Linux user
/// users.conf gives it, whatever installation: this daemon's table.
pub type MapUser = Arc<dyn Fn(&str, &str, &str) -> Result<u32, String> + Send + Sync>;

/// The daemon's guest-RAM flags, handed to the component that serves them.
#[derive(Clone, Default)]
pub struct GuestRam {
    /// `--guest-ram-boot FILE`: where the guest's boot page for it is
    /// (default: the `--gpu-region` file).
    pub boot: Option<PathBuf>,
    pub qemu: Option<QemuRam>,
    /// `--guest-ram-bell SPEC`: its doorbell (a hub socket, `broker:NAME`).
    pub bell: Option<String>,
    pub user: Option<GuestRamUser>,
    /// users.conf, for [`GuestRamUser::Mapped`]; `None` without `--users`.
    pub map_user: Option<MapUser>,
}

impl GuestRam {
    /// Whether the guest-RAM substrate was asked for (`--guest-ram-*`).
    pub fn wanted(&self) -> bool {
        self.qemu.is_some()
    }
}

/// A component the daemon hosts beside the broker.
pub trait Embedded: Send + Sync + 'static {
    /// For the daemon's log.
    fn name(&self) -> &str;
    /// Once, at start, after the broker serves.
    fn attach(&self, broker: &Broker<Unix>, sessions: &SessionsView);
    /// A session has started as its user.
    fn session_up(&self, _s: &HostSession) {}
    /// A session has ended.
    fn session_down(&self, _s: &HostSession) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_answers_by_id_and_by_index() {
        let v = SessionsView::default();
        let s = |id: &str, index: u8, uid: u32| HostSession {
            id: id.into(),
            install: "wcivs".into(),
            index,
            run: 7,
            user: User::uid(uid),
            uid,
            linux_user: Some("desk".into()),
        };
        v.insert(s("a", 1, 1001));
        v.insert(s("b", 2, 1002));
        assert_eq!(v.owner("a"), Some(User::uid(1001)));
        assert_eq!(v.by_index(7, 2).len(), 1);
        assert!(v.by_index(8, 2).is_empty(), "another compositor run");
        assert_eq!(v.clone().remove("a").map(|s| s.uid), Some(1001), "clones share one view");
        assert_eq!(v.owner("a"), None);
        assert_eq!(v.all().len(), 1);
    }
}
