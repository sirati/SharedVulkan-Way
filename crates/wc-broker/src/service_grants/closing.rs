//! Exact logical-reference closing barriers. Native completion and logical
//! completion remain separate operations; neither releases durable ownership.
use super::*;
use native::ProjectionReference;

/// Owner-minted exact closing reference. No caller-selected count or process
/// lifetime fact can construct or satisfy this token.
pub struct ClosingReference {
    pub(super) grant: GrantId,
    pub(super) allocation: u64,
    pub(super) reference: ProjectionReference,
    pub(super) process: Process,
    pub(super) local: Option<(Key, Option<Consumer>)>,
}
impl<S: DurableStore, B: OriginalBacking> ProtectedGrants<S, B> {
    pub(super) fn mark_closing(&mut self, token: ClosingReference) -> io::Result<ClosingReference> {
        let record = self.records.get_mut(&token.grant.id).unwrap();
        if record
            .closing
            .contains_key(&(token.allocation, token.reference))
        {
            return Err(denied("logical reference already closing or retired"));
        }
        record
            .closing
            .insert((token.allocation, token.reference), false);
        Ok(token)
    }
    pub fn begin_close_producer(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        producer: &RegisteredProcess,
        allocation: &AllocationId,
    ) -> io::Result<ClosingReference> {
        self.retained_role(lifecycle, producer, ProcessRole::Producer)?;
        Self::same_grant(&producer.grant, &allocation.grant)?;
        let record = self.record(lifecycle, &allocation.grant)?;
        if record.producers.get(&allocation.key) != Some(&producer.process) {
            return Err(denied("not exact original producer"));
        }
        self.mark_closing(ClosingReference {
            grant: allocation.grant.clone(),
            allocation: allocation.key.allocation,
            reference: ProjectionReference::Producer {
                registration: producer.process.registration(),
            },
            process: producer.process,
            local: Some((allocation.key, None)),
        })
    }
    pub fn begin_close_consumer(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        recipient: &RegisteredProcess,
        consumer: &ConsumerId,
    ) -> io::Result<ClosingReference> {
        self.retained_role(lifecycle, recipient, ProcessRole::Consumer)?;
        Self::same_grant(&recipient.grant, &consumer.allocation.grant)?;
        let record = self.record(lifecycle, &recipient.grant)?;
        if record
            .consumers
            .get(&(consumer.allocation.key, consumer.consumer))
            != Some(&recipient.process)
        {
            return Err(denied("not exact accepted consumer"));
        }
        self.mark_closing(ClosingReference {
            grant: recipient.grant.clone(),
            allocation: consumer.allocation.key.allocation,
            reference: ProjectionReference::Consumer {
                registration: recipient.process.registration(),
                consumer: consumer.consumer.reference(),
            },
            process: recipient.process,
            local: Some((consumer.allocation.key, Some(consumer.consumer))),
        })
    }
    /// Close one logical reference only after its exact export ledger has
    /// drained under the preceding terminal barrier. A retry while exports are
    /// live keeps this token usable and every native/logical hold intact.
    pub fn finish_close_reference(
        &mut self,
        lifecycle: &Lifecycle<S, B>,
        token: &ClosingReference,
    ) -> io::Result<Option<Frame>> {
        let record = self.record(lifecycle, &token.grant)?;
        if record.closing.get(&(token.allocation, token.reference)) != Some(&false) {
            return Err(denied("unknown or completed logical closing reference"));
        }
        if record.untracked_native
            || record
                .native_holds
                .values()
                .any(|h| h.matches_reference(token.allocation, token.reference))
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "exact native lineage is not drained",
            ));
        }
        if token.local.is_none() {
            return self.finish_remote_consumer_close(lifecycle, token);
        }
        let record = self.records.get_mut(&token.grant.id).unwrap();
        let mut terminal_frame = None;
        match token.local {
            Some((key, None)) => {
                if record.producers.get(&key) != Some(&token.process) {
                    return Err(denied("producer identity changed during closing"));
                }
                // SAFETY: every exact export was mediated and its all-alias
                // proof consumed after the no-new-export barrier above.
                unsafe {
                    record.table.close_producer(token.process, key)?;
                }
                record.producers.remove(&key);
                if record.remote_producer_announced(key) {
                    match lifecycle.seal(
                        &mut record.channel,
                        Phase::ProducerRetired,
                        Payload {
                            producer_registration: token.process.registration(),
                            allocation: key.allocation,
                            ..Payload::default()
                        },
                    ) {
                        Ok(frame) => terminal_frame = Some(frame),
                        Err(error) => {
                            record.quarantined = true;
                            return Err(error);
                        }
                    }
                }
            }
            Some((key, Some(consumer))) => {
                if record.consumers.get(&(key, consumer)) != Some(&token.process) {
                    return Err(denied("consumer identity changed during closing"));
                }
                // SAFETY: same exact-reference no-new-export and native drain.
                unsafe {
                    record
                        .table
                        .release_consumer(token.process, key, consumer)?;
                }
                record.consumers.remove(&(key, consumer));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "remote closing not implemented",
                ));
            }
        }
        *record
            .closing
            .get_mut(&(token.allocation, token.reference))
            .unwrap() = true;
        Ok(terminal_frame)
    }
}
