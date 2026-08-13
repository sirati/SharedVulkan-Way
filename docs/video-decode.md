# Hardware video decode, measured

What every driver in reach can decode, whether a frame actually decodes, and
what stands between that and Firefox. Not documentation, not inference — the
drivers' own answers, and where a thing did not work it says so.

Every number below carries the shell it was taken in, because on this machine
that changes the answer: `flake.nix` records that the RTX 5090 does not
enumerate inside our own dev shell, and it turns out the dev shell also carries
a *second* RADV whose answers differ from the system one. Three shells appear:

| shell | what it can see |
|---|---|
| `nix develop` (this repo) | RADV from nixpkgs-unstable **Mesa 26.2.0**, RADV from the system **Mesa 26.1.1**, two llvmpipes. **No NVIDIA.** |
| `nix shell nixpkgs#vulkan-loader` | **RTX 5090**, system RADV 26.1.1, llvmpipe |
| the Windows guest on `testhost` | the passed-through **Intel UHD Graphics**, and nothing else |

The instrument is `crates/video-probe`, and `scripts/sv-video-decode.sh` runs
the whole thing.

---

## 1. The survey

`vulkaninfo` was not enough for this and it is worth saying why. It does print a
video section, and on the RTX 5090 it prints forty-one lines of it — but every
line is of the form

```
placeholder = H.264 Decode (4:2:0 8-bit) Main progressive
```

It names the profiles and prints none of their capabilities. The numbers a
decoder has to obey — DPB slots, bitstream buffer alignment, minimum and
maximum coded extent, whether the output picture may be an image distinct from
the DPB — come from `vkGetPhysicalDeviceVideoCapabilitiesKHR`, and nothing on
this machine was asking it. So `video-probe caps` does, per device and per
profile.

### The Intel iGPU in the testhost guest

This is the one that matters: it is the GPU Firefox's frames would have to be
decoded on. Run 2026-08-13 in the Windows guest over ssh
(`scripts/pl.sh guest 'cd /z/probe && ./video-probe.exe caps'`).

```
device 0: Intel(R) UHD Graphics
  vendor 0x8086 device 0xa7a8 type INTEGRATED_GPU api 1.4.323
  driverID INTEL_PROPRIETARY_WINDOWS driverInfo "101.7076"
  video extensions: VK_KHR_video_decode_av1(1) VK_KHR_video_decode_h264(8)
                    VK_KHR_video_decode_h265(7) VK_KHR_video_decode_queue(7)
                    VK_KHR_video_queue(8)
  queue family 0: count 1 flags GRAPHICS|COMPUTE|TRANSFER|SPARSE codecs -
  queue family 1: count 2 flags VIDEO_DECODE codecs DECODE_H264|DECODE_H265|DECODE_AV1
```

| profile | answer | coded extent | dpb slots / active refs | bitstream align (offset/size) |
|---|---|---|---|---|
| h264 baseline 4:2:0 8-bit progressive | **SUPPORTED** | 8×8 – 4096×4096, granularity 8×16 | 16 / 16 | 4096 / 4096 |
| h264 main 4:2:0 8-bit progressive | **SUPPORTED** | as above | 16 / 16 | 4096 / 4096 |
| h264 high 4:2:0 8-bit progressive | **SUPPORTED** | as above | 16 / 16 | 4096 / 4096 |
| h264 high 4:2:0 8-bit interlaced | **SUPPORTED** | as above | 16 / 16 | 4096 / 4096 |
| h265 main 4:2:0 8-bit | **SUPPORTED** | 8×8 – 8192×8192 | 15 / 15 | 4096 / 4096 |
| h265 main10 4:2:0 10-bit | **SUPPORTED**, but see below | 8×8 – 8192×8192 | 15 / 15 | 4096 / 4096 |
| av1 main 4:2:0 8-bit, film grain | **NOT SUPPORTED** — `ERROR_VIDEO_PROFILE_CODEC_NOT_SUPPORTED_KHR` | | | |
| av1 main 4:2:0 8-bit, no film grain | **SUPPORTED** | 8×8 – 8192×8192 | 8 / 8 | 4096 / 4096 |
| av1 main 4:2:0 10-bit, no film grain | **SUPPORTED** | 8×8 – 8192×8192 | 8 / 8 | 4096 / 4096 |

Five things in that table are worth reading twice.

1. **There is no VP9 and there is no encode.** The extension list has five
   entries and every one is decode. `VK_KHR_video_encode_queue` is absent, so
   the QuickSync encode design.md §11 keeps in reserve for a remote app side is
   not reachable through Vulkan on this driver. `VK_KHR_video_maintenance1` and
   `2` are also absent, which matters more than it looks: `maintenance1` is what
   lets a bitstream buffer be created *without* naming a video profile, and
   without it every buffer the arena hands a decoder has to be created against
   the profile it will be used with.
