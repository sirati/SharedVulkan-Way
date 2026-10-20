# Linux guest on macOS: GPU gates

This document describes offline findings and implementation gates. It does not
claim a hardware run. The guest uses stock Firefox and stock Zink. KosmicKrisp's
Metal platform import implementation is the permitted driver patch.

## GBM export before allocation

Mesa 26.2.4 `src/gallium/drivers/zink/zink_resource.c` performs these operations
in this order:

1. `eval_ici` selects a tiling, usage and flags with `check_ici`.
2. `check_ici` asks `vkGetPhysicalDeviceImageFormatProperties2`. Its input chain
   contains an optional format list and DRM modifier, without an external-memory
   handle. `try_image_config` installs the candidate format list as `ici->pNext`.
3. `setup_image_pnext` later adds `VkExternalMemoryImageCreateInfo` and checks
   the external export handle types.
4. Image creation and memory allocation follow.

The earlier implementation changed only HOST_ALLOCATION external queries and
unconditionally added LINEAR format features. That did not resolve stock Zink's
plain query and advertised support the ordinary allocation path did not provide.

The current transport supplies one storage contract for supported plain and
external LINEAR BGRA/RGBA 2D images. Both image-format query forms ask the real
driver about MTLTEXTURE import. No driver refusal becomes success. The response
requires IMPORTABLE, a compatible MTLTEXTURE handle, and sample-count one. Limits
are narrowed to one mip level, one layer and one sample, matching the wrapper.
Creation uses the same external texture chain and independently checks the exact
usage, flags, dimensions and limits with the driver. Requirements allow only the
transport's DEVICE_LOCAL type and require a dedicated image allocation. An
ordinary dedicated vkAllocateMemory creates the IOSurface, wraps it as a Metal
texture and imports that texture as the image's real memory. The normal bind
path receives that actual image and imported memory with offset zero. The
compositor export protocol is used only for exports; ordinary native images do
not register names with a compositor.

Additional LINEAR format features are conditional on the driver's optimal format
features and successful MTLTEXTURE import queries for the corresponding usage.
COLOR_ATTACHMENT, COLOR_ATTACHMENT_BLEND and BLIT_DST are never added solely
because the format appears in a list. The permitted driver change remains
`~/nix-config/darwin/patches/mesa/0001-kosmickrisp-import-mtltexture.patch`.
Its kk_GetPhysicalDeviceImageFormatProperties2 path permits imported LINEAR
color storage and keeps INPUT_ATTACHMENT refused; its memory import path binds
the actual retained Metal texture. No Zink or Firefox patch is involved.

A retained surface can be imported into another image only when its actual
width, height, bytes per element, fourcc and pitch match that target image's
layout, the layout offset is zero, and the entire required image span fits the
surface's authoritative allocation. This check also applies to newly allocated
surfaces. A target with different dimensions, RGBA/BGRA interpretation, stride,
nonzero offset or excessive span is refused before creating/importing a texture.

The portable metal_image regressions cover query/create storage consistency,
restricted limits, actual import-feature requirements, unsupported usage, and
source/target layout compatibility. The headless macos-linear-image example is
the hardware gate for query, create, dedicated requirements, allocation and bind
for all four supported formats. Cross-linking it does not prove a native driver
run or rendered pixels. Stock Zink's INPUT_ATTACHMENT candidate remains a
separate restriction, and the ordinary image route still requires this native
hardware execution gate before it is claimed to work end to end.

A second independent restriction concerns input attachments. Zink's
`get_image_usage_for_feats` adds INPUT_ATTACHMENT for a render target unless both
PIPE_BIND_LINEAR and PIPE_BIND_SHARED are set. The KosmicKrisp import patch
rejects INPUT_ATTACHMENT for a LINEAR import. Zink therefore can reject a
non-LINEAR GBM request even after it tries the LINEAR modifier as a fallback.
The relevant usage masks are 0x7 for transfer and sample, 0x17 with color
attachment, 0x97 with input attachment, and 0x1f with storage.

`scripts/probe-vk-image-usage.c` asks the plain and DMA_BUF versions of those
queries for BGRA8 and RGBA8, with and without MUTABLE_FORMAT, both plain
LINEAR and LINEAR DRM modifier chains when advertised. Build it on the
build host for the guest architecture, then run with the same render-node
namespace and ICD environment as the app. It creates no windows or allocations.
`scripts/probe-vk-gpu.nix` packages both probes. Pass the guest nixpkgs
`pkgsCross.aarch64-multiplatform` package set to its `pkgs` argument to build on
the build host with the guest loader and C runtime. Copy the result to the
guest without building there.

