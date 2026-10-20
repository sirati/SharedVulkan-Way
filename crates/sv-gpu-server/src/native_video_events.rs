//! Canonical event provenance and owned lowering for native/video recordings.
//!
//! Admission projects accepted operations without claiming native completion.
//! Native event writes remain at their original command positions, including
//! inside active query scopes. Public status queries must read the real native
//! event, never this registry's projected state. Completion commits metadata
//! only after the actual native fence and any intervening decode have finished.
use crate::video_recording::Barrier;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use sv_proto as p;

const DEVICE_ONLY: u32 = 1;
const HOST: u64 = 0x4000;
const IGNORED: u32 = u32::MAX;
const ALL_TRANSFER_STAGES: u32 = 1 << 5;
const ASYMMETRIC: u32 = 1 << 6;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Error(pub &'static str);
type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct EventId {
    pub wire: u64,
    pub lifetime: u64,
}
#[derive(Clone, Debug, PartialEq)]
enum Signal {
    Host,
    Legacy {
        queue: u64,
        stage: u32,
    },
    Sync2 {
        queue: u64,
        dependency: Box<p::VkDependencyInfo>,
    },
}
#[derive(Clone, Debug, PartialEq)]
struct State {
    generation: u64,
    signal: Option<Signal>,
}
#[derive(Clone)]
struct Entry {
    id: EventId,
    flags: u32,
    actual: State,
    actual_revision: u64,
    projected: State,
    projected_revision: u64,
    quarantined: bool,
}
/// Fully owned calls, usable for ordinary CBs and captured secondary histories.
#[derive(Clone, Debug)]
pub(crate) enum Operation {
    Set(p::VkCmdSetEventArgs),
    Set2(p::VkCmdSetEvent2Args),
    Reset(p::VkCmdResetEventArgs),
    Reset2(p::VkCmdResetEvent2Args),
    Wait(p::VkCmdWaitEventsArgs),
    Wait2(p::VkCmdWaitEvents2Args),
}
impl Operation {
    pub(crate) fn from_recorded(call: &sv_gpu_entry::RecordedCall) -> Option<Self> {
        use sv_gpu_entry::RecordedCall as C;
        Some(match call {
            C::VkCmdSetEvent(a) => Self::Set((**a).clone()),
            C::VkCmdSetEvent2(a) => Self::Set2((**a).clone()),
            C::VkCmdResetEvent(a) => Self::Reset((**a).clone()),
            C::VkCmdResetEvent2(a) => Self::Reset2((**a).clone()),
            C::VkCmdWaitEvents(a) => Self::Wait((**a).clone()),
            C::VkCmdWaitEvents2(a) => Self::Wait2((**a).clone()),
            _ => return None,
        })
    }
    fn wires(&self) -> Vec<u64> {
        match self {
            Self::Set(a) => vec![a.event.0],
            Self::Set2(a) => vec![a.event.0],
            Self::Reset(a) => vec![a.event.0],
            Self::Reset2(a) => vec![a.event.0],
            Self::Wait(a) => a.p_events.iter().map(|e| e.0).collect(),
            Self::Wait2(a) => a.p_events.iter().map(|e| e.0).collect(),
        }
    }
}
/// Record these native calls exactly at the original event position. `Set2`
/// carries only an ALL_COMMANDS execution release: resource transitions are
/// in `before`, once.
/// The native GPU write provides observable prefix publication, not a delayed
/// host write at whole-job retirement. `Wait` replaces KK's no-op native wait.
#[derive(Clone, Debug)]
pub(crate) enum Lowered {
    NoOp,
    Set {
        event: EventId,
        before: Vec<Barrier>,
        write: p::VkCmdSetEvent2Args,
    },
    Reset {
        event: EventId,
        before: Vec<Barrier>,
        write: p::VkCmdResetEvent2Args,
    },
    Wait {
        events: Vec<EventId>,
        barriers: Vec<Barrier>,
    },
}
#[derive(Clone)]
struct Change {
    id: EventId,
    base: u64,
    next: u64,
    before: State,
    state: State,
}
/// Immutable admission result. No event status has been published by planning.
pub(crate) struct Plan {
    realm: Arc<()>,
    pub lowered: Vec<Lowered>,
    changes: Vec<Change>,
}
#[derive(Clone)]
pub(crate) struct Ticket {
    realm: Arc<()>,
    serial: u64,
}
struct Pending {
    serial: u64,
    changes: Vec<Change>,
    failed: bool,
}
/// One registry per logical VkDevice. All methods are serialized by its owner.
/// The bound must cover the owner's accepted-job bound, including all queues.
pub(crate) struct Events {
    realm: Arc<()>,
    next_lifetime: u64,
    next_ticket: u64,
    limit: usize,
    entries: HashMap<u64, Entry>,
    pending: Vec<Pending>,
}
/// Preflight tokens borrow the registry exclusively until native success or
/// failure. With the Events mutex held, their consuming commits cannot observe
/// a changed lifetime or revision and perform no fallible validation.
pub(crate) struct Create<'a> {
    events: &'a mut Events,
    wire: u64,
    flags: u32,
    id: EventId,
}
impl Create<'_> {
    pub(crate) fn commit(self) -> EventId {
        let state = State {
            generation: 1,
            signal: None,
        };
        self.events.entries.insert(
            self.wire,
            Entry {
                id: self.id,
                flags: self.flags,
                actual: state.clone(),
                projected: state,
                actual_revision: 0,
                projected_revision: 0,
                quarantined: false,
            },
        );
        self.id
    }
}
pub(crate) struct HostChange<'a> {
    events: &'a mut Events,
    id: EventId,
    state: State,
    revision: u64,
}
impl HostChange<'_> {
    pub(crate) fn commit(self) {
        let e = self.events.entries.get_mut(&self.id.wire).unwrap();
        e.actual = self.state.clone();
        e.projected = self.state;
        e.actual_revision = self.revision;
        e.projected_revision = self.revision;
    }
}
pub(crate) struct Destroy<'a> {
    events: &'a mut Events,
    id: EventId,
}
impl Destroy<'_> {
    pub(crate) fn commit(self) {
        self.events.entries.remove(&self.id.wire);
    }
}
impl Default for Events {
    fn default() -> Self {
        // Process bound: sixteen canonical executors, at most seventeen jobs
        // each. A narrower owner may supply its exact bound with Events::new.
        Self::new(16 * 17)
    }
}
impl Events {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            realm: Arc::new(()),
            next_lifetime: 1,
            next_ticket: 1,
            limit,
            entries: HashMap::new(),
            pending: Vec::new(),
        }
    }
    pub(crate) fn preflight_create(&mut self, wire: u64, flags: u32) -> Result<Create<'_>> {
        if wire == 0 || self.entries.contains_key(&wire) {
            return Err(Error("event already exists or is null"));
        }
        let lifetime = self.next_lifetime;
        self.next_lifetime = lifetime
            .checked_add(1)
            .ok_or(Error("event lifetime exhausted"))?;
        Ok(Create {
            events: self,
            wire,
            flags,
            id: EventId { wire, lifetime },
        })
    }
    pub(crate) fn create(&mut self, wire: u64, flags: u32) -> Result<EventId> {
        Ok(self.preflight_create(wire, flags)?.commit())
    }
    pub(crate) fn id(&self, wire: u64) -> Result<EventId> {
        Ok(self.entry(wire)?.id)
    }
    fn entry(&self, wire: u64) -> Result<&Entry> {
        let e = self
            .entries
            .get(&wire)
            .ok_or(Error("unknown event lifetime"))?;
        if e.quarantined {
            return Err(Error("event provenance quarantined"));
        }
        Ok(e)
    }
    /// Call only after legal native destruction. Pending event owners must not
    /// be destroyed; they require completion/quarantine retention by the owner.
    pub(crate) fn preflight_destroy(&mut self, id: EventId) -> Result<Destroy<'_>> {
        let e = self.entry(id.wire)?;
        if e.id != id || e.actual_revision != e.projected_revision {
            return Err(Error("event lifetime is pending or stale"));
        }
        Ok(Destroy { events: self, id })
    }
    pub(crate) fn destroy(&mut self, id: EventId) -> Result<()> {
        self.preflight_destroy(id)?.commit();
        Ok(())
    }
    pub(crate) fn host_id(&self, wire: u64) -> Result<EventId> {
        let e = self.entry(wire)?;
        if e.flags & DEVICE_ONLY != 0 {
            return Err(Error("device-only host event"));
        }
        Ok(e.id)
    }
    /// Hold the Events mutex and token across the native operation. Drop on
    /// failure; consume commit ONLY on native success. Earlier pending uses
    /// need real completion reconciliation before taking this token.
    pub(crate) fn preflight_host(&mut self, id: EventId, set: bool) -> Result<HostChange<'_>> {
        let e = self.entry(id.wire)?;
        if e.id != id || e.flags & DEVICE_ONLY != 0 {
            return Err(Error("stale or device-only host event"));
        }
        if e.actual_revision != e.projected_revision {
            return Err(Error(
                "host event mutation needs prior completion reconciliation",
            ));
        }
        let mut state = e.actual.clone();
        if set {
            if state.signal.is_none() {
                state.signal = Some(Signal::Host);
            }
        } else {
            reset(&mut state)?;
        }
        let revision = e
            .actual_revision
            .checked_add(1)
            .ok_or(Error("event revision exhausted"))?;
        Ok(HostChange {
            events: self,
            id,
            state,
            revision,
        })
    }
    /// Commit a successful native HOST operation, never a client status word.
    ///
    /// # Safety
    /// The native host call succeeded. All earlier accepted uses of this exact
    /// event have completed/reconciled under real native fences. Host signal
    /// snapshots must be committed BEFORE admission of CBs that wait on them.
    /// Host SetEvent after submission is not supported as a valid event signal.
    pub(crate) unsafe fn host_set(&mut self, id: EventId) -> Result<()> {
        self.host_change(id, true)
    }
    /// # Safety
    /// Same completion/native-success obligations as `host_set`.
    pub(crate) unsafe fn host_reset(&mut self, id: EventId) -> Result<()> {
        self.host_change(id, false)
    }
    fn host_change(&mut self, id: EventId, set: bool) -> Result<()> {
        self.preflight_host(id, set)?.commit();
        Ok(())
    }
    /// Snapshot HOST signals and prior accepted projections at admission. No
    /// future native status or HOST operation is substituted during execution.
    pub(crate) fn plan(&self, queue: u64, operations: &[Operation]) -> Result<Plan> {
        if queue == 0 {
            return Err(Error("null event queue"));
        }
        let mut states = HashMap::<u64, Entry>::new();
        for op in operations {
            for wire in op.wires() {
                states.entry(wire).or_insert(self.entry(wire)?.clone());
            }
        }
        let bases: HashMap<_, _> = states
            .iter()
            .map(|(&wire, e)| (wire, (e.id, e.projected_revision, e.projected.clone())))
            .collect();
        let mut lowered = Vec::with_capacity(operations.len());
        for op in operations {
            lowered.push(lower(queue, op, &mut states)?);
        }
        let mut changes = Vec::with_capacity(states.len());
        for (wire, e) in states {
            let (id, base, ref before) = bases[&wire];
            let next = base
                .checked_add(1)
                .ok_or(Error("event revision exhausted"))?;
            changes.push(Change {
                id,
                base,
                next,
                before: before.clone(),
                state: e.projected,
            });
        }
        changes.sort_by_key(|c| c.id.wire);
        Ok(Plan {
            realm: Arc::clone(&self.realm),
            lowered,
            changes,
        })
    }
    /// Reserve atomically before accepting the job. A stale pre-accept plan
    /// must be rebuilt, never silently rebound to another event incarnation.
    pub(crate) fn reserve(&mut self, plan: &Plan) -> Result<Ticket> {
        if !Arc::ptr_eq(&plan.realm, &self.realm) {
            return Err(Error("foreign event plan"));
        }
        if self.pending.len() >= self.limit {
            return Err(Error("event reservation queue full"));
        }
        for c in &plan.changes {
            let e = self.entry(c.id.wire)?;
            if e.id != c.id || e.projected_revision != c.base {
                return Err(Error("stale event admission plan"));
            }
        }
        let serial = self.next_ticket;
        self.next_ticket = serial
            .checked_add(1)
            .ok_or(Error("event ticket exhausted"))?;
        for c in &plan.changes {
            let e = self.entries.get_mut(&c.id.wire).unwrap();
            e.projected = c.state.clone();
            e.projected_revision = c.next;
        }
        self.pending.push(Pending {
            serial,
            changes: plan.changes.clone(),
            failed: false,
        });
        Ok(Ticket {
            realm: Arc::clone(&self.realm),
            serial,
        })
    }
    /// Release a preparation rejected BEFORE native/job acceptance.
    ///
    /// # Safety
    /// No native command or synthetic action from this ticket was accepted or
    /// executed. Caller serializes reserve+enqueue. Later dependent accepted
    /// jobs forbid rollback and must be failed/quarantined conservatively.
    pub(crate) unsafe fn cancel_unaccepted(&mut self, ticket: &Ticket) -> Result<()> {
        let at = self.ticket_index(ticket)?;
        let pending = &self.pending[at];
        if pending.failed {
            return Err(Error("failed event reservation cannot roll back"));
        }
        for c in &pending.changes {
            let e = self.entry(c.id.wire)?;
            if e.id != c.id || e.projected_revision != c.next || e.actual_revision >= c.next {
                return Err(Error(
                    "event reservation has later dependents or completed effects",
                ));
            }
        }
        let pending = self.pending.remove(at);
        for c in pending.changes {
            let e = self.entries.get_mut(&c.id.wire).unwrap();
            e.projected_revision = c.base;
            e.projected = c.before;
        }
        Ok(())
    }
    /// Metadata completion is not native publication. A GPU event write must
    /// already have executed at its original prefix, not be delayed until here.
    ///
    /// # Safety
    /// Every native event write/barrier and intervening synthetic decode in
    /// this reservation has truly completed, proved by real native fences and
    /// producer completion. Failure/cancel without that proof calls `fail`.
    pub(crate) unsafe fn complete(&mut self, ticket: &Ticket) -> Result<()> {
        let at = self.ticket_index(ticket)?;
        let p = &self.pending[at];
        if p.failed {
            return Err(Error(
                "failed event reservation requires quarantine retention",
            ));
        }
        for c in &p.changes {
            let e = self.entry(c.id.wire)?;
            if e.id != c.id || e.actual_revision != c.base {
                return Err(Error("event completion is out of order"));
            }
        }
        let p = self.pending.remove(at);
        for c in p.changes {
            let e = self.entries.get_mut(&c.id.wire).unwrap();
            e.actual = c.state;
            e.actual_revision = c.next;
        }
        Ok(())
    }
    /// A failed/cancelled accepted prefix cannot be rolled back as if native
    /// work never happened. Transitively quarantine dependent reservations.
    pub(crate) fn fail(&mut self, ticket: &Ticket) -> Result<Vec<EventId>> {
        let at = self.ticket_index(ticket)?;
        let mut affected: HashSet<_> = self.pending[at].changes.iter().map(|c| c.id).collect();
        loop {
            let before = affected.len();
            for p in &mut self.pending[at..] {
                if p.serial == ticket.serial || p.changes.iter().any(|c| affected.contains(&c.id)) {
                    p.failed = true;
                    affected.extend(p.changes.iter().map(|c| c.id));
                }
            }
            if affected.len() == before {
                break;
            }
        }
        for id in &affected {
            if let Some(e) = self.entries.get_mut(&id.wire) {
                if e.id == *id {
                    e.quarantined = true;
                }
            }
        }
        let mut out: Vec<_> = affected.into_iter().collect();
        out.sort_by_key(|id| id.wire);
        Ok(out)
    }
    fn ticket_index(&self, ticket: &Ticket) -> Result<usize> {
        if !Arc::ptr_eq(&ticket.realm, &self.realm) {
            return Err(Error("foreign event ticket"));
        }
        self.pending
            .iter()
            .position(|p| p.serial == ticket.serial)
            .ok_or(Error("stale event ticket"))
    }
}
fn reset(state: &mut State) -> Result<()> {
    state.generation = state
        .generation
        .checked_add(1)
        .ok_or(Error("event reset generation exhausted"))?;
    state.signal = None;
    Ok(())
}
fn check_dependency(d: &p::VkDependencyInfo) -> Result<()> {
    if !d.p_next.is_empty()
        || d.dependency_flags.0 & !(ALL_TRANSFER_STAGES | ASYMMETRIC) != 0
        || d.memory_barrier_count as usize != d.p_memory_barriers.len()
        || d.buffer_memory_barrier_count as usize != d.p_buffer_memory_barriers.len()
        || d.image_memory_barrier_count as usize != d.p_image_memory_barriers.len()
        || d.p_memory_barriers.iter().any(|b| !b.p_next.is_empty())
        || d.p_buffer_memory_barriers
            .iter()
            .any(|b| !b.p_next.is_empty())
        || d.p_image_memory_barriers
            .iter()
            .any(|b| !b.p_next.is_empty())
    {
        return Err(Error("unsupported event dependency extension or count"));
    }
    Ok(())
}
fn source_stages(d: &p::VkDependencyInfo) -> u64 {
    d.p_memory_barriers
        .iter()
        .map(|b| b.src_stage_mask.0)
        .chain(
            d.p_buffer_memory_barriers
                .iter()
                .map(|b| b.src_stage_mask.0),
        )
        .chain(d.p_image_memory_barriers.iter().map(|b| b.src_stage_mask.0))
        .fold(0, |a, b| a | b)
}
fn set_dependency(d: &p::VkDependencyInfo) -> Result<()> {
    if d.dependency_flags.0 == 0 {
        return Ok(());
    }
    if d.dependency_flags.0 != ASYMMETRIC
        || d.p_memory_barriers.len() != 1
        || !d.p_buffer_memory_barriers.is_empty()
        || !d.p_image_memory_barriers.is_empty()
    {
        return Err(Error("invalid asymmetric event signal dependency"));
    }
    let b = &d.p_memory_barriers[0];
    if b.src_access_mask.0 != 0 || b.dst_stage_mask.0 != 0 || b.dst_access_mask.0 != 0 {
        return Err(Error("asymmetric event signal has non-source-stage fields"));
    }
    Ok(())
}
fn physical_dependency(mut d: p::VkDependencyInfo) -> p::VkDependencyInfo {
    d.dependency_flags.0 &= !ASYMMETRIC;
    d
}
fn host_only(d: &p::VkDependencyInfo) -> bool {
    source_stages(d) == HOST
}
fn sync2_barrier(command: p::VkCommandBuffer, d: p::VkDependencyInfo) -> Barrier {
    Barrier::Sync2(p::VkCmdPipelineBarrier2Args {
        command_buffer: command,
        p_dependency_info: Some(Box::new(d)),
    })
}
/// Set-side full barrier already executed each transition/ownership transfer.
/// The wait retains visibility scopes and ranges, without repeating either.
fn wait_dependency(mut d: p::VkDependencyInfo) -> p::VkDependencyInfo {
    for b in &mut d.p_image_memory_barriers {
        b.old_layout = b.new_layout;
        b.src_queue_family_index = IGNORED;
        b.dst_queue_family_index = IGNORED;
    }
    for b in &mut d.p_buffer_memory_barriers {
        b.src_queue_family_index = IGNORED;
        b.dst_queue_family_index = IGNORED;
    }
    d
}
fn set_release(mut d: p::VkDependencyInfo) -> p::VkDependencyInfo {
    d = physical_dependency(d);
    for b in &mut d.p_memory_barriers {
        b.dst_stage_mask = p::VkPipelineStageFlags2(0x10000);
    }
    for b in &mut d.p_buffer_memory_barriers {
        b.dst_stage_mask = p::VkPipelineStageFlags2(0x10000);
    }
    for b in &mut d.p_image_memory_barriers {
        b.dst_stage_mask = p::VkPipelineStageFlags2(0x10000);
    }
    d
}
fn execution_barrier(command: p::VkCommandBuffer, source: u64) -> Barrier {
    sync2_barrier(
        command,
        p::VkDependencyInfo {
            memory_barrier_count: 1,
            p_memory_barriers: vec![p::VkMemoryBarrier2 {
                src_stage_mask: p::VkPipelineStageFlags2(source),
                dst_stage_mask: p::VkPipelineStageFlags2(0x10000),
                ..Default::default()
            }],
            ..Default::default()
        },
    )
}
fn set_write(command: p::VkCommandBuffer, event: p::VkEvent) -> p::VkCmdSetEvent2Args {
    p::VkCmdSetEvent2Args {
        command_buffer: command,
        event,
        p_dependency_info: Some(Box::new(p::VkDependencyInfo {
            memory_barrier_count: 1,
            p_memory_barriers: vec![p::VkMemoryBarrier2 {
                src_stage_mask: p::VkPipelineStageFlags2(0x10000),
                ..Default::default()
            }],
            ..Default::default()
        })),
    }
}
fn lower(queue: u64, op: &Operation, states: &mut HashMap<u64, Entry>) -> Result<Lowered> {
    match op {
        Operation::Set(a) => {
            let e = states.get_mut(&a.event.0).unwrap();
            if e.projected.signal.is_some() {
                return Ok(Lowered::NoOp);
            }
            if u64::from(a.stage_mask.0) & HOST != 0 {
                return Err(Error("device event signal includes HOST"));
            }
            e.projected.signal = Some(Signal::Legacy {
                queue,
                stage: a.stage_mask.0,
            });
            Ok(Lowered::Set {
                event: e.id,
                before: vec![Barrier::Legacy(p::VkCmdPipelineBarrierArgs {
                    command_buffer: a.command_buffer,
                    src_stage_mask: a.stage_mask,
                    dst_stage_mask: p::VkPipelineStageFlags(0x10000),
                    ..Default::default()
                })],
                write: set_write(a.command_buffer, a.event),
            })
        }
        Operation::Set2(a) => {
            let d = a
                .p_dependency_info
                .as_deref()
                .ok_or(Error("missing event dependency"))?;
            check_dependency(d)?;
            set_dependency(d)?;
            let e = states.get_mut(&a.event.0).unwrap();
            if e.projected.signal.is_some() {
                return Ok(Lowered::NoOp);
            }
            if d.p_memory_barriers
                .iter()
                .any(|b| (b.src_stage_mask.0 | b.dst_stage_mask.0) & HOST != 0)
                || d.p_buffer_memory_barriers
                    .iter()
                    .any(|b| (b.src_stage_mask.0 | b.dst_stage_mask.0) & HOST != 0)
                || d.p_image_memory_barriers
                    .iter()
                    .any(|b| (b.src_stage_mask.0 | b.dst_stage_mask.0) & HOST != 0)
            {
                return Err(Error("device event dependency includes HOST"));
            }
            e.projected.signal = Some(Signal::Sync2 {
                queue,
                dependency: Box::new(d.clone()),
            });
            Ok(Lowered::Set {
                event: e.id,
                before: vec![sync2_barrier(a.command_buffer, set_release(d.clone()))],
                write: set_write(a.command_buffer, a.event),
            })
        }
        Operation::Reset(a) => {
            if u64::from(a.stage_mask.0) & HOST != 0 {
                return Err(Error("device event reset includes HOST"));
            }
            let e = states.get_mut(&a.event.0).unwrap();
            reset(&mut e.projected)?;
            Ok(Lowered::Reset {
                event: e.id,
                before: vec![execution_barrier(
                    a.command_buffer,
                    u64::from(a.stage_mask.0),
                )],
                write: p::VkCmdResetEvent2Args {
                    command_buffer: a.command_buffer,
                    event: a.event,
                    stage_mask: p::VkPipelineStageFlags2(0x10000),
                },
            })
        }
        Operation::Reset2(a) => {
            if a.stage_mask.0 & HOST != 0 {
                return Err(Error("device event reset includes HOST"));
            }
            let e = states.get_mut(&a.event.0).unwrap();
            reset(&mut e.projected)?;
            Ok(Lowered::Reset {
                event: e.id,
                before: vec![execution_barrier(a.command_buffer, a.stage_mask.0)],
                write: p::VkCmdResetEvent2Args {
                    command_buffer: a.command_buffer,
                    event: a.event,
                    stage_mask: p::VkPipelineStageFlags2(0x10000),
                },
            })
        }
        Operation::Wait(a) => {
            if a.event_count as usize != a.p_events.len() || a.p_events.is_empty() {
                return Err(Error("invalid legacy event wait count"));
            }
            let mut stages = 0u32;
            let mut events = Vec::new();
            for event in &a.p_events {
                let e = &states[&event.0];
                events.push(e.id);
                match e
                    .projected
                    .signal
                    .as_ref()
                    .ok_or(Error("event wait has no qualifying signal"))?
                {
                    Signal::Host if e.flags & DEVICE_ONLY == 0 => stages |= HOST as u32,
                    Signal::Legacy { queue: q, stage } if *q == queue => stages |= stage,
                    _ => return Err(Error("legacy event wait has wrong queue or signal kind")),
                }
            }
            if stages != a.src_stage_mask.0 {
                return Err(Error(
                    "legacy event wait source stages differ from signal union",
                ));
            }
            let barrier = p::VkCmdPipelineBarrierArgs {
                command_buffer: a.command_buffer,
                src_stage_mask: a.src_stage_mask,
                dst_stage_mask: a.dst_stage_mask,
                dependency_flags: p::VkDependencyFlags(0),
                memory_barrier_count: a.memory_barrier_count,
                p_memory_barriers: a.p_memory_barriers.clone(),
                buffer_memory_barrier_count: a.buffer_memory_barrier_count,
                p_buffer_memory_barriers: a.p_buffer_memory_barriers.clone(),
                image_memory_barrier_count: a.image_memory_barrier_count,
                p_image_memory_barriers: a.p_image_memory_barriers.clone(),
            };
            Ok(Lowered::Wait {
                events,
                barriers: vec![Barrier::Legacy(barrier)],
            })
        }
        Operation::Wait2(a) => {
            if a.event_count as usize != a.p_events.len()
                || a.p_events.len() != a.p_dependency_infos.len()
                || a.p_events.is_empty()
            {
                return Err(Error("invalid sync2 event wait count"));
            }
            let mut barriers = Vec::new();
            let mut events = Vec::new();
            for (event, d) in a.p_events.iter().zip(&a.p_dependency_infos) {
                check_dependency(d)?;
                let e = &states[&event.0];
                events.push(e.id);
                let dependency = match e
                    .projected
                    .signal
                    .as_ref()
                    .ok_or(Error("event wait has no qualifying signal"))?
                {
                    Signal::Host
                        if e.flags & DEVICE_ONLY == 0
                            && host_only(d)
                            && d.dependency_flags.0 & ASYMMETRIC == 0 =>
                    {
                        d.clone()
                    }
                    Signal::Sync2 {
                        queue: q,
                        dependency,
                    } if *q == queue => {
                        if dependency.dependency_flags.0 & ASYMMETRIC != 0 {
                            if d.dependency_flags.0 & ASYMMETRIC == 0
                                || source_stages(d) != source_stages(dependency)
                            {
                                return Err(Error("asymmetric event wait source stages differ"));
                            }
                            // Asymmetric Set had no resource barriers. Its wait
                            // performs the original transitions/transfers once.
                            physical_dependency(d.clone())
                        } else if dependency.as_ref() == d {
                            wait_dependency(d.clone())
                        } else {
                            return Err(Error("event wait dependency differs from signal"));
                        }
                    }
                    _ => {
                        return Err(Error(
                            "sync2 wait has wrong queue, dependency or signal kind",
                        ));
                    }
                };
                barriers.push(sync2_barrier(a.command_buffer, dependency));
            }
            Ok(Lowered::Wait { events, barriers })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn set(event: u64) -> Operation {
        Operation::Set(p::VkCmdSetEventArgs {
            command_buffer: p::VkCommandBuffer(7),
            event: p::VkEvent(event),
            stage_mask: p::VkPipelineStageFlags(0x1000),
        })
    }
    fn reset_op(event: u64) -> Operation {
        Operation::Reset2(p::VkCmdResetEvent2Args {
            command_buffer: p::VkCommandBuffer(7),
            event: p::VkEvent(event),
            stage_mask: p::VkPipelineStageFlags2(0x10000),
        })
    }
    fn dep(layout: i32) -> p::VkDependencyInfo {
        p::VkDependencyInfo {
            image_memory_barrier_count: 1,
            p_image_memory_barriers: vec![p::VkImageMemoryBarrier2 {
                src_stage_mask: p::VkPipelineStageFlags2(0x1000),
                dst_stage_mask: p::VkPipelineStageFlags2(0x1000),
                src_access_mask: p::VkAccessFlags2(0x1000),
                dst_access_mask: p::VkAccessFlags2(0x800),
                old_layout: p::VkImageLayout(0),
                new_layout: p::VkImageLayout(layout),
                image: p::VkImage(99),
                src_queue_family_index: 0,
                dst_queue_family_index: 1,
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    fn set2(event: u64, d: p::VkDependencyInfo) -> Operation {
        Operation::Set2(p::VkCmdSetEvent2Args {
            command_buffer: p::VkCommandBuffer(7),
            event: p::VkEvent(event),
            p_dependency_info: Some(Box::new(d)),
        })
    }
    fn wait2(event: u64, d: p::VkDependencyInfo) -> Operation {
        Operation::Wait2(p::VkCmdWaitEvents2Args {
            command_buffer: p::VkCommandBuffer(7),
            event_count: 1,
            p_events: vec![p::VkEvent(event)],
            p_dependency_infos: vec![d],
        })
    }
    fn wait(event: u64, src: u32) -> Operation {
        Operation::Wait(p::VkCmdWaitEventsArgs {
            command_buffer: p::VkCommandBuffer(7),
            event_count: 1,
            p_events: vec![p::VkEvent(event)],
            src_stage_mask: p::VkPipelineStageFlags(src),
            dst_stage_mask: p::VkPipelineStageFlags(0x1000),
            ..Default::default()
        })
    }
    #[test]
    fn admission_projects_without_claiming_completion_and_chains_jobs() {
        let mut events = Events::new(3);
        let id = events.create(1, 0).unwrap();
        let plan = events.plan(10, &[set(1)]).unwrap();
        let first = events.reserve(&plan).unwrap();
        assert!(events.entries[&1].actual.signal.is_none());
        assert!(events.entries[&1].projected.signal.is_some());
        let next = events.plan(10, &[wait(1, 0x1000)]).unwrap();
        let second = events.reserve(&next).unwrap();
        // SAFETY: injected native-fence completion in a pure state fixture.
        assert!(unsafe { events.complete(&second) }.is_err());
        assert!(unsafe { events.host_reset(id) }.is_err());
        unsafe { events.complete(&first) }.unwrap();
        unsafe { events.complete(&second) }.unwrap();
        assert!(events.entries[&1].actual.signal.is_some());
        assert!(unsafe { events.complete(&first) }.is_err());
    }
    #[test]
    fn host_signal_must_be_in_admission_snapshot_and_respect_device_only() {
        let mut events = Events::new(4);
        let host = events.create(1, 0).unwrap();
        let device = events.create(2, DEVICE_ONLY).unwrap();
        assert!(events.plan(10, &[wait(1, HOST as u32)]).is_err());
        // SAFETY: fixture represents native host SetEvent success before submit.
        unsafe { events.host_set(host) }.unwrap();
        assert!(events.plan(10, &[wait(1, HOST as u32)]).is_ok());
        assert!(unsafe { events.host_set(device) }.is_err());
        let mut d = p::VkDependencyInfo {
            memory_barrier_count: 1,
            p_memory_barriers: vec![p::VkMemoryBarrier2 {
                src_stage_mask: p::VkPipelineStageFlags2(HOST),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(events.plan(10, &[wait2(1, d.clone())]).is_ok());
        d.p_memory_barriers[0].src_stage_mask.0 |= 0x1000;
        assert!(events.plan(10, &[wait2(1, d)]).is_err());
    }
    #[test]
    fn set2_publication_follows_transition_and_wait_does_not_repeat_it() {
        let mut events = Events::new(3);
        events.create(1, 0).unwrap();
        let d = dep(1);
        let plan = events
            .plan(10, &[set2(1, d.clone()), wait2(1, d.clone())])
            .unwrap();
        let Lowered::Set { before, write, .. } = &plan.lowered[0] else {
            panic!("set")
        };
        let Barrier::Sync2(signal) = &before[0] else {
            panic!("barrier")
        };
        assert_eq!(
            signal.p_dependency_info.as_deref(),
            Some(&set_release(d.clone()))
        );
        let publication = write.p_dependency_info.as_ref().unwrap();
        assert_eq!(publication.memory_barrier_count, 1);
        assert_eq!(publication.p_memory_barriers[0].src_stage_mask.0, 0x10000);
        assert!(publication.p_image_memory_barriers.is_empty());
        let Lowered::Wait { barriers, .. } = &plan.lowered[1] else {
            panic!("wait")
        };
        let Barrier::Sync2(wait) = &barriers[0] else {
            panic!("barrier")
        };
        let b = &wait
            .p_dependency_info
            .as_ref()
            .unwrap()
            .p_image_memory_barriers[0];
        assert_eq!(b.old_layout, b.new_layout);
        assert_eq!(b.new_layout, p::VkImageLayout(1));
        assert_eq!(b.src_queue_family_index, IGNORED);
        assert_eq!(b.dst_queue_family_index, IGNORED);
        assert_eq!(
            b.src_access_mask,
            d.p_image_memory_barriers[0].src_access_mask
        );
        assert_eq!(
            b.dst_access_mask,
            d.p_image_memory_barriers[0].dst_access_mask
        );
    }
    #[test]
    fn already_set_sync2_preserves_last_qualifying_dependency() {
        let mut events = Events::new(2);
        events.create(1, 0).unwrap();
        let first = dep(1);
        let ignored = dep(2);
        let plan = events
            .plan(
                10,
                &[
                    set2(1, first.clone()),
                    set2(1, ignored.clone()),
                    wait2(1, first),
                ],
            )
            .unwrap();
        assert!(matches!(plan.lowered[1], Lowered::NoOp));
        assert!(
            events
                .plan(
                    10,
                    &[set2(1, dep(1)), set2(1, ignored.clone()), wait2(1, ignored)]
                )
                .is_err()
        );
    }
    #[test]
    fn reset_generation_cross_queue_and_signal_kind_are_not_interchangeable() {
        let mut events = Events::new(4);
        events.create(1, 0).unwrap();
        let plan = events.plan(10, &[set(1)]).unwrap();
        let ticket = events.reserve(&plan).unwrap();
        assert!(events.plan(11, &[wait(1, 0x1000)]).is_err());
        assert!(
            events
                .plan(10, &[wait2(1, p::VkDependencyInfo::default())])
                .is_err()
        );
        assert!(events.plan(10, &[wait(1, 0x2000)]).is_err());
        let plan = events.plan(10, &[reset_op(1), set2(1, dep(1))]).unwrap();
        let reset = events.reserve(&plan).unwrap();
        assert_eq!(events.entries[&1].projected.generation, 2);
        unsafe { events.complete(&ticket) }.unwrap();
        unsafe { events.complete(&reset) }.unwrap();
        assert!(matches!(
            events.entries[&1].actual.signal,
            Some(Signal::Sync2 { .. })
        ));
    }
    #[test]
    fn stale_lifetimes_plans_and_foreign_tickets_cannot_commit() {
        let mut events = Events::new(4);
        let old = events.create(1, 0).unwrap();
        let stale = events.plan(10, &[set(1)]).unwrap();
        events.destroy(old).unwrap();
        let fresh = events.create(1, 0).unwrap();
        assert_ne!(old, fresh);
        assert!(events.reserve(&stale).is_err());
        let mut other = Events::new(4);
        other.create(1, 0).unwrap();
        let plan = events.plan(10, &[set(1)]).unwrap();
        assert!(other.reserve(&plan).is_err());
        let ticket = events.reserve(&plan).unwrap();
        assert!(unsafe { other.complete(&ticket) }.is_err());
    }
    #[test]
    fn failure_quarantines_dependent_reservations_and_preserves_other_events() {
        let mut events = Events::new(4);
        for wire in 1..=3 {
            events.create(wire, 0).unwrap();
        }
        let a = events.plan(10, &[set(1)]).unwrap();
        let a = events.reserve(&a).unwrap();
        let b = events.plan(10, &[wait(1, 0x1000), set(2)]).unwrap();
        let b = events.reserve(&b).unwrap();
        let c = events.plan(11, &[set(3)]).unwrap();
        let c = events.reserve(&c).unwrap();
        let ids = events.fail(&a).unwrap();
        assert_eq!(ids.iter().map(|id| id.wire).collect::<Vec<_>>(), vec![1, 2]);
        assert!(events.plan(10, &[reset_op(2)]).is_err());
        assert!(unsafe { events.complete(&b) }.is_err());
        unsafe { events.complete(&c) }.unwrap();
        assert!(events.entries[&3].actual.signal.is_some());
    }
    #[test]
    fn unaccepted_rollback_restores_projection_but_not_later_dependencies() {
        let mut events = Events::new(3);
        events.create(1, 0).unwrap();
        let plan = events.plan(10, &[set(1)]).unwrap();
        let first = events.reserve(&plan).unwrap();
        let plan = events.plan(10, &[wait(1, 0x1000)]).unwrap();
        let second = events.reserve(&plan).unwrap();
        // SAFETY: fixture enqueue refusal, neither reservation ran native work.
        assert!(unsafe { events.cancel_unaccepted(&first) }.is_err());
        unsafe { events.cancel_unaccepted(&second) }.unwrap();
        unsafe { events.cancel_unaccepted(&first) }.unwrap();
        assert_eq!(events.entries[&1].projected_revision, 0);
        assert!(events.entries[&1].projected.signal.is_none());
        assert!(events.pending.is_empty());
    }
    #[test]
    fn legacy_set_keeps_its_execution_scope_before_native_publication() {
        let mut events = Events::new(1);
        events.create(1, 0).unwrap();
        let plan = events.plan(10, &[set(1)]).unwrap();
        let Lowered::Set { before, .. } = &plan.lowered[0] else {
            panic!("set")
        };
        let Barrier::Legacy(barrier) = &before[0] else {
            panic!("legacy release")
        };
        assert_eq!(barrier.src_stage_mask.0, 0x1000);
        assert_eq!(barrier.dst_stage_mask.0, 0x10000);
    }
    #[test]
    fn host_preflight_drop_has_no_native_success_effect_and_commit_is_exact() {
        let mut events = Events::default();
        drop(events.preflight_create(1, 0).unwrap());
        assert!(events.id(1).is_err());
        let id = events.preflight_create(1, 0).unwrap().commit();
        drop(events.preflight_host(id, true).unwrap());
        assert!(events.entries[&1].actual.signal.is_none());
        events.preflight_host(id, true).unwrap().commit();
        assert!(matches!(
            events.entries[&1].actual.signal,
            Some(Signal::Host)
        ));
        drop(events.preflight_destroy(id).unwrap());
        assert_eq!(events.id(1).unwrap(), id);
        events.preflight_destroy(id).unwrap().commit();
        assert!(events.id(1).is_err());
    }
    #[test]
    fn completion_validates_every_event_before_any_metadata_mutation() {
        let mut events = Events::new(2);
        events.create(1, 0).unwrap();
        events.create(2, 0).unwrap();
        let plan = events.plan(10, &[set(1), set(2)]).unwrap();
        let ticket = events.reserve(&plan).unwrap();
        events.entries.get_mut(&2).unwrap().quarantined = true;
        // SAFETY: simulated true native completion; metadata refuses quarantine.
        assert!(unsafe { events.complete(&ticket) }.is_err());
        assert_eq!(events.entries[&1].actual_revision, 0);
        assert!(events.entries[&1].actual.signal.is_none());
        assert_eq!(events.pending.len(), 1);
    }
    #[test]
    fn asymmetric_event_transitions_once_at_wait_and_preserves_stage_match() {
        let mut events = Events::new(2);
        events.create(1, 0).unwrap();
        let signal = p::VkDependencyInfo {
            dependency_flags: p::VkDependencyFlags(ASYMMETRIC),
            memory_barrier_count: 1,
            p_memory_barriers: vec![p::VkMemoryBarrier2 {
                src_stage_mask: p::VkPipelineStageFlags2(0x1000),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut waited = dep(1);
        waited.dependency_flags = p::VkDependencyFlags(ASYMMETRIC | ALL_TRANSFER_STAGES);
        let plan = events
            .plan(10, &[set2(1, signal.clone()), wait2(1, waited.clone())])
            .unwrap();
        let Lowered::Set { before, .. } = &plan.lowered[0] else {
            panic!("signal")
        };
        let Barrier::Sync2(before) = &before[0] else {
            panic!("barrier")
        };
        let before = before.p_dependency_info.as_ref().unwrap();
        assert_eq!(before.dependency_flags.0, 0);
        assert!(before.p_image_memory_barriers.is_empty());
        let Lowered::Wait { barriers, .. } = &plan.lowered[1] else {
            panic!("wait")
        };
        let Barrier::Sync2(wait) = &barriers[0] else {
            panic!("barrier")
        };
        let wait = wait.p_dependency_info.as_ref().unwrap();
        assert_eq!(wait.dependency_flags.0, ALL_TRANSFER_STAGES);
        assert_eq!(
            wait.p_image_memory_barriers[0].old_layout,
            p::VkImageLayout(0)
        );
        assert_eq!(
            wait.p_image_memory_barriers[0].new_layout,
            p::VkImageLayout(1)
        );
        assert_eq!(wait.p_image_memory_barriers[0].dst_queue_family_index, 1);
        waited.p_image_memory_barriers[0].src_stage_mask = p::VkPipelineStageFlags2(0x2000);
        assert!(
            events
                .plan(10, &[set2(1, signal), wait2(1, waited)])
                .is_err()
        );
    }
    #[test]
    fn reset_write_uses_physical_stage_and_preserves_logical_release_in_barrier() {
        let mut events = Events::new(1);
        events.create(1, 0).unwrap();
        let source = ash::vk::PipelineStageFlags2::VIDEO_DECODE_KHR.as_raw();
        let plan = events
            .plan(
                10,
                &[Operation::Reset2(p::VkCmdResetEvent2Args {
                    command_buffer: p::VkCommandBuffer(7),
                    event: p::VkEvent(1),
                    stage_mask: p::VkPipelineStageFlags2(source),
                })],
            )
            .unwrap();
        let Lowered::Reset { before, write, .. } = &plan.lowered[0] else {
            panic!("reset")
        };
        assert_eq!(write.stage_mask.0, 0x10000);
        let Barrier::Sync2(barrier) = &before[0] else {
            panic!("release")
        };
        assert_eq!(
            barrier
                .p_dependency_info
                .as_ref()
                .unwrap()
                .p_memory_barriers[0]
                .src_stage_mask
                .0,
            source
        );
    }
    #[test]
    fn queue_wire_aliases_qualify_only_through_the_same_actual_queue() {
        use ash::vk::{self, Handle};
        use sv_proto::HandleType;
        let mut objects = crate::table::Objects::new();
        objects.insert(2, HandleType::VkQueue, vk::Queue::from_raw(10));
        objects.insert(222, HandleType::VkQueue, vk::Queue::from_raw(10));
        objects.insert(333, HandleType::VkQueue, vk::Queue::from_raw(11));
        let actual = |wire| {
            objects
                .get::<vk::Queue>(HandleType::VkQueue, wire)
                .unwrap()
                .as_raw()
        };
        let mut events = Events::new(4);
        events.create(1, 0).unwrap();
        events.create(2, 0).unwrap();
        let dependency = dep(1);
        let signal = events
            .plan(actual(2), &[set2(1, dependency.clone()), set(2)])
            .unwrap();
        events.reserve(&signal).unwrap();
        assert!(
            events
                .plan(
                    actual(222),
                    &[wait2(1, dependency.clone()), wait(2, 0x1000)]
                )
                .is_ok()
        );
        assert!(events.plan(actual(333), &[wait2(1, dependency)]).is_err());
        assert!(events.plan(actual(333), &[wait(2, 0x1000)]).is_err());
    }
    #[test]
    fn pending_bound_and_revision_exhaustion_refuse_without_partial_reservation() {
        let mut events = Events::new(1);
        events.create(1, 0).unwrap();
        events.create(2, 0).unwrap();
        let plan = events.plan(10, &[set(1)]).unwrap();
        let ticket = events.reserve(&plan).unwrap();
        let blocked = events.plan(10, &[set(2)]).unwrap();
        assert!(events.reserve(&blocked).is_err());
        assert_eq!(events.entries[&2].projected_revision, 0);
        unsafe { events.complete(&ticket) }.unwrap();
        events.entries.get_mut(&2).unwrap().projected_revision = u64::MAX;
        assert!(events.plan(10, &[set(2)]).is_err());
    }
}
