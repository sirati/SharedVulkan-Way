# How big the API actually is

Counted from the pinned registry, `vulkan-headers 1.4.350.0`, the same
`vk.xml` the dev shell exports as `$VK_XML`:

| entity     | count |
|------------|-------|
| commands   | 1885  |
| structs    | 1701  |
| unions     | 16    |
| enums      | 346   |
| handles    | 62    |
| extensions | 689   |

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

**Structs are the harder half.** Commands are a flat dispatch problem.
Structs are not: they nest, they chain through `pNext`, they contain pointers
to arrays whose lengths live in sibling fields, and some are read-write
(the callee fills them in). A generator that handles commands but treats
structs naively will produce something that appears to work and then corrupts
memory under a real workload. The `len=`, `optional=`, and `pNext` metadata
in the registry is what makes this tractable, and it is the reason to
generate from the XML rather than from C headers.
