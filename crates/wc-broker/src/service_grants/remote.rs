//! Typed remote records scoped to a currently authenticated grant channel.
//! A remote registration NEVER implements native ServiceProcess. No wire phase
//! here is completion or retirement authority. All old alias holds remain.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
pub struct RemoteRegistration {
    grant: GrantId,
    registration: u64,
    role: ProcessRole,
}
#[derive(Clone)]
pub struct RemoteAllocationId {
    grant: GrantId,
    allocation: u64,
}
/// Owner-minted outgoing pending lineage. A wire transfer number cannot mint it.
pub struct OutgoingTransfer {
    grant: GrantId,
    transfer: u64,
}
#[derive(Clone)]
pub struct RemotePending {
    allocation: RemoteAllocationId,
    transfer: u64,
}
#[derive(Clone)]
pub struct RemoteConsumerId {
    allocation: RemoteAllocationId,
    consumer: u64,
}
pub enum RemoteApplied {
    Registered(RemoteRegistration),
    Allocated(RemoteAllocationId, Extent),
    Resized(RemoteAllocationId, Extent),
    Prepared(RemotePending),
    Accepted,
    ProducerRetired,
    ConsumerReleased,
    Cancelled(Frame),
    CancellationAcknowledged,
    Declined(Frame),
    DeclineAcknowledged,
}
struct Allocation {
    retired: bool,
    producer: u64,
    extent: Extent,
}
struct IncomingPending {
    allocation: u64,
    producer: u64,
    recipient: Process,
}
struct OutgoingPending {
    external: crate::bar_control::ExternalHold,
    cancelling: bool,
    allocation: Key,
    producer: Process,
    recipient: u64,
}
struct Consumer {
    producer: u64,
    allocation: u64,
    process: Process,
}
#[derive(Default)]
pub(super) struct State {
    registrations: BTreeMap<u64, ProcessRole>,
    allocations: BTreeMap<u64, Allocation>,
    incoming_pending: BTreeMap<u64, IncomingPending>,
    highest_incoming_transfer: u64,
    highest_allocation: u64,
    highest_announced_allocation: u64,
    outgoing_pending: BTreeMap<u64, OutgoingPending>,
    consumers: BTreeMap<u64, Consumer>,
    outgoing_consumers: BTreeMap<u64, OutgoingPending>,
    announced_processes: BTreeSet<Process>,
    announced_allocations: BTreeSet<Key>,
    next_transfer: u64,
    next_consumer: u64,
}
impl<B: OriginalBacking> Record<B> {
    pub(super) fn admitted_references(&self) -> usize {
        self.table.admitted_references()
            + self.remote.incoming_pending.len()
            + self.remote.outgoing_pending.len()
            + self.remote.consumers.len()
            + self.remote.outgoing_consumers.len()
    }
    fn prune_remote_allocation(&mut self, allocation: u64) {
        if self
            .remote
            .allocations
            .get(&allocation)
            .is_some_and(|a| a.retired)
            && !self
                .remote
                .incoming_pending
                .values()
                .any(|p| p.allocation == allocation)
            && !self
                .remote
                .consumers
                .values()
                .any(|c| c.allocation == allocation)
        {
            self.remote.allocations.remove(&allocation);
        }
    }

