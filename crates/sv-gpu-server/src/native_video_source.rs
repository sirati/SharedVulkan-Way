//! Original shared compressed-buffer ownership, keyed only by wire identities.
//! Preparation resolves metadata without reading payload. The queue must finish
//! its waits before activating the per-job immutable input lease.
use crate::{server::Shared, table::Objects};
use ash::vk;
use std::{collections::HashMap, ops::Range, sync::Arc};
use sv_transport::region::SharedRegion;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Binding {
    memory: u64,
    offset: u64,
}
#[derive(Clone, Copy)]
struct Buffer {
    size: u64,
    usage: vk::BufferUsageFlags,
    binding: Option<Binding>,
}
#[derive(Default)]
pub(crate) struct SourceRegistry {
    buffers: HashMap<u64, Buffer>,
}
impl SourceRegistry {
    /// Called only after successful native creation, with the original wire
    /// usage before logical VIDEO_DECODE_SRC is translated for the driver.
    pub(crate) fn created(&mut self, wire: u64, size: u64, usage: vk::BufferUsageFlags) {
        if wire != 0 {
            self.buffers.insert(
                wire,
                Buffer {
                    size,
                    usage,
                    binding: None,
                },
            );
        }
    }
    /// Called only after the native bind succeeds, before losing the original
    /// wire memory identity and its arena/pool offset to native translation.
    pub(crate) fn bound(&mut self, wire: u64, memory: u64, offset: u64) {
        if let Some(buffer) = self.buffers.get_mut(&wire) {
            buffer.binding = Some(Binding { memory, offset });
        }
    }
    pub(crate) fn destroyed(&mut self, wire: u64) {
        self.buffers.remove(&wire);
    }
    pub(crate) fn clear(&mut self) {
        self.buffers.clear();
    }
    fn source_range(&self, wire: u64, offset: u64, len: u64) -> Result<Range<u64>, String> {
        let buffer = self
            .buffers
            .get(&wire)
            .ok_or("unknown original compressed buffer")?;
        if !buffer
            .usage
            .contains(vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR)
        {
            return Err("compressed buffer lacks logical VIDEO_DECODE_SRC usage".into());
        }
        if len == 0 || offset.checked_add(len).is_none_or(|end| end > buffer.size) {
            return Err("compressed range exceeds original buffer size".into());
        }
        let binding = buffer.binding.ok_or("compressed buffer is unbound")?;
        // Host arena allocations are rewritten to this wire identity by the
        // stock client. Native device-local memory is not an original mapping.
        if binding.memory != crate::ARENA_MEMORY_ID {
            return Err("compressed buffer is not backed by original shared arena memory".into());
        }
        let start = binding
            .offset
            .checked_add(offset)
            .ok_or("compressed binding offset overflow")?;
        let end = start.checked_add(len).ok_or("compressed range overflow")?;
        Ok(start..end)
    }
    /// No payload bytes are read here. `objects` must be the object table of
    /// `shared`; the caller holds that session's object-table lock throughout.
    pub(crate) fn prepare(
        &self,
        shared: Arc<Shared>,
        objects: &Objects,
        wire: u64,
        offset: u64,
        len: u64,
    ) -> Result<Prepared, String> {
        let range = self.source_range(wire, offset, len)?;
        let (region, range) =
            resolve_region(objects, &shared.arena.region, shared.arena.len, range)?;
        Ok(Prepared {
            shared,
            region,
            range,
        })
    }
}
fn resolve_region(
    objects: &Objects,
    arena: &Arc<SharedRegion>,
    arena_len: u64,
    range: Range<u64>,
) -> Result<(Arc<SharedRegion>, Range<usize>), String> {
    let len = range
        .end
        .checked_sub(range.start)
        .filter(|len| *len > 0)
        .ok_or("invalid compressed range")?;
    let (region, local) = if let Some(pool) = &objects.pool {
        if !objects.leased_range(range.start, len) {
            return Err("compressed range is outside one live session pool lease".into());
        }
        let (id, local) = sv_arena::pool::split(range.start);
        let (last_id, _) = sv_arena::pool::split(range.end - 1);
        if id != last_id {
            return Err("compressed range crosses pool regions".into());
        }
        (
            pool.server
                .region(id)
                .ok_or("compressed pool region is absent")?,
            local,
        )
    } else {
        if range.end > arena_len {
            return Err("compressed range exceeds original arena".into());
        }
        (arena.clone(), range.start)
    };
    let range = checked_local_range(local, len, region.len())?;
    Ok((region, range))
}
fn checked_local_range(offset: u64, len: u64, capacity: usize) -> Result<Range<usize>, String> {
    let start = usize::try_from(offset).map_err(|_| "compressed offset exceeds address space")?;
    let len = usize::try_from(len).map_err(|_| "compressed length exceeds address space")?;
    let end = start
        .checked_add(len)
        .ok_or("compressed address range overflow")?;
    if len == 0 || end > capacity || len > isize::MAX as usize {
        return Err("compressed range exceeds shared mapping".into());
    }
    Ok(start..end)
}
pub(crate) struct Prepared {
    // Keep the exact session and mapping alive through the final CM FreeBlock.
    shared: Arc<Shared>,
    region: Arc<SharedRegion>,
    range: Range<usize>,
}
#[cfg(target_os = "macos")]
impl Prepared {
    /// # Safety
    /// All submit waits and prior writers have completed. The caller transfers
    /// an immutable read lease over this exact original range until the final
    /// CompressedBytes owner drops, including across process/allocator reuse.
    /// Metadata preparation alone never establishes this ownership handoff.
    pub(crate) unsafe fn after_wait(
        self,
    ) -> (
        Arc<dyn crate::video_nv12::videotoolbox::CompressedBytes>,
        Range<usize>,
    ) {
        let len = self.range.len();
        (Arc::new(InputLease(self)), 0..len)
    }
}
#[cfg(target_os = "macos")]
struct InputLease(Prepared);
#[cfg(target_os = "macos")]
unsafe impl crate::video_nv12::videotoolbox::CompressedBytes for InputLease {
    fn bytes(&self) -> &[u8] {
        // SAFETY: after_wait's ownership contract covers this exact bounded
        // range until this final lease drops. No ordinary payload copy occurs.
        unsafe { original_bytes(&self.0.region, &self.0.range) }
    }
}
// Caller owns immutable access to this checked region range for the returned
// borrow, including the peer process. This function never reads or copies early.
#[cfg(any(target_os = "macos", test))]
unsafe fn original_bytes<'a>(region: &'a SharedRegion, range: &Range<usize>) -> &'a [u8] {
    unsafe { std::slice::from_raw_parts(region.base_ptr().add(range.start), range.len()) }
}
/// Preserve every ordinary usage bit while replacing unsupported logical video
/// source usage with a driver-supported source usage. Call only when ready.
pub(crate) fn driver_usage(usage: vk::BufferUsageFlags) -> vk::BufferUsageFlags {
    if usage.contains(vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR) {
        (usage & !vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR) | vk::BufferUsageFlags::TRANSFER_SRC
    } else {
        usage
    }
}

