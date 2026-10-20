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
}
struct Allocation {
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
    allocation: Key,
    producer: Process,
    recipient: u64,
}
struct Consumer {
    allocation: u64,
    process: Process,
}
#[derive(Default)]
pub(super) struct State {
    registrations: BTreeMap<u64, ProcessRole>,
    allocations: BTreeMap<u64, Allocation>,
    incoming_pending: BTreeMap<u64, IncomingPending>,
    seen_incoming_transfers: BTreeSet<u64>,
    outgoing_pending: BTreeMap<u64, OutgoingPending>,
    consumers: BTreeMap<u64, Consumer>,
    outgoing_consumers: BTreeMap<u64, OutgoingPending>,
    announced_processes: BTreeSet<Process>,
    announced_allocations: BTreeSet<Key>,
    next_transfer: u64,
    next_consumer: u64,
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
        Self::same_grant(&producer.grant, &allocation.grant)?;
        let extent = self.allocation(lifecycle, allocation)?;
        let record = self.records.get_mut(&allocation.grant.id).unwrap();
        if record.producers.get(&allocation.key) != Some(&producer.process)
            || !record
                .remote
                .announced_processes
                .contains(&producer.process)
            || !record.remote.announced_allocations.insert(allocation.key)
        {
            return Err(denied(
                "allocation producer missing or allocation already announced",
            ));
        }
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
        self.process_role(lifecycle, producer, ProcessRole::Producer)?;
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
        let transfer = State::serial(&mut record.remote.next_transfer)?;
        let external = record
            .table
            .pin_external(producer.process, allocation.key)?;
        record.remote.outgoing_pending.insert(
            transfer,
            OutgoingPending {
                external,
                allocation: allocation.key,
                producer: producer.process,
                recipient: recipient.registration,
            },
        );
        lifecycle.seal(
            &mut record.channel,
            Phase::Prepare,
            Payload {
                producer_registration: producer.process.registration(),
                recipient_registration: recipient.registration,
                allocation: allocation.key.allocation,
                transfer,
                ..Payload::default()
            },
        )
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
                    || record.remote.allocations.contains_key(&p.allocation)
                {
                    return Err(denied("unknown remote producer or duplicate allocation"));
                }
                let extent = remote_extent(binding, p)?;
                let end = extent.offset + extent.capacity;
                if record.remote.allocations.values().any(|a| {
                    extent.offset < a.extent.offset + a.extent.capacity && a.extent.offset < end
                }) {
                    return Err(denied("remote allocation overlaps original capacity"));
                }
                record.remote.allocations.insert(
                    p.allocation,
                    Allocation {
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
                if old.producer != p.producer_registration
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
                if allocation.producer != p.producer_registration
                    || record.remote.seen_incoming_transfers.contains(&p.transfer)
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
                record.remote.seen_incoming_transfers.insert(p.transfer);
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
