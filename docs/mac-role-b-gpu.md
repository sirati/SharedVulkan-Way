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

The macOS server changes a HOST_ALLOCATION external query into MTLTEXTURE import.
It leaves a query without an external handle unchanged. The KosmicKrisp import
patch permits LINEAR color attachments only for MTLTEXTURE import, while the
server advertises LINEAR color-attachment format features for its IOSurface
formats. Consequently stock Zink can reject the earlier plain LINEAR candidate
before it ever asks for shared memory. This explains a possible absence of
`svAllocShared`, but needs the query matrix on the actual guest to prove which
candidate failed.

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

## IOSurface direct presentation

The existing shared image allocation imports an IOSurface-backed Metal texture
and transfers its Mach send right to the compositor under its section name.
The compositor currently reads that mapping into its shadow. The Windows-only
DirectFrame path cannot simply be enabled on macOS. Both macOS UI implementations
currently ignore ToUi::Direct, and the face process needs its own IOSurface right.

A safe direct implementation needs all of these properties:

- The candidate is one complete visible surface, with matching geometry,
  format, pitch and supported crop/scale. Other trees use the current compositor.
- The producing GPU session's fence record completes before the layer receives
  the surface. Importing or retaining an IOSurface is not a GPU completion wait.
- The UI process retains the IOSurface and increments its use count for the
  complete display lifetime. Face routing transfers a Mach right, not a global
  IOSurface id. These operations stay within the Mac.
- Wayland releases the client buffer only after the presentation consumer has
  stopped reading it. IOSurface use counts alone do not prevent the application
  from reusing and overwriting a released buffer. A CALayer transaction callback
  alone does not establish that the display has retired its reads. The chosen
  presentation API needs a documented completion event or explicit consumer
  fence, plus tests for rapid reuse, resize, fallback and process death.
- A fallback first installs the shadow and acknowledges the transition before
  dropping the prior direct frame. The broker retains any guest-RAM mapping
  until the existing release protocol permits freeing it.

Until the consumer completion contract is implemented, direct CALayer assignment
would introduce a buffer-reuse race and is not a zero-copy completion claim.

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
Advertising Vulkan decode alone does not make stock Firefox call it. Verify the
stock Mesa VA-API frontend, Firefox's VA-API selection and the sandboxed render
node independently before claiming hardware Firefox decode. The end-to-end proof
must identify the selected hardware decoder, decoded output format and frame
completion without readback.