2. **The decode queue family has no `TRANSFER` bit.** Family 1 is
   `VIDEO_DECODE` and nothing else, so the queue that decodes cannot copy the
   result anywhere. Two queues on two families are needed for one frame, and
   RADV is the same shape. This is the single most awkward fact for this
   project, and §5 below is about it.
3. **AV1 film grain is refused.** Not "supported and ignored" — the profile
   query returns `ERROR_VIDEO_PROFILE_CODEC_NOT_SUPPORTED_KHR`. YouTube's AV1
   streams do use film grain synthesis, so this is a real hole rather than a
   pedantic one. RADV and NVIDIA both accept it.
4. **`maxLevelIdc` is 1.** `StdVideoH264LevelIdc` value 1 is level 1.1 — 176×144
   at 15 fps. The same driver simultaneously advertises a 4096×4096 coded
   extent, so the field is not describing reality. It is reported here rather
   than corrected because a consumer that believes it will refuse every real
   stream. H.265 says 1, i.e. level 2.0, with an 8192×8192 extent.
5. **H.265 reports `decodeFlags` with neither `DPB_AND_OUTPUT_COINCIDE` nor
   `DPB_AND_OUTPUT_DISTINCT` set** (the raw value is `0x1100`, two bits this
   header does not name). The specification requires at least one of the two,
   because between them they say where the decoded picture may be written. And
   H.265 main10 answers `SUPPORTED` while
   `vkGetPhysicalDeviceVideoFormatPropertiesKHR` returns **no formats at all**
   for both DST and DPB usage — a supported profile that cannot name an image to
   decode into. H.264 and AV1 on the same driver answer both questions
   consistently, so this is specific to H.265 on driver 101.7076.

### RADV and the RTX 5090

RADV, taken in `nix develop` (Mesa 26.2.0) and in `nix shell` (Mesa 26.1.1); the
RTX 5090 taken in `nix shell` only, per `flake.nix`.

```
RADV RAPHAEL_MENDOCINO, vendor 0x1002 device 0x13c0, api 1.4.348/1.4.354
  video extensions: decode av1(1) h264(9) h265(8) vp9(1) decode_queue(8) queue(8)
                    encode h264(14) h265(14) encode_queue(12) intra_refresh(1)
                    quantization_map(2) maintenance1(1) maintenance2(1)
                    VK_VALVE_video_encode_rgb_conversion(1)
  queue family 2: count 1 flags VIDEO_DECODE  codecs H264|H265|AV1|VP9
  queue family 3: count 1 flags VIDEO_ENCODE  codecs H264|H265

NVIDIA GeForce RTX 5090, driver 595.71.05, api 1.4.329
  video extensions: the same decode set, plus encode av1(1)
  queue family 3: count 2 flags TRANSFER|SPARSE|VIDEO_DECODE  codecs H264|H265|AV1|VP9
  queue family 4: count 2 flags TRANSFER|SPARSE|VIDEO_ENCODE  codecs H264|H265|AV1
```

Every H.264, H.265 and AV1 profile in the probe's list is supported on both.
Differences that matter to a forwarder:

| | RADV | NVIDIA | Intel (guest) |
|---|---|---|---|
| decode output may be distinct from the DPB | yes | **no — `COINCIDE` only** | yes (H.264, AV1) |
| bitstream buffer alignment, offset/size | 256 / 128 | 256 / 256 | **4096 / 4096** |
| decode queue can also transfer | **no** | yes | **no** |
| `VK_KHR_video_maintenance1` | yes | yes | **no** |
| VP9 decode | yes | yes | no |
| AV1 film grain | yes | yes | **no** |
| max DPB slots (H.264) | 17 | 17 | 16 |

The `COINCIDE`/`DISTINCT` split is not a detail. On NVIDIA the decoded picture
*must* be written into the DPB image itself and read from there; on RADV and
Intel it may go to a separate image. Any forwarded decode has to carry both
shapes, because the app side cannot choose one — `video-probe` has both paths
and they were both exercised (§3).

### A driver disagreement, found by accident

The dev shell holds two RADVs, and they answer differently:

| | Mesa 26.1.1 (system) | Mesa 26.2.0 (nixpkgs-unstable) |
|---|---|---|
| H.264 `maxLevelIdc` | 14 — the `StdVideoH264LevelIdc` enum value for level 5.1 | **52** — the *bitstream* `level_idc` byte for level 5.2 |
| H.265 `maxLevelIdc` | 8 — the enum value for level 5.1 | **186** — `general_level_idc`, i.e. 6.2 × 30 |
| AV1 `maxLevel` | 17 | **0** |

