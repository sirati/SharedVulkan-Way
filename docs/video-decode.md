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

§1–§3 are the drivers and were measured 2026-08-13. §4 was the forwarder and it
did not work; **§4a and §6 are the day after**, when it partly did.

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
   the QuickSync encode docs/design.md §11 keeps in reserve for a remote app side is
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

## 4. The wall was a type, and the type had a registry

> **[SUPERSEDED — 2026-08-13. The section below is kept because the reasoning
> that turned out to be wrong is the useful part. §4a says what happened.]**
>
> **It does not forward today, and the wall is not where the phase-2 research
> expected.** It is not the queue family and it is not `VkVideoSessionKHR`. It is
> that `vk.xml` does not define the types.
>
> The experiment is one command and it is in the tree, as
> `vk_registry::profiles::video` — the Zink profile plus `VK_KHR_video_queue`,
> `VK_KHR_video_decode_queue` and `VK_KHR_video_decode_h264`:
>
> ```
> $ cargo run -p sv-codegen -- --profile video --stats
> Error: planning profile "video"
>
> Caused by:
>     VkVideoDecodeH264CapabilitiesKHR.maxLevelIdc: StdVideoH264LevelIdc comes
>     from a platform header and is not serialisable (docs/design.md §6.1)
> ```
>
> The profile resolves — every dependency is satisfied by a core 1.3 profile — and
> then `plan.rs` refuses on the first `StdVideo*` type it reaches. The registry
> carries these types as `TypeDef::External`, because that is all `vk.xml` says
> about them:
>
> ```xml
> <type requires="vk_video/vulkan_video_codec_h264std.h" name="StdVideoH264LevelIdc"/>
> ```
>
> Six of them are on the H.264 decode surface: `StdVideoH264ProfileIdc`,
> `StdVideoH264LevelIdc`, `StdVideoH264SequenceParameterSet`,
> `StdVideoH264PictureParameterSet`, `StdVideoDecodeH264PictureInfo` and
> `StdVideoDecodeH264ReferenceInfo`. The definitions live in
> `vk_video/vulkan_video_codec_h264std.h`, a header this project has never
> parsed — the generator reads one XML file and nothing else, which is §7.1's
> whole design.
>
> Four consequences, in increasing order of how much work they are.
>
> 1. **The C ABI pass is already wrong about them, latently.** `abi.rs` maps
>    `TypeDef::External` to `AbiType::Opaque`, emitted as a zero-sized incomplete
>    type on the reasoning that an external type is only ever seen behind a
>    pointer. `VkVideoDecodeH264ProfileInfoKHR::stdProfileIdc` is a
>    `StdVideoH264ProfileIdc` **by value**, and so is the `maxLevelIdc` above. The
>    assumption holds for `HWND` and `xcb_connection_t` and does not hold here.
> 2. **The per-frame structures could cross as bytes; the per-stream ones cannot.**
>    `StdVideoDecodeH264PictureInfo` and `StdVideoDecodeH264ReferenceInfo` are
>    flat: bitfields, integers, a two-element array, no pointers.
>    `StdVideoH264SequenceParameterSet` holds **three pointers**
>    (`pOffsetForRefFrame`, `pScalingLists`, `pSequenceParameterSetVui`) and
>    `StdVideoH264PictureParameterSet` holds one. They are not `pNext` chains and
>    carry no `sType`, so nothing generic can walk them; §1a's rule that no
>    descriptor crosses has an obvious sibling here, which is that no *pointer*
>    crosses either, and these have to be deep-copied by hand. H.265 is worse —
>    its sequence parameter set has seven pointers and nests four deep.
> 3. **The decode queue needs a family that does not exist on either side.** …
> 4. **The alternative is to teach the pipeline about `vk_video`**, either by
>    parsing those headers or by a new exception category carrying a known layout.
>    Both put a second source of truth about a C layout into a project whose whole
>    argument is that there is one. Recorded as the option, not as the
>    recommendation.

Item 3 is unchanged and is now the only one left; it is restated in §6. Items 1,
2 and 4 did not survive contact.

### 4a. `video.xml`

**The `vk_video` headers are generated from a registry, and that registry ships
in the same package as `vk.xml`.** The first line of every one of them says so:

```c
/*
** This header is generated from the Khronos Vulkan XML API Registry.
**
*/
```

and the file is `share/vulkan/registry/video.xml`, 196 KB, in the same nix store
path `$VK_XML` points into. It is the same schema — `<types>`, `<enums>`,
`<member len="…" optional="…">` — so `vk-parse` reads it and
`vk_registry::Model::merge_video` merges it with the same `build_types` and
`build_enums` the main registry gets. **No C parser, no hand-written table of
layouts, and no second source of truth**, which was §4 item 4's whole objection.

That was the decision, and the rest of this section is what it cost and what it
found. Three things had to change in the generator and none of them is
video-specific.

**1. Bitfields, which §7.4 had put out of scope.** `vk.xml` has one bitfield
struct and it is ray tracing. `video.xml` has **293 members of the form
`uint32_t x : 1`**, and every `StdVideo*Flags` type is one of these and nothing
else. So `plan::FlagWordPlan` carries a struct whose members are *all* `uint32_t`
bitfields summing to at most 32 bits, as the one word it occupies:
`#[repr(transparent)] struct StdVideoH264SpsFlags(pub u32)`, with a getter and a
setter per field. A struct that *mixes* bitfields with ordinary members is still
refused, so `VkAccelerationStructureInstanceKHR` stays out and its exclusion
stays a live tripwire.

The bit order is worth being precise about. **The wire does not depend on it**:
the word is copied end to end and neither side takes it apart, so what has to
agree is the two sides' C compilers — which they must anyway, because each
side's compiler has to agree with its own driver. The generated *accessors* do
depend on it, and they assume what the x86-64 System V ABI and MSVC x64 both
specify: first member at bit 0, ascending. §3's byte-identical decodes are the
evidence, since `video-probe` uses bindgen's reading of the same headers on both
Linux and Windows.

**2. `abi.rs`'s zero-sized `AbiType::Opaque`, which was live and not latent.**
§4 item 1 said the by-value problem was latent because nothing reached it. It
was not. `abi::build` now refuses any external type used by value, and the first
thing it refused was not a video type at all:

```
Error: vkGetPhysicalDeviceXcbPresentationSupportKHR.visual_id: xcb_visualid_t is
a platform type the registry only names, used *by value*.
```

`xcb_visualid_t` is a `uint32_t` and the ICD had been declaring it as a
zero-sized incomplete type in an `extern "system"` signature — in the **zink**
profile, shipping today. It has never had a symptom, because that entry point
exists only to stop the loader's terminator calling `abort()` and it ignores the
argument. `exceptions::PLATFORM_SCALARS` is now a two-entry table, closed, with
the header line that gives each width; anything not in it is a build failure.

**3. Two-dimensional arrays, which no profile had reached.**
`StdVideoH264ScalingLists::ScalingList4x4` is `uint8_t[6][16]`. The GPU side's
`raise` asked the *whole member* for its element type at every level of the
recursion, so both levels got `[u8; 16]` and the inner loop pushed a `u8` into a
`Vec<[u8; 16]>`. Caught by the compiler, but only once a profile reached it; the
cross-check's extent comparison had the same fault in a quieter form, comparing
the *product* of the extents, which calls `uint8_t[6][16]` and `uint8_t[96]` the
same thing. Both fixed.

And §4 item 2 was simply wrong. The per-stream structures do **not** have to be
deep-copied by hand: `video.xml` states `len="num_ref_frames_in_pic_order_cnt_cycle"`
on `pOffsetForRefFrame` and `optional="true"` on the other two, so the ordinary
generator produces `Vec<i32>` and `Option<Box<…>>` and walks them like any other
pointer graph. The thing that made them look unwalkable — no `sType`, no
`pNext` — is only a problem for a *generic* walker, and this generator is not
one.

### 4b. What is checked, and by what

The whole argument for reading `video.xml` instead of the headers is that it is
the source the headers come from. That is only worth anything if the two are
compared, so they are, by the same machinery `vulkan_core.h` already had
(`sv-testkit/src/cheader.rs`, `xcheck.rs`):

- the `vk_video/*.h` headers are now part of `cheader::header_paths()`, so every
  `STD_VIDEO_*` array extent and every `StdVideo*` enum value is compared with
  the constants and enum values of the rest of the API rather than in a special
  case;
- `xcheck::check_structs` compares every `StdVideo*` struct's member sequence,
  each member's C type, and each fixed array's extents **level by level**;
- `xcheck::check_flag_words` compares each bitfield struct's member sequence,
  each field's width in bits, and the offset that follows — the check that makes
  the flag-word layout a comparison rather than an assertion;
- three negative controls plant the faults that matter: a one-bit flag widened
  to two (which shifts every flag after it), two bitfields swapped, and a member
  dropped from `StdVideoH264SequenceParameterSet`.

What none of it can see is the *direction* a C compiler allocates bits in. The
header does not say and no reading of it can. That is stated as an assumption
where it is made, with §3 as the evidence.