A successful external query paired with a failed plain color query isolates the
preflight disagreement. Keep the result distinct from an allocation proof.

Do not fix this by fabricating a successful plain query or silently removing
INPUT_ATTACHMENT usage. Vulkan requires the eventual image and operations to
match the advertised support. A driver fix must implement the claimed image
usage, including its input-attachment representation, or the transport must
provide a genuinely supported image implementation. Patching Zink to bypass the
query is outside the allowed scope.

## Format-list crash isolation

`scripts/probe-vk-formats.c` tests legacy format properties, properties2,
properties3, both modifier-list versions, and their combined chain. It also
tests VkSubpassResolvePerformanceQueryEXT, alone and with properties3, when the
multisampled-render-to-single-sampled extension is advertised. Current upstream
vulkaninfo includes that structure in its generated format chain. With headers
and a driver supporting VK_KHR_extended_flags, the probe includes
VkFormatProperties4KHR and its combinations with the other output structures. It tests
count and fill calls separately and flushes the format and chain before every
call. The last unmatched `begin` line identifies a crashing call. A single
format and chain can be replayed by passing their numeric values.

The probe sweeps core formats, YCbCr, ASTC HDR and PVRTC. It does not enable
modifiers when the extension is absent, or properties3 without Vulkan 1.3 or its KHR extension.
Run the same probe with the dma-buf environment off and on, then directly against
the native driver when available. Preserve the exact executable and ICD hashes
with the output outside the source repository. A crash without dma-buf enabled
must not be attributed to modifier rewriting without reproducing its chain.

## Metal composition and source retirement

The compositor now implements a no-CPU-readback path for a complete, single
ARGB8888 root surface backed by an imported BGRA IOSurface. It checks the window
geometry, scale, format, offset, dimensions and pitch. The first frame still uses
the ordinary path to establish the tree. Crops, transforms, subsurfaces, XRGB
alpha normalization and unsupported Metal devices use the shadow path.

`macos_metal::snapshot` wraps the source and a fresh compositor-owned output as
Metal textures, encodes a GPU blit and calls `MTLCommandBuffer.waitUntilCompleted`.
Apple documents this method as blocking until the command buffer's execution
finishes. Only a completed command with status Completed produces MetalCopied.
Textures and both IOSurfaces remain owned across the wait, including errors.
References: Apple's MTLCommandBuffer `waitUntilCompleted()` and `status` APIs.

The owned output is immutable for its complete lifetime. The router transfers
its Mach rights to the face before its Surface and Present messages; a face
restart can resend that same immutable output. The standalone AppKit backend
also assigns only this owned output to its layer. Neither the layer nor the
face ever receives the application allocation as layer contents. This is Metal
GPU composition, not direct CALayer scanout. It uses one GPU copy and no CPU
readback on eligible steady-state frames.

MetalCopied carries an independent retained output back to Wayland state. The
client buffer is released after that GPU completion, so even a client reusing
one buffer need not wait for another frame to release it. If the scene later
requires CPU composition, it reads the immutable output once, not the released
application allocation. An error before completion leaves the source held until
the ordinary fallback has copied it. Normal Windows direct-paint lifetime rules
remain unchanged. `WC_DIRECT_PRESENT=off` disables this fast path too.

Fresh outputs are never recycled or rewritten. Core Animation retains its
contents; Mach rights and the state snapshot retain any other consumers. Display
retirement therefore cannot race a write into an old output. This initial
implementation trades allocation cost for a documented source-retirement
contract. Pooling those outputs requires a separate display-consumer retirement
contract. No QoS or watchdog setting changes are involved.

The headless `macos-metal-retirement` example creates a padded source fixture,
copies it on Metal, immediately rewrites the source, makes a second snapshot,
and checks that both outputs retain their expected distinct pixels. CPU access
in that example is fixture setup and verification, not the product path. Build
it on the build host for aarch64-darwin. A real-Metal run, rapid reuse, fallback,
resize and face-restart runs remain hardware gates, not inferred successes.

## VideoToolbox decode

Neither available Metal Vulkan driver supplies Vulkan Video decode. The macOS
GPU server would have to implement it rather than forward missing entry points.
Do not advertise video extensions or queues until the complete supported subset
exists. Start with H.264 progressive 8-bit 4:2:0; query VideoToolbox hardware
support and refuse software fallback. HEVC and AV1 require separate capability
and decode proofs.