The field is typed `StdVideoH264LevelIdc`, which is an enum running 0 (level
1.0) to 18 (level 6.2). 52 and 186 are outside it. Same hardware, same day, two
drivers, and the newer one has started reporting raw bitstream codes where the
older reported enum values. `video-probe` prints both readings side by side
rather than picking one, which is how the disagreement became visible at all:

```
h264 maxLevelIdc 52 [outside the StdVideo enum; as a bitstream level_idc it is 5.2]
h264 maxLevelIdc 14 [StdVideo enum: level 5.1]
```

Between this, Intel's `maxLevelIdc = 1`, and Intel's H.265 flags, **the level
and capability fields of Vulkan Video are not yet something a consumer can
believe**. The profile query — "will you take this codec, chroma format and bit
depth" — is answered honestly by all three drivers. The numbers attached to the
answer are not.

---

## 2. Where the frames are, and where they are not

The phase-2 research argued decode is the best-suited workload for this project
because the compressed side is tiny and the decoded side never has to cross.
Measured on a real stream rather than quoted:

**1920×1080, 300 frames (10 s at 30 fps), H.264 High, x264 `-preset medium`,
capped at 4 Mbit/s** — roughly what a video site serves at this size.

| | total | per frame |
|---|---|---|
| compressed bitstream | 4,953,584 B | 16,511 B |
| NV12 frames | 933,120,000 B | 3,110,400 B |
| **ratio** | **188×** | |

The same content at CRF 23 with no rate cap comes to 7,979,649 B, or **117×**.
At 4K the frame side is 12,441,600 B and the ratio is larger still. So:

- **A 1080p60 stream is 0.5 MB/s of bitstream and 187 MB/s of frames.** §0.6
  measured presentation at 5.4 GB/s of pixels through the arena, so 187 MB/s is
  affordable — but it is 37% of one 1080p60 *presented* frame budget spent
  moving a frame that did not need to move at all.
- **The 188× is the whole argument.** Bitstream up, frame reference back; the
  decoded picture is created, referenced by the DPB and consumed by the
  compositor entirely on the GPU side.

One number cuts the other way and belongs here rather than in a footnote. The
Intel driver's submit floor was measured at **1.347 ms for an empty dispatch**
(`measured-gpu-side.md`), against RADV's 0.651 ms and the RTX 5090's 0.051 ms.
A 60 Hz frame is 16.7 ms. If each decoded frame costs one forwarded submit round
trip, that is 8% of the frame budget on this hardware before any decoding
happens — which is a reason to batch, not a reason to stop, and it is the same
number every other forwarded workload pays.

---

## 3. A frame decoded — three times, on three drivers

`video-probe decode` runs the whole Vulkan Video sequence: parse the SPS and
PPS, build `VkVideoSessionKHR`, query and bind its memory, build
`VkVideoSessionParametersKHR` from the parsed parameter sets, allocate a DPB and
a decode target, then `vkCmdBeginVideoCodingKHR` → `vkCmdControlVideoCodingKHR`
(reset) → `vkCmdDecodeVideoKHR` → `vkCmdEndVideoCodingKHR`, and copy the result
out as NV12.

The fixture is 24 frames of 320×180, H.264 Main, **every frame an IDR**
(`x264-params keyint=1`). That is the honest scope: no reference list
construction, no picture order count arithmetic. Inter prediction is a decoder
and not a probe, and a half-right one produces a picture that looks nearly
correct, which is the worst kind of result. The probe refuses a non-IDR picture
rather than decoding it.

The check is not that the call succeeded. It is that the frames are the frames:

| device | shell | frames | median submit→fence | vs libavcodec |
|---|---|---|---|---|
| RADV RAPHAEL_MENDOCINO | `nix develop` | 24 | **0.806 ms** | **byte-identical** |
| NVIDIA GeForce RTX 5090 | `nix shell` | 24 | **0.116 ms** | **byte-identical** |
| **Intel UHD Graphics, in the testhost guest** | guest ssh | 24 | **0.664 ms** | **byte-identical** |

All three produce `sha256 b3a1374e…` over 2,073,600 bytes, and so does
`ffmpeg -i idr.264 -pix_fmt nv12 -f rawvideo`. Four independent H.264
implementations — three hardware decoders and libavcodec — agreeing to the byte.
That is a stronger statement than the 64.1 dB PSNR §0.4 got from zink against
llvmpipe, and it is available here only because H.264 decoding is exactly
specified where rasterisation is not.

Three shapes were exercised getting there, and each of them is a difference the
forwarder would have to carry:

