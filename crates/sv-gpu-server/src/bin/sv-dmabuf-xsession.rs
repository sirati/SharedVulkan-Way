//! `sv-dmabuf-xsession` — a dma-buf handed from one application to another
//! through one GPU process (waycompose-design.md §104.11, task (d)).
//!
//! Two processes, two ICD sessions, one `sv-gpu-server pool-guest`: what a
//! Plasma client and KWin are on the test host.
//!
//! ```text
//! sv-dmabuf-xsession producer SOCK &     # "the client"
//! sv-dmabuf-xsession consumer SOCK       # "KWin"
//! ```
//!
//! 1. The producer allocates an exportable LINEAR image, records a slow batch
//!    (many clears of a large scratch image) that ends with a clear of the
//!    shared image to colour X, submits it, exports its sync fd (returns at
//!    once, -1) and hands the token fd over the unix socket at once — while
//!    the GPU is still busy.
//! 2. The consumer imports the token (`svImportShared` of a section another
//!    session made), acquires it as Zink does (`vkGetMemoryFdKHR` with no
//!    export in front: the next submit waits for the producer's work on the
//!    GPU side), copies the image into a host-visible buffer and checks every
//!    pixel is X. Without the cross-session wait the copy could read the image
//!    before the producer's clear.
//! 3. The consumer then clears the imported image to Y, exports its sync fd,
//!    and sends the producer its fence record and number; the producer waits
//!    on that record as the compositor would and reads Y from the section by
//!    name, and then copies its own image on the GPU (after an acquire, which
//!    waits for the consumer's marks on the section) and sees Y too.
//! 4. The producer frees its memory first; the section survives (the consumer
//!    still holds it), and goes when the consumer frees it.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sv-dmabuf-xsession: the dma-buf token is a Linux memfd; nothing to test here");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(target_os = "linux")]
mod linux {
use ash::vk;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

const W: u32 = 256;
const H: u32 = 128;
const FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;
const X: [f32; 4] = [0.2, 0.4, 0.6, 0.8];
const X_PIXEL: [u8; 4] = [153, 102, 51, 204];
const Y: [f32; 4] = [1.0, 0.0, 0.2, 1.0];
const Y_PIXEL: [u8; 4] = [51, 0, 255, 255];
/// The scratch image the producer keeps the GPU busy with.
const BIG: u32 = 2048;
const BUSY_CLEARS: usize = 600;

pub fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (role, sock) = match (args.get(1).map(String::as_str), args.get(2)) {
        (Some(r @ ("producer" | "consumer")), Some(s)) => (r.to_string(), s.clone()),
        _ => {
            eprintln!("usage: sv-dmabuf-xsession producer|consumer SOCKET");
            return std::process::ExitCode::from(2);
        }
    };
    let r = if role == "producer" { producer(&sock) } else { consumer(&sock) };
    match r {
        Ok(()) => {
            println!("[{role}] PASS");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[{role}] FAIL: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn say(role: &str, ok: bool, what: impl std::fmt::Display) -> Result<(), String> {
    if ok {
        println!("[{role}] ok   {what}");
        Ok(())
    } else {
        Err(format!("{what}"))
    }
}

// -- the socket: one line of text, with at most one fd -----------------------

fn send(s: &UnixStream, line: &str, fd: Option<RawFd>) -> Result<(), String> {
    let bytes = format!("{line}\n");
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut _, iov_len: bytes.len() };
    let mut cbuf = [0u8; 64];
    // SAFETY: a zeroed msghdr pointing at live buffers.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(fd) = fd {
        // SAFETY: CMSG macros over a buffer large enough for one fd.
        unsafe {
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(4) as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(4) as _;
            std::ptr::copy_nonoverlapping((&fd as *const RawFd).cast::<u8>(), libc::CMSG_DATA(c), 4);
        }
    }
    // SAFETY: as above.
    let n = unsafe { libc::sendmsg(s.as_raw_fd(), &msg, 0) };
    if n as usize != bytes.len() {
        return Err(format!("sendmsg: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

fn recv(s: &mut UnixStream) -> Result<(String, Option<OwnedFd>), String> {
    let mut buf = vec![0u8; 1024];
    let mut cbuf = [0u8; 64];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    // SAFETY: a zeroed msghdr pointing at live buffers.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = cbuf.len() as _;
    // SAFETY: as above.
    let n = unsafe { libc::recvmsg(s.as_raw_fd(), &mut msg, 0) };
    if n <= 0 {
        return Err(format!("recvmsg: {} ({n})", std::io::Error::last_os_error()));
    }
    let mut fd = None;
    // SAFETY: walking the control messages the kernel wrote.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
            let mut raw: RawFd = -1;
            std::ptr::copy_nonoverlapping(libc::CMSG_DATA(c), (&mut raw as *mut RawFd).cast::<u8>(), 4);
            fd = Some(OwnedFd::from_raw_fd(raw));
        }
    }
    let line = String::from_utf8_lossy(&buf[..n as usize]).trim_end().to_string();
    Ok((line, fd))
}

// -- Vulkan -------------------------------------------------------------------

struct Gpu {
    _entry: ash::Entry,
    instance: ash::Instance,
    d: ash::Device,
    queue: vk::Queue,
    memfd: ash::khr::external_memory_fd::Device,
    semfd: ash::khr::external_semaphore_fd::Device,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    pool: vk::CommandPool,
}

impl Gpu {
    fn new(name: &std::ffi::CStr) -> Result<Gpu, String> {
        let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("no Vulkan loader: {e}"))?;
        let app = vk::ApplicationInfo::default().application_name(name).api_version(vk::API_VERSION_1_2);
        let instance = unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) }
            .map_err(|e| format!("vkCreateInstance: {e:?}"))?;
        let pd = *unsafe { instance.enumerate_physical_devices() }.map_err(|e| format!("{e:?}"))?.first().ok_or("no device")?;
        let names = [
            c"VK_KHR_external_memory_fd".as_ptr(),
            c"VK_EXT_external_memory_dma_buf".as_ptr(),
            c"VK_EXT_image_drm_format_modifier".as_ptr(),
            c"VK_EXT_queue_family_foreign".as_ptr(),
            c"VK_KHR_external_semaphore_fd".as_ptr(),
        ];
        let prio = [1.0f32];
        let qi = [vk::DeviceQueueCreateInfo::default().queue_family_index(0).queue_priorities(&prio)];
        let d = unsafe {
            instance.create_device(pd, &vk::DeviceCreateInfo::default().queue_create_infos(&qi).enabled_extension_names(&names), None)
        }
        .map_err(|e| format!("vkCreateDevice: {e:?}"))?;
        let queue = unsafe { d.get_device_queue(0, 0) };
        let memfd = ash::khr::external_memory_fd::Device::new(&instance, &d);
        let semfd = ash::khr::external_semaphore_fd::Device::new(&instance, &d);
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };
        let pool = unsafe { d.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(0), None) }
            .map_err(|e| format!("{e:?}"))?;
        Ok(Gpu { _entry: entry, instance, d, queue, memfd, semfd, mem_props, pool })
    }

    fn image(&self, explicit_pitch: Option<u64>) -> Result<vk::Image, String> {
        let usage = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC;
        let mods = [0u64];
        let planes = [vk::SubresourceLayout { offset: 0, size: 0, row_pitch: explicit_pitch.unwrap_or(0), array_pitch: 0, depth_pitch: 0 }];
        let mut list = vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&mods);
        let mut ex = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default().drm_format_modifier(0).plane_layouts(&planes);
        let mut ext = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D { width: W, height: H, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        ci = if explicit_pitch.is_some() { ci.push_next(&mut ex) } else { ci.push_next(&mut list) };
        let ci = ci.push_next(&mut ext);
        unsafe { self.d.create_image(&ci, None) }.map_err(|e| format!("vkCreateImage: {e:?}"))
    }

    /// Dedicated type-0 memory, exportable, or imported from `fd`.
    fn bind(&self, image: vk::Image, fd: Option<RawFd>) -> Result<vk::DeviceMemory, String> {
        let r = unsafe { self.d.get_image_memory_requirements(image) };
        let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut ded = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import = vk::ImportMemoryFdInfoKHR::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT).fd(fd.unwrap_or(-1));
        let mut ai = vk::MemoryAllocateInfo::default().allocation_size(r.size).memory_type_index(0);
        if fd.is_some() {
            ai = ai.push_next(&mut import);
        }
        let ai = ai.push_next(&mut export).push_next(&mut ded);
        let m = unsafe { self.d.allocate_memory(&ai, None) }.map_err(|e| format!("vkAllocateMemory(dma-buf): {e:?}"))?;
        unsafe { self.d.bind_image_memory(image, m, 0) }.map_err(|e| format!("vkBindImageMemory: {e:?}"))?;
        Ok(m)
    }

    fn memory_fd(&self, memory: vk::DeviceMemory) -> Result<OwnedFd, String> {
        let fd = unsafe {
            self.memfd.get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default().memory(memory).handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
            )
        }
        .map_err(|e| format!("vkGetMemoryFdKHR: {e:?}"))?;
        // SAFETY: the descriptor the call just gave us.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn row_pitch(&self, image: vk::Image) -> u64 {
        let l = unsafe {
            self.d.get_image_subresource_layout(
                image,
                vk::ImageSubresource { aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT, mip_level: 0, array_layer: 0 },
            )
        };
        l.row_pitch
    }

    fn begin(&self) -> Result<vk::CommandBuffer, String> {
        let cb = unsafe {
            self.d.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(self.pool).command_buffer_count(1))
        }
        .map_err(|e| format!("{e:?}"))?[0];
        unsafe { self.d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()) }.map_err(|e| format!("{e:?}"))?;
        Ok(cb)
    }

    fn barrier(&self, cb: vk::CommandBuffer, image: vk::Image, from: u32, to: u32, old: vk::ImageLayout, new: vk::ImageLayout) {
        let b = vk::ImageMemoryBarrier::default()
            .src_queue_family_index(from)
            .dst_queue_family_index(to)
            .old_layout(old)
            .new_layout(new)
            .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
            .image(image)
            .subresource_range(RANGE);
        unsafe {
            self.d.cmd_pipeline_barrier(
                cb,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[b],
            )
        };
    }

    fn clear(&self, cb: vk::CommandBuffer, image: vk::Image, c: [f32; 4]) {
        unsafe {
            self.d.cmd_clear_color_image(cb, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &vk::ClearColorValue { float32: c }, &[RANGE])
        };
    }

    /// Submit `cb`; with `export`, signal an exportable semaphore and export
    /// it as Zink does after a flush (the sync fd, then the memory's fd), and
    /// return how long the export took.
    fn submit(&self, cb: vk::CommandBuffer, export: Option<vk::DeviceMemory>, fence: vk::Fence) -> Result<std::time::Duration, String> {
        unsafe { self.d.end_command_buffer(cb) }.map_err(|e| format!("{e:?}"))?;
        let cbs = [cb];
        let mut esci = vk::ExportSemaphoreCreateInfo::default().handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let sem = unsafe { self.d.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut esci), None) }
            .map_err(|e| format!("{e:?}"))?;
        let sigs = [sem];
        let mut si = vk::SubmitInfo::default().command_buffers(&cbs);
        if export.is_some() {
            si = si.signal_semaphores(&sigs);
        }
        unsafe { self.d.queue_submit(self.queue, &[si], fence) }.map_err(|e| format!("vkQueueSubmit: {e:?}"))?;
        let t = std::time::Instant::now();
        if let Some(m) = export {
            let s = unsafe {
                self.semfd.get_semaphore_fd(
                    &vk::SemaphoreGetFdInfoKHR::default().semaphore(sem).handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD),
                )
            }
            .map_err(|e| format!("vkGetSemaphoreFdKHR: {e:?}"))?;
            if s != -1 {
                return Err(format!("the SYNC_FD export gave {s}, not -1"));
            }
            drop(self.memory_fd(m)?);
        }
        let took = t.elapsed();
        unsafe { self.d.destroy_semaphore(sem, None) };
        Ok(took)
    }

    /// A host-visible buffer the image is copied into.
    fn readback(&self, image: vk::Image) -> Result<Vec<u8>, String> {
        let size = u64::from(W * H * 4);
        let buf = unsafe {
            self.d.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::TRANSFER_DST), None)
        }
        .map_err(|e| format!("{e:?}"))?;
        let r = unsafe { self.d.get_buffer_memory_requirements(buf) };
        let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let ty = (0..self.mem_props.memory_type_count)
            .find(|&i| r.memory_type_bits & (1 << i) != 0 && self.mem_props.memory_types[i as usize].property_flags.contains(want))
            .ok_or("no host-visible memory type")?;
        let mem = unsafe { self.d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(r.size).memory_type_index(ty), None) }
            .map_err(|e| format!("{e:?}"))?;
        unsafe { self.d.bind_buffer_memory(buf, mem, 0) }.map_err(|e| format!("{e:?}"))?;
        let cb = self.begin()?;
        self.barrier(cb, image, vk::QUEUE_FAMILY_FOREIGN_EXT, 0, vk::ImageLayout::GENERAL, vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
        let region = vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 })
            .image_extent(vk::Extent3D { width: W, height: H, depth: 1 });
        unsafe { self.d.cmd_copy_image_to_buffer(cb, image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, buf, &[region]) };
        self.barrier(cb, image, 0, vk::QUEUE_FAMILY_FOREIGN_EXT, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, vk::ImageLayout::GENERAL);
        let fence = unsafe { self.d.create_fence(&vk::FenceCreateInfo::default(), None) }.map_err(|e| format!("{e:?}"))?;
        self.submit(cb, None, fence)?;
        unsafe { self.d.wait_for_fences(&[fence], true, 30_000_000_000) }.map_err(|e| format!("the readback: {e:?}"))?;
        let p = unsafe { self.d.map_memory(mem, 0, size, vk::MemoryMapFlags::empty()) }.map_err(|e| format!("{e:?}"))?;
        let out = unsafe { std::slice::from_raw_parts(p as *const u8, size as usize) }.to_vec();
        unsafe {
            self.d.unmap_memory(mem);
            self.d.destroy_fence(fence, None);
            self.d.destroy_buffer(buf, None);
            self.d.free_memory(mem, None);
        }
        Ok(out)
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            let _ = self.d.device_wait_idle();
            self.d.destroy_command_pool(self.pool, None);
            self.d.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

