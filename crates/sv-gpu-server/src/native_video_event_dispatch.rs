//! Serialized native HOST event operations and canonical provenance commits.
//! Events is locked across preflight, actual native call and infallible commit.
//! No queue is admitted or waited here. Status always comes from the native
//! event, including publication of a completed prefix of an in-flight job.
use crate::{native_video_events::Events, server::GpuServer};
use ash::vk;
use std::sync::{Arc, Mutex};
use sv_proto::{self as p, Server};
use sv_wire::{Result as WireResult, WireError};

type Registry = Arc<Mutex<Events>>;
fn result(error: vk::Result) -> p::VkResult {
    p::VkResult(error.as_raw())
}
fn invalid() -> p::VkResult {
    result(vk::Result::ERROR_INITIALIZATION_FAILED)
}
fn lost() -> p::VkResult {
    result(vk::Result::ERROR_DEVICE_LOST)
}
impl GpuServer {
    /// Clone the serialization owner while holding native_video, then release
    /// that lock before Events or native forwarding can acquire Objects.
    fn event_registry(&self) -> std::result::Result<Registry, p::VkResult> {
        if self.shared.driver.native_submission_failure().is_some() {
            return Err(lost());
        }
        let state = self
            .shared
            .native_video
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state.failure().is_some() {
            return Err(lost());
        }
        Ok(Arc::clone(&state.events))
    }
    fn event_forward(&self) -> sv_gpu_entry::Forward<GpuServer> {
        let mut host = GpuServer::new(Arc::clone(&self.shared));
        host.native_queue_worker = true;
        sv_gpu_entry::Forward::new(host)
    }
    pub(crate) fn native_event_create(
        &mut self,
        a: &p::VkCreateEventArgs,
    ) -> Option<WireResult<p::VkCreateEventReply>> {
        let outcome = (|| {
            let registry = match self.event_registry() {
                Ok(registry) => registry,
                Err(result) => {
                    return Ok(p::VkCreateEventReply {
                        result,
                        p_event: None,
                    });
                }
            };
            let Some(info) = a.p_create_info.as_ref() else {
                return Ok(p::VkCreateEventReply {
                    result: invalid(),
                    p_event: None,
                });
            };
            let Some(event) = a.p_event.filter(|event| event.0 != 0) else {
                return Ok(p::VkCreateEventReply {
                    result: invalid(),
                    p_event: None,
                });
            };
            let mut events = registry.lock().unwrap_or_else(|e| e.into_inner());
            // A new output ID must not overwrite any canonical object type.
            // Vulkan wire IDs are globally fresh within this Shared.
            if self.shared.objects().raw(event.0).is_some() {
                return Ok(p::VkCreateEventReply {
                    result: invalid(),
                    p_event: None,
                });
            }
            let transaction = match events.preflight_create(event.0, info.flags.0) {
                Ok(transaction) => transaction,
                Err(_) => {
                    return Ok(p::VkCreateEventReply {
                        result: invalid(),
                        p_event: None,
                    });
                }
            };
            let mut reply = self.event_forward().create_event(a.clone())?;
            if reply.result.0 == vk::Result::SUCCESS.as_raw() {
                transaction.commit();
            } else {
                // Generic CreateEvent binds its output slot even on VkResult
                // failure. This reserved ID was absent before the native call;
                // discard that failed output without publishing a lifetime.
                self.shared.objects().remove(event.0);
                reply.p_event = None;
            }
            Ok(reply)
        })();
        Some(outcome)
    }
    pub(crate) fn native_event_destroy(
        &mut self,
        a: &p::VkDestroyEventArgs,
    ) -> Option<WireResult<()>> {
        Some((|| {
            let registry = self.event_registry().map_err(|_| WireError::Table)?;
            // Vulkan specifies destruction of a null event as a no-op.
            if a.event.0 == 0 {
                return Ok(());
            }
            let mut events = registry.lock().unwrap_or_else(|e| e.into_inner());
            if self.shared.fns.vk_destroy_event.is_none() {
                return Err(WireError::Table);
            }
            let id = events.id(a.event.0).map_err(|_| WireError::Table)?;
            let transaction = events.preflight_destroy(id).map_err(|_| WireError::Table)?;
            // Missing entry points and pending owners were refused above. The
            // bypass call performs the real destruction and canonical unbind.
            self.event_forward().destroy_event(a.clone())?;
            transaction.commit();
            Ok(())
        })())
    }
    pub(crate) fn native_event_set(
        &mut self,
        a: &p::VkSetEventArgs,
    ) -> Option<WireResult<p::VkSetEventReply>> {
        Some((|| {
            let registry = match self.event_registry() {
                Ok(registry) => registry,
                Err(result) => return Ok(p::VkSetEventReply { result }),
            };
            let mut events = registry.lock().unwrap_or_else(|e| e.into_inner());
            let id = match events.host_id(a.event.0) {
                Ok(id) => id,
                Err(_) => return Ok(p::VkSetEventReply { result: invalid() }),
            };
            let transaction = match events.preflight_host(id, true) {
                Ok(transaction) => transaction,
                Err(_) => return Ok(p::VkSetEventReply { result: invalid() }),
            };
            let reply = self.event_forward().set_event(a.clone())?;
            if reply.result.0 == vk::Result::SUCCESS.as_raw() {
                transaction.commit();
            }
            Ok(reply)
        })())
    }
    pub(crate) fn native_event_reset(
        &mut self,
        a: &p::VkResetEventArgs,
    ) -> Option<WireResult<p::VkResetEventReply>> {
        Some((|| {
            let registry = match self.event_registry() {
                Ok(registry) => registry,
                Err(result) => return Ok(p::VkResetEventReply { result }),
            };
            let mut events = registry.lock().unwrap_or_else(|e| e.into_inner());
            let id = match events.host_id(a.event.0) {
                Ok(id) => id,
                Err(_) => return Ok(p::VkResetEventReply { result: invalid() }),
            };
            let transaction = match events.preflight_host(id, false) {
                Ok(transaction) => transaction,
                Err(_) => return Ok(p::VkResetEventReply { result: invalid() }),
            };
            let reply = self.event_forward().reset_event(a.clone())?;
            if reply.result.0 == vk::Result::SUCCESS.as_raw() {
                transaction.commit();
            }
            Ok(reply)
        })())
    }
    pub(crate) fn native_event_status(
        &mut self,
        a: &p::VkGetEventStatusArgs,
    ) -> Option<WireResult<p::VkGetEventStatusReply>> {
        Some((|| {
            let registry = match self.event_registry() {
                Ok(registry) => registry,
                Err(result) => return Ok(p::VkGetEventStatusReply { result }),
            };
            let events = registry.lock().unwrap_or_else(|e| e.into_inner());
            if events.host_id(a.event.0).is_err() {
                return Ok(p::VkGetEventStatusReply { result: invalid() });
            }
            // The lock prevents destruction during this call. Pending device
            // writes are legal to observe here; never consult projected state.
            self.event_forward().get_event_status(a.clone())
        })())
    }
}
