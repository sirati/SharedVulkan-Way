//! Terminal whole-grant barriers and opaque current-owner resource census.
//! None of these tokens frees durable ownership or physical capacity.
use super::*;

pub(super) struct Barrier {
    generation: u64,
    local_nonce: Option<Arc<()>>,
    peer_quiescent: bool,
    quiescent_sent: bool,
    peer_commit: bool,
    local_quiescent_sent: bool,
    canonical_quiescent: bool,
    release_ready: bool,
}
impl Barrier {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            local_nonce: None,
            peer_quiescent: false,
            quiescent_sent: false,
            peer_commit: false,
            local_quiescent_sent: false,
            canonical_quiescent: false,
            release_ready: false,
        }
    }
}
/// Owner-derived exact closing generation, never a client authorization tuple.
pub struct GrantClosing {
    grant: GrantId,
    generation: u64,
}
/// One-shot complete local census under the terminal closing barrier.
pub struct GrantCensus {
    closing: GrantClosing,
    nonce: Arc<()>,
    revision: u64,
    digest: [u8; 32],
    binding: Binding,
    channel: Arc<()>,
}
/// Non-Clone evidence that this owner stopped permanently after every current
/// tracked grant completed actual durable release. Unknown exclusions stay.
/// The controller must retain the original guard, drop old lifecycles/context,
/// reopen EXISTING registry history, issue a fresh epoch/mutual challenge and
/// independently authenticate the provider. This token cannot reset any ID,
/// enrollment, replay history or store in place and does not perform rollover.
pub struct DrainedContext<S: DurableStore, B: OriginalBacking> {
    context: Arc<ServiceContext<S, B>>,
}
impl<S: DurableStore, B: OriginalBacking> DrainedContext<S, B> {
    pub fn historical_exclusions(&self) -> io::Result<Vec<(u64, u64)>> {
        self.context.exclusions()
    }
}
/// Both-service barrier satisfied. Still no durable release has occurred.
pub struct GrantReleaseReady {
    census: GrantCensus,
}
impl GrantReleaseReady {
    pub fn binding(&self) -> &Binding {
        &self.census.binding
    }
    pub fn close_generation(&self) -> u64 {
        self.census.closing.generation
    }
}
impl<B: OriginalBacking> Record<B> {
    pub(super) fn open_for_work(&self) -> io::Result<()> {
        if self.barrier.is_some() {
            return Err(denied("whole grant is terminally closing"));
        }
        // Actual bounded active references determine the drain-message reserve.
        // These counters admit work; they cannot mint a completion census.
        let required = self
            .admitted_references()
            .checked_add(self.producers.len())
            .and_then(|n| n.checked_add(self.admitted_remote_allocations()))
            .and_then(|n| n.checked_add(6))
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(|| denied("closing sequence reserve overflow"))?;
        let serial_required = u64::try_from(self.local_pending.len())
            .ok()
            .and_then(|n| n.checked_add(4))
            .ok_or_else(|| denied("serial closing reserve overflow"))?;
        if !self.table.has_serial_budget(serial_required) || !self.has_remote_serial_budget() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "resource IDs require terminal drain and context rollover",
            ));
        }
        if !self.channel.has_work_budget(required) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "sequence reserve requires terminal drain and context rollover",
            ));
        }
        Ok(())
    }
}
impl<S: DurableStore, B: OriginalBacking> ProtectedGrants<S, B> {
    /// Terminal drain permit for deliberate fresh-context rollover. A failed
    /// attempt retains the owner; no live/unknown current grant can be skipped.
    pub fn prepare_context_drain(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
    ) -> io::Result<DrainedContext<S, B>> {
        self.check_lifecycle(lifecycle)?;
        if !self.records.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "current grants have not all durably released",
            ));
        }
        if let Err(error) = self.context.backing().validate_live_source() {
            self.poisoned = true;
            return Err(error);
        }
        if let Some(provider) = self
            .native_exports
            .lock()
            .map_err(|_| denied("native provider poisoned"))?
            .as_ref()
            && let Err(error) = provider.validate_live()
        {
            self.poisoned = true;
            return Err(error);
        }
        self.drained = true;
        Ok(DrainedContext {
            context: self.context.clone(),
        })
    }
    pub fn begin_grant_close(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        grant: &GrantId,
    ) -> io::Result<(GrantClosing, Frame)> {
        let record = self.record(lifecycle, grant)?;
        if record.barrier.is_some() {
            return Err(denied("grant already closing"));
        }
        if self.context.origin() != record.guard.binding().roles.grantor {
            return Err(denied("only canonical grantor starts grant closing"));
        }
        let generation = self.next_close;
        self.next_close = generation
            .checked_add(1)
            .ok_or_else(|| denied("grant closing generations exhausted"))?;
        let record = self.records.get_mut(&grant.id).unwrap();
        record.bump_lineage()?;
        record.barrier = Some(Barrier::new(generation));
        let frame = lifecycle
            .seal(
                &mut record.channel,
                Phase::GrantClosing,
                Payload {
                    close_generation: generation,
                    ..Payload::default()
                },
            )
            .inspect_err(|_| {
                record.quarantined = true;
            })?;
        Ok((
            GrantClosing {
                grant: grant.clone(),
                generation,
            },
            frame,
        ))
    }
    pub(super) fn apply_grant_barrier(
        &mut self,
        grant: &GrantId,
        phase: Phase,
        generation: u64,
    ) -> io::Result<RemoteApplied> {
        let record = self.records.get_mut(&grant.id).unwrap();
        match phase {
            Phase::GrantClosing => {
                if record.barrier.is_some() {
                    return Err(denied("grant already closing"));
                }
                if self.context.origin() == record.guard.binding().roles.grantor {
                    return Err(denied("grantor cannot receive peer closing authority"));
                }
                record.bump_lineage()?;
                record.barrier = Some(Barrier::new(generation));
                Ok(RemoteApplied::GrantClosing(GrantClosing {
                    grant: grant.clone(),
                    generation,
                }))
            }
            Phase::GrantLocalQuiescent => {
                let barrier = record
                    .barrier
                    .as_mut()
                    .ok_or_else(|| denied("canonical census without closing barrier"))?;
                if self.context.origin() == record.guard.binding().roles.grantor
                    || barrier.generation != generation
                    || barrier.canonical_quiescent
                    || barrier.release_ready
                {
                    return Err(denied("foreign, stale or duplicate canonical local census"));
                }
                barrier.canonical_quiescent = true;
                if let Err(error) = record.recover_outgoing_after_peer_census(generation) {
                    record.quarantined = true;
                    return Err(error);
                }
                Ok(RemoteApplied::GrantLocalQuiescent)
            }
            Phase::GrantQuiescent => {
                let barrier = record
                    .barrier
                    .as_mut()
                    .ok_or_else(|| denied("quiescence without closing barrier"))?;
                if self.context.origin() != record.guard.binding().roles.grantor
                    || barrier.generation != generation
                    || barrier.peer_quiescent
                    || barrier.release_ready
                {
                    return Err(denied("foreign, stale or duplicate peer quiescence"));
                }
                barrier.peer_quiescent = true;
                if let Err(error) = record.recover_outgoing_after_peer_census(generation) {
                    record.quarantined = true;
                    return Err(error);
                }
                Ok(RemoteApplied::GrantQuiescent)
            }
            Phase::GrantReleaseCommit => {
                let barrier = record
                    .barrier
                    .as_mut()
                    .ok_or_else(|| denied("release commit without closing barrier"))?;
                if self.context.origin() == record.guard.binding().roles.grantor
                    || barrier.generation != generation
                    || !barrier.quiescent_sent
                    || barrier.local_nonce.is_none()
                    || barrier.peer_commit
                    || barrier.release_ready
                {
                    return Err(denied(
                        "commit without exact local census/quiescent barrier",
                    ));
                }
                barrier.peer_commit = true;
                Ok(RemoteApplied::GrantReleaseCommitted)
            }
            _ => Err(denied("not a grant barrier phase")),
        }
    }
    fn validate_native_source(&mut self, grant: &GrantId) -> io::Result<()> {
        let record = self.records.get_mut(&grant.id).unwrap();
        if let Err(error) = record.guard.original().validate_live_source() {
            record.quarantined = true;
            self.poisoned = true;
            return Err(error);
        }
        let provider = self
            .native_exports
            .lock()
            .map_err(|_| denied("native provider state poisoned"))?;
        if let Some(provider) = provider.as_ref()
            && let Err(error) = provider.validate_live()
        {
            record.quarantined = true;
            self.poisoned = true;
            return Err(error);
        }
        Ok(())
    }
    pub fn mint_grant_census(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        closing: &GrantClosing,
    ) -> io::Result<GrantCensus> {
        let record = self.record(lifecycle, &closing.grant)?;
        let barrier = record
            .barrier
            .as_ref()
            .ok_or_else(|| denied("census without closing barrier"))?;
        if barrier.generation != closing.generation
            || barrier.local_nonce.is_some()
            || barrier.release_ready
        {
            return Err(denied("census generation stale or already consumed"));
        }
        // Enumerate actual owner lineage collections. The private table's empty
        // allocation check is an additional consistency check, not completion.
        if record.untracked_native
            || !record.producers.is_empty()
            || !record.consumers.is_empty()
            || !record.local_pending.is_empty()
            || !record.closing.is_empty()
            || !record.native_holds.is_empty()
            || record.table.admitted_allocations() != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "local exact lineage census incomplete",
            ));
        }
        record.enumerate_remote_census()?;
        self.validate_native_source(&closing.grant)?;
        let record = self.records.get_mut(&closing.grant.id).unwrap();
        let nonce = Arc::new(());
        record.barrier.as_mut().unwrap().local_nonce = Some(nonce.clone());
        Ok(GrantCensus {
            closing: GrantClosing {
                grant: closing.grant.clone(),
                generation: closing.generation,
            },
            nonce,
            revision: record.lineage_revision,
            digest: record.lineage_digest,
            binding: record.guard.binding().clone(),
            channel: record.channel.identity().clone(),
        })
    }
    fn check_grant_census(
        &self,
        lifecycle: &Lifecycle<S, B>,
        census: &GrantCensus,
    ) -> io::Result<()> {
        let record = self.record(lifecycle, &census.closing.grant)?;
        let barrier = record
            .barrier
            .as_ref()
            .ok_or_else(|| denied("census barrier lost"))?;
        if barrier.generation != census.closing.generation
            || !barrier
                .local_nonce
                .as_ref()
                .is_some_and(|nonce| Arc::ptr_eq(nonce, &census.nonce))
            || census.revision != record.lineage_revision
            || census.digest != record.lineage_digest
            || census.binding != *record.guard.binding()
            || !Arc::ptr_eq(&census.channel, record.channel.identity())
            || barrier.release_ready
        {
            return Err(denied(
                "census does not bind current terminal owner revision",
            ));
        }
        Ok(())
    }
    /// Canonical LOCAL native/logical census statement. Initial Closing alone
    /// never supplies this authority to recover the opposite producer's pins.
    pub fn seal_grant_local_quiescent(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        census: &GrantCensus,
    ) -> io::Result<Frame> {
        self.check_grant_census(lifecycle, census)?;
        let record = self.records.get_mut(&census.closing.grant.id).unwrap();
        let barrier = record.barrier.as_mut().unwrap();
        if self.context.origin() != record.guard.binding().roles.grantor
            || barrier.local_quiescent_sent
        {
            return Err(denied("not canonical one-shot local census transition"));
        }
        barrier.local_quiescent_sent = true;
        lifecycle
            .seal(
                &mut record.channel,
                Phase::GrantLocalQuiescent,
                Payload {
                    close_generation: census.closing.generation,
                    ..Payload::default()
                },
            )
            .inspect_err(|_| {
                record.quarantined = true;
            })
    }
    pub fn seal_grant_quiescent(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        census: &GrantCensus,
    ) -> io::Result<Frame> {
        self.check_grant_census(lifecycle, census)?;
        let record = self.records.get_mut(&census.closing.grant.id).unwrap();
        let barrier = record.barrier.as_mut().unwrap();
        if self.context.origin() == record.guard.binding().roles.grantor || barrier.quiescent_sent {
            return Err(denied("not peer's one-shot quiescence transition"));
        }
        barrier.quiescent_sent = true;
        lifecycle
            .seal(
                &mut record.channel,
                Phase::GrantQuiescent,
                Payload {
                    close_generation: census.closing.generation,
                    ..Payload::default()
                },
            )
            .inspect_err(|_| {
                record.quarantined = true;
            })
    }
    /// Produces no free-list mutation and no registry release. Actual current
    /// Reservation consumption remains a separate narrowly scoped operation.
    pub fn complete_grant_barrier(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        census: &GrantCensus,
    ) -> io::Result<GrantReleaseReady> {
        self.check_grant_census(lifecycle, census)?;
        let record = &self.records[&census.closing.grant.id];
        let barrier = record.barrier.as_ref().unwrap();
        let canonical = self.context.origin() == record.guard.binding().roles.grantor;
        if (canonical && !barrier.peer_quiescent)
            || (!canonical && (!barrier.quiescent_sent || !barrier.peer_commit))
        {
            return Err(denied("both-service current closing barrier incomplete"));
        }
        self.validate_native_source(&census.closing.grant)?;
        self.records
            .get_mut(&census.closing.grant.id)
            .unwrap()
            .barrier
            .as_mut()
            .unwrap()
            .release_ready = true;
        Ok(GrantReleaseReady {
            census: GrantCensus {
                closing: GrantClosing {
                    grant: census.closing.grant.clone(),
                    generation: census.closing.generation,
                },
                nonce: census.nonce.clone(),
                revision: census.revision,
                digest: census.digest,
                binding: census.binding.clone(),
                channel: census.channel.clone(),
            },
        })
    }
}