The Zink profile's `protocol_hash` is **unchanged** at `0x44c19f913c79a5fa`
across the whole change, which is the statement that merging a second registry
is inert for a profile that does not reach it.

---

## 5. Firefox, and the gap that is not where it was

> **This whole section reads mozilla-central's tip, and the Firefox this project
> *deploys* is 151.0.2, which predates every line of it.** Checked against the
> binary in `design.md` §0.17: 151's `libxul.so` contains no
> `FFmpegVulkanVideoDecoder` and no `media.hardware-video-decoding-vulkan.enabled`,
> so its only hardware decode paths are VA-API and V4L2. Do not quote §5a as a
> fact about what is currently running.
>
> **Do quote it as a fact about what is one flake bump away.** §0.17 also
> measures `firefox-bin` **153.0.4** out of current `nixpkgs-unstable` — prebuilt,
> no compile — which has all four symbols, grows an
> `about:support` row `HARDWARE_VIDEO_DECODING_VULKAN available`, and logs
> `VulkanDeviceHolder: created shared VkDevice`. So §5a and §5b are the right
> analysis of the browser we would get, and the reason they are not yet the
> binding constraint is §6 below: a forwarded device offers no `VIDEO_DECODE`
> queue family, so decode is refused before any of §5a's two output paths runs.

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

## 6. What crosses the boundary now, and where it stops

Measured 2026-08-13 in `nix develop`, with the GPU side pointed at the **system**
RADV (Mesa 26.1.1) so that the comparison is transport against no transport
rather than one driver against another — the dev shell carries two RADVs and §1
records that they answer differently. `scripts/sv-video-decode.sh` checks 5 and 6
run all of this; the binaries have to be built for the profile:

```
SV_PROTO_PROFILE=video cargo build -p sv-icd -p sv-gpu-server -p video-probe
```

**The profile plans, and the whole stack builds against it.**

| | zink | video |
|---|---|---|
| wire commands | 239 (+35 aliases) | **252** (+35) |
| wire structs / unions | 338 / 2 | **371** / 2 |
| enums / bitmasks | 127 / 98 | **144** / 111 |
| `protocol_hash` | `0x44c19f913c79a5fa` | `0xdd37c6aa3d072455` |
| ICD C surface | 260 commands, 615 types | **273**, 685 |
| GPU side forwarded generically | 217 | **230** |

The thirteen new commands are `VK_KHR_video_queue`'s and
`VK_KHR_video_decode_queue`'s, all of them forwarded by the generator with no
hand-written line on either side. 636 generated round-trip tests pass for the
video profile, including `roundtrip_std_video_h264_sequence_parameter_set`,
`roundtrip_std_video_h264_scaling_lists`,
`roundtrip_std_video_decode_h264_picture_info` and
`roundtrip_cmd_vk_cmd_decode_video_khr`.

**The forwarded device advertises the video extensions and answers the
capability queries with the driver's own numbers.** `video-probe caps` against
our ICD, GPU side on RADV:

```
device 0: AMD Ryzen 9 9950X3D 16-Core Processor (RADV RAPHAEL_MENDOCINO)
  video extensions: VK_KHR_video_decode_h264(9) VK_KHR_video_decode_queue(8)
                    VK_KHR_video_queue(8)
```

and the check that matters is not that it answered but that it answered the same
thing. The H.264 block of the report — four profiles, their coded extents, DPB
slot counts, bitstream buffer alignments, decode flags, every
`vkGetPhysicalDeviceVideoFormatPropertiesKHR` row, and `maxLevelIdc`, which is
the `StdVideoH264LevelIdc` that used to stop the planner — is **44 lines and
byte-identical** between the direct run and the forwarded one, `sha256
9c1dca7512d844d6…`. That is the same argument §3 makes with the frames, applied
to the numbers: §1 says these numbers are not believable, so a forwarder that
quietly rounded one would be indistinguishable from a driver that meant it,
unless the two are put side by side.

What crossed to produce it is worth naming, because it is exactly what §4 said
could not: a `pNext` chain of `VkVideoDecodeCapabilitiesKHR` and
`VkVideoDecodeH264CapabilitiesKHR` in, a `StdVideoH264LevelIdc` by value out,
and a `VkVideoProfileInfoKHR` carrying a `VkVideoDecodeH264ProfileInfoKHR` whose
`stdProfileIdc` is a `StdVideoH264ProfileIdc` by value.

> **[SUPERSEDED — 2026-10-15, §7.]** The decode runs. The GPU side now serves
> its driver's decode queue family, and the reasoning below for *not* doing so
> — a second ring, a family the application must be told not to use — did not
> survive being checked against the code: every queue already shares one ring,
> and a family the application can record `vkCmdDecodeVideoKHR` on is exactly
> what ffmpeg and Firefox need. §7.1 says what was decided instead and why.

**The decode does not run, and the wall has moved to where §4 item 3 said it
would.**

```
$ SV_SOCKET=… VK_DRIVER_FILES=…/sv_icd.json video-probe decode idr.264 out.nv12
Error: no device can decode this profile:
  AMD Ryzen 9 9950X3D 16-Core Processor (RADV RAPHAEL_MENDOCINO):
    no queue family with VIDEO_DECODE
```

The ICD advertises one universal queue family (`advertise.rs`, docs/design.md §0.4
item 7) and the GPU side serves one compute-capable family (`driver.rs`). A
video decode queue is a second family, and D8 gives every family its own ring.

**That is not a thing to fix by adding a second family**, and this stage
deliberately did not. docs/design.md §0.8 item 2 names the shape the topology wants:
if the GPU side owns the video session, the DPB and the decode queue *privately*,
then what crosses is a compressed bitstream and an image id, the second ring
never exists, and the app side never sees a queue family it cannot use. Adding a
visible decode family would be building the alternative that section rejects,
and it would then have to be unbuilt. The measurement here is that everything
*below* that decision now works: the types cross, the queries cross, and the
only thing left is whose queue submits.

The honest cost of the remaining step, so it is not underestimated: a private
video session means the GPU side allocating the DPB images, the decode target
and the session parameters from its own device, keeping a slot table per
session, and answering a protocol command that is not a Vulkan command. Every
one of those is code that the generator cannot write, which is precisely §0.6's
test being met — and it is a stage, not an afternoon.

---

## 7. Decoded through the forwarder

Measured 2026-10-15 on **devhost** (Ryzen 9 9950X3D, RTX 5090), branch
`decode-forwarder`. The GPU side ran against **RADV RAPHAEL_MENDOCINO**, the
9950X3D's integrated GPU, on two Mesas: the dev shell's 26.2.0 and the system's
26.2.4. The RTX 5090 was not used — it is leased to other work through
`gpu-run`, and the lease this stage queued (job 120, shared, 2 GiB) had not
started when this was written; §7.9 says what that leaves open. The guest's
Intel iGPU was not used either, by instruction.

**Everything in §6's last paragraph happened.** `video-probe decode` runs through
the forwarder and its frames are byte-identical to libavcodec's; ffmpeg's own
Vulkan decoder — the one Firefox 153+ decodes with — runs through it for H.264
with B-frames, H.265 8- and 10-bit, AV1 and VP9, and every stream's frames are
byte-identical to libavcodec's software decode of the same stream. Getting there
found seven faults in the forwarder, none of them video-specific in kind, and
each is §7.4's.

### 7.1 The design, and the decision it overturns

§6, docs/design.md §0.8 item 2, §0.21.2 and §11 all said the same thing: do
**not** show the application a decode queue family; let the GPU side own the
video session, the DPB and the decode queue *privately*, behind a protocol
command of our own, and let what crosses be "a compressed bitstream and an
image id". The reason given was D8: a second family is a second ring, and a
family the application would have to be told not to use.

Both halves of the reason were checked against the code before building
against them, and neither held:

- **There is no second ring.** D8's "SPSC, per queue" was never built. Every
  `vkQueueSubmit` from every queue already travels `RingClass::Queue`, the GPU
  side serialises the driver calls on one `queue_lock`, and a queue handle is
  just an id the GPU side maps to a driver `VkQueue`. A second family costs a
  `vkGetDeviceQueue` and an entry in a table.
- **The family is not one the application must avoid — it is the one it
  needs.** ffmpeg's Vulkan hwaccel, and therefore Firefox's
  `FFmpegVulkanVideoDecoder`, creates its own `VkVideoSessionKHR` on a family
  it found with `VK_QUEUE_VIDEO_DECODE_BIT_KHR` and records
  `vkCmdDecodeVideoKHR` on it. A private session behind a protocol command has
  no consumer: nothing that exists would call it, and Firefox would need tier 2
  of docs/design.md §0.21.9 before a single frame decoded.

And the property the private design was protecting — **the GPU side owns the
session and the pictures never cross** — holds anyway, because it is true of
every forwarded object. The session, its memory, the session parameters and
every DPB and output image are the driver's objects, created and bound on the
GPU side; the application holds ids. What crosses per frame is the bitstream —
in a host-visible buffer, which on this design *is* the arena — and the
commands. A decoded picture crosses only if the application copies it out,
which is the readback of docs/design.md §0.21.3, and that is the application's
choice rather than the transport's. `video-probe` now prints where every
allocation landed, and through the forwarder:

