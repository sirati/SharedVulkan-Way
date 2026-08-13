# How big the API actually is

Counted from the pinned registry, `vulkan-headers 1.4.350.0`, the same
`vk.xml` the dev shell exports as `$VK_XML`:

| entity           | count |
|------------------|-------|
| commands         | 747   |
| command aliases  | 105   |
| structs          | 1701  |
| unions           | 16    |
| handles          | 62    |
| enum types       | 404   |
| extensions       | 689   |

Counted by parsing the registry (`crates/vk-registry`), not by grepping it.
The first version of this table said 1885 commands, which was wrong: `grep -c
"<command"` also counts every `<command name="..."/>` *reference* inside the
`<require>` blocks of features and extensions, and there are far more
references than definitions. 747 definitions plus 105 aliases is the real
figure.

That the corrected number is smaller does not weaken the argument below — 747
commands and 1701 structs is still several times what sharedgl hand-wrote for
OpenGL — but a design should not rest on a number nobody checked.

## What follows from this

**Hand-written marshalling is not an option.** 1885 commands, each needing a
client stub, a wire encoding, a server decode and a dispatch, is somewhere
around five figures of mechanical Rust. Written by hand it would be wrong in
a hundred places and stale the day the registry moves. The wire format has to
be *generated* from `vk.xml`, and the generator is therefore the first real
component of this project rather than a convenience added later.

This is also why the dev shell pins `vk.xml` out of the nix store instead of
finding it on the machine: the generated protocol is a function of the
registry, so the registry has to be a pinned input, moving only when the
flake is deliberately bumped. Otherwise two developers generate two different
protocols and the incompatibility shows up as corrupted command streams
rather than as a build error.

**Not all 1885 are equal.** The counts above are the whole registry including
every extension; a useful subset is much smaller. But the subset is not ours
to choose freely — it is dictated by what Zink, DXVK and vkd3d-proton
actually call, since making those three work is the entire point. Their
required extension lists are what defines "done", and establishing them is
part of the research task rather than something to guess at here.

## How much of it we carry, and what is missing

Two commands answer that, and neither is prose:

```
target/debug/sv-codegen gap        # from the registry: what the profile omits
scripts/sv-cts.sh                  # from the suite: what a test actually asked for
```

The first, measured 2026-08-13 against the zink profile:

| | count |
|---|---|
| commands defined for `vulkan` | 740 |
| carried by the profile | 239 |
| not carried | 501 |
| …of those, unreachable by *any* profile | 23 |

"Unreachable" means defined but required by nothing the registry supports for
`vulkan` — a Vulkan SC command, or one behind a `disabled` extension. No profile
can reach them, so they are counted and not listed.

The 478 that remain are grouped by **the extension or core version that
introduces each**, because that is the unit a profile is edited in: nobody adds
`vkCmdTraceRaysKHR`, they add `VK_KHR_ray_tracing_pipeline`. The largest entries
are `VK_EXT_extended_dynamic_state3` (31), `VK_KHR_device_address_commands` (22),
`VK_KHR_acceleration_structure` (16), `VK_EXT_opacity_micromap` (14) and
`VK_KHR_video_queue` (12); `VK_BASE_VERSION_1_4` contributes 6, which is the
whole of what "core 1.0–1.3" costs.

**A count of missing commands is not a list of missing capability**, which is
why the second command exists. `scripts/sv-cts.sh` runs the conformance suite
against the forwarded ICD *and* against the same driver unforwarded, and reports
— in deqp's own words, restricted to tests the unforwarded control passes — why
the forwarded device declined. At a 1-in-20000 sample the top of that list is
`VK_EXT_shader_object` (12 tests), **the one synthesised queue family** (11:
eight `No matching queue found` and three `Exclusive compute queue not
supported`), `VK_EXT_graphics_pipeline_library` (7) and the ray-tracing family
(6). The second entry is not an extension and is not in the first table at all.
design.md §0.15.

**Structs are the harder half.** Commands are a flat dispatch problem.
Structs are not: they nest, they chain through `pNext`, they contain pointers
to arrays whose lengths live in sibling fields, and some are read-write
(the callee fills them in). A generator that handles commands but treats
structs naively will produce something that appears to work and then corrupts
memory under a real workload. The `len=`, `optional=`, and `pNext` metadata
in the registry is what makes this tractable, and it is the reason to
generate from the XML rather than from C headers.