const RANGE: vk::ImageSubresourceRange =
    vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 };

fn wrong(bytes: &[u8], pitch: usize, px: [u8; 4]) -> usize {
    (0..H as usize)
        .flat_map(|y| (0..W as usize).map(move |x| (y, x)))
        .filter(|&(y, x)| bytes[y * pitch + x * 4..y * pitch + x * 4 + 4] != px)
        .count()
}

// -- the producer ("the client") ---------------------------------------------

fn producer(sock: &str) -> Result<(), String> {
    let r = "producer";
    let _ = std::fs::remove_file(sock);
    let listener = UnixListener::bind(sock).map_err(|e| format!("bind {sock}: {e}"))?;
    let g = Gpu::new(c"sv-dmabuf-xsession producer")?;
    let image = g.image(None)?;
    let memory = g.bind(image, None)?;
    let pitch = g.row_pitch(image);
    let token_fd = g.memory_fd(memory)?;
    let token = sv_drm::token::read(std::os::fd::AsFd::as_fd(&token_fd)).map_err(|e| e.to_string())?;
    say(r, sv_drm::token::record_name_ok(&token.record), format_args!("section {} with fence record {}", token.name, token.record))?;

    // A big scratch image to keep the GPU busy before the shared clear.
    let big = unsafe {
        g.d.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(FORMAT)
                .extent(vk::Extent3D { width: BIG, height: BIG, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::TRANSFER_DST)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
    }
    .map_err(|e| format!("{e:?}"))?;
    let req = unsafe { g.d.get_image_memory_requirements(big) };
    let ty = req.memory_type_bits.trailing_zeros();
    let big_mem = unsafe { g.d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(ty), None) }
        .map_err(|e| format!("the scratch image: {e:?}"))?;
    unsafe { g.d.bind_image_memory(big, big_mem, 0) }.map_err(|e| format!("{e:?}"))?;

    // The consumer is connected before the work starts, so the token reaches
    // it while the GPU is still busy.
    let (mut s, _) = listener.accept().map_err(|e| format!("accept: {e}"))?;
    let (hello, _) = recv(&mut s)?;
    if hello != "ready" {
        return Err(format!("unexpected {hello:?}"));
    }
    let cb = g.begin()?;
    g.barrier(cb, big, vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
    // `SVX_BUSY=n`: more work, for a longer race window.
    let busy: usize = std::env::var("SVX_BUSY").ok().and_then(|v| v.parse().ok()).unwrap_or(BUSY_CLEARS);
    for i in 0..busy {
        let c = [i as f32 / busy as f32, 0.5, 0.5, 1.0];
        unsafe { g.d.cmd_clear_color_image(cb, big, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &vk::ClearColorValue { float32: c }, &[RANGE]) };
        g.barrier(cb, big, vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
    }
    g.barrier(cb, image, vk::QUEUE_FAMILY_FOREIGN_EXT, 0, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
    g.clear(cb, image, X);
    g.barrier(cb, image, 0, vk::QUEUE_FAMILY_FOREIGN_EXT, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::GENERAL);
    let fence = unsafe { g.d.create_fence(&vk::FenceCreateInfo::default(), None) }.map_err(|e| format!("{e:?}"))?;
    let t = std::time::Instant::now();
    let took = g.submit(cb, Some(memory), fence)?;
    say(r, took < std::time::Duration::from_millis(100), format_args!("the export returned in {took:.1?}, without waiting for the GPU"))?;

    // Hand the buffer on at once, as a client commits it.
    send(&s, &format!("token {pitch}"), Some(token_fd.as_raw_fd()))?;
    println!("[{r}] handed the token on {:.1?} after the submit", t.elapsed());

    // 2. the consumer's verdict on X.
    let (line, _) = recv(&mut s)?;
    say(r, line.starts_with("saw-x 0 "), format_args!("the consumer read X through its own device: {line}"))?;

    // 3. the consumer's frame: Y, with its record and number.
    let (line, _) = recv(&mut s)?;
    let mut it = line.split_whitespace();
    let (Some("drew-y"), Some(rec), Some(seq)) = (it.next(), it.next(), it.next().and_then(|n| n.parse::<u64>().ok())) else {
        return Err(format!("unexpected {line:?}"));
    };
    let waited = wait_record(rec, seq, std::time::Duration::from_secs(20))?;
    let bytes = std::fs::read(&token.name).map_err(|e| format!("read {}: {e}", token.name))?;
    let bad = wrong(&bytes, pitch as usize, Y_PIXEL);
    say(r, bad == 0, format_args!("after the consumer's record reached {seq} ({waited:.1?}), the section by name is Y ({bad} wrong)"))?;
    // The acquire: the next submit waits for the other session's marks on
    // the section (its write of Y) on the GPU side.
    drop(g.memory_fd(memory)?);
    let back = g.readback(image)?;
    let bad = wrong(&back, (W * 4) as usize, Y_PIXEL);
    say(r, bad == 0, format_args!("a GPU copy of the producer's own image is Y too ({bad} wrong)"))?;

    // 4. the producer lets go first.
    unsafe {
        let _ = g.d.wait_for_fences(&[fence], true, 30_000_000_000);
        g.d.destroy_fence(fence, None);
        g.d.destroy_image(big, None);
        g.d.free_memory(big_mem, None);
        g.d.destroy_image(image, None);
        g.d.free_memory(memory, None);
        let _ = g.d.device_wait_idle();
    }
    drop(token_fd);
    send(&s, "freed", None)?;
    let (line, _) = recv(&mut s)?;
    say(r, line == "survived-and-gone", format_args!("the consumer: {line}"))?;
    Ok(())
}

// -- the consumer ("KWin") ----------------------------------------------------

fn consumer(sock: &str) -> Result<(), String> {
    let r = "consumer";
    let g = Gpu::new(c"sv-dmabuf-xsession consumer")?;
    let mut s = None;
    for _ in 0..200 {
        if let Ok(c) = UnixStream::connect(sock) {
            s = Some(c);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let mut s = s.ok_or_else(|| format!("no producer at {sock}"))?;
    send(&s, "ready", None)?;
    let (line, fd) = recv(&mut s)?;
    let fd = fd.ok_or("no fd came with the token")?;
    let pitch: u64 = line.strip_prefix("token ").and_then(|p| p.parse().ok()).ok_or_else(|| format!("unexpected {line:?}"))?;
    let token = sv_drm::token::read(std::os::fd::AsFd::as_fd(&fd)).map_err(|e| e.to_string())?;
    let t = std::time::Instant::now();
    let image = g.image(Some(pitch))?;
    let raw = std::os::fd::IntoRawFd::into_raw_fd(fd);
    let memory = g.bind(image, Some(raw))?;
    say(r, true, format_args!("imported {} (made by the other session) in {:.1?}", token.name, t.elapsed()))?;

    // Zink's acquire of an imported dma-buf asks for its fd first: the next
    // submit waits for the producer's work on the GPU side.
    // `SVX_NO_ACQUIRE=1`: the negative control — no acquire, no wait, and
    // the copy may read the image before the producer's clear.
    if std::env::var_os("SVX_NO_ACQUIRE").is_none() {
        drop(g.memory_fd(memory)?);
    }
    let back = g.readback(image)?;
    let bad = wrong(&back, (W * 4) as usize, X_PIXEL);
    println!("[{r}] the copy of the producer's image finished {:.1?} after the token came", t.elapsed());
    send(&s, &format!("saw-x {bad} first={:?}", &back[..4]), None)?;
    say(r, bad == 0, format_args!("every pixel the consumer's device read is X ({bad} wrong)"))?;

    // Draw Y into it and hand it back, as a compositor would never do but a
    // client of a shared buffer might.
    let cb = g.begin()?;
    drop(g.memory_fd(memory)?);
    g.barrier(cb, image, vk::QUEUE_FAMILY_FOREIGN_EXT, 0, vk::ImageLayout::GENERAL, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
    g.clear(cb, image, Y);
    g.barrier(cb, image, 0, vk::QUEUE_FAMILY_FOREIGN_EXT, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::GENERAL);
    let fence = unsafe { g.d.create_fence(&vk::FenceCreateInfo::default(), None) }.map_err(|e| format!("{e:?}"))?;
    g.submit(cb, Some(memory), fence)?;
    // This session's record and number: the GPU side's answer to the mark
    // that just went out is what a reader of this frame waits for. The
    // consumer cannot write the producer's token page, so it says them here.
    let (rec, seq) = own_record(&g, memory)?;
    send(&s, &format!("drew-y {rec} {seq}"), None)?;

    let (line, _) = recv(&mut s)?;
    if line != "freed" {
        return Err(format!("unexpected {line:?}"));
    }
    let survived = std::path::Path::new(&token.name).exists();
    unsafe {
        let _ = g.d.wait_for_fences(&[fence], true, 30_000_000_000);
        g.d.destroy_fence(fence, None);
        g.d.destroy_image(image, None);
        g.d.free_memory(memory, None);
        let _ = g.d.device_wait_idle();
    }
    let gone = !std::path::Path::new(&token.name).exists();
    send(&s, if survived && gone { "survived-and-gone" } else { "lost" }, None)?;
    say(r, survived, "the section outlived the producer's free")?;
    say(r, gone, "and went with the consumer's")?;
    Ok(())
}

/// The fence record this session's ICD uses and the number of its last
/// export: an allocation of its own carries both in its token.
fn own_record(g: &Gpu, _memory: vk::DeviceMemory) -> Result<(String, u64), String> {
    let image = g.image(None)?;
    let m = g.bind(image, None)?;
    let fd = g.memory_fd(m)?;
    let t = sv_drm::token::read(std::os::fd::AsFd::as_fd(&fd)).map_err(|e| e.to_string())?;
    unsafe {
        g.d.destroy_image(image, None);
        g.d.free_memory(m, None);
    }
    // The ICD numbers exports from 1 per session; the consumer has made
    // exactly one.
    Ok((t.record, 1))
}

fn wait_record(path: &str, seq: u64, limit: std::time::Duration) -> Result<std::time::Duration, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("open the fence record {path}: {e}"))?;
    let t = std::time::Instant::now();
    let mut b = [0u8; 16];
    loop {
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        f.read_exact(&mut b).map_err(|e| format!("read {path}: {e}"))?;
        let done = u64::from_le_bytes(b[8..16].try_into().unwrap());
        if &b[..8] == b"SVFENCE1" && done >= seq {
            return Ok(t.elapsed());
        }
        if t.elapsed() > limit {
            return Err(format!("{path} stayed at {done}, below {seq}"));
        }
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
}

#[allow(dead_code)]
fn flush(s: &mut UnixStream) {
    let _ = s.flush();
}
}
