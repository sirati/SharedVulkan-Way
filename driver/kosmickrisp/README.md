# KosmicKrisp IOSurface imports

This flake pins the Mesa 26.2.4 provider and vmhost cross-toolchain inputs used
by the native Mac renderer. It does not activate a configuration or install a
driver. Build on an x86_64 Linux host through the project's serialized build
wrapper and a uniquely named vm-tests.slice scope, with a 10 GiB memory cap
and no swap. Pass `--max-jobs 2 --cores 8` to Nix.

The patch extends `VK_EXT_external_memory_metal` imports. For NV12, create one
non-disjoint `VK_FORMAT_G8_B8R8_2PLANE_420_UNORM` sampled image, with one mip,
one layer and one sample. Import its IOSurface plane-0 `R8Unorm` Metal texture
using `VkImportMemoryMetalHandleInfoEXT` and a dedicated allocation naming
that image. Bind at offset zero. Do not allocate separate Vulkan images or
pass an array in the Metal handle.

The driver validates a two-plane `420v` or `420f` IOSurface, checks actual
plane extents, element sizes, strides and allocation bounds, and creates an
`RG8Unorm` texture of plane 1 on the same Metal device and IOSurface. It retains
both textures in the allocation and image and adds both to residency. Existing
Mesa image views, immutable samplers and YCbCr shader lowering consume the
joined image. No chroma pixels are copied. The caller must finish the producer's
writes before GPU sampling and retain decoder resources until GPU completion.
The imported texture and derived texture retain the IOSurface, but this does
not substitute for VideoToolbox callback/completion ownership in the server.

MTLTexture handle properties and image memory requirements expose only
non-host-visible memory types. Texture import rejects a host-visible selected
memory type before retaining any resources. The provider adds a GPU-only type
on the same device-local heap and preserves its original host-visible type
first for ordinary allocations and Metal heap imports. Imported textures have
no Vulkan CPU mapping, and host-image-copy usage is refused for this path.
The NV12 helper rejects zero extents and computes chroma extents without
unsigned addition overflow.

The caller supplies the real stream's conversion model, range and chroma
location through standard `VkSamplerYcbcrConversion`. The IOSurface FourCC
alone does not encode the stream's matrix or chroma location. The driver does
not infer or override those values. NV12 storage, rendering, transfers,
disjoint imports and other multiplane formats remain unsupported for this
external handle path. Existing single-plane imports remain available.

Build `.#kosmickrisp` and `.#nv12Test`. Run the signed test with that output's
ICD manifest in `VK_DRIVER_FILES` and an existing native Vulkan loader's lib
directory in `DYLD_LIBRARY_PATH`. Coordinate the bounded headless run with
the renderer owner. The test samples spatially varying luma and chroma through
one joined VkImage, a YCbCr image view and immutable sampler into a storage
buffer. It checks all 16 float pixels against BT.601 full and narrow range
results after a fence and host barrier. It drops the caller's texture and
IOSurface references before sampling. This is a native GPU fixture, separate
from VideoToolbox decode, guest forwarding and real application validation.

Build and runtime evidence is kept outside git. Compilation alone does not
establish successful native import or YCbCr sampling.

The pinned provider and native fixture cross-build passed on Linux. The exact
signed binaries then passed the isolated headless fixture on native A18 Pro
hardware. Both full and narrow range cases verified all 16 pixels after the
GPU fence, including negative RGB values produced by the specified conversion.
The fixture also verified actual plane offsets/pitches, stable allocation
requirements and refusal of unsupported queries and wrong-plane handles.
The renderer's existing ordinary single-plane allocation fixture passed with
unchanged SRGB shader-write and input-attachment refusals. That allocation
fixture does not establish single-plane rendering or sampling completion.
VideoToolbox production, renderer retirement and guest/application completion
remain separate integration work.
The native fixture also confirms that texture handle properties and image
requirements exclude every host-visible type and that selecting one for a
texture import returns `VK_ERROR_INVALID_EXTERNAL_HANDLE`. Ordinary and
MTLHEAP imports using the original host-visible type still map successfully.
