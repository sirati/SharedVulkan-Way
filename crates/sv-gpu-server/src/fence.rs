//! Fence records: implicit sync for the dma-buf path without anybody waiting
//! when a frame is handed on (waycompose-design.md §104.11).
//!
//! Each session that exports dma-buf sync fds has one **fence record**: a
//! 4 KiB named section this process creates (`Local\sv-fence-<pid>-<n>-<tag>`;
//! a `/dev/shm` file on Linux), holding
//!
//! ```text
//!   0  "SVFENCE1"
//!   8  completed: u64, little-endian, written with release ordering
//! ```
//!
//! The app side numbers the work it hands on — every `SYNC_FD` export of a
//! semaphore is `svFenceMark{queue, semaphore, seq}` on the queue ring, `seq`
//! counting up from 1 per session — and stores `seq` in the token's sync page
//! (`sv_drm::token`). Here the mark becomes an empty submit that waits on the
//! semaphore, with a fence; a watcher thread waits for the fences in order and
//! writes `completed = seq` into the record. Nothing waits for the GPU on the
//! application's thread.
//!
//! Who reads a record:
//!
//! * the **compositor**, which maps it by name, is told `(record, seq)` for
//!   each commit of a dma-buf buffer by `wc-proxy` (`wc_gpu_fence_v1`), and
//!   waits until `completed >= seq` before reading the section;
//! * **another session of this process** (KWin sampling a Plasma client's
//!   buffer): its `svFenceWait{memory, seq, record}` waits on the in-process
//!   [`Progress`] of the record, on that session's queue-ring thread, before
//!   its next submit.
//!
//! The number the consumer waits for comes from the producer's app side, which
//! assigned it synchronously before the frame could be committed, so a
//! consumer that sees the commit before this process has even dequeued the
//! mark still waits for the right work.

use crate::driver::Driver;
use ash::vk;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

/// A record's first eight bytes.
pub const RECORD_MAGIC: [u8; 8] = *b"SVFENCE1";
/// A record's size: one page.
pub const RECORD_LEN: u64 = 4096;
/// Where `completed` is.
pub const COMPLETED_OFFSET: usize = 8;
/// How long a consumer session waits for another session's work before it
/// says so and goes on: long enough for any frame, short enough that a
/// producer that died does not stop its consumers.
const CROSS_WAIT: Duration = Duration::from_secs(2);

/// How far a record has got, for waiters in this process.
pub struct Progress {
    completed: Mutex<u64>,
    cond: Condvar,
}

impl Progress {
    fn new() -> Progress {
        Progress { completed: Mutex::new(0), cond: Condvar::new() }
    }

    fn advance(&self, seq: u64) {
        let mut c = self.completed.lock().unwrap_or_else(|e| e.into_inner());
        if seq > *c {
            *c = seq;
        }
        self.cond.notify_all();
    }