- **`DPB_AND_OUTPUT_DISTINCT`** on RADV and Intel: a separate decode target
  image, and a queue-family ownership problem because the decoding queue cannot
  copy. Solved with `VK_SHARING_MODE_CONCURRENT` across the two families and a
  binary semaphore between the two submissions.
- **`DPB_AND_OUTPUT_COINCIDE`** on NVIDIA: no separate image; the DPB slot *is*
  the output, and the copy reads the DPB layer in
  `VK_IMAGE_LAYOUT_VIDEO_DECODE_DPB_KHR`.
- **Session memory varies from nothing to four allocations.** Intel wants **0
  binds**, RADV **1 bind of 177,152 bytes**, NVIDIA **4 binds totalling 294,912
  bytes**. A forwarder must carry the count-then-fill honestly; §0.4 item 5 is
  the record of what happens when it does not.

### Two things that cost a run each

**A missing `pNext` is a segfault, not an error.** The first version of
`choose_device` called `vkGetPhysicalDeviceVideoCapabilitiesKHR` with a bare
`VkVideoCapabilitiesKHR`. The specification requires
`VkVideoDecodeCapabilitiesKHR` and the codec's own capabilities structure in the
chain for a decode profile; RADV writes through the chain without checking there
is one. The process died with `SIGSEGV` before printing a line, and the crash
names nothing. Fixed and commented in place.

**`StdVideoH264LevelIdc` is an enum, not the `level_idc` byte.** The fixture is
level 1.2, which is `level_idc = 12` in the bitstream and enum value **2** in
the header. Passing the byte through gives the driver a value outside the enum.
`decode::level_idc_enum` is the table and it is pinned by a test — this is the
same field the two RADVs disagree about in §1, from the other side.

---

## 4. What it would take to forward it, measured

**It does not forward today, and the wall is not where the phase-2 research
expected.** It is not the queue family and it is not `VkVideoSessionKHR`. It is
that `vk.xml` does not define the types.

The experiment is one command and it is in the tree, as
`vk_registry::profiles::video` — the Zink profile plus `VK_KHR_video_queue`,
`VK_KHR_video_decode_queue` and `VK_KHR_video_decode_h264`:

```
$ cargo run -p sv-codegen -- --profile video --stats
Error: planning profile "video"

Caused by:
    VkVideoDecodeH264CapabilitiesKHR.maxLevelIdc: StdVideoH264LevelIdc comes
    from a platform header and is not serialisable (design.md §6.1)
```

The profile resolves — every dependency is satisfied by a core 1.3 profile — and
then `plan.rs` refuses on the first `StdVideo*` type it reaches. The registry
carries these types as `TypeDef::External`, because that is all `vk.xml` says
about them:

```xml
<type requires="vk_video/vulkan_video_codec_h264std.h" name="StdVideoH264LevelIdc"/>
```

Six of them are on the H.264 decode surface: `StdVideoH264ProfileIdc`,
`StdVideoH264LevelIdc`, `StdVideoH264SequenceParameterSet`,
`StdVideoH264PictureParameterSet`, `StdVideoDecodeH264PictureInfo` and
`StdVideoDecodeH264ReferenceInfo`. The definitions live in
`vk_video/vulkan_video_codec_h264std.h`, a header this project has never
parsed — the generator reads one XML file and nothing else, which is §7.1's
whole design.

Four consequences, in increasing order of how much work they are.

1. **The C ABI pass is already wrong about them, latently.** `abi.rs` maps
   `TypeDef::External` to `AbiType::Opaque`, emitted as a zero-sized incomplete
   type on the reasoning that an external type is only ever seen behind a
   pointer. `VkVideoDecodeH264ProfileInfoKHR::stdProfileIdc` is a
   `StdVideoH264ProfileIdc` **by value**, and so is the `maxLevelIdc` above. The
   assumption holds for `HWND` and `xcb_connection_t` and does not hold here.
2. **The per-frame structures could cross as bytes; the per-stream ones cannot.**
   `StdVideoDecodeH264PictureInfo` and `StdVideoDecodeH264ReferenceInfo` are
   flat: bitfields, integers, a two-element array, no pointers.
   `StdVideoH264SequenceParameterSet` holds **three pointers**
   (`pOffsetForRefFrame`, `pScalingLists`, `pSequenceParameterSetVui`) and
   `StdVideoH264PictureParameterSet` holds one. They are not `pNext` chains and
   carry no `sType`, so nothing generic can walk them; §1a's rule that no
   descriptor crosses has an obvious sibling here, which is that no *pointer*
   crosses either, and these have to be deep-copied by hand. H.265 is worse —
   its sequence parameter set has seven pointers and nests four deep.