```
memory: session 177152 B, bits 0x1 -> type 0 DEVICE_LOCAL
memory: dpb image 92160 B, bits 0xf -> type 0 DEVICE_LOCAL
memory: decode target image 98304 B, bits 0xf -> type 0 DEVICE_LOCAL
memory: bitstream buffer 8192 B, bits 0xe -> type 1 HOST_VISIBLE | HOST_COHERENT
memory: readback buffer 94208 B, bits 0xe -> type 1 HOST_VISIBLE | HOST_COHERENT
```

Session, DPB and target in docs/design.md §9.1's type 0, which is GPU-side
memory and never mapped; bitstream and the probe's own readback in the arena.
That is §0.21.2's decision 1, kept, by a different mechanism.

**What is advertised, per codec.** Nothing is named. The GPU side reports the
families it serves; the app side ORs their flags and hands that to the derived
gate of docs/design.md §0.22 (`unservable_queue_requirement`), which now opens
for `VK_KHR_video_queue` and `VK_KHR_video_decode_queue` and, through the
`depends=` closure, for each `VK_KHR_video_decode_*` codec extension the
driver has. Encode stays refused for the same reason it always was — the
encode family is not served (`VIDEO_ENCODE` is masked) — and nothing had to be
added to a list to keep it so. The decode family's `videoCodecOperations` is
restricted to codecs whose extension the device enabled, so the family and the
extension list cannot disagree. On RADV that is H.264, H.265, AV1 and VP9; on
the guest Intel it would be H.264, H.265 and AV1, because that driver has no
VP9 extension (§1).

`SV_VIDEO_DECODE=0` on the GPU side puts back exactly the one family it served
before, for bisecting.

### 7.2 Queue families at the driver's own indices

The served families are the driver's prefix `0..=max(compute, decode)`, **at
their own indices**, one queue each — not a compact list with the decode family
renumbered to 1. Renumbering would make every `queueFamilyIndex` in the API a
value to translate: `VkCommandPoolCreateInfo`, `VkVideoSessionCreateInfoKHR`,
`pQueueFamilyIndices` of every concurrently shared buffer and image, and the
`src`/`dstQueueFamilyIndex` of every barrier *inside a recorded command
buffer*. None of those is typed as a family index in `vk.xml`, so no generated
pass could find them. With the driver's numbering there is nothing to
translate.

| driver | families served | what the application sees |
|---|---|---|
| RADV (measured) | 0, 1, 2 | `GRAPHICS\|COMPUTE\|TRANSFER\|SPARSE`, `COMPUTE\|TRANSFER\|SPARSE`, `VIDEO_DECODE` |
| NVIDIA (from its table, not run) | 0, 1, 2, 3 | graphics, transfer, compute, `TRANSFER\|SPARSE\|VIDEO_DECODE` |
| guest Intel (from §1, not run) | 0, 1 | universal, `VIDEO_DECODE` |

The price is that an application sees the families in between — RADV's
async-compute family, NVIDIA's transfer and compute families. They are real
families with a real queue opened on each, so seeing them is true rather than
merely harmless; ffmpeg uses RADV's family 1 for its compute transfers. Without
a decode family the GPU side serves the one compute family it always did,
presented as family 0, unchanged.

### 7.3 Memory: the driver's numbering is not ours

`memoryTypeBits` comes back from a driver in its own numbering and is read by
the application against docs/design.md §9.1's four types. Buffers were already
renumbered app-side. Images and sessions were forwarded verbatim, which was
right only where the numberings happen to agree — on RADV they nearly do (its
type 0 is `DEVICE_LOCAL`); on NVIDIA they do not (its type 0 has no flags and
type 1 is `DEVICE_LOCAL`, so a DPB with the driver's `0x2` would have read as
our type 1, the arena). Four commands are now hand-written on the GPU side,
which is the only side that knows which driver type backs our type 0:

| command | our bit 0 | our bits 1–3 (arena) |
|---|---|---|
| `vkGetImageMemoryRequirements`, `…2`, `vkGetDeviceImageMemoryRequirements` | iff the driver's bits include our type 0's driver type | iff they include the arena's driver type |
| `vkGetVideoSessionMemoryRequirementsKHR` | iff the driver accepts any type | never |

The session's bit 0 is unconditional because of a fifth: **RADV's AV1 session
wants host-visible memory** — driver bits `0x678`, none of them its type 0 —
and §9.1 has exactly one non-arena type to offer. `vkBindVideoSessionMemoryKHR`
is the first moment the GPU side knows which session and bind index a
`VkDeviceMemory` is for, so a *dedicated* type-0 allocation (offset 0, whole
size bound — what ffmpeg and `video-probe` both do) is re-made there from a
type the driver accepts. A shared allocation is passed through and the driver
answers. Without this, AV1 failed in ffmpeg with `No memory type found for
flags 0xffffffff`, and VP9's hwaccel failed to initialise; both decode with it.
The VP9 failure's cause was not separately confirmed.

`vkAllocateMemory` now also carries `VkMemoryDedicatedAllocateInfo`. RADV
reports `requiresDedicatedAllocation` for ffmpeg's decode pictures, and the GPU
side's validation layer said `VUID-VkBindImageMemoryInfo-image-01445` ten times
a run while the link was dropped. RADV decoded correctly regardless; a driver
that relies on dedication would not.

### 7.4 Seven faults, found by running it

Each was invisible until a real consumer reached it, which is the pattern
docs/design.md §0.9 item 2 names: *implemented* had meant *compiled*.

1. **`pStdHeaderVersion` crossed as `{ "", 0 }`.** `VkExtensionProperties` is
   `returnedonly`, and the app side's `lower_*` writes zeros for a
   `returnedonly` struct because on the way out its storage is uninitialised.
   `VkVideoSessionCreateInfoKHR::pStdHeaderVersion` takes one *in*. The GPU
   side's validation layer: `VUID-VkVideoSessionCreateInfoKHR-pStdHeaderVersion-07190`,
   *extensionName '' is an unsupported Video Std header name*. RADV accepted
   it; a driver that checks the version would refuse every session. Now
   `exceptions::RETURNED_ONLY_READ_AS_INPUT`, one entry, checked against the
   registry by `sv-codegen/tests/returned_only_inputs.rs`.
2. **Nested fixed arrays shadowed their loop index.** `StdVideoH264ScalingLists::
   ScalingList4x4` is `uint8_t[6][16]`; both loops were `for __i`, every read
   was `[__i][__i]` with the inner index, and the seventh read of the first
   dimension panicked — inside `vkCmdDecodeVideoKHR`'s lowering, under ffmpeg,
   as an abort. `video-probe`'s fixtures never set scaling matrices; x264's
   High-profile output does. docs/design.md §0.9 item 5 fixed this array's
   *element type* and left the index; the shadowing was in the app side's
   lowering, the GPU side's raise and the app side's writeback. Each level now
   has its own index (`__f0` innermost). Tests in `sv-icd-entry` and
   `sv-gpu-entry` give every element a distinct value; the raise one fails
   without the fix.
3. **H.265 lost the device and took the GPU side with it.** `video.xml` gives
   `StdVideoH265HrdParameters::pSubLayerHrdParametersNal`/`…Vcl` the length
   `len="*_max_sub_layers_minus1 + 1"` — a count held by whichever VPS or SPS
   the HRD parameters belong to, not by a sibling. With no lowering for the HRD
   struct there was none for the VPS, the SPS (through its VUI) or any H.265
   session- or inline-parameters struct, and **those links fell out of their
   `pNext` chains without a word**. RADV received an H.265 session-parameters
   object with no parameter sets, the first submit returned
   `VK_ERROR_DEVICE_LOST`, and `sv-gpu-server` segfaulted. Now
   `exceptions::LEN_FROM_CONTEXT` names the array and
   `LEN_CONTEXT_PROVIDERS` the two ancestors; the ancestor's generated lowering
   publishes the count on a per-thread stack for exactly as long as its members
   are lowered (`sv_icd_entry::lowering_context`), and an HRD with no ancestor
   above it is refused with `Shape` rather than read to a guessed bound.
   `tests/h265_context.rs` checks both paths read exactly the published count;
   it runs clean under Miri.
4. **Session memory the driver will not take from our type 0** — §7.3.
5. **A file descriptor forwarded as a number.** The forwarded device advertises
   `VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf`,
   `VK_KHR_external_semaphore_fd` and `VK_KHR_external_fence_fd` — they are in
   the `full` profile and on the driver — while the external-properties queries
   answer "no handle type". `vkGetMemoryFdKHR`, forwarded, returned the number
   of a descriptor open in the GPU side's process; the importers would have had
   the GPU side's driver take ownership of, and close, its own descriptor of
   that number. Nothing had called them until there was a decode queue, and
   Firefox's Vulkan decoder calls `vkGetMemoryFdKHR` on its first frame. The
   six commands are now refused app-side (`VK_ERROR_TOO_MANY_OBJECTS`,
   `VK_ERROR_INVALID_EXTERNAL_HANDLE`). Withdrawing the extensions would be
   cleaner and is not done: Zink reads them, and narrowing the advertised set
   is not this branch's call.