The first implementation needs Vulkan session and parameter lifetime management,
SPS/PPS parsing, reference-picture and DPB tracking, bitstream assembly into
CMSampleBuffer, asynchronous VideoToolbox decode, and ordered completion through
the existing Vulkan submission and fence machinery. Compressed bytes and output
pictures stay in shared regions. VideoToolbox completion retains each
CVPixelBuffer until every Vulkan and display consumer has finished; session
teardown drains callbacks before freeing those objects.

NV12 output is one IOSurface with two planes. Metal wraps plane zero as R8 and
plane one as RG8 at their actual offsets, strides and sizes. The KosmicKrisp
import patch currently accepts a single four-channel texture, so it cannot
represent that decoded Vulkan image. Extend its plane storage, image memory
binding, views and YCbCr sampling together, with a cross-built plane-import
acceptance test. A pair of arbitrary unrelated textures does not establish a
valid multi-plane VkImage. Never infer chroma pitch or plane offset from width.

The guest also needs a stock media frontend that actually selects Vulkan Video.
Advertising Vulkan decode alone does not make stock Firefox call it. Release
Firefox 157 does not enable direct Vulkan decode by default; its Vulkan decode
preference is Nightly-only and the current GfxInfo path blocklists every device.
Use default Firefox VA-API through the upstream Zink VA frontend, which consumes
forwarded Vulkan Video. Do not enable experimental Firefox preferences for the
proof. Verify the stock Mesa VA-API frontend, Firefox's VA-API selection and the
sandboxed render node independently. Firefox's FFmpeg decoder can log
`VA-API FFmpeg init successful` and `VA-API frame pts=`. The end-to-end proof must
also identify the selected hardware decoder, decoded output format and frame
completion without readback.


### Implemented NV12 import boundary

`sv_gpu_server::video_nv12` validates independent plane dimensions, offsets,
strides, allocation bounds and non-overlap, including odd luma sizes and checked
arithmetic. The macOS backend retains the completed CVPixelBuffer and its color
attachments. It verifies that CoreVideo and IOSurface agree on both planes and
that their element sizes are one byte for Y and two bytes for interleaved UV.
Full-range and video-range fourccs remain distinct. It does not guess a color
matrix from resolution.

The backend wraps actual IOSurface plane zero as R8 and plane one as RG8. Both
Metal textures keep their original completed picture alive. It can import those
textures as two dedicated single-plane Vulkan images using the existing
KosmicKrisp MTLTEXTURE platform patch. It queries importability first and refuses
an image whose reported row pitch disagrees with the decoder plane. Images and
memory are freed before the textures and decoded picture, after the unsafe
consumer contract has required all GPU readers to retire. Partial import errors
clean up already-created objects.

This is an initial two-plane backend boundary. It is not a multi-plane NV12
VkImage, a Vulkan Video session implementation or a claim of Firefox decode.
No codec, queue or extension is synthesized or advertised. The subsequent driver
work must combine the plane handles into one Vulkan YCbCr image before exposing
that image through the stock Zink VA frontend. The standalone plane pair cannot
substitute for those image semantics.

The headless `macos-nv12-import` example allocates an IOSurface-backed NV12
fixture, validates both native plane layouts, wraps the Metal textures and
attempts both dedicated imports. It inventories hardware H.264 support without
advertising it as implemented Vulkan capability. A real decode callback,
reference-picture lifetime, initial ownership/layout transitions and sampling
verification still require hardware and the remaining video implementation.


### IOSurface registration lifetime

The V2 section protocol carries a broker run and connection generation. Before
accepting its first name, the compositor asks the shared broker to observe that
live connection and compare its authenticated PID with the kernel Mach audit
trailer. User identity also has to match. The owner connection remains open in
the GPU process. Disconnect, kernel process exit, or observer broker EOF removes
all discoverable names for that owner. Existing CF references held by consumers
remain valid until those consumers retire. There is no separate keeper, PID
poller or reclamation timer.

ADD validates its exact message shape, descriptor count/type/disposition, name
and audit trailer before importing the surface. Every successfully received
message destroys unadopted Mach rights, including rejected complex messages.
Persistent receive errors clear registration state and end the receive loop.
Allocation size comes only from IOSurfaceGetAllocSize; checked non-planar
geometry must fit it. The sender waits for registration acknowledgement before
publishing the export token and withdraws a possibly delivered registration
when acknowledgement fails. The two-second request timeout does not reclaim
live consumer references. Native Mach right accounting and GPU completion still
require the authorized hardware validation gate.