    pub(super) fn take_remote_producer_announcement(&mut self, key: Key) -> bool {
        self.remote.announced_allocations.remove(&key)
    }
}
impl State {
    fn serial(counter: &mut u64) -> io::Result<u64> {
        let value = counter
            .checked_add(1)
            .ok_or_else(|| denied("remote resource IDs exhausted"))?;
        *counter = value;
        Ok(value)
    }
}
fn remote_extent(binding: &Binding, p: Payload) -> io::Result<Extent> {
    let end = binding
        .grant_offset
        .checked_add(binding.grant_length)
        .ok_or_else(|| denied("canonical grant overflow"))?;
    if p.offset < binding.grant_offset
        || !p.offset.is_multiple_of(PAGE)
        || !p.capacity.is_multiple_of(PAGE)
        || p.capacity == 0
        || p.length == 0
        || p.length > p.capacity
        || !p.offset.checked_add(p.capacity).is_some_and(|n| n <= end)
    {
        return Err(denied(
            "remote extent differs from exact canonical reservation",
        ));
    }
    Ok(Extent {
        offset: p.offset,
        capacity: p.capacity,
        length: p.length,
    })
}
impl<S: DurableStore, B: OriginalBacking> ProtectedGrants<S, B> {
    pub fn begin_close_remote_consumer(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        recipient: &RegisteredProcess,
        consumer: &RemoteConsumerId,
    ) -> io::Result<ClosingReference> {
        self.retained_role(lifecycle, recipient, ProcessRole::Consumer)?;
        Self::same_grant(&recipient.grant, &consumer.allocation.grant)?;
        let record = self.record(lifecycle, &recipient.grant)?;
        let hold = record
            .remote
            .consumers
            .get(&consumer.consumer)
            .ok_or_else(|| denied("unknown remote consumer"))?;
        if hold.allocation != consumer.allocation.allocation || hold.process != recipient.process {
            return Err(denied("not exact remote consumer"));
        }
        self.mark_closing(ClosingReference {
            grant: recipient.grant.clone(),
            allocation: hold.allocation,
            reference: native::ProjectionReference::RemoteConsumer {
                registration: recipient.process.registration(),
                consumer: consumer.consumer,
            },
            process: recipient.process,
            local: None,
        })
    }
    pub(super) fn finish_remote_consumer_close(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        token: &ClosingReference,
    ) -> io::Result<Option<Frame>> {
        let record = self.record(lifecycle, &token.grant)?;
        if record.closing.get(&(token.allocation, token.reference)) != Some(&false)
            || record.untracked_native
            || record
                .native_holds
                .values()
                .any(|h| h.matches_reference(token.allocation, token.reference))
        {
            return Err(denied(
                "remote consumer closing barrier or native drain incomplete",
            ));
        }
        let native::ProjectionReference::RemoteConsumer {
            registration,
            consumer,
        } = token.reference
        else {
            return Err(denied("not remote consumer closing token"));
        };
        let record = self.records.get_mut(&token.grant.id).unwrap();
        let hold = record
            .remote
            .consumers
            .get(&consumer)
            .ok_or_else(|| denied("remote consumer already released"))?;
        if hold.allocation != token.allocation
            || hold.process != token.process
            || registration != hold.process.registration()
        {
            return Err(denied("remote consumer closing lineage differs"));
        }
        let producer = hold.producer;
        record.bump_lineage()?;
        record.remote.consumers.remove(&consumer);
        record.prune_remote_allocation(token.allocation);
        record.note_terminal(token.allocation, token.reference, None);
        record.closing.remove(&(token.allocation, token.reference));
        match lifecycle.seal(
            &mut record.channel,
            Phase::ConsumerReleased,
            Payload {
                producer_registration: producer,
                recipient_registration: registration,
                allocation: token.allocation,
                consumer,
                ..Payload::default()
            },
        ) {
            Ok(frame) => Ok(Some(frame)),
            Err(error) => {
                record.quarantined = true;
                Err(error)
            }
        }
    }
    /// Emit only after actual native watch enrollment at its exact local role.
    pub fn announce_registration(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        native: &RegisteredProcess,
    ) -> io::Result<Frame> {
        self.process_role(lifecycle, native, native.role)?;
        let record = self.records.get_mut(&native.grant.id).unwrap();
        if !record.remote.announced_processes.insert(native.process) {
            return Err(denied("native registration already announced"));
        }
        let id = native.process.registration();
        let (phase, payload) = match native.role {
            ProcessRole::Producer => (
                Phase::RegisterProducer,
                Payload {
                    producer_registration: id,
                    ..Payload::default()
                },
            ),
            ProcessRole::Consumer => (
                Phase::RegisterConsumer,
                Payload {
                    recipient_registration: id,
                    ..Payload::default()
                },
            ),
        };
        lifecycle.seal(&mut record.channel, phase, payload)
    }
    /// Derive an Allocate frame from the existing canonical producer record.
    /// No caller-selected offset, capacity, registration or payload is signed.
    pub fn announce_allocation(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        producer: &RegisteredProcess,
        allocation: &AllocationId,
    ) -> io::Result<Frame> {
        self.process_role(lifecycle, producer, ProcessRole::Producer)?;
        self.producer_open(lifecycle, producer, allocation)?;
        Self::same_grant(&producer.grant, &allocation.grant)?;
        let extent = self.allocation(lifecycle, allocation)?;
        let record = self.records.get_mut(&allocation.grant.id).unwrap();
        if record.producers.get(&allocation.key) != Some(&producer.process)
            || !record
                .remote
                .announced_processes
                .contains(&producer.process)
            || allocation.key.allocation <= record.remote.highest_announced_allocation
            || !record.remote.announced_allocations.insert(allocation.key)
        {
            return Err(denied(
                "allocation producer missing or allocation already announced",
            ));
        }
        record.remote.highest_announced_allocation = allocation.key.allocation;
        let payload = Payload {
            producer_registration: producer.process.registration(),
            allocation: allocation.key.allocation,
            offset: extent.offset,
            capacity: extent.capacity,
            length: extent.length,
            ..Payload::default()
        };
        lifecycle.seal(&mut record.channel, Phase::Allocate, payload)
    }
    pub fn resize_and_announce(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        producer: &RegisteredProcess,
        allocation: &AllocationId,
        length: u64,
    ) -> io::Result<Frame> {
        self.process_role(lifecycle, producer, ProcessRole::Producer)?;
        self.allocation(lifecycle, allocation)?;
        if !self.records[&allocation.grant.id]
            .remote
            .announced_allocations
            .contains(&allocation.key)
        {
            return Err(denied("allocation not announced"));
        }
        let extent = self.resize(lifecycle, producer, allocation, length)?;
        let record = self.records.get_mut(&allocation.grant.id).unwrap();
        lifecycle.seal(
            &mut record.channel,
            Phase::Resize,
            Payload {
                producer_registration: producer.process.registration(),
                allocation: allocation.key.allocation,
                offset: extent.offset,
                capacity: extent.capacity,
                length: extent.length,
                ..Payload::default()
            },
        )
    }
    /// Prepare against a distinct authenticated remote consumer registration.
    /// The pending hold exists BEFORE the descriptor/frame can be observed.
    pub fn prepare_remote(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        producer: &RegisteredProcess,
        allocation: &AllocationId,
        recipient: &RemoteRegistration,
    ) -> io::Result<Frame> {
        self.prepare_remote_with_handle(lifecycle, producer, allocation, recipient)
            .map(|(_, frame)| frame)
    }
    pub fn begin_cancel_remote(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        producer: &RegisteredProcess,
        transfer: &OutgoingTransfer,
    ) -> io::Result<Frame> {
        self.retained_role(lifecycle, producer, ProcessRole::Producer)?;
        Self::same_grant(&producer.grant, &transfer.grant)?;
        let record = self.records.get_mut(&transfer.grant.id).unwrap();
        let pending = record
            .remote
            .outgoing_pending
            .get_mut(&transfer.transfer)
            .ok_or_else(|| denied("outgoing pending already terminal or foreign"))?;
        if pending.producer != producer.process || pending.cancelling {
            return Err(denied("not exact open outgoing pending"));
        }
        pending.cancelling = true;
        let payload = Payload {
            producer_registration: pending.producer.registration(),
            recipient_registration: pending.recipient,
            allocation: pending.allocation.allocation,
            transfer: transfer.transfer,
            ..Payload::default()
        };
        lifecycle
            .seal(&mut record.channel, Phase::TransferCancelled, payload)
            .inspect_err(|_| {
                record.quarantined = true;
            })
    }
    pub fn prepare_remote_with_handle(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        producer: &RegisteredProcess,
        allocation: &AllocationId,
        recipient: &RemoteRegistration,
    ) -> io::Result<(OutgoingTransfer, Frame)> {
        self.process_role(lifecycle, producer, ProcessRole::Producer)?;
        self.producer_open(lifecycle, producer, allocation)?;
        Self::same_grant(&producer.grant, &allocation.grant)?;
        Self::same_grant(&recipient.grant, &allocation.grant)?;
        self.allocation(lifecycle, allocation)?;
        let record = self.records.get_mut(&allocation.grant.id).unwrap();
        if recipient.role != ProcessRole::Consumer
            || record.remote.registrations.get(&recipient.registration)
                != Some(&ProcessRole::Consumer)
            || record.producers.get(&allocation.key) != Some(&producer.process)
            || !record
                .remote
                .announced_allocations
                .contains(&allocation.key)
        {
            return Err(denied("remote transfer registration/allocation mismatch"));
        }
        if record.admitted_references() >= self.resource_limits.references {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "remote reference admission saturated",
            ));
        }
        let transfer = State::serial(&mut record.remote.next_transfer)?;
        let external = record
            .table
            .pin_external(producer.process, allocation.key)?;
        record.remote.outgoing_pending.insert(
            transfer,
            OutgoingPending {
                external,
                cancelling: false,
                allocation: allocation.key,
                producer: producer.process,
                recipient: recipient.registration,
            },
        );
        let frame = lifecycle.seal(
            &mut record.channel,
            Phase::Prepare,
            Payload {
                producer_registration: producer.process.registration(),
                recipient_registration: recipient.registration,
                allocation: allocation.key.allocation,
                transfer,
                ..Payload::default()
            },
        )?;
        Ok((
            OutgoingTransfer {
                grant: allocation.grant.clone(),
                transfer,
            },
            frame,
        ))
    }
    /// Verify and mutate in this single current-context owner operation. No
    /// method accepts a historical VerifiedMessage for later resource apply.
    pub fn apply_remote(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        grant: &GrantId,
        frame: &Frame,
    ) -> io::Result<RemoteApplied> {
        self.record(lifecycle, grant)?;
        let record = self.records.get_mut(&grant.id).unwrap();
        let message = lifecycle.verify(&mut record.channel, frame)?;
        let message = message.message();
        let p = message.payload;
        let binding = record.guard.binding();
        match message.phase {
            Phase::RegisterProducer | Phase::RegisterConsumer => {
                if record.remote.registrations.len() >= self.resource_limits.registrations {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "remote registration admission saturated",
                    ));
                }
                let (role, id) = if message.phase == Phase::RegisterProducer {
                    (ProcessRole::Producer, p.producer_registration)
                } else {
                    (ProcessRole::Consumer, p.recipient_registration)
                };
                if record.remote.registrations.contains_key(&id) {
                    return Err(denied("duplicate remote registration ID"));
                }
                record.remote.registrations.insert(id, role);
                Ok(RemoteApplied::Registered(RemoteRegistration {
                    grant: grant.clone(),
                    registration: id,
                    role,
                }))
            }
            Phase::Allocate => {
                if record.remote.registrations.get(&p.producer_registration)
                    != Some(&ProcessRole::Producer)
                    || p.allocation <= record.remote.highest_allocation
                    || record.remote.allocations.contains_key(&p.allocation)
                {
                    return Err(denied("unknown remote producer or duplicate allocation"));
                }
                if record.remote.allocations.len() >= self.resource_limits.allocations {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "remote allocation admission saturated",
                    ));
                }
                let extent = remote_extent(binding, p)?;
                let end = extent.offset + extent.capacity;
                if record.remote.allocations.values().any(|a| {
                    extent.offset < a.extent.offset + a.extent.capacity && a.extent.offset < end
                }) {
                    return Err(denied("remote allocation overlaps original capacity"));
                }
                record.remote.highest_allocation = p.allocation;
                record.remote.allocations.insert(
                    p.allocation,
                    Allocation {
                        retired: false,
                        producer: p.producer_registration,
                        extent,
                    },
                );
                Ok(RemoteApplied::Allocated(
                    RemoteAllocationId {
                        grant: grant.clone(),
                        allocation: p.allocation,
                    },
                    extent,
                ))
            }
            Phase::Resize => {
                let extent = remote_extent(binding, p)?;
                let old = record
                    .remote
                    .allocations
                    .get(&p.allocation)
                    .ok_or_else(|| denied("unknown remote resize allocation"))?;
                if old.retired
                    || old.producer != p.producer_registration
                    || old.extent.offset != extent.offset
                    || old.extent.capacity != extent.capacity
                    || old.extent.length > extent.length
                {
                    return Err(denied(
                        "remote resize changes original producer/offset/capacity",
                    ));
                }
                record
                    .remote
                    .allocations
                    .get_mut(&p.allocation)
                    .unwrap()
                    .extent = extent;
                Ok(RemoteApplied::Resized(
                    RemoteAllocationId {
                        grant: grant.clone(),
                        allocation: p.allocation,
                    },
                    extent,
                ))
            }
            Phase::Prepare => {
                let allocation = record
                    .remote
                    .allocations
                    .get(&p.allocation)
                    .ok_or_else(|| denied("unknown remote prepare allocation"))?;
                if allocation.retired
                    || allocation.producer != p.producer_registration
                    || p.transfer <= record.remote.highest_incoming_transfer
                {
                    return Err(denied("foreign producer or duplicate pending transfer"));
                }
                let recipient = record
                    .processes
                    .iter()
                    .find(|(process, native)| {
                        process.registration() == p.recipient_registration
                            && native.role == ProcessRole::Consumer
                    })
                    .map(|(process, _)| *process)
                    .ok_or_else(|| denied("remote pending does not name actual local consumer"))?;
                if !record.remote.announced_processes.contains(&recipient) {
                    return Err(denied("local consumer was not enrolled on peer channel"));
                }
                record.processes[&recipient].live()?;
                record.remote.highest_incoming_transfer = p.transfer;
                if record.admitted_references() >= self.resource_limits.references {
                    let decline = lifecycle
                        .seal(&mut record.channel, Phase::PrepareDeclined, p)
                        .inspect_err(|_| {
                            record.quarantined = true;
                        })?;
                    return Ok(RemoteApplied::Declined(decline));
                }
                record.remote.incoming_pending.insert(
                    p.transfer,
                    IncomingPending {
                        allocation: p.allocation,
                        producer: p.producer_registration,
                        recipient,
                    },
                );
                Ok(RemoteApplied::Prepared(RemotePending {
                    allocation: RemoteAllocationId {
                        grant: grant.clone(),
                        allocation: p.allocation,
                    },
                    transfer: p.transfer,
                }))
            }
            Phase::Accept => {
                let pending = record
                    .remote
                    .outgoing_pending
                    .get(&p.transfer)
                    .ok_or_else(|| denied("unknown original pending transfer"))?;
                if pending.allocation.allocation != p.allocation
                    || pending.producer.registration() != p.producer_registration
                    || pending.recipient != p.recipient_registration
                    || record.remote.outgoing_consumers.contains_key(&p.consumer)
                {
                    return Err(denied("remote accept differs from exact pending/recipient"));
                }
                record.table.check_external(&pending.external)?;
                // Move the same table pin. There is never a zero-hold interval.
                let hold = record.remote.outgoing_pending.remove(&p.transfer).unwrap();
                record.remote.outgoing_consumers.insert(p.consumer, hold);
                Ok(RemoteApplied::Accepted)
            }
            Phase::ProducerRetired => {
                let allocation = record
                    .remote
                    .allocations
                    .get_mut(&p.allocation)
                    .ok_or_else(|| denied("unknown retired remote producer allocation"))?;
                if allocation.retired || allocation.producer != p.producer_registration {
                    return Err(denied("foreign or duplicate producer retirement"));
                }
                allocation.retired = true;
                record.prune_remote_allocation(p.allocation);
                Ok(RemoteApplied::ProducerRetired)
            }
            Phase::ConsumerReleased => {
                let hold = record
                    .remote
                    .outgoing_consumers
                    .get(&p.consumer)
                    .ok_or_else(|| denied("unknown exact remote consumer release"))?;
                if hold.allocation.allocation != p.allocation
                    || hold.producer.registration() != p.producer_registration
                    || hold.recipient != p.recipient_registration
                {
                    return Err(denied("remote consumer release lineage differs"));
                }
                record.table.check_external(&hold.external)?;
                let hold = record
                    .remote
                    .outgoing_consumers
                    .remove(&p.consumer)
                    .unwrap();
                // SAFETY: current authenticated privileged consumer service can
                // emit this exact transition only after its closing/native drain.
                unsafe {
                    record.table.release_external(hold.external)?;
                }
                Ok(RemoteApplied::ConsumerReleased)
            }
            Phase::TransferCancelled => {
                let pending = record
                    .remote
                    .incoming_pending
                    .get(&p.transfer)
                    .ok_or_else(|| denied("pending already accepted, cancelled or unknown"))?;
                if pending.allocation != p.allocation
                    || pending.producer != p.producer_registration
                    || pending.recipient.registration() != p.recipient_registration
                {
                    return Err(denied("cancel request lineage differs"));
                }
                // No preaccept projection route exists. Removing this pending
                // is the serialized no-future-accept barrier before ACK emission.
                record.remote.incoming_pending.remove(&p.transfer);
                record.prune_remote_allocation(p.allocation);
                let ack = lifecycle
                    .seal(&mut record.channel, Phase::TransferCancelAck, p)
                    .inspect_err(|_| {
                        record.quarantined = true;
                    })?;
                Ok(RemoteApplied::Cancelled(ack))
            }
            Phase::PrepareDeclined => {
                let pending = record
                    .remote
                    .outgoing_pending
                    .get(&p.transfer)
                    .ok_or_else(|| denied("decline for terminal or unknown pending"))?;
                if pending.allocation.allocation != p.allocation
                    || pending.producer.registration() != p.producer_registration
                    || pending.recipient != p.recipient_registration
                {
                    return Err(denied("decline lineage differs"));
                }
                record.table.check_external(&pending.external)?;
                let pending = record.remote.outgoing_pending.remove(&p.transfer).unwrap();
                // SAFETY: exact receiver has burned this one-shot transfer
                // without admitting any prepared/accepted/native view.
                unsafe {
                    record.table.release_external(pending.external)?;
                }
                Ok(RemoteApplied::DeclineAcknowledged)
            }
            Phase::TransferCancelAck => {
                let pending = record
                    .remote
                    .outgoing_pending
                    .get(&p.transfer)
                    .ok_or_else(|| {
                        denied("cancel ACK for accepted, cancelled or unknown pending")
                    })?;
                if !pending.cancelling
                    || pending.allocation.allocation != p.allocation
                    || pending.producer.registration() != p.producer_registration
                    || pending.recipient != p.recipient_registration
                {
                    return Err(denied("cancel ACK lineage or state differs"));
                }
                record.table.check_external(&pending.external)?;
                let pending = record.remote.outgoing_pending.remove(&p.transfer).unwrap();
                // SAFETY: exact privileged peer has atomically burned the only
                // unaccepted pending before ACK; Accept can no longer win there.
                unsafe {
                    record.table.release_external(pending.external)?;
                }
                Ok(RemoteApplied::CancellationAcknowledged)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "wire phase is not typed registration/resource apply; native retirement proof required",
            )),
        }
    }
    /// Accept only the exact local native consumer named by authenticated
    /// pending state. A consumer hold exists before the response is exposed.
    pub fn accept_remote(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        recipient: &RegisteredProcess,
        pending: &RemotePending,
    ) -> io::Result<(RemoteConsumerId, Frame)> {
        self.process_role(lifecycle, recipient, ProcessRole::Consumer)?;
        Self::same_grant(&recipient.grant, &pending.allocation.grant)?;
        let record = self.records.get_mut(&recipient.grant.id).unwrap();
        let prepared = record
            .remote
            .incoming_pending
            .get(&pending.transfer)
            .ok_or_else(|| denied("stale remote pending"))?;
        if prepared.allocation != pending.allocation.allocation
            || prepared.recipient != recipient.process
        {
            return Err(denied(
                "remote pending belongs to different actual consumer",
            ));
        }
        let producer = prepared.producer;
        let consumer = State::serial(&mut record.remote.next_consumer)?;
        record.remote.consumers.insert(
            consumer,
            Consumer {
                producer,
                allocation: prepared.allocation,
                process: recipient.process,
            },
        );
        record.remote.incoming_pending.remove(&pending.transfer);
        let frame = lifecycle.seal(
            &mut record.channel,
            Phase::Accept,
            Payload {
                producer_registration: producer,
                recipient_registration: recipient.process.registration(),
                allocation: pending.allocation.allocation,
                transfer: pending.transfer,
                consumer,
                ..Payload::default()
            },
        )?;
        Ok((
            RemoteConsumerId {
                allocation: pending.allocation.clone(),
                consumer,
            },
            frame,
        ))
    }
    pub fn project_remote_consumer<P: ProjectionBackend<B>>(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        recipient: &RegisteredProcess,
        consumer: &RemoteConsumerId,
        backend: &mut P,
    ) -> io::Result<P::Projection> {
        self.process_role(lifecycle, recipient, ProcessRole::Consumer)?;
        Self::same_grant(&recipient.grant, &consumer.allocation.grant)?;
        let record = self.records.get(&recipient.grant.id).unwrap();
        if record.closing.contains_key(&(
            consumer.allocation.allocation,
            native::ProjectionReference::RemoteConsumer {
                registration: recipient.process.registration(),
                consumer: consumer.consumer,
            },
        )) {
            return Err(denied("remote consumer closing or released"));
        }
        let hold = record
            .remote
            .consumers
            .get(&consumer.consumer)
            .ok_or_else(|| denied("unknown remote consumer hold"))?;
        if hold.allocation != consumer.allocation.allocation || hold.process != recipient.process {
            return Err(denied("projection not accepted by this exact consumer"));
        }
        let extent = record
            .remote
            .allocations
            .get(&hold.allocation)
            .ok_or_else(|| denied("remote allocation mirror missing"))?
            .extent;
        let guard = record.guard.clone();
        let target = record.processes[&recipient.process].process.clone();
        self.records
            .get_mut(&recipient.grant.id)
            .unwrap()
            .untracked_native = true;
        match backend.project(guard, extent, target, ProjectionAccess::ReadOnly) {
            Ok(projection) => Ok(projection),
            Err(e) => {
                self.records
                    .get_mut(&recipient.grant.id)
                    .unwrap()
                    .quarantined = true;
                Err(e)
            }
        }
    }
    pub fn project_remote_consumer_tracked<P: native::TrackedProjectionBackend<B>>(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        recipient: &RegisteredProcess,
        consumer: &RemoteConsumerId,
        backend: &mut P,
    ) -> io::Result<P::Projection> {
        self.process_role(lifecycle, recipient, ProcessRole::Consumer)?;
        Self::same_grant(&recipient.grant, &consumer.allocation.grant)?;
        let record = self.records.get(&recipient.grant.id).unwrap();
        if record.closing.contains_key(&(
            consumer.allocation.allocation,
            native::ProjectionReference::RemoteConsumer {
                registration: recipient.process.registration(),
                consumer: consumer.consumer,
            },
        )) {
            return Err(denied("remote consumer closing or released"));
        }
        let hold = record
            .remote
            .consumers
            .get(&consumer.consumer)
            .ok_or_else(|| denied("unknown remote consumer hold"))?;
        if hold.allocation != consumer.allocation.allocation || hold.process != recipient.process {
            return Err(denied(
                "projection not accepted by exact native remote consumer",
            ));
        }
        let extent = record
            .remote
            .allocations
            .get(&hold.allocation)
            .ok_or_else(|| denied("remote allocation mirror missing"))?
            .extent;
        self.tracked_extent(
            lifecycle,
            &recipient.grant,
            native::Request {
                allocation: consumer.allocation.allocation,
                reference: native::ProjectionReference::RemoteConsumer {
                    registration: recipient.process.registration(),
                    consumer: consumer.consumer,
                },
                extent,
                target: recipient.process,
                access: ProjectionAccess::ReadOnly,
            },
            backend,
        )
    }
}