6. **`vkGetDeviceQueue2` was a refused stub** that left `*pQueue` untouched.
   ffmpeg 9.0.1 — the ffmpeg Firefox 157 loads — fetches every queue with it,
   then submitted on an uninitialised handle; the GPU side mapped id 0 to a
   null `VkQueue` and RADV segfaulted in `vk_common_QueueSubmit2`. ffmpeg 8
   used `vkGetDeviceQueue` and never met it. Now hand-written beside
   `vkGetDeviceQueue`.
7. **A race in command-blob recording**, which is not video's at all and is the
   one with the widest reach. `Transport::record` advanced a blob's `len` under
   the lock and copied the command in after releasing it, on the argument that
   the GPU side cannot see a range until the blob is published — but
   `publish_recorded_first` runs before every unrecorded command from *any*
   thread and publishes every open blob up to its `len`. ffmpeg's frame threads
   record on one thread while another submits, and about one multi-threaded
   1080p decode in seven stalled for ten seconds: the GPU side read zeros
   (`stream truncated: need 4 bytes, 0 remain`), took the ring fatal, and the
   application waited out `CALL_TIMEOUT`. None of twenty-five runs with
   `-threads 1` did. The copy now happens before `len` advances, under the
   lock; zero stalls in thirty runs after. Once, before the fix, a script run
   produced *wrong H.265 frames* rather than a stall — consistent with the same
   race landing a partly-written command the decoder could still parse — and
   that did not recur in thirty-three runs after it; the attribution is
   inferred, not shown.

Fault 3's shape deserves its own line: **an app-side `pNext` link with no
lowering is dropped, not refused.** That is why H.265 failed as a device loss
three calls later instead of as an error at the call that carried the
unlowerable struct. It is not changed here — `lower_pnext` belongs to another
branch in flight — and it is the next thing that will hide a fault like this
one.

### 7.5 The results

**`video-probe decode`, through the forwarder.** Same binary, same fixture as
§3:

| fixture | GPU side | frames | sha256 | vs libavcodec |
|---|---|---|---|---|
| 320×180 H.264 Main, 24 IDR | RADV, Mesa 26.2.0 (dev shell), debug build | 24 | `b3a1374e…` | **byte-identical** |
| 320×180 H.264 Main, 24 IDR | RADV, Mesa 26.2.4 (system), release build, ×7 | 24 | `b3a1374e…` | **byte-identical** |
| 1920×1080 H.264 High, 30 IDR | RADV, Mesa 26.2.0 | 30 | `9cc17c94…` | **byte-identical** |
| 1920×1080 H.264 High, 30 IDR | RADV, Mesa 26.2.4, ×7 | 30 | `9cc17c94…` | **byte-identical** |

**ffmpeg 8.1.2's `-hwaccel vulkan`, through the forwarder.** Every frame
downloaded (`hwdownload`, which is `av_hwframe_transfer_data`) and hashed
against libavcodec's software decode of the same file. Streams are
`testsrc2`, encoded by x264/x265/SVT-AV1/libvpx with their usual GOPs:

| stream | frames | direct | forwarded | software |
|---|---|---|---|---|
| H.264 High 1080p30, 4 Mbit/s, B-frames | 300 | `5d146c24…` | `5d146c24…` | `5d146c24…` |
| H.265 Main 720p30 | 120 | `24f006fc…` | `24f006fc…` | `24f006fc…` |
| H.265 Main10 720p30, P010 | 120 | `2d5ae18a…` | `2d5ae18a…` | `2d5ae18a…` |
| AV1 Main 720p30 (SVT-AV1) | 120 | `e5c97c2f…` | `e5c97c2f…` | `e5c97c2f…` |
| VP9 profile 0 720p30 | 120 | `c77e30cb…` | `c77e30cb…` | `c77e30cb…` |

Measured on both Mesas; the table is identical for both. Every row was taken
again with the final code — after faults 6 and 7 — with the release build on the
system Mesa, and every hash is unchanged; ffmpeg 9.0.1 (which reaches the
forwarder only since fault 6) gives the same hashes for the four 8-bit streams.

**The validation layer, on both sides at once** (`VK_INSTANCE_LAYERS` in the
environment of the GPU side and the application), over all four ffmpeg
streams, and again over H.264 and AV1 after the dedicated-allocation fix: the
**application** side reports nothing. The GPU side, after that fix, reports two
things, neither new: `VUID-VkDeviceCreateInfo-pNext-04748` and
`-03328` (the GPU side enables both `VK_KHR_` and `VK_EXT_buffer_device_address`
when it opens its device), and `VUID-vkGetMemoryHostPointerPropertiesEXT-pHostPointer-parameter`,
which is ffmpeg probing `VK_EXT_external_memory_host` with a `malloc` pointer
the app side translates as if it were arena memory. That last is the same shape
as fault 5 and is recorded, not fixed: `VK_EXT_external_memory_host` is
advertised and its `pHostPointer` is only honest when it points into the arena.

### 7.6 What the forwarder costs a frame

**Latency**: `video-probe`'s submit→fence time, one frame at a time — decode on
the decode family, copy on family 0, a binary semaphore between, a fence wait —
release build of the final code, system Mesa, seven runs each, the median of
each run, sorted (bold is the median of the seven):

| fixture | direct (ms) | forwarded (ms) |
|---|---|---|
| 320×180 | 0.762 0.783 0.799 **0.802** 0.861 0.869 0.873 | 0.749 0.770 0.771 **0.812** 0.861 0.879 1.047 |
| 1920×1080 | 3.882 3.988 4.033 **4.232** 4.392 4.442 4.449 | 3.993 4.273 4.282 **4.440** 4.443 4.461 4.528 |

**At 320×180 the forwarder's cost is inside the run-to-run spread; at 1080p it
is about 0.2 ms, 5%, on the median.** Two earlier sets, before the final fixes
and with other work on the CPU, read +0.3 ms and −0.1 ms at 1080p, so the
sign and size of that number move with the machine's load and this is not a
precise figure. What makes it small at all is that both
`vkQueueSubmit`s are asynchronous on the wire (docs/design.md §0.3) and only the
fence wait is a round trip, against an RADV decode whose own floor for a
320×180 picture is three quarters of a millisecond.

**Throughput**: ffmpeg decoding with frames left on the GPU (`-f null`),
`-benchmark`'s `rtime`, three runs each, final code:

| stream | direct (s) | forwarded (s) |
|---|---|---|
| H.264 1080p, 300 frames | 0.511 0.477 0.497 | 0.489 0.495 0.503 |
| H.265 720p, 120 frames | 0.185 0.186 0.188 | 0.181 0.174 0.185 |
| AV1 720p, 120 frames | 0.227 0.223 0.216 | 0.225 0.222 0.223 |
| VP9 720p, 120 frames | 0.189 0.227 0.195 | 0.200 0.196 0.189 |

About 555–600 frames a second of 1080p H.264 either way: ffmpeg keeps several
frames in flight, so a pipelined decoder does not wait on the transport at all.
Before fault 7 was fixed, about one forwarded run in seven took **ten seconds**
instead of half of one — the app side's `CALL_TIMEOUT`, after the GPU side had
taken the ring fatal on a half-written command. That was the race, not a
transport cost, and thirty runs after the fix had none.

### 7.7 Firefox decodes H.264 on the forwarded device

**Which Firefox.** Checked 2026-10-15 against `nixpkgs-unstable`, no
compiling:

| attribute | version | how it arrives |
|---|---|---|
| `firefox` / `firefox-unwrapped` | **157.0.1** | cache.nixos.org |
| `firefox-bin` | 157.0.1 | unfree, so not on cache.nixos.org; nix-community.cachix.org |
| `firefox-devedition` | 158.0b1 | substituter |
| `firefox-esr` | 153.4.0esr | substituter |
| `firefox-beta`, `-nightly` | aliases of `firefox`, or absent | — |