/// Captured before driver handle/usage mapping. A failed create never registers.
pub(crate) struct Creation {
    wire: u64,
    size: u64,
    usage: vk::BufferUsageFlags,
}
impl Creation {
    pub(crate) fn prepare(active: bool, args: &mut sv_proto::VkCreateBufferArgs) -> Option<Self> {
        if !active {
            return None;
        }
        let info = args.p_create_info.as_deref_mut()?;
        let mut effective = u64::from(info.usage.0);
        for link in &info.p_next {
            if let sv_proto::PNextLink::VkBufferUsageFlags2CreateInfo(flags) = link {
                effective = flags.usage.0;
            }
        }
        let original = Self {
            wire: args.p_buffer?.0,
            size: info.size,
            usage: vk::BufferUsageFlags::from_raw(effective as u32),
        };
        if effective & u64::from(vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR.as_raw()) != 0 {
            info.usage.0 = driver_usage(vk::BufferUsageFlags::from_raw(info.usage.0)).as_raw();
            for link in &mut info.p_next {
                if let sv_proto::PNextLink::VkBufferUsageFlags2CreateInfo(flags) = link {
                    flags.usage.0 = (flags.usage.0
                        & !u64::from(vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR.as_raw()))
                        | u64::from(vk::BufferUsageFlags::TRANSFER_SRC.as_raw());
                }
            }
            // This describes the logical video resource. The native buffer is
            // an ordinary source buffer on a driver without Vulkan Video.
            info.p_next
                .retain(|link| !matches!(link, sv_proto::PNextLink::VkVideoProfileListInfoKHR(_)));
        }
        Some(original)
    }
    pub(crate) fn complete(self, registry: &mut SourceRegistry, result: i32) {
        if result == 0 {
            registry.created(self.wire, self.size, self.usage);
        }
    }
}

