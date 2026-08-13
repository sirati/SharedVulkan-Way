# Third-party material

The workspace is `MIT OR Apache-2.0`. `design.md` §6.6 states the licence
position once; this file records what has actually been taken so far, and from
where.

## Reference material read as data (not compiled, not vendored into the product)

### Mesa Venus — `reference/venus/`

| What | Licence | Source |
|---|---|---|
| `vn_protocol_driver_*.h`, `vn_protocol_renderer_*.h` | MIT | Mesa tag `mesa-26.2.0`, `src/virtio/venus-protocol/`; virglrenderer `virglrenderer-1.3.0`, `src/venus/venus-protocol/` |
| `vn_protocol.py`, `venus-protocol-templates/` | MIT | `gitlab.freedesktop.org/virgl/venus-protocol` @ main |
| `VK_EXT_command_serialization.txt` | MIT | the same repository |
| `mesa_vn_cs.[ch]`, `mesa_vn_ring.[ch]`, `mesa_vn_instance.c`, `mesa_vn_common.h`, `virgl_vkr_cs.[ch]`, `virgl_vkr_ring.[ch]` | MIT | Mesa and virglrenderer, at the tags above |

These files are **input to a test**, not to the build. `crates/sv-testkit` parses
the generated headers as text in order to compare Venus's serialisation
decisions against ours (`security-model.md` R8, `design.md` §6.2 and §7.3). No
Venus code is compiled, linked or translated into `sv-proto`, and `sv-testkit`
is `publish = false`.

They are checked in rather than fetched, for the same reason `vk.xml` is pinned
through nix: a differential test whose reference moves underneath it is a test
whose green result means nothing. The provenance note in
`reference/venus/NOTES.md` records that the generated headers no longer exist in
Mesa `main` — venus-protocol became a meson subproject that generates them at
build time — so the tags above are the last releases that carry them.

**Note on `main` vs the tags.** Because upstream now generates rather than
vendors these headers, "update the reference" is a deliberate act: regenerate
from `vn_protocol.py` against a chosen `vk.xml`, or bump to a newer release tag.
It is not something a `git pull` will do by accident.

## Designs taken, code not taken

Recorded here because `design.md` §6.6 commits to crediting them.

| Source | Licence | What was taken |
|---|---|---|
| Venus (`VK_EXT_command_serialization`, `vn_cs`) | MIT | The wire format **rules** — primitive widths, array framing, the presence flag, the three-state output validity and the `_partial` encodings — reimplemented in Rust in `sv-wire` and `sv-codegen`. |
| gfxstream | Apache-2.0 (output), MIT (generators) | **Design only:** the pass taxonomy of `design.md` §7.2, length-prefixed `pNext` links, the boxed-handle table, the `(memory, offset)` transform. No code was read into ours. |
| Khronos `vk.xml` | Apache-2.0 OR MIT | The registry itself, pinned by nix and never vendored. |
| Khronos `video.xml` | Apache-2.0 OR MIT | The video std registry — the `StdVideo*` types of `VK_KHR_video_decode_h264` and its siblings. Read from the same pinned `vulkan-headers` path as `vk.xml`, beside it, and never vendored. `crates/vk-registry/src/lib.rs` says why it is read instead of the `vk_video/*.h` headers it generates. |
| Khronos `vulkan_core.h`, `vk_video/*.h` | Apache-2.0 | Read as **data at test time only**, by `sv-testkit`, out of the same store path, as the independent cross-check that our reading of the two registries is right. Nothing from them is compiled or vendored. |

## Deviations from Venus that are ours

Stated here so that nobody reading the differential test mistakes agreement for
identity:

1. **Padding bytes are written as zero.** Venus advances past them without
   writing. Ours is deterministic and does not leak stale bytes into shared
   memory. Same layout, different values in bytes nobody may read.
2. **A `u32` byte length in every command header** (`design.md` §6.2, R13).
3. **Length-prefixed, flat `pNext` links with a zero terminator**, from
   gfxstream rather than Venus (`design.md` D6). This is the deviation that
   makes a whole-struct byte comparison against Venus impossible.
4. **A length-prefixed reply stream** rather than a fixed mailbox.