3. **The decode queue needs a family that does not exist on either side.** The
   GPU side picks one compute-capable family at startup (`driver.rs`) and the
   ICD advertises one universal family (`advertise.rs`), and D8 gives each queue
   its own ring. A video decode queue is a second family and therefore a second
   ring. But — and this is the interesting part — **it may not have to be
   visible at all.** §0.6's rule is that a capability expressible in the
   forwarded API does not deserve a protocol extension; the mirror of it applies
   here. If the GPU side owns the video session, the DPB and the decode queue
   privately, then what crosses is "decode these bytes into this image", the
   second ring never exists, and the app side never sees a queue family it
   cannot use. That is a protocol extension, and it is the first candidate since
   `vkUpdateDescriptorSetWithTemplate` that passes §0.6's test — because the API
   *cannot* be expressed by a generator that only reads `vk.xml`.
4. **The alternative is to teach the pipeline about `vk_video`**, either by
   parsing those headers or by a new exception category carrying a known layout.
   Both put a second source of truth about a C layout into a project whose whole
   argument is that there is one. Recorded as the option, not as the
   recommendation.

Meanwhile the forwarded device says so honestly. `video-probe caps` against our
own ICD, with the GPU side on RADV:

```
device 0: AMD Ryzen 9 9950X3D 16-Core Processor (RADV RAPHAEL_MENDOCINO)
  video extensions: none
```

which is §0.4 item 8 working exactly as designed: the device extension list is
the profile's extensions intersected with the driver's, and the profile has
none of these.

---

## 5. Firefox, and the gap that is not where it was

**The phase-2 research's answer here is out of date, and the correction goes in
Firefox's favour.** It recorded that Firefox uses VA-API on Linux, that its
Vulkan video decoder was Linux/GTK-only with both prefs defaulting to false, and
that VA-API-on-Vulkan does not exist. Checked 2026-08-13:

- **Firefox 153, released 2026-07-21, ships a Vulkan Video decoder.** Bug
  1753129 was closed as a duplicate of bug 2021722, which is fixed with target
  milestone 153. It is not a separate decoder module — it lives inside the
  existing FFmpeg platform decoder as
  `dom/media/platforms/ffmpeg/FFmpegVulkanVideoDecoder.cpp`, and the actual
  decoding is `av_hwdevice_ctx_create(AV_HWDEVICE_TYPE_VULKAN, …)`, i.e.
  ffmpeg's Vulkan hwaccel. FFmpeg has had H.264, H.265 and AV1 Vulkan decode
  since 6.1 (Nov 2023) and VP9 since 8.0.
- It is **opt-in**: `media.hardware-video-decoding-vulkan.enabled` defaults to
  `false`, as does `…-vulkan.direct-export.enabled`. Enabling it by default for
  allowlisted hardware is still an open bug.
- `media.ffmpeg.vaapi.enabled` no longer exists; it was removed in Firefox 137
  and folded into `media.hardware-video-decoding.force-enabled`.
- **VA-API over Vulkan Video still does not exist.** Mesa MR !31517 (Airlie,
  "zink: add initial video decoding support (h264)") is still a *draft*, opened
  2024-10-03, last touched 2026-01-25, `merge_status: cannot_be_merged_recheck`.
  `src/gallium/frontends/va/` contains no Vulkan file. The two third-party
  attempts on GitHub are 68 KB and 233 KB, unpushed since late 2025 and early
  2026 respectively. Nothing to depend on.
- Chromium has **no** Vulkan Video decode path; `media/gpu/` has no vulkan
  directory, and the Khronos integration design document is still marked draft,
  dated 2025-12-04.

So the gap the phase-2 research named — "Firefox does not speak Vulkan Video" —
is closed. A different one is open, and it is worse for this topology.

### 5a. What the decoder actually does with a frame — read, not inferred

The first version of this section asserted "ends in a dma-buf, no fallback" from
search results. That is not evidence, and the claim decides whether hardware
decode is reachable at all, so it was checked against the source. Everything
below is from **mozilla-firefox/firefox at `265f4b8f` (2026-08-07)**, the last
revision to touch `FFmpegVulkanVideoDecoder.cpp`, plus **FFmpeg master's
`libavutil/hwcontext_vulkan.c`**. The conclusion did not change. Five things
about it did.

**There are exactly two output paths, and the pref chooses between them at the
first frame.** `FFmpegVideoDecoder.cpp`:

