//! API and wire regressions for acquisition signals independent of GPU queues.
//! The deterministic backend models a queue blocked on a future host timeline
//! signal; these tests make no hardware or native-driver progress claim.
use super::*;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Backend {
    submits: Vec<p::VkQueueSubmitArgs>,
    submits2: Vec<p::VkQueueSubmit2Args>,
    sparse: Vec<p::VkQueueBindSparseArgs>,
    waits: Vec<p::VkWaitForFencesArgs>,
    resets: Vec<p::VkResetFencesArgs>,
    destroyed: Vec<u64>,
    timeline: u64,
    submit_result: i32,
    fence_result: i32,
}
impl p::Server for Backend {
    fn queue_submit(
        &mut self,
        args: p::VkQueueSubmitArgs,
    ) -> sv_wire::Result<p::VkQueueSubmitReply> {
        self.submits.push(args);
        Ok(p::VkQueueSubmitReply {
            result: p::VkResult(self.submit_result),
        })
    }
    fn queue_submit2(
        &mut self,
        args: p::VkQueueSubmit2Args,
    ) -> sv_wire::Result<p::VkQueueSubmit2Reply> {
        self.submits2.push(args);
        Ok(p::VkQueueSubmit2Reply {
            result: p::VkResult(self.submit_result),
        })
    }
    fn queue_bind_sparse(
        &mut self,
        args: p::VkQueueBindSparseArgs,
    ) -> sv_wire::Result<p::VkQueueBindSparseReply> {
        self.sparse.push(args);
        Ok(p::VkQueueBindSparseReply {
            result: p::VkResult(self.submit_result),
        })
    }
    fn get_fence_status(
        &mut self,
        _: p::VkGetFenceStatusArgs,
    ) -> sv_wire::Result<p::VkGetFenceStatusReply> {
        Ok(p::VkGetFenceStatusReply {
            result: p::VkResult(self.fence_result),
        })
    }
    fn wait_for_fences(
        &mut self,
        args: p::VkWaitForFencesArgs,
    ) -> sv_wire::Result<p::VkWaitForFencesReply> {
        self.waits.push(args);
        Ok(p::VkWaitForFencesReply {
            result: p::VkResult(self.fence_result),
        })
    }
    fn reset_fences(
        &mut self,
        args: p::VkResetFencesArgs,
    ) -> sv_wire::Result<p::VkResetFencesReply> {
        self.resets.push(args);
        Ok(p::VkResetFencesReply {
            result: p::VkResult(0),
        })
    }
    fn destroy_fence(&mut self, args: p::VkDestroyFenceArgs) -> sv_wire::Result<()> {
        self.destroyed.push(args.fence.0);
        Ok(())
    }
    fn destroy_semaphore(&mut self, _: p::VkDestroySemaphoreArgs) -> sv_wire::Result<()> {
        Ok(())
    }
    fn signal_semaphore(
        &mut self,
        args: p::VkSignalSemaphoreArgs,
    ) -> sv_wire::Result<p::VkSignalSemaphoreReply> {
        self.timeline = args.p_signal_info.unwrap().value;
        Ok(p::VkSignalSemaphoreReply {
            result: p::VkResult(0),
        })
    }
}
struct BlockingGate {
    entered: std::sync::mpsc::SyncSender<()>,
    resume: Mutex<std::sync::mpsc::Receiver<()>>,
    reject: bool,
}
struct Transport {
    blocking: Option<Arc<BlockingGate>>,
    backend: Arc<Mutex<Backend>>,
    fail_send: Arc<std::sync::atomic::AtomicBool>,
}
impl sv_icd_entry::Transport for Transport {
    fn call(
        &self,
        _: sv_icd_entry::RingClass,
        opcode: u32,
        bytes: &[u8],
        reply: &mut Vec<u8>,
    ) -> IcdResult<()> {
        if let Some(gate) = &self.blocking
            && [
                p::Opcode::VkQueueSubmit as u32,
                p::Opcode::VkQueueSubmit2 as u32,
                p::Opcode::VkQueueBindSparse as u32,
            ]
            .contains(&opcode)
        {
            gate.entered.send(()).unwrap();
            gate.resume.lock().unwrap().recv().unwrap();
            if gate.reject {
                return Err(LowerError::Result(-1));
            }
        }
        p::dispatch(
            &mut sv_wire::Decoder::new(bytes),
            &mut *self.backend.lock().unwrap(),
            reply,
        )
        .map_err(LowerError::Wire)?;
        Ok(())
    }
    fn send(&self, class: sv_icd_entry::RingClass, opcode: u32, bytes: &[u8]) -> IcdResult<()> {
        if self.fail_send.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(LowerError::Result(-4));
        }
        self.call(class, opcode, bytes, &mut Vec::new())
    }
}
fn setup() -> (
    SharedVulkan,
    Arc<Mutex<Backend>>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let backend = Arc::new(Mutex::new(Backend {
        fence_result: 2,
        ..Default::default()
    }));
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut icd = SharedVulkan::new(1 << 20).unwrap();
    icd.set_test_transport(Box::new(Transport {
        blocking: None,
        backend: backend.clone(),
        fail_send: fail.clone(),
    }));
    (icd, backend, fail)
}
fn legacy(waits: &[u64]) -> p::VkQueueSubmitArgs {
    p::VkQueueSubmitArgs {
        queue: p::VkQueue(7),
        submit_count: 1,
        p_submits: vec![p::VkSubmitInfo {
            wait_semaphore_count: waits.len() as u32,
            p_wait_semaphores: waits.iter().copied().map(p::VkSemaphore).collect(),
            p_wait_dst_stage_mask: waits
                .iter()
                .map(|_| p::VkPipelineStageFlags(0x10000))
                .collect(),
            ..Default::default()
        }],
        ..Default::default()
    }
}
fn sync2(waits: &[(u64, u64)]) -> p::VkQueueSubmit2Args {
    p::VkQueueSubmit2Args {
        queue: p::VkQueue(7),
        submit_count: 1,
        p_submits: vec![p::VkSubmitInfo2 {
            wait_semaphore_info_count: waits.len() as u32,
            p_wait_semaphore_infos: waits
                .iter()
                .map(|&(sem, value)| p::VkSemaphoreSubmitInfo {
                    semaphore: p::VkSemaphore(sem),
                    value,
                    stage_mask: p::VkPipelineStageFlags2(0x10000),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }],
        ..Default::default()
    }
}
fn wait(icd: &SharedVulkan, fences: &[u64], all: bool, timeout: u64) -> i32 {
    icd.wait_for_fences(p::VkWaitForFencesArgs {
        device: p::VkDevice(1),
        fence_count: fences.len() as u32,
        p_fences: fences.iter().copied().map(p::VkFence).collect(),
        wait_all: all,
        timeout,
    })
    .unwrap()
    .result
    .0
}
fn reset(icd: &SharedVulkan, fence: u64) -> IcdResult<p::VkResetFencesReply> {
    icd.reset_fences(p::VkResetFencesArgs {
        device: p::VkDevice(1),
        fence_count: 1,
        p_fences: vec![p::VkFence(fence)],
    })
}
#[test]
fn acquisition_fence_completes_before_future_host_timeline_signal() {
    let (icd, backend, _) = setup();
    // Queue work is waiting on T=1, which the application will signal only
    // after acquisition fence F completes. This order is legal for timelines.
    icd.queue_submit2(sync2(&[(10, 1)])).unwrap();
    crate::wsi::test_available_swapchain(&icd, 80);
    assert_eq!(
        crate::wsi::acquire_next_image(&icd, 1, 80, 0, 20, 30).unwrap(),
        0
    );
    assert_eq!(wait(&icd, &[30], true, u64::MAX), 0);
    assert_eq!(
        icd.get_fence_status(p::VkGetFenceStatusArgs {
            device: p::VkDevice(1),
            fence: p::VkFence(30)
        })
        .unwrap()
        .result
        .0,
        0
    );
    icd.signal_semaphore(p::VkSignalSemaphoreArgs {
        device: p::VkDevice(1),
        p_signal_info: Some(Box::new(p::VkSemaphoreSignalInfo {
            semaphore: p::VkSemaphore(10),
            value: 1,
            ..Default::default()
        })),
    })
    .unwrap();
    let backend = backend.lock().unwrap();
    assert_eq!(backend.timeline, 1);
    assert_eq!(backend.submits2.len(), 1);
    assert!(
        backend.submits.is_empty(),
        "acquisition must not put its signal behind blocked work"
    );
    assert!(
        backend.waits.is_empty(),
        "already completed acquisition fence must not wait on native queue"
    );
}
#[test]
fn acquisition_binary_is_consumed_once_and_legacy_parallel_arrays_stay_aligned() {
    let (icd, backend, _) = setup();
    icd.complete_acquisition(20, 0);
    let mut args = legacy(&[10, 20, 11]);
    args.p_submits[0].p_next = vec![
        p::PNextLink::VkTimelineSemaphoreSubmitInfo(Box::new(p::VkTimelineSemaphoreSubmitInfo {
            wait_semaphore_value_count: 3,
            p_wait_semaphore_values: vec![1, 0, 2],
            ..Default::default()
        })),
        p::PNextLink::VkDeviceGroupSubmitInfo(Box::new(p::VkDeviceGroupSubmitInfo {
            wait_semaphore_count: 3,
            p_wait_semaphore_device_indices: vec![0, 0, 0],
            ..Default::default()
        })),
    ];
    icd.queue_submit(args).unwrap();
    icd.queue_submit(legacy(&[20])).unwrap();
    let backend = backend.lock().unwrap();
    let first = &backend.submits[0].p_submits[0];
    assert_eq!(
        first
            .p_wait_semaphores
            .iter()
            .map(|s| s.0)
            .collect::<Vec<_>>(),
        vec![10, 11]
    );
    assert_eq!(first.p_wait_dst_stage_mask.len(), 2);
    if let p::PNextLink::VkTimelineSemaphoreSubmitInfo(t) = &first.p_next[0] {
        assert_eq!(t.p_wait_semaphore_values, vec![1, 2]);
    } else {
        panic!();
    }
    if let p::PNextLink::VkDeviceGroupSubmitInfo(g) = &first.p_next[1] {
        assert_eq!(g.p_wait_semaphore_device_indices.len(), 2);
    } else {
        panic!();
    }
    assert_eq!(
        backend.submits[1].p_submits[0].p_wait_semaphores.len(),
        1,
        "local signal is one-shot"
    );
}
#[test]
fn rejected_or_failed_submissions_preserve_local_binary_payload() {
    let (icd, backend, fail) = setup();
    icd.complete_acquisition(20, 0);
    fail.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(icd.queue_submit(legacy(&[20])).is_err());
    assert!(icd.state().semaphores[&20].presignalled);
    fail.store(false, std::sync::atomic::Ordering::Relaxed);
    backend.lock().unwrap().submit_result = -1;
    assert_eq!(icd.queue_submit2(sync2(&[(20, 0)])).unwrap().result.0, -1);
    assert!(icd.state().semaphores[&20].presignalled);
    backend.lock().unwrap().submit_result = 0;
    assert_eq!(
        icd.queue_submit2(sync2(&[(20, 0), (10, 3)]))
            .unwrap()
            .result
            .0,
        0
    );
    assert!(!icd.state().semaphores[&20].presignalled);
    let backend = backend.lock().unwrap();
    let waits = &backend.submits2.last().unwrap().p_submits[0].p_wait_semaphore_infos;
    assert_eq!(waits.len(), 1);
    assert_eq!((waits[0].semaphore.0, waits[0].value), (10, 3));
}
#[test]
fn mixed_fence_waits_preserve_native_results_timeouts_and_reset_reuse() {
    let (icd, backend, fail) = setup();
    icd.complete_acquisition(0, 30);
    assert_eq!(wait(&icd, &[30, 31], false, 0), 0);
    assert!(backend.lock().unwrap().waits.is_empty());
    assert_eq!(wait(&icd, &[30, 31], true, 17), 2);
    {
        let backend = backend.lock().unwrap();
        assert_eq!(backend.waits[0].p_fences[0].0, 31);
        assert_eq!(backend.waits[0].timeout, 17);
    }
    backend.lock().unwrap().fence_result = -4;
    assert_eq!(wait(&icd, &[30, 31], true, 0), -4);
    // Empty lists are invalid API usage, and must remain backend validation's
    // responsibility rather than accidentally becoming a local success.
    assert_eq!(wait(&icd, &[], true, 0), -4);
    assert_eq!(wait(&icd, &[], false, 0), -4);
    fail.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(reset(&icd, 30).is_err());
    assert!(icd.state().acquired_fences.contains(&30));
    fail.store(false, std::sync::atomic::Ordering::Relaxed);
    reset(&icd, 30).unwrap();
    assert!(!icd.state().acquired_fences.contains(&30));
    assert_eq!(wait(&icd, &[30], true, 0), -4);
    let mut native = legacy(&[]);
    native.fence = p::VkFence(30);
    icd.queue_submit(native).unwrap();
    assert_eq!(backend.lock().unwrap().submits.last().unwrap().fence.0, 30);
    icd.complete_acquisition(0, 30);
    icd.destroy_fence(p::VkDestroyFenceArgs {
        device: p::VkDevice(1),
        fence: p::VkFence(30),
    })
    .unwrap();
    assert!(!icd.state().acquired_fences.contains(&30));
    assert_eq!(backend.lock().unwrap().destroyed, vec![30]);
}
#[test]
fn acquisition_sync_fd_export_consumes_payload_and_destroy_forgets_it() {
    crate::dmabuf::test_enabled_scope(|| {
        let (icd, backend, _) = setup();
        icd.complete_acquisition(20, 0);
        let reply = icd
            .get_semaphore_fd_khr(p::VkGetSemaphoreFdKHRArgs {
                device: p::VkDevice(1),
                p_get_fd_info: Some(Box::new(p::VkSemaphoreGetFdInfoKHR {
                    semaphore: p::VkSemaphore(20),
                    handle_type: p::VkExternalSemaphoreHandleTypeFlagBits(
                        crate::dmabuf::SYNC_FD as i32,
                    ),
                    ..Default::default()
                })),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(reply.p_fd, Some(-1));
        assert!(!icd.state().semaphores[&20].presignalled);
        icd.queue_submit(legacy(&[20])).unwrap();
        assert_eq!(
            backend.lock().unwrap().submits[0].p_submits[0]
                .p_wait_semaphores
                .len(),
            1
        );
        icd.complete_acquisition(20, 0);
        icd.destroy_semaphore(p::VkDestroySemaphoreArgs {
            device: p::VkDevice(1),
            semaphore: p::VkSemaphore(20),
        })
        .unwrap();
        assert!(!icd.state().semaphores.contains_key(&20));
    });
}

#[test]
fn sparse_bind_consumes_acquisition_binary_and_preserves_timeline_values() {
    let (icd, backend, _) = setup();
    icd.complete_acquisition(20, 0);
    let args = p::VkQueueBindSparseArgs {
        queue: p::VkQueue(7),
        bind_info_count: 1,
        p_bind_info: vec![p::VkBindSparseInfo {
            wait_semaphore_count: 2,
            p_wait_semaphores: vec![p::VkSemaphore(20), p::VkSemaphore(10)],
            p_next: vec![p::PNextLink::VkTimelineSemaphoreSubmitInfo(Box::new(
                p::VkTimelineSemaphoreSubmitInfo {
                    wait_semaphore_value_count: 2,
                    p_wait_semaphore_values: vec![0, 5],
                    ..Default::default()
                },
            ))],
            ..Default::default()
        }],
        ..Default::default()
    };
    backend.lock().unwrap().submit_result = -1;
    assert_eq!(icd.queue_bind_sparse(args.clone()).unwrap().result.0, -1);
    assert!(icd.state().semaphores[&20].presignalled);
    backend.lock().unwrap().submit_result = 0;
    assert_eq!(icd.queue_bind_sparse(args).unwrap().result.0, 0);
    assert!(!icd.state().semaphores[&20].presignalled);
    let backend = backend.lock().unwrap();
    let batch = &backend.sparse[1].p_bind_info[0];
    assert_eq!(batch.wait_semaphore_count, 1);
    assert_eq!(batch.p_wait_semaphores[0].0, 10);
    if let p::PNextLink::VkTimelineSemaphoreSubmitInfo(t) = &batch.p_next[0] {
        assert_eq!(t.p_wait_semaphore_values, vec![5]);
    } else {
        panic!();
    }
}

#[test]
fn temporary_import_wait_and_export_restore_permanent_acquisition_payload() {
    crate::dmabuf::test_enabled_scope(|| {
        let (icd, _, _) = setup();
        icd.complete_acquisition(20, 0);
        let import = || {
            icd.import_semaphore_fd_khr(p::VkImportSemaphoreFdKHRArgs {
                device: p::VkDevice(1),
                p_import_semaphore_fd_info: Some(Box::new(p::VkImportSemaphoreFdInfoKHR {
                    semaphore: p::VkSemaphore(20),
                    flags: p::VkSemaphoreImportFlags(1),
                    handle_type: p::VkExternalSemaphoreHandleTypeFlagBits(
                        crate::dmabuf::SYNC_FD as i32,
                    ),
                    fd: -1,
                    ..Default::default()
                })),
            })
            .unwrap()
        };
        import();
        icd.queue_submit(legacy(&[20])).unwrap();
        assert!(icd.state().semaphores[&20].presignalled);
        assert!(!icd.state().semaphores[&20].temporary_presignalled);
        import();
        let exported = icd
            .get_semaphore_fd_khr(p::VkGetSemaphoreFdKHRArgs {
                device: p::VkDevice(1),
                p_get_fd_info: Some(Box::new(p::VkSemaphoreGetFdInfoKHR {
                    semaphore: p::VkSemaphore(20),
                    handle_type: p::VkExternalSemaphoreHandleTypeFlagBits(
                        crate::dmabuf::SYNC_FD as i32,
                    ),
                    ..Default::default()
                })),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(exported.p_fd, Some(-1));
        assert!(icd.state().semaphores[&20].presignalled);
        assert!(!icd.state().semaphores[&20].temporary_presignalled);
        icd.queue_submit2(sync2(&[(20, 0)])).unwrap();
        assert!(!icd.state().semaphores[&20].presignalled);
    });
}
#[test]
fn failed_acquire_does_not_publish_sync_payloads() {
    let (icd, _, _) = setup();
    crate::wsi::test_available_swapchain(&icd, 80);
    crate::wsi::acquire_next_image(&icd, 1, 80, 0, 20, 30).unwrap();
    assert!(matches!(
        crate::wsi::acquire_next_image(&icd, 1, 80, 0, 21, 31),
        Err(LowerError::Result(1))
    ));
    assert!(!icd.state().semaphores.contains_key(&21));
    assert!(!icd.state().acquired_fences.contains(&31));
}

#[test]
fn blocked_transport_never_blocks_unrelated_acquisition_or_fence_operations() {
    for mode in 0..3 {
        for reject in [false, true] {
            let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
            let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
            let backend = Arc::new(Mutex::new(Backend::default()));
            let mut icd = SharedVulkan::new(1 << 20).unwrap();
            icd.set_test_transport(Box::new(Transport {
                backend,
                fail_send: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                blocking: Some(Arc::new(BlockingGate {
                    entered: entered_tx,
                    resume: Mutex::new(resume_rx),
                    reject,
                })),
            }));
            let icd = Arc::new(icd);
            icd.complete_acquisition(20, 0);
            crate::wsi::test_available_swapchain(&icd, 80);
            let submitting = Arc::clone(&icd);
            let worker = std::thread::spawn(move || match mode {
                0 => submitting.queue_submit(legacy(&[20])).map(|r| r.result.0),
                1 => submitting
                    .queue_submit2(sync2(&[(20, 0)]))
                    .map(|r| r.result.0),
                _ => submitting
                    .queue_bind_sparse(p::VkQueueBindSparseArgs {
                        queue: p::VkQueue(7),
                        bind_info_count: 1,
                        p_bind_info: vec![p::VkBindSparseInfo {
                            wait_semaphore_count: 1,
                            p_wait_semaphores: vec![p::VkSemaphore(20)],
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .map(|r| r.result.0),
            });
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
            let acquiring = Arc::clone(&icd);
            let unrelated = std::thread::spawn(move || {
                let index = crate::wsi::acquire_next_image(&acquiring, 1, 80, 0, 21, 31).unwrap();
                let status = acquiring
                    .get_fence_status(p::VkGetFenceStatusArgs {
                        device: p::VkDevice(1),
                        fence: p::VkFence(31),
                    })
                    .unwrap()
                    .result
                    .0;
                reset(&acquiring, 31).unwrap();
                done_tx.send((index, status)).unwrap();
            });
            let result = done_rx.recv_timeout(std::time::Duration::from_secs(2));
            // Always release the transport before assertions so a regression
            // cannot leave the test's worker blocked during failure cleanup.
            resume_tx.send(()).unwrap();
            let submitted = worker.join().unwrap();
            unrelated.join().unwrap();
            assert_eq!(result.unwrap(), (0, 0));
            assert_eq!(submitted.is_err(), reject);
            assert_eq!(icd.state().semaphores[&20].presignalled, reject);
            assert!(
                icd.state().semaphores[&21].presignalled,
                "rollback must not overwrite an unrelated acquisition"
            );
            assert!(!icd.state().acquired_fences.contains(&31));
        }
    }
}