#[cfg(target_os = "macos")]
mod hooks {
    use super::*;
    use crate::server::GpuServer;
    use sv_proto::{self as p, Server};
    impl GpuServer {
        pub(crate) fn native_bind_video_source(
            &self,
            a: &p::VkBindBufferMemoryArgs,
        ) -> Option<sv_wire::Result<p::VkBindBufferMemoryReply>> {
            if !self
                .shared
                .native_video
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .available()
            {
                return None;
            }
            let mut replay = GpuServer::new(self.shared.clone());
            replay.native_queue_worker = true;
            let result = sv_gpu_entry::Forward::new(replay).bind_buffer_memory(a.clone());
            Some(result.map(|reply| {
                if reply.result.0 == 0 {
                    self.shared.objects().native_video.sources.bound(
                        a.buffer.0,
                        a.memory.0,
                        a.memory_offset,
                    );
                }
                reply
            }))
        }
        pub(crate) fn native_bind_video_sources(
            &self,
            a: &p::VkBindBufferMemory2Args,
        ) -> Option<sv_wire::Result<p::VkBindBufferMemory2Reply>> {
            if !self
                .shared
                .native_video
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .available()
            {
                return None;
            }
            let mut replay = GpuServer::new(self.shared.clone());
            replay.native_queue_worker = true;
            let result = sv_gpu_entry::Forward::new(replay).bind_buffer_memory2(a.clone());
            Some(result.map(|reply| {
                // A failed batch may have partially bound native objects. Do not
                // manufacture successful original associations for any member.
                if reply.result.0 == 0 {
                    let mut objects = self.shared.objects();
                    for info in &a.p_bind_infos {
                        objects.native_video.sources.bound(
                            info.buffer.0,
                            info.memory.0,
                            info.memory_offset,
                        );
                    }
                }
                reply
            }))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn registry() -> SourceRegistry {
        let mut r = SourceRegistry::default();
        r.created(3, 100, vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR);
        r
    }
    #[test]
    fn wire_binding_keeps_original_offset_and_exact_buffer_bounds() {
        let mut r = registry();
        assert!(r.source_range(3, 0, 1).is_err());
        r.bound(3, crate::ARENA_MEMORY_ID, 4096);
        assert_eq!(r.source_range(3, 10, 90).unwrap(), 4106..4196);
        assert!(r.source_range(3, 10, 91).is_err());
        assert!(r.source_range(3, 0, 0).is_err());
        r.destroyed(3);
        assert!(r.source_range(3, 0, 1).is_err());
    }
    #[test]
    fn native_allocations_usage_mismatch_and_overflow_are_refused() {
        let mut r = registry();
        r.bound(3, 7, 0);
        assert!(r.source_range(3, 0, 1).is_err());
        r.bound(3, crate::ARENA_MEMORY_ID, u64::MAX);
        assert!(r.source_range(3, 1, 1).is_err());
        r.created(3, 100, vk::BufferUsageFlags::TRANSFER_SRC);
        r.bound(3, crate::ARENA_MEMORY_ID, 0);
        assert!(r.source_range(3, 0, 1).is_err());
    }
    #[test]
    fn logical_usage_translation_preserves_other_buffer_uses() {
        let ordinary = vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST;
        assert_eq!(driver_usage(ordinary), ordinary);
        assert_eq!(
            driver_usage(ordinary | vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR),
            ordinary | vk::BufferUsageFlags::TRANSFER_SRC
        );
    }
    #[test]
    fn pointer_range_rejects_mapping_and_address_overflows() {
        assert_eq!(checked_local_range(5, 7, 12).unwrap(), 5..12);
        assert!(checked_local_range(5, 8, 12).is_err());
        assert!(checked_local_range(u64::MAX, 2, usize::MAX).is_err());
        assert!(checked_local_range(0, 0, 12).is_err());
    }
    #[test]
    fn original_payload_is_the_exact_mapping_slice() {
        let region = SharedRegion::heap(4096).unwrap();
        region.write_bytes(100, &[0, 0, 1, 0x65, 42]).unwrap();
        let range = checked_local_range(100, 5, region.len()).unwrap();
        // This test owns the region and performs no writes during the borrow.
        let bytes = unsafe { original_bytes(&region, &range) };
        assert_eq!(bytes, &[0, 0, 1, 0x65, 42]);
        assert_eq!(bytes.as_ptr(), unsafe { region.base_ptr().add(100) });
    }
    #[test]
    fn inactive_create_is_unchanged_and_failed_create_does_not_register() {
        let mut args = sv_proto::VkCreateBufferArgs::default();
        args.p_buffer = Some(sv_proto::VkBuffer(3));
        let mut info = sv_proto::VkBufferCreateInfo::default();
        info.size = 100;
        info.usage.0 = vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR.as_raw();
        args.p_create_info = Some(Box::new(info));
        assert!(Creation::prepare(false, &mut args).is_none());
        assert_eq!(
            args.p_create_info.as_ref().unwrap().usage.0,
            vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR.as_raw()
        );
        let capture = Creation::prepare(true, &mut args).unwrap();
        assert_eq!(
            args.p_create_info.as_ref().unwrap().usage.0,
            vk::BufferUsageFlags::TRANSFER_SRC.as_raw()
        );
        let mut registry = SourceRegistry::default();
        capture.complete(&mut registry, -1);
        assert!(!registry.buffers.contains_key(&3));
    }
    #[test]
    fn pool_source_must_fit_one_current_session_lease_and_keeps_region_identity() {
        use crate::pool::{Importer, PoolServer};
        use crate::table::LeaseMem;
        use sv_arena::pool::{PoolConfig, Purpose};
        let region = Arc::new(SharedRegion::heap(64 << 20).unwrap());
        let pool = PoolServer::new(
            PoolConfig::for_capacity(64 << 20),
            region.clone(),
            Importer::HostPointer,
        )
        .unwrap();
        let binding = pool.open_session("compressed-test");
        let lease = pool
            .lease_raw(binding.session, 1 << 20, Purpose::Chunk)
            .unwrap();
        let mut objects = Objects::new();
        objects.set_pooled(binding);
        objects.add_lease(
            lease.offset,
            LeaseMem {
                len: lease.len,
                memory: vk::DeviceMemory::null(),
                base: 0,
                owned: false,
            },
        );
        let source = lease.offset + 100..lease.offset + 105;
        let (actual, local) =
            resolve_region(&objects, &region, region.len() as u64, source).unwrap();
        assert!(Arc::ptr_eq(&actual, &region));
        assert_eq!(
            local.start as u64,
            sv_arena::pool::split(lease.offset).1 + 100
        );
        assert!(
            resolve_region(
                &objects,
                &region,
                region.len() as u64,
                lease.offset + lease.len - 1..lease.offset + lease.len + 1
            )
            .is_err()
        );
        objects.remove_lease(lease.offset);
        assert!(
            resolve_region(
                &objects,
                &region,
                region.len() as u64,
                lease.offset..lease.offset + 1
            )
            .is_err()
        );
        pool.release(lease.offset).unwrap();
    }
    #[test]
    fn effective_usage2_preserves_high_ordinary_bits_and_records_logical_source() {
        let mut args = sv_proto::VkCreateBufferArgs::default();
        args.p_buffer = Some(sv_proto::VkBuffer(9));
        let mut info = sv_proto::VkBufferCreateInfo::default();
        info.size = 100;
        info.usage.0 = vk::BufferUsageFlags::STORAGE_BUFFER.as_raw();
        let mut usage = sv_proto::VkBufferUsageFlags2CreateInfo::default();
        let high = 1u64 << 40;
        usage.usage.0 = high | u64::from(vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR.as_raw());
        info.p_next
            .push(sv_proto::PNextLink::VkBufferUsageFlags2CreateInfo(
                Box::new(usage),
            ));
        args.p_create_info = Some(Box::new(info));
        let capture = Creation::prepare(true, &mut args).unwrap();
        let sv_proto::PNextLink::VkBufferUsageFlags2CreateInfo(flags) =
            &args.p_create_info.as_ref().unwrap().p_next[0]
        else {
            panic!()
        };
        assert_eq!(
            flags.usage.0,
            high | u64::from(vk::BufferUsageFlags::TRANSFER_SRC.as_raw())
        );
        let mut registry = SourceRegistry::default();
        capture.complete(&mut registry, 0);
        registry.bound(9, crate::ARENA_MEMORY_ID, 0);
        assert_eq!(registry.source_range(9, 0, 100).unwrap(), 0..100);
    }
}