```cpp
if (hf->tiling == VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT && vkf &&
    mVulkanDecoder.mGetImageDrmFormatModifierPropertiesEXT &&
    VulkanDirectDecodeExportEnabled()) {
  …
  mVulkanDecodeUsesDrmModifier = r == VK_SUCCESS;
  FFMPEG_LOG("[VULKAN] DRM modifier query: 0x{:x} -> {}", …,
             mVulkanDecodeUsesDrmModifier ? "direct export" : "copy");
} else {
  FFMPEG_LOG("[VULKAN] Tiling: OPTIMAL -> copy");
}
bool directExport = mVulkanDecodeUsesDrmModifier;
```

`VulkanDirectDecodeExportEnabled()` is the conjunction of both prefs, so with
`direct-export.enabled = false` — the default — `directExport` is always false
and the **copy path is the only one that runs**. That much the coordinator's
reading had right.

**1. The copy path's destination is another exportable `VkImage`, never host
memory.** `InitCopyRingBuffer` allocates a ring of 32 images
(`kNumBuffers = 32`), and every one of them is built for export:

```cpp
extImgInfo.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
exportInfo.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
…
imgInfo.tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT;
imgInfo.usage  = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT;
…
fdInfo.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
res = mGetMemoryFdKHR(mDevice, &fdInfo, &mNv12BaseFd[buf]);
```

The copy itself is `vkCmdCopyImage` — image to image, twice, once per plane —
and the header says so in as many words:

> *Export the decoded Vulkan frame as a DMA-BUF fd (`AVDRMFrameDescriptor`) …
> **Used for the copy path**, when FFmpeg decoded into opaque/optimal tiling
> that the compositor cannot directly import. … **The copy path is the
> default** and serves as a reliable fallback for all currently released FFmpeg
> versions.*

So "copy path" means *copy into a differently-tiled dma-buf*, not *copy to the
CPU*. **The whole file contains no `av_hwframe_transfer_data`, no
`vkCmdCopyImageToBuffer` and no `vkMapMemory`.** The grep is in §5c.

**2. Both paths converge on the same type, and it is a dma-buf descriptor.**
Direct export calls `av_hwframe_map(drmFrame, mFrame, 0)` into an
`AV_PIX_FMT_DRM_PRIME` frame; the copy path calls
`FillDRMDescriptorYUVSingleObject(format, baseFd, …)`. Both then hand the same
`AVDRMFrameDescriptor` to `mVideoFramePool->GetVideoFrameSurface(*desc, …)`,
whose product is a `DMABufSurface`. There is no third branch and nothing
downstream that takes bytes.

**3. There is a *second* descriptor per frame, which the first version of this
section missed.** The copy is synchronised with an exported semaphore:

```cpp
int fd = dup(mVulkanDecoder.mCopyDoneSemFd[vulkanCopyBufIdx]);
…
surface->GetDMABufSurface()->SetSemaphoreFd(fd, useSyncFd);
```

with `mSemHandleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT`. So the
per-frame contract is *two* file descriptors, a dma-buf and a `sync_fd`. §1a
rules out both, and the sync_fd has no more of a Windows equivalent than the
dma-buf does.

**4. The no-`TRANSFER` decode queue is already handled upstream, and that is
good news we did not have.** §1 measured that neither RADV's nor the guest
Intel's video decode family carries `VK_QUEUE_TRANSFER_BIT`. Firefox's `InitCtx`
scans for a transfer-only family itself, in exactly the shape `video-probe` had
to invent:

```cpp
if (props[i].queueCount > 0 &&
    (props[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT)) == 0 &&
    (props[i].queueFlags & VK_QUEUE_TRANSFER_BIT)) {
  copyQueueFamilyIndex = i;
```

falling back to ffmpeg's `queue_family_tx_index`, then to family 0, and retrying
the command-pool creation on the fallback family if the chosen one is refused.
The comment on the member list is `// Device & copy queues (decode queue is
owned by FFmpeg)`. On the guest Intel that lands on family 0
(`GRAPHICS|COMPUTE|TRANSFER`), which is correct. **This constraint is not part of
the gap.**

**5. The device would be selected, and the failure would land in the worst
place.** `PhysicalDeviceHasVulkanVideoDecodeStack` requires exactly
`VK_KHR_video_queue` and `VK_KHR_video_decode_queue`; the enumeration also
requires non-CPU and Vulkan ≥ 1.3, and sorts `VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU`
last but accepts it. **It does not check `VK_EXT_image_drm_format_modifier` at
selection time**, and ffmpeg does not fail without it either —
`hwcontext_vulkan.c` says

```c
av_log(ctx, AV_LOG_WARNING, "%s extension \"%s\" not found, excluding.\n", …);
```

for every extension in Firefox's requested `device_extensions` string that the
device lacks. So a forwarded device is chosen, a `VkDevice` is created without
the modifier extension, and the first frame reaches `vkCreateImage` with
`VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` on a device that never enabled it.