    pub fn completed(&self) -> u64 {
        *self.completed.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait until `completed >= seq`, at most `limit`. Whether it got there.
    pub fn wait(&self, seq: u64, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        let mut c = self.completed.lock().unwrap_or_else(|e| e.into_inner());
        while *c < seq {
            let now = Instant::now();
            if now >= end {
                return false;
            }
            c = self.cond.wait_timeout(c, end - now).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }
}

/// Every live record of this process, by name.
static RECORDS: Mutex<Option<HashMap<String, Weak<Progress>>>> = Mutex::new(None);

fn records<R>(f: impl FnOnce(&mut HashMap<String, Weak<Progress>>) -> R) -> R {
    let mut g = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

/// The progress of record `name`, if a live session of this process owns it.
pub fn progress(name: &str) -> Option<Arc<Progress>> {
    records(|r| r.get(name).and_then(Weak::upgrade))
}

/// The mapped record, written by the watcher only.
struct RecordView {
    view: crate::shared::View,
}

impl RecordView {
    fn completed_cell(&self) -> &AtomicU64 {
        // SAFETY: the view is at least RECORD_LEN bytes, page aligned, and
        // lives as long as `self`; offset 8 is 8-byte aligned and only ever
        // accessed atomically here (readers in other processes only read).
        unsafe { &*(self.view.base().add(COMPLETED_OFFSET) as *const AtomicU64) }
    }

    fn publish(&self, seq: u64) {
        self.completed_cell().store(seq, Ordering::Release);
    }
}

struct Pending {
    seq: u64,
    fence: vk::Fence,
}

/// One session's record, its watcher and its fences.
pub struct SessionFence {
    pub name: String,
    progress: Arc<Progress>,
    driver: Arc<Driver>,
    queue_lock: Arc<Mutex<()>>,
    /// Signalled-and-reset fences ready for the next mark.
    free: Arc<Mutex<Vec<vk::Fence>>>,
    tx: Mutex<Option<mpsc::Sender<Pending>>>,
    watcher: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The highest number marked so far, for the log.
    marked: AtomicU64,
}

impl SessionFence {
    /// A new record and its watcher.
    pub fn create(driver: Arc<Driver>, queue_lock: Arc<Mutex<()>>) -> Result<Arc<SessionFence>, String> {
        let name = crate::shared::named("fence");
        let view = crate::shared::create_section(&name, RECORD_LEN)?;
        // SAFETY: the start of the writable view just created; nobody else
        // has the name yet.
        unsafe { std::ptr::copy_nonoverlapping(RECORD_MAGIC.as_ptr(), view.base(), 8) };
        let rec = RecordView { view };
        rec.publish(0);
        let progress = Arc::new(Progress::new());
        let free = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel::<Pending>();
        let watcher = {
            let driver = Arc::clone(&driver);
            let progress = Arc::clone(&progress);
            let free = Arc::clone(&free);
            let label = name.clone();
            std::thread::Builder::new()
                .name("sv-fence".into())
                .spawn(move || watch(&driver, &rec, &progress, &free, rx, &label))
                .map_err(|e| format!("the fence watcher thread: {e}"))?
        };
        records(|r| {
            r.retain(|_, w| w.strong_count() > 0);
            r.insert(name.clone(), Arc::downgrade(&progress));
        });
        Ok(Arc::new(SessionFence {
            name,
            progress,
            driver,
            queue_lock,
            free,
            tx: Mutex::new(Some(tx)),
            watcher: Mutex::new(Some(watcher)),
            marked: AtomicU64::new(0),
        }))
    }

    pub fn progress(&self) -> &Arc<Progress> {
        &self.progress
    }

    /// `svFenceMark`: an empty submit on `queue` that waits on `semaphore`,
    /// with a fence the watcher turns into `completed = seq`.
    ///
    /// No GPU work: the batch has no command buffers. It is what the export
    /// did before §104.11 too, minus the wait for it.
    pub fn mark(&self, queue: vk::Queue, semaphore: vk::Semaphore, seq: u64, section: Option<&str>) -> Result<(), String> {
        let fence = match self.free.lock().unwrap_or_else(|e| e.into_inner()).pop() {
            Some(f) => f,
            // SAFETY: a plain fence on the session's own device.
            None => unsafe { self.driver.device.create_fence(&vk::FenceCreateInfo::default(), None) }
                .map_err(|e| format!("vkCreateFence for a fence mark: {e:?}"))?,
        };
        let waits = [semaphore];
        let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        let si = vk::SubmitInfo::default().wait_semaphores(&waits).wait_dst_stage_mask(&stages);
        let r = {
            let _g = self.queue_lock.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: the session's queue and semaphore, under the queue lock;
            // the fence is unsignalled and not in use (fresh, or reset by the
            // watcher before it went back on the free list).
            unsafe { self.driver.device.queue_submit(queue, &[si], fence) }
        };
        if let Err(e) = r {
            self.free.lock().unwrap_or_else(|e| e.into_inner()).push(fence);
            // Nothing will ever signal this number now. Say it is done rather
            // than leave a consumer waiting for it: the device is most likely
            // lost, and what the consumer reads no longer matters.
            self.progress.advance(seq);
            return Err(format!("vkQueueSubmit of fence mark {seq}: {e:?}"));
        }
        if self.marked.fetch_max(seq, Ordering::Relaxed) == 0 {
            eprintln!(
                "[gpu] {}: the first sync fd export, work {seq}, marked without waiting, for {}",
                self.name,
                section.unwrap_or("no shared section")
            );
        }
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        match tx.as_ref() {
            Some(t) if t.send(Pending { seq, fence }).is_ok() => Ok(()),
            _ => Err("the fence watcher is gone".into()),
        }
    }
}

impl Drop for SessionFence {
    fn drop(&mut self) {
        records(|r| r.remove(&self.name));
        // Closing the channel ends the watcher once it has waited for what
        // was already marked.
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = self.watcher.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
        for f in self.free.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            // SAFETY: signalled and reset, no longer referenced by any submit.
            unsafe { self.driver.device.destroy_fence(f, None) };
        }
        #[cfg(target_os = "linux")]
        let _ = std::fs::remove_file(&self.name);
    }
}

/// The watcher: one fence at a time, in the order they were submitted on the
/// one queue, so `completed` only ever grows.
fn watch(
    driver: &Driver,
    rec: &RecordView,
    progress: &Progress,
    free: &Mutex<Vec<vk::Fence>>,
    rx: mpsc::Receiver<Pending>,
    label: &str,
) {
    let mut said = false;
    for p in rx {
        loop {
            // SAFETY: a fence of this device that a submit signals.
            let r = unsafe { driver.device.wait_for_fences(&[p.fence], true, 1_000_000_000) };
            match r {
                Ok(()) => break,
                Err(vk::Result::TIMEOUT) => continue,
                Err(e) => {
                    if !said {
                        eprintln!("[gpu] {label}: waiting for fence mark {}: {e:?}; counted as done", p.seq);
                        said = true;
                    }
                    break;
                }
            }
        }
        // SAFETY: signalled (or the device is lost); no submit holds it.
        let _ = unsafe { driver.device.reset_fences(&[p.fence]) };
        free.lock().unwrap_or_else(|e| e.into_inner()).push(p.fence);
        rec.publish(p.seq);
        progress.advance(p.seq);
    }
}

/// `svFenceWait` on the consumer's queue-ring thread: wait for another
/// session's work on a section before this session's next submit. `own` is
/// the consumer's own record, whose work its own queue already orders.
///
/// Waits for (1) `seq` of `record`, the number the producer's app side put
/// in the token when the frame was handed on, and (2) every mark another
/// session has made on `section` that this process has already seen — the
/// reads of a consumer, before a producer writes the buffer again. (2) is as
/// good as the order the marks arrive in; (1) is exact.
///
/// A record this process does not know (the producer's session has ended, or
/// it is not ours) is not waited for: its work is finished or will never be.
pub fn wait_foreign(own: Option<&str>, record: &str, seq: u64, section: Option<&str>) -> Vec<String> {
    let mut late = Vec::new();
    let mut targets: Vec<(String, u64)> = Vec::new();
    if !record.is_empty() && seq > 0 && Some(record) != own {
        targets.push((record.to_string(), seq));
    }
    if let Some(s) = section {
        for (r, n) in crate::shared::marks_of(s) {
            if Some(r.as_str()) != own && n > 0 {
                targets.push((r, n));
            }
        }
    }
    for (r, n) in targets {
        if let Some(p) = progress(&r)
            && !p.wait(n, CROSS_WAIT)
        {
            late.push(format!("{r} >= {n} (at {})", p.completed()));
        }
    }
    late
}


/// The session's record, made on first use.
fn session_fence(shared: &crate::server::Shared) -> Result<Arc<SessionFence>, String> {
    if let Some(f) = shared.objects().fence.clone() {
        return Ok(f);
    }
    let f = SessionFence::create(Arc::clone(&shared.driver), Arc::clone(&shared.queue_lock))?;
    let mut o = shared.objects();
    Ok(Arc::clone(o.fence.get_or_insert(f)))
}

/// The `svFenceRecord` arm of `session::protocol_command`.
pub fn serve_record(shared: &crate::server::Shared) -> sv_proto::sv::FenceRecordReply {
    match session_fence(shared) {
        Ok(f) => {
            eprintln!("[gpu] fence record {}", f.name);
            sv_proto::sv::FenceRecordReply { result: 0, name: f.name.clone() }
        }
        Err(e) => {
            eprintln!("[gpu] svFenceRecord refused: {e}");
            // VK_ERROR_INITIALIZATION_FAILED: the app side falls back to
            // waiting for its exports itself.
            sv_proto::sv::FenceRecordReply { result: -3, name: String::new() }
        }
    }
}

/// The `svFenceMark` arm. A failure is said and not fatal: the consumer of
/// this number is told it is done (see [`SessionFence::mark`]).
pub fn serve_mark(shared: &crate::server::Shared, d: sv_proto::sv::FenceMark) {
    let resolved = (|| {
        let f = shared.objects().fence.clone().ok_or("svFenceMark before svFenceRecord")?;
        let o = shared.objects();
        let queue = match o.get::<vk::Queue>(sv_proto::HandleType::VkQueue, d.queue) {
            Ok(q) if q != vk::Queue::null() => q,
            _ => shared.driver.queue,
        };
        let sem = o
            .get::<vk::Semaphore>(sv_proto::HandleType::VkSemaphore, d.semaphore)
            .map_err(|e| e.to_string())?;
        if sem == vk::Semaphore::null() {
            return Err("svFenceMark of no semaphore".to_string());
        }
        let section = o.shared_imports.get(&d.memory).map(|i| i.name.clone());
        Ok((f, queue, sem, section))
    })();
    match resolved {
        Ok((f, queue, sem, section)) => {
            if let Some(s) = &section {
                crate::shared::note_mark(s, &f.name, d.seq);
            }
            if let Err(e) = f.mark(queue, sem, d.seq, section.as_deref()) {
                eprintln!("[gpu] {e}");
            }
        }
        Err(e) => {
            eprintln!("[gpu] svFenceMark {}: {e}", d.seq);
            if let Some(f) = shared.objects().fence.clone() {
                f.progress.advance(d.seq);
            }
        }
    }
}

/// The `svFenceWait` arm, on the queue-ring thread: see [`wait_foreign`].
pub fn serve_wait(shared: &crate::server::Shared, d: sv_proto::sv::FenceWait) {
    let (own, section) = {
        let o = shared.objects();
        (o.fence.as_ref().map(|f| f.name.clone()), o.shared_imports.get(&d.memory).map(|i| i.name.clone()))
    };
    let t = Instant::now();
    let late = wait_foreign(own.as_deref(), &d.record, d.seq, section.as_deref());
    if !d.record.is_empty() && own.as_deref() != Some(d.record.as_str()) && d.seq > 0 {
        static SAID: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
        let first = SAID.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(d.record.clone());
        if first {
            eprintln!("[gpu] first wait on another session's work: {} >= {}, {:.2?}", d.record, d.seq, t.elapsed());
        }
    }
    if !late.is_empty() {
        eprintln!(
            "[gpu] svFenceWait for memory {}: gave up after {:.2?} on {}",
            d.memory,
            t.elapsed(),
            late.join(", ")
        );
    } else if t.elapsed() > Duration::from_millis(1) && sv_transport::frametrace::enabled() {
        sv_transport::frametrace::emit(
            "gs.fence_wait",
            format_args!("memory={} record={} seq={} us={}", d.memory, d.record, d.seq, t.elapsed().as_micros()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_only_grows_and_wakes_waiters() {
        let p = Arc::new(Progress::new());
        assert!(p.wait(0, Duration::ZERO));
        assert!(!p.wait(1, Duration::from_millis(10)));
        let q = Arc::clone(&p);
        let t = std::thread::spawn(move || q.wait(3, Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(20));
        p.advance(2);
        p.advance(3);
        assert!(t.join().unwrap());
        p.advance(1);
        assert_eq!(p.completed(), 3);
    }

    #[test]
    fn a_foreign_wait_finds_records_by_name_and_skips_its_own_and_unknown_ones() {
        let p = Arc::new(Progress::new());
        records(|r| r.insert("test-rec-a".into(), Arc::downgrade(&p)));
        p.advance(5);
        assert!(wait_foreign(None, "test-rec-a", 5, None).is_empty());
        let late = wait_foreign(None, "test-rec-a", 6, None);
        assert_eq!(late.len(), 1, "{late:?}");
        // Its own record, or one nobody owns: nothing to wait for.
        assert!(wait_foreign(Some("test-rec-a"), "test-rec-a", 99, None).is_empty());
        assert!(wait_foreign(None, "test-rec-none", 99, None).is_empty());
        drop(p);
        assert!(progress("test-rec-a").is_none());
        records(|r| r.remove("test-rec-a"));
    }
}
