//! The GPU side's dispatchers have to fit on the thread that runs them, with
//! room left for the driver.
//!
//! docs/design.md §0.28. In a debug build `sv_proto::dispatch` was a
//! **391,208-byte** frame and `sv_proto::subdecode` a 104,568-byte one, because
//! a `match` whose arms decode and call inline is one frame holding every arm's
//! locals at once. They run on `sv-gpu-server`'s ring threads, which Rust gives
//! 2 MiB, and the driver they call into — RADV compiling a pipeline, say — runs
//! below them. Nothing had faulted; §0.27 is what the same class did on the app
//! side once it did.
//!
//! Each test runs one dispatcher on a thread with **64 KiB**, the budget
//! `sv-icd-entry/tests/stack.rs` uses for the app side's walkers. Against the old
//! generator both abort the test binary with "has overflowed its stack", which
//! is why they live in a file of their own.

use sv_proto::*;

const STACK: usize = 64 * 1024;

fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .name("sv-small-stack".into())
        .stack_size(STACK)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("the dispatcher returned rather than unwinding")
}

#[derive(Default)]
struct Seen {
    pipelines: usize,
    vertices: u32,
}

impl Server for Seen {
    fn create_graphics_pipelines(
        &mut self,
        args: VkCreateGraphicsPipelinesArgs,
    ) -> sv_wire::Result<VkCreateGraphicsPipelinesReply> {
        self.pipelines += args.p_create_infos.len();
        Ok(VkCreateGraphicsPipelinesReply {
            result: VkResult(0),
            p_pipelines: args.p_pipelines,
        })
    }

    fn cmd_draw(&mut self, args: VkCmdDrawArgs) -> sv_wire::Result<()> {
        self.vertices += args.vertex_count;
        Ok(())
    }
}

fn encoded(size: usize, f: impl FnOnce(&mut sv_wire::Encoder<'_>) -> sv_wire::Result<()>) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    let mut enc = sv_wire::Encoder::new(&mut buf);
    f(&mut enc).unwrap();
    enc.finish().unwrap();
    buf
}

/// The ring dispatcher, on the command with the widest arguments Zink sends.
#[test]
fn dispatch_fits_in_64_kib() {
    let pipelines = on_small_stack(|| {
        let args = VkCreateGraphicsPipelinesArgs {
            device: VkDevice(1),
            create_info_count: 2,
            p_create_infos: vec![VkGraphicsPipelineCreateInfo::default(); 2],
            p_pipelines: vec![VkPipeline(7), VkPipeline(8)],
            ..Default::default()
        };
        let bytes = encoded(size_cmd_vk_create_graphics_pipelines(&args), |enc| {
            encode_cmd_vk_create_graphics_pipelines(enc, 0, &args)
        });
        let mut srv = Seen::default();
        let mut reply = Vec::new();
        let op = dispatch(&mut sv_wire::Decoder::new(&bytes), &mut srv, &mut reply).unwrap();
        assert_eq!(op, Opcode::VkCreateGraphicsPipelines);
        assert!(!reply.is_empty(), "a command with a reply encodes one");
        srv.pipelines
    });
    assert_eq!(pipelines, 2);
}

/// The blob sub-decoder, on a recording of draws.
#[test]
fn subdecode_fits_in_64_kib() {
    let (count, vertices) = on_small_stack(|| {
        let mut blob = Vec::new();
        for i in 0..100u32 {
            let args = VkCmdDrawArgs { command_buffer: VkCommandBuffer(3), vertex_count: i, ..Default::default() };
            blob.extend(encoded(size_cmd_vk_cmd_draw(&args), |enc| encode_cmd_vk_cmd_draw(enc, 0, &args)));
        }
        let mut srv = Seen::default();
        let n = subdecode(&mut sv_wire::Decoder::new(&blob), &mut srv).unwrap();
        (n, srv.vertices)
    });
    assert_eq!(count, 100);
    assert_eq!(vertices, (0..100).sum::<u32>());
}