**And that failure is fatal, not a fallback.** The failure classification is
sharp and it matters:

| where it fails | result code | what happens |
|---|---|---|
| `SelectVulkanDecoderPhysicalDevice`, `CreateVulkanDeviceContext` | `InitVulkanDecoder` returns failure | `InitHWDecoderIfAllowed` falls through to VA-API → V4L2 → software. **Clean.** |
| `InitCopyRingBuffer`, `PrepareImageToDRM` — i.e. the first frame | `NS_ERROR_DOM_MEDIA_FATAL_ERR` (`"Failed to create NV12 image"`, `"Failed to export NV12 FD"`) | **No retry.** |

`MediaFormatReader`'s hardware-to-software recovery — the block that sets
`decoder.mHardwareDecodingDisabled = true` and asks for a new decoder — is
guarded by `if (decoder.mError && !decoder.HasFatalError())`, and
`HasFatalError()` returns false only for `NS_ERROR_DOM_MEDIA_DECODE_ERR` under a
retry budget and for `NS_ERROR_DOM_MEDIA_NEED_NEW_DECODER`. `FATAL_ERR` is
neither. **A forwarded device with the Vulkan pref on would not fall back to
software; the video element would error out.** That is a stronger negative than
the first version of this section claimed, and it is an argument for leaving the
pref alone rather than for trying it.

### 5b. So the conclusion stands, for the reason the coordinator called plausible

Both paths end in `vkGetMemoryFdKHR`. The copy path copies into another
exportable image; it does not copy to the CPU. There is no host-memory output
anywhere in `FFmpegVulkanVideoDecoder`, and the `direct-export` pref does not
choose between "GPU" and "CPU" — it chooses between *zero copies and one copy*,
both of which end in a descriptor.

That is fatal here for two independent reasons, and they should not be
conflated:

1. **A dma-buf is a host-kernel object and the frame is in a Windows guest.**
   No forwarded Vulkan can mint one, whatever else it does. This is not a
   limitation of our design; it is what dma-buf *is*.
2. **§1a forbids it anyway**, and now twice over — the dma-buf and the
   `sync_fd`. Even on the same machine, §0.7 measured that the one local route
   to a dma-buf, `/dev/udmabuf` over a named object, is closed, because a named
   object cannot be sealed.

### 5c. The greps, so this can be checked rather than believed

```
$ for f in FFmpegVulkanVideoDecoder.cpp FFmpegVulkanVideoDecoder.h; do
    curl -sO https://raw.githubusercontent.com/mozilla-firefox/firefox/main/dom/media/platforms/ffmpeg/$f; done

$ grep -n 'GetMemoryFdKHR\|GetSemaphoreFdKHR\|CmdCopyImage\|CmdCopyImageToBuffer\|
          av_hwframe_transfer_data\|MapMemory' FFmpegVulkanVideoDecoder.cpp
266:  load(mCmdCopyImage, "vkCmdCopyImage");
282:  load(mGetMemoryFdKHR, "vkGetMemoryFdKHR");
288:  load(mGetSemaphoreFdKHR, "vkGetSemaphoreFdKHR");
1034:    res = mGetMemoryFdKHR(mDevice, &fdInfo, &mNv12BaseFd[buf]);
1269:    mCmdCopyImage(mCopyCmdBuf[copySlot], srcVkFrame->img[0],
1278:    mCmdCopyImage(mCopyCmdBuf[copySlot], srcVkFrame->img[0],
1396:    if (mGetSemaphoreFdKHR(mDevice, &fdInfo, &mCopyDoneSemFd[bufIdx]) !=
```

No `vkCmdCopyImageToBuffer`, no `av_hwframe_transfer_data`, no `vkMapMemory`.

### The honest ordering

1. **Software decode on the host is the answer today**, and it costs nothing to
   implement because it is what happens when the pref stays false. 1080p is
   unremarkable on this host's CPU. This is the recommendation, and §5a's fatal
   classification makes it a stronger one than it was: turning the pref on
   against a forwarded device does not degrade, it breaks.
2. **It is not a pref flip.** The `direct-export` pref's *off* state — the
   default, the copy path — is the one that calls `vkGetMemoryFdKHR` directly.
   There is no combination of prefs that produces a frame in host memory.