157's `libxul.so` has `FFmpegVulkanVideoDecoder` and the three
`media.hardware-video-decoding-vulkan.*` prefs, and its wrapper puts
**ffmpeg 9.0.1** (built `--enable-vulkan`, with the H.264, H.265, AV1 and VP9
Vulkan hwaccels) on `LD_LIBRARY_PATH`. Read at the 157.0.1, 158.0 and `main`
tags: **neither Vulkan output path stopped ending in `vkGetMemoryFdKHR`**, and
the VA-API path is `vaExportSurfaceHandle` only, so §5's conclusion stands for
the newest Firefox too. Mesa !31517 (Zink's VA-API frontend) is out of draft but
unmerged, and a VA-API surface reaches Firefox as a dma-buf anyway, so no VA-API
route exists for this topology. The route is §5's item 3, docs/design.md
§0.21.9's tier 1: a Firefox patch.

**The patch is `nix/firefox-vulkan-readback.patch`**: 123 added lines in five
files, behind a new pref `media.hardware-video-decoding-vulkan.readback.enabled`
(default false), and it turned out to need **three** changes where §0.21.8
predicted one:

1. **The third output path**, in `FFmpegVideoDecoder::CreateImageVulkan`:
   `av_hwframe_transfer_data` (now loaded beside `av_hwframe_map`) copies the
   decoded NV12 frame to host memory, and `VideoData::CreateAndCopyData` makes
   the `PlanarYCbCrImage` the software decoder makes, Cb and Cr taken from the
   one interleaved plane with a skip of one. No dma-buf, no `sync_fd`, and the
   RDD process's shmem path carries it unchanged. Anything but NV12 is a
   `DECODE_ERR`, which `MediaFormatReader` recovers from with software decode,
   so a 10-bit stream degrades instead of failing the element. The dma-buf copy
   ring (`InitCtx`, which needs the export entry points) is skipped.
2. **The GL test.** `GfxInfo` probes the Vulkan device only if glxtest said the
   GL is accelerated, and Mesa's EGL says Zink on a device with no DRM node is
   `EGL_MESA_device_software`. That is true of the forwarder here and of the real
   host, which has no `/dev/dri` at all — so **an unpatched Firefox on this
   topology never even tries Vulkan decode**, whatever its prefs. With readback
   on, the probe no longer requires an accelerated GL: Vulkan decode does not
   use GL.
3. **"Requires DMABUF"**: `gfxPlatformGtk` disables `HARDWARE_VIDEO_DECODING`
   outright when the compositor offers no dma-buf. A readback frame is never
   one, so with the Vulkan decoder enabled and readback on, that requirement
   is dropped for decoding (encoding keeps it).

Reaching the decoder also needed two forwarder fixes and found a third, all in
§7.4's sense: `vkGetDeviceQueue2` had been a refused stub, and ffmpeg 9 fetches
every queue with it (the application then submitted on an uninitialised
handle and RADV segfaulted in `vk_common_QueueSubmit2`); the command-blob race
of §7.4 fault 7; and the debug build of `sv-icd` overflows the stack of Firefox's
`Renderer` thread in `lower_pnext` — the fault another branch owns — so the run
below uses the release ICD, whose frames are smaller.

**Built locally, inside the shared slice.** `nix develop` on
`firefox-unwrapped`, the nixpkgs patches, then `./mach configure` with nixpkgs'
flags minus `--enable-lto` and `--enable-debug-symbols`, and `make` in a scope
capped at 6–7 GiB inside `builds-10g.slice`. Two local build-file edits, not in
the patch, were needed to fit: Rust's `codegen-units=1` (the `style` crate
alone passed 6 GiB) and the fat `-Clto` of `gkrust` (7 GiB and still growing).
About forty minutes of wall time for the last Rust and C++ stages once those
were out of the way (earlier attempts died to the OOM killer); an incremental
rebuild of the patched files is 25 seconds.

**The run**, `scripts/sv-firefox-vulkan-decode.sh` with the patched build:
Xvfb (on this machine Mesa's Wayland EGL refuses Zink — "failed to choose
pdev", with or without this branch — while Zink through the same ICD on Xvfb
comes up on the forwarded RADV), the GPU side on RADV, Firefox loading only
`sv-icd`, `MOZ_DISABLE_RDD_SANDBOX=1` (the RDD sandbox brokers `connect()` to
the display sockets only, so the decoder process could not reach the GPU side),
no VA-API driver reachable, a 10-second 720p H.264 High clip looped in one tab
and about:support in the other:

```
  ok   the GPU side served 4 session(s)
  ok   the RDD process made its decode device: AMD Ryzen 9 9950X3D 16-Core Processor (RADV RAPHAEL_MENDOCINO) (gen 1)
  ok   1770 frames decoded on the forwarded device and read back
  ok   the page presented 1757 frames with no media error (frames 1757 dropped 0 t=8.5 err=none)
  about:support → Graphics: zink Vulkan 1.4(… RADV RAPHAEL_MENDOCINO …) [mesa/software-unknown], WebRender
        H264 SWDEC HWDEC
        VP9 SWDEC HWDEC
        AV1 SWDEC HWDEC
        HEVC SWDEC HWDEC
  about:support → Graphics → features:
        HARDWARE_VIDEO_DECODING: available
        HARDWARE_VIDEO_DECODING_VULKAN: force_enabled ; Force enabled by pref; Blocklisted by gfxInfo
  ok   about:support lists H.264 hardware decoding
sv-firefox-vulkan-decode: PASS
```

The 1770 is the RDD process's own per-frame log line, which only the readback
branch prints, after a `vkCmdDecodeVideoKHR` on the forwarded device produced
the frame. about:support was read as data through Marionette
(`Troubleshoot.snapshot()`), and photographed through WebDriver; the
photographs were checked by a separate reader, who reported the Media table's
H264 row as Hardware Decoding **Supported** and the video tab as a correct
testsrc2 frame (bars in order, no channel swap, no blocking) with the status
line `frames … dropped 0 … err=none`. The `HWDEC` column is Firefox's probe's
belief, not a measurement; the per-frame log line is the measurement.

Controls, same machine, same GPU side:

| Firefox | prefs | what decoded | about:support H264 |
|---|---|---|---|
| stock 157.0.1 | Vulkan on, force-enabled | **software**: the probe never ran (GL "software") | `SWDEC` |
| patched | Vulkan on, readback **off** | **software**, same reason | `SWDEC` |
| patched | Vulkan on, readback on | **Vulkan on the forwarded RADV**, 1770 frames | `SWDEC HWDEC` |

Both controls play cleanly. That is §0.21.6's hazard measured rather than
predicted, and it is milder than predicted: on this topology a stock Firefox
with the Vulkan pref on does not reach the fatal export path at all, because the
GL precondition stops it first — and if it ever did, the fd refusal of §7.4
fault 5 turns the bogus descriptor into a `vkGetMemoryFdKHR` error.

**What it costs** was not measured: no CPU or power comparison against
software decode was taken, and docs/design.md §0.21.11's verdict — tier 1 is a
stepping stone, not the goal — is unchanged by this run. What the run
establishes is the stepping stone: the private session §0.21 asked for is the
application's own forwarded session, and a real browser decodes on it.

### 7.8 The gates

Run on the branch's final code on devhost, 2026-10-15.

