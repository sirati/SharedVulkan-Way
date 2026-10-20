//! Captured arrays keep their owned allocation and original wire handles.
use sv_gpu_entry::{DriverFns, RecordedCall, RecordedRestoration};
use sv_proto as p;

#[test]
fn recorded_arguments_move_and_snapshots_are_independent() {
    let buffers = vec![p::VkBuffer(81), p::VkBuffer(82)];
    let original_allocation = buffers.as_ptr();
    let call = RecordedCall::VkCmdBindVertexBuffers(Box::new(p::VkCmdBindVertexBuffersArgs {
        command_buffer: p::VkCommandBuffer(41),
        first_binding: 3,
        binding_count: 2,
        p_buffers: buffers,
        p_offsets: vec![16, 32],
    }));
    assert_eq!(call.command_buffer(), 41);
    assert_eq!(call.name(), "vkCmdBindVertexBuffers");
    assert!(call.is_state_capable());
    assert!(!call.allowed_in_video_scope());
    assert_eq!(call.restoration(), RecordedRestoration::AuditedState);
    let mut snapshot = call.clone();
    let RecordedCall::VkCmdBindVertexBuffers(original) = &call else {
        unreachable!()
    };
    assert_eq!(original.p_buffers.as_ptr(), original_allocation);
    let RecordedCall::VkCmdBindVertexBuffers(snapshot) = &mut snapshot else {
        unreachable!()
    };
    snapshot.p_buffers[0] = p::VkBuffer(999);
    snapshot.p_offsets[0] = 999;
    assert_eq!(original.p_buffers[0].0, 81);
    assert_eq!(original.p_offsets[0], 16);
    assert_eq!(original.command_buffer.0, 41);
}

#[test]
fn availability_checks_the_exact_native_entrypoint() {
    unsafe extern "system" fn draw(
        _: sv_gpu_entry::VkCommandBuffer,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
    ) {
    }
    let call = RecordedCall::VkCmdDraw(Box::new(p::VkCmdDrawArgs {
        command_buffer: p::VkCommandBuffer(41),
        vertex_count: 3,
        instance_count: 1,
        first_vertex: 0,
        first_instance: 0,
    }));
    assert!(!call.is_state_capable());
    assert!(!call.allowed_in_video_scope());
    assert!(
        RecordedCall::VkCmdPipelineBarrier2(Box::new(Default::default())).allowed_in_video_scope()
    );
    let mut fns = DriverFns::default();
    assert!(!call.available(&fns));
    fns.vk_cmd_draw = Some(draw);
    assert!(call.available(&fns));
    assert!(!RecordedCall::VkCmdBindVertexBuffers(Box::new(Default::default())).available(&fns));
}