3. **If hardware decode is wanted, the work is a Firefox patch and not a
   forwarder feature**, and it is a *third* output path rather than a change to
   either existing one. Copy into a host-visible buffer — which for us is *the
   arena*, so the copy lands in memory the host already maps — and produce a
   `PlanarYCbCrImage` instead of a `DMABufSurface`. The Vulkan half is the forty
   lines `video-probe` already has; the awkward half is that
   `GetVideoFrameSurface` takes an `AVDRMFrameDescriptor` and everything below it
   is dma-buf typed, so the patch has to bypass `FFmpegVideoFramePool` and reach
   the software path's image construction instead. Still a few hundred lines,
   still in one file plus its pool — but it is a new branch through the frame
   plumbing, not a substitution at the copy destination, and that is the
   correction to the earlier estimate. It costs 187 MB/s at 1080p60 across the
   boundary against 0.5 MB/s if the frame could stay put, and no upstream
   reviewer wants it, so it would have to be carried.
4. **Do not build a VA-API bridge.** Firefox no longer needs one, and a VA-API
   surface is exported to Firefox as a DRM PRIME dma-buf, so it ends at the same
   wall one layer lower.
5. **The version of this that is actually good does not involve Firefox's
   decoder at all**, and it is the one the topology wants: the compositor is on
   the Windows side, so a decoded frame that stays GPU-side is already where it
   needs to be. That needs a Wayland buffer type carrying a guest-side image
   handle — the `wp_win32_buffer` the phase-2 research says to build first — and
   a Firefox that can be handed one. Neither exists. It is the right shape and
   it is not close.

---

## What this does not tell us

- **Nothing has been decoded through the forwarder.** §4 is a measurement of why
  not, and it is precise, but it is not a decode. Every number in §3 is a driver
  reached directly.
- **Inter prediction has not been decoded at all**, on any device. Every frame
  in the fixture is an IDR. A real stream is 1 IDR in 250 frames and the other
  249 need reference list construction, which is where a decoder's real
  complexity is. That the IDR path is byte-exact says nothing about the rest.
- **One codec.** H.265 and AV1 were surveyed and not decoded. Intel's H.265
  format query returning nothing (§1) would have been found by trying, and was
  found by surveying; AV1's `StdVideoDecodeAV1PictureInfo` has eight pointers
  per frame where H.264's has none, so the marshalling argument in §4 is
  H.264-shaped and does not transfer.
- **No 4K, and no sustained rate.** 24 frames of 320×180 measures correctness.
  The submit medians in §3 are the whole submit-to-fence round trip including a
  copy of a tiny picture, and they are not a decode rate.
- **The Intel numbers are from a guest whose GPU is passed through**, which is
  the production topology, but the probe ran *inside* the guest with no VM
  boundary in the path. That is the same shape as `sv-windows-e2e.sh` and it
  isolates the driver from the transport deliberately.
- **`maxLevelIdc` is not believed** (§1) and no attempt was made to find out
  which reading is right. It did not block anything: the fixture is level 1.2
  and every device decoded it.
- **Firefox has not been run against the forwarded device.** §5a is a reading of
  `FFmpegVulkanVideoDecoder.cpp` at `265f4b8f`, which is mozilla-central tip on
  2026-08-07 and therefore *newer* than the 153 release; the shipped 153 code may
  differ in detail, though the two output paths and both prefs are present in the
  original landing. The failure sequence it predicts — device selected, `VkDevice`
  created without `VK_EXT_image_drm_format_modifier`, first frame fatal at
  `vkCreateImage` — is a prediction from source and not an observation. Running
  it would need a Firefox with `MOZ_ENABLE_VULKAN_VIDEO`, an FFmpeg ≥ 6.1 and a
  forwarded ICD in the same process, and would be worth an afternoon before
  anyone acts on the estimate in §5's item 3.

## Re-running it

```
# everything, from the dev shell
nix develop
cargo build -p video-probe -p sv-icd -p sv-gpu-server
scripts/sv-video-decode.sh

# NVIDIA — must be a clean shell, see flake.nix
nix shell nixpkgs#vulkan-loader --command bash -c \
  'export LD_LIBRARY_PATH=$(nix eval --raw nixpkgs#vulkan-loader)/lib
   SV_VIDEO_DEVICE=NVIDIA target/release/video-probe caps'

# the Intel iGPU in the guest
cargo build --release -p video-probe --target x86_64-pc-windows-gnu
scripts/pl.sh push target/x86_64-pc-windows-gnu/release/video-probe.exe
scripts/pl.sh root 'install -m0755 /tmp/video-probe.exe /srv/winshare/probe/'
scripts/pl.sh guest 'cd /z/probe && ./video-probe.exe caps'
scripts/pl.sh guest 'cd /z/probe && ./video-probe.exe decode idr-320x180.264 Z:/probe/out.nv12'

# the wall
cargo run -p sv-codegen -- --profile video --stats
```

`SV_VIDEO_DEVICE` matches a substring of the device name, case-insensitively,
and is the only knob.