| gate | result |
|---|---|
| `cargo test --workspace --no-fail-fast` | **2718 passed, 0 failed**, 8 ignored (baseline 2704; the 14 new are this branch's) |
| `cargo clippy --workspace --all-targets` | exit 0; no warning in code this branch wrote (two pre-existing elsewhere) |
| `cargo check --workspace --target x86_64-pc-windows-gnu` | exit 0 |
| Miri: `sv-icd-entry` `h265_context`, `fixed_arrays`; `sv-gpu-entry` `fixed_arrays` | 5 tests, clean |
| `scripts/sv-video-decode.sh` (dev-shell RADV, debug build) | **PASS** three runs in a row, 13 checks each |
| ffmpeg 8.1.2 through the forwarder, 5 streams, system RADV, release | all equal libavcodec (§7.5) |
| ffmpeg 9.0.1 through the forwarder, 4 streams + Firefox's clip | all equal libavcodec |
| validation layer on both sides, ffmpeg H.264 and AV1 | application: none; GPU side: only findings that predate this branch |
| multi-threaded decode stall (fault 7) | 0 of 30 runs after the fix, against ~1 in 7 before |
| `scripts/sv-firefox-vulkan-decode.sh`, patched 157.0.1 | **PASS**: 1770 frames decoded and read back, page `err=none`, about:support `H264 SWDEC HWDEC` |
| the same, stock 157.0.1 and patched with readback off | software decode, plays cleanly (the expected FAIL of the Vulkan checks) |
| NVIDIA RTX 5090 | **not run** — lease job 120 queued |
| guest Intel UHD | **not run** — by instruction; steps in §7.9 |

### 7.9 What this does not tell us, and the steps for the real guest

- **NVIDIA has not run through it.** The RTX 5090 is leased through `gpu-run`
  and job 120 (shared, 2 GiB; a script kept outside the tree that runs §7.5's
  and §7.6's measurements — `video-probe` direct and forwarded ×5 on both
  fixtures, ffmpeg on all five streams, a validation-layer pass — under the
  newer glibc the system NVIDIA driver needs) was still queued when this was
  written. §7.2's NVIDIA
  row and §7.3's argument for the image renumbering are read from its family
  and memory tables, not from a decode. NVIDIA is the one driver here whose
  decode family has `TRANSFER` and whose output must coincide with the DPB, so
  it exercises paths RADV does not.
- **The guest's Intel has not run through it**, by instruction. Three things
  about it are untested and specific to it: its decode family has no
  `TRANSFER` and no `VK_KHR_video_maintenance1`, so every bitstream buffer is
  created against its profile; its bitstream alignment is 4096; and the
  bitstream buffer is bound into **host-pointer-imported** guest RAM (the
  Windows arena provider), with `VIDEO_DECODE_SRC` usage — whether driver
  101.7076 accepts that combination is the first thing the guest run will
  answer. If it does not, the fallback is a device-local bitstream buffer and
  one copy out of the arena per frame, which is a GPU-side change only.
- **Concurrency beyond ffmpeg's.** Fault 7 was found because ffmpeg records
  and submits from different threads; nothing here stresses more than one
  application thread pair, or several decoding processes on one GPU side.
- **Interlaced H.264, AV1 film grain, 4:2:2/4:4:4** were not decoded. Film
  grain is refused by the guest Intel anyway (§1).
- **No sustained-playback power or CPU measurement** of the forwarded decode
  was taken; docs/design.md §0.21.11's question — is tier 1 worth having over
  software decode — is still open, and it needs the guest.

**The real-guest verification, in order.** Every step's pass condition is a
hash or a log line, not a judgement.

1. Build both halves from one commit (`scripts/sv-firefox-guest.sh`'s banner
   says why): `cargo build --release -p sv-icd -p video-probe` on the host and
   `cargo build --release -p sv-guest --target x86_64-pc-windows-gnu` for the
   guest's `sv-guest.exe` (whose `gpu guest` mode is the GPU side), deployed
   as that script deploys it. `SV_VIDEO_DECODE=0` in the guest's environment
   puts back the single-family GPU side, for a before/after on the same build.
2. **Capabilities.** On the host, `video-probe caps` through the ICD against the
   guest's GPU side; compare every codec block with `video-probe.exe caps` run
   in the guest (`scripts/sv-video-decode.sh` check 6's comparison). Expect the
   decode family as family 1, `videoCodecOperations`
   `DECODE_H264|DECODE_H265|DECODE_AV1`, no VP9 extension, no encode.
3. **`video-probe decode`** of the 320×180 IDR fixture through the forwarder:
   `sha256 b3a1374e…`, the same as §3's in-guest run. The `memory:` lines must
   show session, DPB and target in type 0 and the bitstream in the arena.
4. **ffmpeg through the forwarder** (`scripts/sv-video-decode.sh` check 7, with
   `SV_SOCKET` naming the guest session): H.264, H.265 8-bit and AV1 must equal
   libavcodec; VP9 must be *refused* at device selection (no extension), not
   fail mid-stream; H.265 Main10 is expected to fail on this driver's empty
   format list (§1 finding 5) — record which way.
5. **The validation layer** on both sides for steps 3 and 4.
6. **Firefox**: the patched build (`nix/firefox-vulkan-readback.patch`) with
   `scripts/sv-firefox-vulkan-decode.sh`'s prefs and environment, on
   `sv-firefox-guest.sh`'s topology; pass condition is the RDD log's
   `[VULKAN] readback: … copied to host memory` lines and the page's frame
   count with `err=none`, then about:support's H264 row.


**Status, 2026-10-15.** None of the steps above has run on the guest.
`waycompose-design.md` §86 built the readback-patched Firefox as a store path
(`scripts/sv-firefox-readback-pkg.sh`) and wired the launcher to turn decode on
when a session has a forwarded GPU and the browser carries the readback marker,
but GPU work on the guest is suspended until the Intel driver fault of `waycompose-design.md` §86.4 is
fixed on the forwarder's side.

**Later the same day (`waycompose-design.md` §87): steps 2 and 3 ran on the
guest.** Step 2 needed a fix first: Intel's driver writes four bytes past
`VkVideoDecodeCapabilitiesKHR`, which corrupted the GPU side's heap
(`STATUS_HEAP_CORRUPTION`) until every allocation handed to a driver got guard
bytes; with them the capabilities through the forwarder are §1's survey, except
one decode queue where the driver has two. **Step 3 passed**: 24 frames,
`sha256 b3a1374e…`, byte-identical to libavcodec, the bitstream buffer bound into
host-pointer-imported memory — an ivshmem BAR since §87 — with
`VIDEO_DECODE_SRC`, so §7.9's first question is answered yes; submit→fence median
1.027 ms. The run ended with a fault in `igvk64.dll` in the GPU side's own
session cleanup (a command pool destroyed twice, fixed and proven on RADV), and by
the rule of §86.5 steps 4–6 did not run.

### 7.10 The fault in Intel's `vkCmdDecodeVideoKHR`: what is known, the instrument, and one guest run

Branch `decode-reinit`, 2026-10-15, devhost. Nothing was run on the guest.

**The root cause is not identified.** What follows is what the guest's logs do
and do not say, what was reproduced locally and what was not, the instrument
that will name the faulting call on the next guest run, and the causes still
standing, ranked.

#### 7.10.1 What the guest run actually shows

waycompose-design.md §87.12 records `igvk64.dll+0x2f185b`, a read of `0x7c`,
inside `vkCmdDecodeVideoKHR` replayed from a command blob, and reads the RDD
log's last lines as coming from a *second decoder instance* (`MediaPDecoder #2`).
**That reading is wrong.** `MediaPDecoder #1` and `#2` are the two threads of
Firefox's decoder task queue, and they alternate frame by frame from the first
frame. The RDD log (`/tmp/svgpu-soak/moz.log.child-2.moz_log` on testhost) has
exactly one `Initialising Vulkan FFmpeg decoder` and one `Vulkan FFmpeg init
successful`. What happens at each of the 10-second clip's loop points is a
drain (`End of stream`) and `ProcessFlush()` — `avcodec_flush_buffers` — at log
lines 1599–1612, 3105–3118 and 4611–4624, three times. No decoder is destroyed
or re-created, and no SPS or PPS is re-sent: the clip (`clip.mp4`, x264 High,
`keyint` 250, IDRs at packets 0 and 250) carries its parameter sets only in the
`avcC` extradata.

The last logged sample is `mTime=8233333` in the fourth loop, decode-order
packets 245–247, a few packets before the second IDR (packet 250), and about 245
frames *after* the third flush. The same packets decoded cleanly in loops 1–3.
The MOZ_LOG file is buffered and the RDD process crashed later, so the log's
end is not an exact time; the page's own count (`frames 1147 … err=3`, with
`t` reaching 9.9 while no frame arrived) puts the stall at about that frame. So
the fault **builds up over time or is a race**. It is not tied to the
re-initialisation §87.12 suspected, which did not happen, nor to a particular
frame of the stream.

The fault report's ring lines do not name the faulting session:
`crate::fault`'s slots are indexed by ring number and shared by every session
of the process, so the `vkQueueSubmit` and `vkWaitForFences` it lists may
belong to the WebRender or glxtest sessions. The backtrace
(`cmd_decode_video_khr` ← `subdecode` ← `drain`) is the only part that is
certainly the decoder's. The guest's GPU-side stderr from that run was not kept
on testhost.

#### 7.10.2 What Intel's ffmpeg path does that RADV's did not

Every earlier local run took a different path through ffmpeg 9.0.1 than the
guest did, because the two drivers answer differently:

| | RADV (every earlier run) | Intel 101.7076 (guest) |
|---|---|---|
| `VK_KHR_video_maintenance2` | yes: SPS/PPS **inline** in every decode | no: a `VkVideoSessionParametersKHR` **object** |
| `VK_KHR_video_maintenance1` | yes | no: buffers created against a profile list, plus an empty parameters object for the reset |
| H.264 decode flags | `DPB_AND_OUTPUT_DISTINCT` (0x2) | `DPB_AND_OUTPUT_DISTINCT \| 0x10000` |
| DPB slots, bitstream alignment | 17, 256/128 | 16, 4096/4096 |
| `SEPARATE_REFERENCE_IMAGES` | set | **never recorded** |

Both drivers lack `COINCIDE`, so ffmpeg uses a dedicated DPB on both. The first
two rows, though, mean RADV never exercised the parameters-object path at all.
`scripts/vk-intel-shape-layer.c` is a Vulkan layer, loaded **in the GPU side
only** (`SVFD_GPU_LAYERS`), that makes RADV answer the way Intel does: it hides
both maintenance extensions, caps H.264 at 16 slots, raises both alignments to
4096 and clears `COINCIDE`; with `SV_SHAPE_SEPARATE_REFS=0` it also clears
`SEPARATE_REFERENCE_IMAGES`, which gives ffmpeg's layered DPB. It only takes
capabilities away, so the driver below can still do everything the application
is told it can.

#### 7.10.3 The instrument: `SV_VIDEO_LOG`

`sv_gpu_server::video_log` writes one line per video call, **before** the
driver sees it, from the arguments exactly as the driver receives them. So on a
thread that dies inside the driver, the last `decode` line is the faulting call.
`SV_VIDEO_LOG=1` writes to stderr; a path appends to a file, one unbuffered
`write` per line. The hook is generated: `sv_codegen::exceptions::GPU_OBSERVED`
names 18 commands, and their generated forwarders call `GpuHost::observe`
before and after the driver call. Off, the cost is one call to `observing()`
per observed command. `tests/gpu_observed.rs` checks that every entry is
generated and calls the observer twice, and that nothing else does.
`vkCreateBuffer` is hand-written and reports itself.

It logs the capabilities the driver answered (including the general `flags`
Intel's survey never recorded); sessions; parameters objects with each SPS
and PPS id; images with a video usage, their views, bitstream buffers and their
memory binding; every `begin`, `control`, `end` and `decode` with each slot,
picture resource, view and layer; and the submits of command buffers that
recorded video. It marks with `!!`:

- a slot index outside the session's DPB;
- a session, parameters object, image or view that is already destroyed, or
  was never seen;
- a reference not bound by the scope's `begin`, or bound to a different
  resource;
- a missing setup slot, codec link or standard struct, or NULL slice offsets;
- a decode outside a coding scope, or a submit with a scope still open;
- no live parameters object of the session carrying the picture's PPS;
- and the **bytes at every slice offset**, read through the GPU side's own
  mapping of the arena: a start code, an H.264 slice NAL, IDR agreeing with
  `IdrPicFlag`, and a `pic_parameter_set_id` equal to the picture's.

The last check is there because of how Intel's decoder works. Intel's hardware
takes parsed slice parameters, and Vulkan Video gives a driver only slice
offsets. Mesa's ANV therefore parses each slice header **on the CPU, from the
bitstream buffer, at `vkCmdDecodeVideoKHR`** (`vk_video_parse_h264_slice_header`).
That Intel's Windows driver does the same is an inference, not a measurement.
If it does, the bytes are an *input to the recording*. A header naming a PPS
the session lacks would be a NULL PPS to that parser, while AMD's firmware
parses on the GPU and would at worst damage a frame.

At a fault, `crate::fault`'s Windows filter now also writes:

- the video log's last decode line;
- live counts of sessions, parameters, video images, views and bitstream
  buffers;
- the command buffers with a coding scope still open;
- all of it into the log file as well as to stderr;
- and, when `SV_MINIDUMP=<file>` is set, a minidump from `MiniDumpWriteDump`
  (thread stacks, data segments and indirectly referenced memory), so that the
  structures the faulting frame points at can be read afterwards.

The state is read with `try_lock`, so a thread that died holding it costs one
line rather than a hang.

Tests: `video_log`'s three unit tests (slice-header parsing through emulation
prevention, view liveness, reading a raw `VkVideoDecodeInfoKHR`) run clean under
Miri.

#### 7.10.4 The local proof, and what it leaves

`scripts/sv-firefox-vulkan-decode.sh 140` with the guest's own `clip.mp4`
(`SVFD_CLIP`), the guest's 64 MiB arena (`SVFD_ARENA_MIB=64`), the readback
Firefox 157.0.1, the GPU side shaped as Intel and under
`VK_LAYER_KHRONOS_validation` with synchronisation validation, and
`SV_VIDEO_LOG`:

| | |
|---|---|
| frames decoded and read back | **4165**; page 4151, dropped 0, `err=none` |
| loops | ~14 (13 flushes in the RDD log) |
| decodes logged | 4167, of them 28 IDR |
| parameters objects, sessions | 2 (the reset's empty one, destroyed after its wait, and one with sps0/pps0), 1 |
| largest setup slot index | 4 (of 16) |
| `!!` findings | **0**: every slice offset a slice NAL with pps 0, every view and image live, every reference bound by its `begin` |
| validation, video and `SYNC-` messages | **0** |
| validation, anything else | §7.5's three: `pHostPointer` NULL, `-03328`, `-04748`; plus Zink's dynamic-state messages (below) |

Two shorter runs agree: 80 s with `SEPARATE_REFERENCE_IMAGES` as RADV reports
it and 80 s with it cleared (the layered DPB), 2341 and 2368 frames, no video
messages; and 115 s with the log alone, 3412 frames, 0 findings. RADV never
handed out a destroyed view's address again: 6828 views created and 6818
destroyed in 3414 decodes, two of each per frame, by ffmpeg's
`ff_vk_decode_prepare_frame` and `ff_vk_decode_free_frame`.

So the forwarder delivers what Firefox asked for, consistently, through more
than three times the guest's frame count. The forwarder-side candidates the
investigation started from are ruled out on this path:

- a slot index beyond the DPB;
- parameters or a session used after destroy (one parameters object lives for
  the decoder's life);
- inconsistent `begin` slots;
- NULL standard structs;
- nested-struct lowering, because the GPU-side validation layer sees what the
  driver sees;
- a stale handle reaching the driver as NULL. Handles are never NULLed: an
  unknown id is a fatal table error, and ordering is `publish_recorded_first`'s
  (docs/design.md §0.23).

**What this leaves untested is precisely what differs on the guest:**

- the arena imported as host pointers, slab by slab, from an ivshmem BAR (RADV
  cannot import a host pointer at all, `sv_gpu_server::arena`);
- the Windows GPU side;
- and Intel's own internal state.

NVIDIA imports host pointers and would cover the first. A `gpu-run` lease had
12 jobs queued ahead, so it was not waited for.

An application-side validation layer (in every Firefox process) was tried
once. Firefox then never created its decode device, so that run says nothing
about decode.

Found on the way, not part of this fault: Zink, through the forwarder, records
`VK_EXT_extended_dynamic_state3` commands that the validation layer says are
not enabled (`VUID-vkCmdSet*-None-09423`, 40 each per run).
`sv_gpu_server::driver` enables every feature struct it chains, and does not
chain `VkPhysicalDeviceExtendedDynamicState3FeaturesEXT`.

#### 7.10.5 The causes still standing, ranked

Each one has to fit: Intel-only, invisible to RADV and to the validation layer,
appearing after about a thousand frames, and not tied to a frame of the stream.

1. **Intel keeps a DPB slot's picture by its view object, and ffmpeg destroys
   views every frame.** ffmpeg creates the output and DPB views for each picture
   and destroys them in `ff_vk_decode_free_frame` once the picture is released,
   while the DPB slot that picture set up stays active for later decodes. The
   specification binds a slot's picture through the view named *in each call*.
   A driver that instead remembered the view from the setup decode would read a
   freed object, and would only fault once its allocator had reused or cleared
   that memory: no fixed frame, after enough churn. That is a driver defect if
   so, and an exact workaround is cheap: the GPU side keeps one driver view per
   image and view description alive until the image is destroyed. It is **not
   applied**, because nothing yet shows it. The log names each slot's view on
   every decode and says when a new view lands at a destroyed view's address.
2. **The bitstream bytes Intel parses at record time are not the ones the
   application wrote** — a visibility or ordering race on the guest's ivshmem
   arena that RADV's GPU-side parsing would never notice. Bitstream buffers are
   pooled (2 MiB each in the arena, rotated as decodes complete), so a slice
   offset can land on an earlier frame's bytes if the write is not yet visible
   through the guest's mapping when the blob is replayed. Locally every slice
   offset held the right header 4167 times; the log reads the bytes at the same
   instant on the guest and marks a mismatch.
3. **A command buffer re-recorded while Intel still has it in flight.**
   ffmpeg's exec pool waits on each command buffer's fence before reusing it,
   and the app side orders the replay after that wait. A difference on the
   guest's rings (four sessions on one GPU process) would show in the log as a
   `begin` on a command buffer whose previous submit has not completed, or a
   scope open at submit.

Ruled out, from the coordinator's list:

- **Query reuse:** ffmpeg 9.0.1 creates its decode exec pool with no queries.
- **The bitstream offset or alignment wrapping:** `srcBufferOffset` is always
  0, the range at most 28672 of a 2 MiB buffer, and the 4096 alignment was
  emulated.
- **DPB slot activation leaking:** slot indices stay within 0–4 across 28 IDRs
  and 13 flushes. Whether Intel's *internal* activation state leaks is
  candidate 1's question.

#### 7.10.6 The one guest run

One run, the §87.12 soak unchanged except for two variables and the build:

1. Build both halves from this branch's commit, as §7.9 step 1 says:
   `cargo build --release -p sv-icd` on the host, and
   `cargo build --release -p sv-guest --target x86_64-pc-windows-gnu` for the
   guest. Deploy them to the `wcgpu` test installation (`~/wc-launch-gpu` and
   `C:\Users\<guest-user>\wcgpu`) as §87.6 did.
2. Start the session as §87.6 did, with two variables added to `wc-launch.exe`'s
   environment, which the GPU process inherits:
   `SV_VIDEO_LOG=C:\Users\<guest-user>\wcgpu\vlog.txt` and
   `SV_MINIDUMP=C:\Users\<guest-user>\wcgpu\gpu.dmp`. Keep `gpu.env`'s
   `MOZ_LOG=PlatformDecoderModule:5,FFmpegVideo:5`.
3. Drive Firefox with `~/svgpu-test/decode-drive.py` on the same
   `/tmp/svgpu-soak/pages/play.html` and `clip.mp4`, for 16 × 2.5 s. If it
   survives, run it once more for 60 s: §87.12 faulted at 38 s, and a
   no-fault result needs at least twice that.
4. Collect `vlog.txt`, `gpu.dmp` if written, the GPU process's stderr, the RDD
   MOZ_LOG and the soak's JSONL.

The pass/fail reading needs no judgement:

- `vlog.txt`'s first lines must include `caps codec 0x1: flags …`. That is
  also Intel's `SEPARATE_REFERENCE_IMAGES`, the one capability still unknown.
- If it faults: the last `decode #N` line before `[gpu] FAULT` is the faulting
  call, with every handle, slot and slice header it carried. A `!!` on it, or
  on anything since the previous IDR, names the cause. No `!!` and a view on
  that line created at a destroyed view's address points to candidate 1, and
  the minidump shows what the faulting instruction's base register held.
- If it does not fault: the run is still the first record of Intel's own
  answers (`caps`, the parameters object, the view and slot pattern), and the
  next step is the candidate-1 workaround behind a switch, measured against
  this log.

By §86.5's rule this is one run. A fault ends GPU work on the guest until it is
read.

### 7.11 The device's features, and the guest capture run

Branch `decode-capture`, 2026-10-15, devhost and testhost.

#### 7.11.1 Every feature an application is told about is a feature the device enabled

§7.10.4 found Zink recording `VK_EXT_extended_dynamic_state3` commands into
a device that had not enabled one of that extension's features. The cause was
wider than that one extension. The GPU side opens its device before any
application connects, so it has to enable up front everything an application
may later ask for. Meanwhile `vkGetPhysicalDeviceFeatures2` was forwarded
verbatim, so an application was told what the *driver* supports. The device
chained a hand-kept list of two dozen extension feature structs, and every
struct missing from that list was enabled false and advertised true.

The baseline is master's build, under the GPU-side validation layer, in a
120 s `scripts/sv-firefox-wayland.sh soak` on RADV: **395 validation errors**,
nearly all of that one kind.

| VUID family | what was not enabled |
|---|---|
| `vkCmdSet*EXT-None-09423` (17 commands), `VkGraphicsPipelineCreateInfo-extendedDynamicState3*` (17) | `VK_EXT_extended_dynamic_state3` |
| `vkCmdSetVertexInputEXT-None-08546`, `pDynamicStates-04807` | `vertexInputDynamicState` |
| `negativeOneToOne-06470`, `depthClipControl-07453` | `depthClipControl` |
| `attachmentFeedbackLoopDynamicState-08862`, `attachmentFeedbackLoopLayout-07313` | both feedback-loop features |
| `VkShaderModuleCreateInfo-pCode-08740` | `shaderFloatControls2` (a 1.4 feature) |
| `vkCmdBeginRendering-pRenderingInfo-11750` | `maintenance10` |
| `VkDeviceCreateInfo-pNext-04748`, `-03328` | `VK_EXT_buffer_device_address` enabled beside core 1.2's |

The fix, in `sv_gpu_server::features`:

- **The chain comes from the registry.** `sv-codegen` now emits
  `DEVICE_FEATURE_STRUCTS`: every struct that extends both
  `VkPhysicalDeviceFeatures2` and `VkDeviceCreateInfo`, listed with the core
  versions and profile extensions that provide it (245 in the `full`
  profile). It also emits a default link per `sType` and each struct's
  `VkBool32` members by registry name. The device chains the 1.1–1.4 core
  blocks its version has, plus every struct an enabled extension provides that
  no core block covers. One trap: vk.xml 1.4.350 files most promoted structs
  under `VK_BASE_VERSION_1_x`, `VK_COMPUTE_VERSION_1_x` and
  `VK_GRAPHICS_VERSION_1_x` rather than `VK_VERSION_1_x`.
- **What the device leaves off on purpose, it does not advertise.**
  `vkGetPhysicalDeviceFeatures2` is now hand-written on the GPU side
  (`exceptions::GPU_HAND_WRITTEN`). It asks the driver, then clears every
  member the device did not enable (`mask_to_enabled`). Two things are left
  off on purpose. One is an exclusive pair: the KHR fragment shading rate is
  kept over `VK_NV_shading_rate_image` and `VK_EXT_fragment_density_map`. The
  other is `VK_EXT_descriptor_buffer`. Zink never used it through the
  forwarder while it was advertised and not enabled: a descriptor-buffer
  command would have drawn a message, and the baseline has none. It is
  driver-written bytes holding GPU-side addresses, which nothing here has
  tested. Enabled, it is also `VUID-VkDeviceCreateInfo-None-08095` beside RADV's
  `VK_AMD_shader_fragment_mask`. With it left off, Zink stays on the path
  every earlier run measured, on RADV and on Intel alike.
- **A request the device cannot honour is refused.** `vkCreateDevice` checks
  the application's chain and `pEnabledFeatures` against the enabled set
  (`not_enabled`; a promoted struct is matched against its core block by member
  name). A miss is `VK_ERROR_FEATURE_NOT_PRESENT`, the specification's answer,
  with the members named on stderr. The first build after the registry
  rewrite showed why this matters. Its block list stopped at 1.3, Zink chained
  `VkPhysicalDeviceVulkan14Features`, and the check refused 17 members by name.
  Before the check, that would have been silent undefined behaviour.
  Extensions are reported, not refused.
- `VK_EXT_buffer_device_address` is no longer enabled beside core 1.2's
  feature.

The same soak with the fix: **0 validation errors**, no refusal, and verdicts
equal to the baseline: 24 loads, WebRender on Zink throughout, about 8000
submits per 30 s, WebGL 328 frames. `sv-firefox-vulkan-decode.sh 90` with the
guest's clip, a 64 MiB arena, the Intel shape layer and validation: 2663
frames decoded and read back, 0 dropped, H.264 `HWDEC`. Validation reports
only §7.5's `pHostPointer` message, so the dynamic-state messages and both
BDA messages are gone.

Tests (`features::tests`, seven): the regression on a real driver
(`everything_the_driver_reports_for_an_enabled_extension_is_enabled`). It asks
the driver about every struct an enabled extension or core version provides,
requests all of it, and requires that the device has every member except the
ones withheld on purpose. Once masked, it requires all of them with no
exceptions. It passes on RADV and lavapipe. With EDS3 taken out of the chain,
it fails on RADV and names all of EDS3's members. The other six are pure: EDS3
is chained without being named, the whole profile chains each extension struct
once, promoted structs stay in their block, a missing member is named, a
promoted struct matches its block, and of an exclusive pair the KHR one is
kept.

Found on the way: with two tests in one process each opening a real driver,
the first gate segfaulted in the Vulkan loader (`loader_scanned_icd_add` under
`vkEnumerateInstanceExtensionProperties`) on every parallel run. The guest's
GPU process has the same shape in production. Each ivshmem session opens and
later drops its own driver on its own thread, and `glxtest`'s session closes
while the browser's opens. `Driver::open` and `Driver`'s drop (including the
loader's unload) now hold one process-wide lock.

## What this does not tell us

> **[PARTLY SUPERSEDED — 2026-10-15, §7.]** The first two bullets and the
> "one codec" and "inter prediction" ones are answered by §7 for RADV on this
> workstation: frames have been decoded through the forwarder, video objects
> created through it, and ffmpeg's decoder has run H.264 with B-frames, H.265
> (8- and 10-bit), AV1 and VP9 through it byte-identically to libavcodec.
> §7.9 is the list that replaces this one for the forwarded path; the bullets
> about the drivers themselves still stand.

- **Nothing has been decoded through the forwarder.** §6 moves the wall from a
  type to a queue family and measures the capability path across it exactly, but
  a capability query is not a decode. Every frame in §3 is a driver reached
  directly.
- **No video *object* has been created through the forwarder either.**
  `vkCreateVideoSessionKHR` takes a `queueFamilyIndex` that must name a decode
  family, so the session, its memory requirements and its parameters — which is
  where `StdVideoH264SequenceParameterSet`'s three pointers would actually
  cross a boundary rather than only round-trip in a test — are all downstream of
  the same missing family. The round-trip tests exercise the marshalling; no
  driver has yet been handed the result.
- **The bit order of the flag words is assumed, not measured here.** §4a states
  the assumption and §3 is the evidence for it, but the evidence is indirect:
  three drivers agreed with *bindgen's* reading of the headers, not with this
  generator's. A direct check would compare the two layouts in one process, and
  it has not been written.
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
# everything, from the dev shell. The default `full` profile carries the video
# extensions; the script finds the dev shell's own RADV (the system Mesa needs
# a newer glibc than the dev shell's binaries, §7).
nix develop
cargo build -p video-probe -p sv-icd -p sv-gpu-server
scripts/sv-video-decode.sh

# Firefox decoding on the forwarded device (§7.7): build 157.0.1 with the patch,
# then point the harness at it. Release binaries: the debug ICD overflows
# Firefox's Renderer stack (another branch's fix).
cargo build --release -p sv-icd -p sv-gpu-server
SVFD_FIREFOX=/path/to/patched/firefox scripts/sv-firefox-vulkan-decode.sh 60

# the GPU side as it was before §7: one family, no decode
SV_VIDEO_DECODE=0 target/debug/sv-gpu-server serve --socket …

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

# the profile that used to be the wall
cargo run -p sv-codegen -- --profile video --stats

# the two checks that keep video.xml honest against the vk_video headers
cargo test -p sv-testkit --test header_crosscheck
```

`SV_VIDEO_DEVICE` matches a substring of the device name, case-insensitively,
and is the only knob.
