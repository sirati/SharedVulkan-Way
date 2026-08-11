# Venus / VK_EXT_command_serialization wire format — reference notes

Everything below is quoted verbatim from the files in this directory. Nothing is inferred
unless explicitly marked "NOT FOUND" / "inference".

## Provenance

The generated headers are **no longer in mesa `main`**: as of mesa 26.2 the venus-protocol
headers are pulled in as a meson subproject (`subprojects/venus-protocol.wrap` →
`https://gitlab.freedesktop.org/virgl/venus-protocol.git`, rev `e94b12f301b9eb27ebead757128a18420b4f7994`).
The venus-protocol repo checks in only the *generator* (`vn_protocol.py`) and the Mako
`templates/`; the `vn_protocol_*_*.h` are produced at build time.

| local file(s) | source |
|---|---|
| `vn_protocol_driver*.h` | mesa tag `mesa-26.2.0`, `src/virtio/venus-protocol/` (last mesa release that still vendors them) |
| `mesa_vn_cs.h`, `mesa_vn_cs.c`, `mesa_vn_ring.h`, `mesa_vn_ring.c`, `mesa_vn_instance.c`, `mesa_vn_common.h` | mesa tag `mesa-26.2.0`, `src/virtio/vulkan/` |
| `venus-protocol-templates/*`, `vn_protocol.py`, `VK_EXT_command_serialization.txt`, `venus-protocol-README` | `gitlab.freedesktop.org/virgl/venus-protocol` @ `main` |
| `vn_protocol_renderer_*.h` | `gitlab.freedesktop.org/virgl/virglrenderer` tag `virglrenderer-1.3.0`, `src/venus/venus-protocol/` (virglrenderer `main` also uses the subproject now) |
| `virgl_vkr_cs.[ch]`, `virgl_vkr_ring.[ch]` | virglrenderer `main`, `src/venus/` |

The `VK_EXT_command_serialization` spec is `docs/VK_EXT_command_serialization.txt` in
venus-protocol (135 lines) — saved here as `VK_EXT_command_serialization.txt`. It is a
prose summary; the generator + templates are the normative definition.

---

## A. Primitive encoding

### The two chokepoints

`vn_protocol_driver_cs.h` (whole functional content of the file):

```c
static inline void
vn_encode(struct vn_cs_encoder *enc, size_t size, const void *data, size_t data_size)
{
   assert(size % 4 == 0);
   /* TODO check if the generated code is optimal */
   vn_cs_encoder_write(enc, size, data, data_size);
}

static inline void
vn_decode(struct vn_cs_decoder *dec, size_t size, void *data, size_t data_size)
{
   assert(size % 4 == 0);
   vn_cs_decoder_read(dec, size, data, data_size);
}
```

`mesa_vn_cs.h`:

```c
static inline void
vn_cs_encoder_write(struct vn_cs_encoder *enc,
                    size_t size,
                    const void *val,
                    size_t val_size)
{
   assert(val_size <= size);
   assert(size <= enc->end - enc->cur);

   /* we should not rely on the compiler to optimize away memcpy... */
   memcpy(enc->cur, val, val_size);
   enc->cur += size;
}
```

**Alignment / granularity: 4 bytes.** There is *no* `#define VN_CS_ENCODER_ALIGN`-style
macro anywhere. The granularity is expressed purely as `assert(size % 4 == 0)` in
`vn_encode`/`vn_decode` (both driver and renderer side — `vn_protocol_renderer_cs.h` has the
identical assert). The only `VN_CS_ENCODER_*` macros in `mesa_vn_cs.h` are
`VN_CS_ENCODER_BUFFER_INITIALIZER`, `VN_CS_ENCODER_INITIALIZER[_LOCAL]` and
`enum vn_cs_encoder_storage_type` — none of them concern alignment.

Note the asymmetry in `vn_cs_encoder_write`: it copies `val_size` bytes but advances
`size` bytes. **The pad bytes are never written** — they are whatever was already in the
buffer (uninitialized shmem / stale data). The spec text says exactly this: *"it is
serialized in little endian byte order, and is padded to 32-bit (with garbage?)"*.
A decoder must therefore ignore pad bytes; an encoder is free to zero them.

Also: nothing byte-swaps. Everything is a raw host-endian `memcpy`; Venus is
little-endian-only in practice.

### The generator rule (`venus-protocol-templates/types_scalar.h`)

```mako
<%def name="vn_sizeof_scalar_body(ty)">\
% if ty.category == ty.DEFAULT:
<% ty_size = GEN.PRIMITIVE_TYPES[ty.name] %>\
    assert(sizeof(*val) == ${ty_size});
    return ${ty_size if ty_size >= 4 else 4};
% elif ty.category == ty.BASETYPE:
    return vn_sizeof_${ty.typedef.name}(val);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 32:
    assert(sizeof(*val) == sizeof(int32_t));
    return vn_sizeof_int32_t((const int32_t *)val);
% elif ty.category == ty.ENUM and ty.enums.bitwidth == 64:
    assert(sizeof(*val) == sizeof(uint64_t));
    return vn_sizeof_uint64_t((const uint64_t *)val);
% endif
</%def>

<%def name="vn_encode_scalar_body(ty)">\
% if ty.category == ty.DEFAULT:
<% ty_size = GEN.PRIMITIVE_TYPES[ty.name] %>\
%   if ty_size >= 4:
    vn_encode(enc, ${ty_size}, val, sizeof(*val));
%   else:
    vn_encode(enc, 4, val, sizeof(*val));
%   endif
...
```

`vn_protocol.py`:

```python
    PRIMITIVE_TYPES = {
        'float': 4,
        'double': 8,
        'uint8_t': 1,
        'uint16_t': 2,
        'uint32_t': 4,
        'uint64_t': 8,
        'int32_t': 4,
        'int64_t': 8,
    }
```

`int8_t` and `int16_t` are **not** in `PRIMITIVE_TYPES` at all — they are not serializable
by Venus and do not occur in Vulkan's serializable surface.

### Concrete generated code

```c
static inline size_t vn_sizeof_uint64_t(const uint64_t *val) { assert(sizeof(*val) == 8); return 8; }
static inline void   vn_encode_uint64_t(struct vn_cs_encoder *enc, const uint64_t *val) { vn_encode(enc, 8, val, sizeof(*val)); }

static inline size_t vn_sizeof_uint32_t(const uint32_t *val) { assert(sizeof(*val) == 4); return 4; }
static inline void   vn_encode_uint32_t(struct vn_cs_encoder *enc, const uint32_t *val) { vn_encode(enc, 4, val, sizeof(*val)); }

static inline size_t vn_sizeof_int32_t(const int32_t *val)   { assert(sizeof(*val) == 4); return 4; }
static inline void   vn_encode_int32_t(struct vn_cs_encoder *enc, const int32_t *val)   { vn_encode(enc, 4, val, sizeof(*val)); }

static inline size_t vn_sizeof_float(const float *val)       { assert(sizeof(*val) == 4); return 4; }
static inline void   vn_encode_float(struct vn_cs_encoder *enc, const float *val)       { vn_encode(enc, 4, val, sizeof(*val)); }

static inline size_t vn_sizeof_uint8_t(const uint8_t *val)   { assert(sizeof(*val) == 1); return 4; }
static inline void   vn_encode_uint8_t(struct vn_cs_encoder *enc, const uint8_t *val)   { vn_encode(enc, 4, val, sizeof(*val)); }

static inline size_t vn_sizeof_uint16_t(const uint16_t *val) { assert(sizeof(*val) == 2); return 4; }
static inline void   vn_encode_uint16_t(struct vn_cs_encoder *enc, const uint16_t *val) { vn_encode(enc, 4, val, sizeof(*val)); }
```

`size_t` is widened to `uint64_t`:

```c
static inline size_t
vn_sizeof_size_t(const size_t *val)
{
    return vn_sizeof_uint64_t(&(uint64_t){ *val });
}

static inline void
vn_encode_size_t(struct vn_cs_encoder *enc, const size_t *val)
{
    const uint64_t tmp = *val;
    vn_encode_uint64_t(enc, &tmp);
}

static inline void
vn_decode_size_t(struct vn_cs_decoder *dec, size_t *val)
{
    uint64_t tmp;
    vn_decode_uint64_t(dec, &tmp);
    *val = tmp;
}
```

Typedefs:

```c
/* typedef uint32_t VkBool32 */
static inline size_t vn_sizeof_VkBool32(const VkBool32 *val) { return vn_sizeof_uint32_t(val); }
static inline void   vn_encode_VkBool32(struct vn_cs_encoder *enc, const VkBool32 *val) { vn_encode_uint32_t(enc, val); }

/* typedef uint32_t VkFlags */
static inline void   vn_encode_VkFlags(struct vn_cs_encoder *enc, const VkFlags *val) { vn_encode_uint32_t(enc, val); }

/* typedef uint64_t VkFlags64 */
static inline void   vn_encode_VkFlags64(struct vn_cs_encoder *enc, const VkFlags64 *val) { vn_encode_uint64_t(enc, val); }

/* typedef uint64_t VkDeviceSize / VkDeviceAddress */  -> vn_encode_uint64_t
/* typedef uint32_t VkSampleMask */                    -> vn_encode_uint32_t
```

Enums (32-bit) are cast to `int32_t`:

```c
static inline size_t
vn_sizeof_VkStructureType(const VkStructureType *val)
{
    assert(sizeof(*val) == sizeof(int32_t));
    return vn_sizeof_int32_t((const int32_t *)val);
}
static inline void
vn_encode_VkStructureType(struct vn_cs_encoder *enc, const VkStructureType *val)
{
    vn_encode_int32_t(enc, (const int32_t *)val);
}
```

`VkResult`, `VkSharingMode`, `VkCommandTypeEXT`, … all follow this. 64-bit-bitwidth enums
(e.g. `VkPipelineStageFlagBits2`) go through `vn_encode_uint64_t` per the template.

### Summary table

| C type | wire bytes | notes |
|---|---|---|
| `uint8_t` / `int8_t`-shaped | 4 | 1 byte payload + 3 pad (pad not written) |
| `uint16_t` | 4 | 2 byte payload + 2 pad |
| `uint32_t`, `int32_t` | 4 | exact |
| `uint64_t`, `int64_t` | 8 | exact |
| `float` | 4 | raw IEEE-754 bits |
| `double` | 8 | raw bits (see caveat below) |
| `size_t` | 8 | widened/narrowed via `uint64_t` |
| `VkBool32` | 4 | as `uint32_t` |
| `VkFlags` | 4 | as `uint32_t` |
| `VkFlags64` | 8 | as `uint64_t` |
| 32-bit enum / bitmask-enum | 4 | cast to `int32_t` |
| 64-bit enum | 8 | cast to `uint64_t` |
| handle (any) | 8 | `uint64_t` object id |
| `char` | only ever as `char[]`, 1 byte/elem, whole array padded to 4 |
| `int8_t`, `int16_t` | **not serializable** — absent from `PRIMITIVE_TYPES` |

Caveat, verified by grep over the fetched headers: **no `vn_encode_double`, no
`vn_encode_int64_t`, no `vn_encode_int8_t`/`int16_t`, and no `vn_*_uint16_t_array` are
actually emitted** in the mesa-26.2.0 generated protocol. `double`/`int64_t` sizes above
come from `PRIMITIVE_TYPES` + `types_scalar.h`, not from an emitted instance.
`vn_sizeof_uint16_t`/`vn_encode_uint16_t` (scalar) *are* emitted.

---

## B. Array encoding

Two distinct shapes. Which one is used depends on whether the array is behind a pointer
(dynamic, length-carrying) or is a fixed-size C array member.

### B.1 The element encoders

`uint32_t` array — packed, no per-element padding, no trailing padding (element size ≥ 4):

```c
static inline size_t
vn_sizeof_uint32_t_array(const uint32_t *val, uint32_t count)
{
    assert(sizeof(*val) == 4);
    const size_t size = sizeof(*val) * count;
    assert(size >= count);
    return size;
}

static inline void
vn_encode_uint32_t_array(struct vn_cs_encoder *enc, const uint32_t *val, uint32_t count)
{
    const size_t size = sizeof(*val) * count;
    assert(size >= count);
    vn_encode(enc, size, val, size);
}
```

`uint8_t` array — packed 1 byte/element, **padded once at the end** to a multiple of 4:

```c
static inline size_t
vn_sizeof_uint8_t_array(const uint8_t *val, uint32_t count)
{
    assert(sizeof(*val) == 1);
    const size_t size = sizeof(*val) * count;
    assert(size >= count);
    return (size + 3) & ~3;
}

static inline void
vn_encode_uint8_t_array(struct vn_cs_encoder *enc, const uint8_t *val, uint32_t count)
{
    const size_t size = sizeof(*val) * count;
    assert(size >= count);
    vn_encode(enc, (size + 3) & ~3, val, size);
}
```

This matches the spec: *"Padding to 32-bit, if needed, happens after all values are
serialized, not after each individual value."*

Opaque blob (`void*` with a byte size) — identical rule:

```c
static inline size_t
vn_sizeof_blob_array(const void *val, size_t size)
{
    return (size + 3) & ~3;
}

static inline void
vn_encode_blob_array(struct vn_cs_encoder *enc, const void *val, size_t size)
{
    vn_encode(enc, (size + 3) & ~3, val, size);
}
```

### B.2 Arrays of structs / arrays of handles — element-by-element loop

There is **no** `vn_encode_VkFoo_array`. Structs and handles are emitted with an explicit
loop, so each element is individually encoded (and each element is already a multiple of 4
bytes long, so no padding question arises).

Array of handles (`vn_protocol_driver_queue.h`, `VkSubmitInfo`):

```c
    vn_encode_uint32_t(enc, &val->waitSemaphoreCount);
    if (val->pWaitSemaphores) {
        vn_encode_array_size(enc, val->waitSemaphoreCount);
        for (uint32_t i = 0; i < val->waitSemaphoreCount; i++)
            vn_encode_VkSemaphore(enc, &val->pWaitSemaphores[i]);
    } else {
        vn_encode_array_size(enc, 0);
    }
```

Array of structs (`vn_protocol_driver_queue.h`, `vn_encode_vkQueueSubmit`):

```c
    vn_encode_VkQueue(enc, &queue);
    vn_encode_uint32_t(enc, &submitCount);
    if (pSubmits) {
        vn_encode_array_size(enc, submitCount);
        for (uint32_t i = 0; i < submitCount; i++)
            vn_encode_VkSubmitInfo(enc, &pSubmits[i]);
    } else {
        vn_encode_array_size(enc, 0);
    }
```

Array of `uint32_t` (`vn_encode_VkBufferCreateInfo_self`):

```c
    vn_encode_uint32_t(enc, &val->queueFamilyIndexCount);
    if (val->sharingMode == VK_SHARING_MODE_CONCURRENT) {
        vn_encode_array_size(enc, val->queueFamilyIndexCount);
        vn_encode_uint32_t_array(enc, val->pQueueFamilyIndices, val->queueFamilyIndexCount);
    } else {
        vn_encode_array_size(enc, 0);
    }
```

### B.3 Answers

- **Is the count written?** Yes, as a **`uint64_t`** (`vn_encode_array_size`), immediately
  before the elements. Note this is *in addition to* the Vulkan `xxxCount` struct member,
  which is itself encoded as a plain `uint32_t` where it exists. So `pQueueFamilyIndices`
  costs `4` (queueFamilyIndexCount) `+ 8` (array_size) `+ 4*n`.
- **Where does padding go?** Only after the whole array, and only for element widths < 4
  (`uint8_t`, `char`, blobs). Element widths ≥ 4 are packed with no padding at all.
- **Fixed-size C array members** (e.g. `deviceName[VK_MAX_PHYSICAL_DEVICE_NAME_SIZE]`,
  `pipelineCacheUUID[VK_UUID_SIZE]`) are *also* prefixed with an `array_size`, using the
  constant as the size:

```c
    size += vn_sizeof_array_size(VK_MAX_PHYSICAL_DEVICE_NAME_SIZE);
    size += vn_sizeof_char_array(val->deviceName, VK_MAX_PHYSICAL_DEVICE_NAME_SIZE);
    size += vn_sizeof_array_size(VK_UUID_SIZE);
    size += vn_sizeof_uint8_t_array(val->pipelineCacheUUID, VK_UUID_SIZE);
```

  and on decode the size is *checked* against the constant:

```c
    {
        const size_t array_size = vn_decode_array_size(dec, VK_MAX_PHYSICAL_DEVICE_NAME_SIZE);
        vn_decode_char_array(dec, val->deviceName, array_size);
    }
```

---

## C. `vn_encode_array_size` / `vn_sizeof_array_size`

Exact definition (`vn_protocol_driver_types.h`):

```c
/* array size (uint64_t) */

static inline size_t
vn_sizeof_array_size(uint64_t size)
{
    return vn_sizeof_uint64_t(&size);
}

static inline void
vn_encode_array_size(struct vn_cs_encoder *enc, uint64_t size)
{
    vn_encode_uint64_t(enc, &size);
}

static inline uint64_t
vn_decode_array_size(struct vn_cs_decoder *dec, uint64_t expected_size)
{
    uint64_t size;
    vn_decode_uint64_t(dec, &size);
    if (size != expected_size) {
        vn_cs_decoder_set_fatal(dec);
        size = 0;
    }
    return size;
}

static inline uint64_t
vn_decode_array_size_unchecked(struct vn_cs_decoder *dec)
{
    uint64_t size;
    vn_decode_uint64_t(dec, &size);
    return size;
}

static inline uint64_t
vn_peek_array_size(struct vn_cs_decoder *dec)
{
    uint64_t size;
    vn_cs_decoder_peek(dec, sizeof(size), &size, sizeof(size));
    return size;
}
```

- **8 bytes, `uint64_t`, always.**
- **NULL array pointer ⇒ `vn_encode_array_size(enc, 0)` and nothing else.** The elements are
  omitted entirely. See every `} else { vn_encode_array_size(enc, 0); }` branch above.
- Consequence, stated in the spec: *"A pointer serialization with zero value must be
  deserialized to `NULL`. It is impossible to distinguish `NULL` and a non-`NULL` pointer
  with zero value after serialization."* i.e. a non-NULL pointer with count 0 and a NULL
  pointer are the same on the wire.
- `vn_decode_array_size` is a **strict equality check**, not a clamp; a mismatch calls
  `vn_cs_decoder_set_fatal`, which on the driver side is `abort()`:

```c
static inline void
vn_cs_decoder_set_fatal(const struct vn_cs_decoder *dec)
{
   abort();
}
```

---

## D. Simple (single-value) pointers

```c
/* non-array pointer */

static inline size_t
vn_sizeof_simple_pointer(const void *val)
{
    return vn_sizeof_array_size(val ? 1 : 0);
}

static inline bool
vn_encode_simple_pointer(struct vn_cs_encoder *enc, const void *val)
{
    vn_encode_array_size(enc, val ? 1 : 0);
    return val;
}

static inline bool
vn_decode_simple_pointer(struct vn_cs_decoder *dec)
{
    return vn_decode_array_size_unchecked(dec);
}
```

**The presence flag is 8 bytes** (`uint64_t` 0 or 1) — it is literally an array size of
0 or 1. It returns the pointer so the caller can chain:

```c
    if (vn_encode_simple_pointer(enc, pCreateInfo))
        vn_encode_VkBufferCreateInfo(enc, pCreateInfo);
```

---

## E. Strings (`const char*`, `len="null-terminated"`)

```c
/* string */

static inline size_t
vn_sizeof_char_array(const char *val, size_t size)
{
    return vn_sizeof_blob_array(val, size);
}

static inline void
vn_encode_char_array(struct vn_cs_encoder *enc, const char *val, size_t size)
{
    assert(size && strlen(val) < size);
    vn_encode_blob_array(enc, val, size);
}

static inline void
vn_decode_char_array(struct vn_cs_decoder *dec, char *val, size_t size)
{
    vn_decode_blob_array(dec, val, size);
    if (size)
        val[size - 1] = '\0';
    else
        vn_cs_decoder_set_fatal(dec);
}
```

Use site (`vn_encode_VkApplicationInfo_self`):

```c
    /* skip val->{sType,pNext} */
    if (val->pApplicationName) {
        const size_t string_size = strlen(val->pApplicationName) + 1;
        vn_encode_array_size(enc, string_size);
        vn_encode_char_array(enc, val->pApplicationName, string_size);
    } else {
        vn_encode_array_size(enc, 0);
    }
```

- Length **is** written: `uint64_t` array size = `strlen + 1`.
- The **NUL is included** in both the length and the bytes.
- The character bytes are then padded up to a multiple of 4 (`vn_encode_blob_array` →
  `(size + 3) & ~3`).
- NULL `const char*` ⇒ a single `uint64_t` 0 and nothing else.
- Renderer side allocates from the count and force-terminates:

```c
    if (vn_peek_array_size(dec)) {
        const size_t string_size = vn_decode_array_size_unchecked(dec);
        args->pLayerName = vn_cs_decoder_alloc_temp_array(dec, sizeof(*args->pLayerName), string_size);
        if (!args->pLayerName) return;
        vn_decode_char_array(dec, (char *)args->pLayerName, string_size);
    } else {
        vn_decode_array_size_unchecked(dec);
        args->pLayerName = NULL;
    }
```

  (note `vn_decode_array_size_unchecked` — the renderer trusts the guest's length here, then
  clobbers the last byte with `'\0'` in `vn_decode_char_array`.)

---

## F. Arrays of strings (`const char* const*`)

`vn_encode_VkInstanceCreateInfo_self`:

```c
    vn_encode_uint32_t(enc, &val->enabledLayerCount);
    if (val->ppEnabledLayerNames) {
        vn_encode_array_size(enc, val->enabledLayerCount);
        for (uint32_t i = 0; i < val->enabledLayerCount; i++) {
            const size_t string_size = strlen(val->ppEnabledLayerNames[i]) + 1;
            vn_encode_array_size(enc, string_size);
            vn_encode_char_array(enc, val->ppEnabledLayerNames[i], string_size);
        }
    } else {
        vn_encode_array_size(enc, 0);
    }
    vn_encode_uint32_t(enc, &val->enabledExtensionCount);
    if (val->ppEnabledExtensionNames) {
        vn_encode_array_size(enc, val->enabledExtensionCount);
        for (uint32_t i = 0; i < val->enabledExtensionCount; i++) {
            const size_t string_size = strlen(val->ppEnabledExtensionNames[i]) + 1;
            vn_encode_array_size(enc, string_size);
            vn_encode_char_array(enc, val->ppEnabledExtensionNames[i], string_size);
        }
    } else {
        vn_encode_array_size(enc, 0);
    }
```

So: `uint32_t count` (the struct member), then `uint64_t outer_count`, then for each
string `uint64_t strlen+1` + padded bytes. Two nested length prefixes, both `uint64_t`.
Note each string is *individually* padded to 4 (`vn_encode_char_array` per element), so
successive strings always start 4-byte aligned.

---

## G. Handles

Every handle, dispatchable or not, is one `uint64_t` object id.

```c
/* VK_DEFINE_NON_DISPATCHABLE_HANDLE(VkBuffer) */

static inline size_t
vn_sizeof_VkBuffer(const VkBuffer *val)
{
    return sizeof(uint64_t);
}

static inline void
vn_encode_VkBuffer(struct vn_cs_encoder *enc, const VkBuffer *val)
{
    const uint64_t id = vn_cs_handle_load_id((const void **)val, VK_OBJECT_TYPE_BUFFER);
    vn_encode_uint64_t(enc, &id);
}

static inline void
vn_decode_VkBuffer(struct vn_cs_decoder *dec, VkBuffer *val)
{
    uint64_t id;
    vn_decode_uint64_t(dec, &id);
    vn_cs_handle_store_id((void **)val, id, VK_OBJECT_TYPE_BUFFER);
}
```

Dispatchable handles are byte-for-byte identical in shape:

```c
static inline size_t vn_sizeof_VkDevice(const VkDevice *val) { return sizeof(uint64_t); }
static inline void
vn_encode_VkDevice(struct vn_cs_encoder *enc, const VkDevice *val)
{
    const uint64_t id = vn_cs_handle_load_id((const void **)val, VK_OBJECT_TYPE_DEVICE);
    vn_encode_uint64_t(enc, &id);
}
```

**`VK_NULL_HANDLE` ⇒ object id 0**, from `mesa_vn_cs.h`:

```c
static inline vn_object_id
vn_cs_handle_load_id(const void **handle, VkObjectType type)
{
   return *handle ? vn_object_get_id(*handle, type) : 0;
}
```

and the spec: *"A `NULL` or `VK_NULL_HANDLE` object handle has object id 0."*

The id is a driver-allocated 64-bit token (not a host pointer). The renderer maps id →
object via `vkr_cs_decoder_lookup_object`. Renderer-side `vn_cs_get_object_handle` returns
`obj ? obj->handle.u64 : 0`.

---

## H. pNext

`vn_encode_VkBufferCreateInfo_pnext`, in full (`vn_protocol_driver_buffer.h`):

```c
static inline void
vn_encode_VkBufferCreateInfo_pnext(struct vn_cs_encoder *enc, const void *val)
{
    const VkBaseInStructure *pnext = val;

    while (pnext) {
        switch ((int32_t)pnext->sType) {
        case VK_STRUCTURE_TYPE_BUFFER_USAGE_FLAGS_2_CREATE_INFO:
            if (!vn_cs_renderer_protocol_has_extension(471 /* VK_KHR_maintenance5 */))
                break;
            vn_encode_simple_pointer(enc, pnext);
            vn_encode_VkStructureType(enc, &pnext->sType);
            vn_encode_VkBufferCreateInfo_pnext(enc, ((const VkBufferUsageFlags2CreateInfo *)pnext)->pNext);
            vn_encode_VkBufferUsageFlags2CreateInfo_self(enc, (const VkBufferUsageFlags2CreateInfo *)pnext);
            return;
        case VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO:
            vn_encode_simple_pointer(enc, pnext);
            vn_encode_VkStructureType(enc, &pnext->sType);
            vn_encode_VkBufferCreateInfo_pnext(enc, ((const VkExternalMemoryBufferCreateInfo *)pnext)->pNext);
            vn_encode_VkExternalMemoryBufferCreateInfo_self(enc, (const VkExternalMemoryBufferCreateInfo *)pnext);
            return;
        case VK_STRUCTURE_TYPE_BUFFER_OPAQUE_CAPTURE_ADDRESS_CREATE_INFO:
            vn_encode_simple_pointer(enc, pnext);
            vn_encode_VkStructureType(enc, &pnext->sType);
            vn_encode_VkBufferCreateInfo_pnext(enc, ((const VkBufferOpaqueCaptureAddressCreateInfo *)pnext)->pNext);
            vn_encode_VkBufferOpaqueCaptureAddressCreateInfo_self(enc, (const VkBufferOpaqueCaptureAddressCreateInfo *)pnext);
            return;
        case VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_CREATE_INFO_EXT:
            if (!vn_cs_renderer_protocol_has_extension(245 /* VK_EXT_buffer_device_address */))
                break;
            vn_encode_simple_pointer(enc, pnext);
            vn_encode_VkStructureType(enc, &pnext->sType);
            vn_encode_VkBufferCreateInfo_pnext(enc, ((const VkBufferDeviceAddressCreateInfoEXT *)pnext)->pNext);
            vn_encode_VkBufferDeviceAddressCreateInfoEXT_self(enc, (const VkBufferDeviceAddressCreateInfoEXT *)pnext);
            return;
        default:
            /* ignore unknown/unsupported struct */
            break;
        }
        pnext = pnext->pNext;
    }

    vn_encode_simple_pointer(enc, NULL);
}
```

and the matching sizeof:

```c
static inline size_t
vn_sizeof_VkBufferCreateInfo_pnext(const void *val)
{
    const VkBaseInStructure *pnext = val;
    size_t size = 0;

    while (pnext) {
        switch ((int32_t)pnext->sType) {
        case VK_STRUCTURE_TYPE_BUFFER_USAGE_FLAGS_2_CREATE_INFO:
            if (!vn_cs_renderer_protocol_has_extension(471 /* VK_KHR_maintenance5 */))
                break;
            size += vn_sizeof_simple_pointer(pnext);
            size += vn_sizeof_VkStructureType(&pnext->sType);
            size += vn_sizeof_VkBufferCreateInfo_pnext(((const VkBufferUsageFlags2CreateInfo *)pnext)->pNext);
            size += vn_sizeof_VkBufferUsageFlags2CreateInfo_self((const VkBufferUsageFlags2CreateInfo *)pnext);
            return size;
        ...
        default:
            /* ignore unknown/unsupported struct */
            break;
        }
        pnext = pnext->pNext;
    }

    return vn_sizeof_simple_pointer(NULL);
}
```

Key facts:

1. The chain is **filtered**: only sTypes the generator knows for *this parent struct* are
   serialized, and only if the renderer advertises the extension
   (`vn_cs_renderer_protocol_has_extension(<ext number>)`). Unknown/unsupported links are
   silently skipped (`pnext = pnext->pNext` and keep looping).
2. Per emitted link, the wire layout is:
   `uint64_t 1` (simple_pointer present) · `int32_t sType` · `<recursive pnext of that link>` · `<that link's _self body>`
   Note the ordering: **sType, then the rest of the chain, then this link's own fields** —
   i.e. the chain is encoded *nested/recursive*, not as a flat list. Decoding is naturally
   recursive too.
3. The **terminator is `vn_encode_simple_pointer(enc, NULL)` = `uint64_t` 0**, 8 bytes.
4. The `_pnext` recursion uses *the parent struct's* whitelist function
   (`vn_encode_VkBufferCreateInfo_pnext` calls itself), so the set of acceptable sTypes is
   per-parent-struct, and a chain can only ever be as deep as the whitelist allows.
5. A struct with no known extenders emits the terminator unconditionally:

```c
static inline void
vn_encode_VkExternalMemoryBufferCreateInfo_pnext(struct vn_cs_encoder *enc, const void *val)
{
    /* no known/supported struct */
    vn_encode_simple_pointer(enc, NULL);
}
```

6. Full struct = `sType` · `_pnext` · `_self`, with sType re-emitted from a literal:

```c
static inline void
vn_encode_VkBufferCreateInfo(struct vn_cs_encoder *enc, const VkBufferCreateInfo *val)
{
    assert(val->sType == VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO);
    vn_encode_VkStructureType(enc, &(VkStructureType){ VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO });
    vn_encode_VkBufferCreateInfo_pnext(enc, val->pNext);
    vn_encode_VkBufferCreateInfo_self(enc, val);
}
```

---

## I. The command header

```c
static inline size_t vn_sizeof_vkCreateBuffer(VkDevice device, const VkBufferCreateInfo* pCreateInfo, const VkAllocationCallbacks* pAllocator, VkBuffer* pBuffer)
{
    const VkCommandTypeEXT cmd_type = VK_COMMAND_TYPE_vkCreateBuffer_EXT;
    const VkFlags cmd_flags = 0;
    size_t cmd_size = vn_sizeof_VkCommandTypeEXT(&cmd_type) + vn_sizeof_VkFlags(&cmd_flags);

    cmd_size += vn_sizeof_VkDevice(&device);
    cmd_size += vn_sizeof_simple_pointer(pCreateInfo);
    if (pCreateInfo)
        cmd_size += vn_sizeof_VkBufferCreateInfo(pCreateInfo);
    cmd_size += vn_sizeof_simple_pointer(pAllocator);
    if (pAllocator)
        assert(false);
    cmd_size += vn_sizeof_simple_pointer(pBuffer);
    if (pBuffer)
        cmd_size += vn_sizeof_VkBuffer(pBuffer);

    return cmd_size;
}

static inline void vn_encode_vkCreateBuffer(struct vn_cs_encoder *enc, VkCommandFlagsEXT cmd_flags, VkDevice device, const VkBufferCreateInfo* pCreateInfo, const VkAllocationCallbacks* pAllocator, VkBuffer* pBuffer)
{
    const VkCommandTypeEXT cmd_type = VK_COMMAND_TYPE_vkCreateBuffer_EXT;

    vn_encode_VkCommandTypeEXT(enc, &cmd_type);
    vn_encode_VkFlags(enc, &cmd_flags);

    vn_encode_VkDevice(enc, &device);
    if (vn_encode_simple_pointer(enc, pCreateInfo))
        vn_encode_VkBufferCreateInfo(enc, pCreateInfo);
    if (vn_encode_simple_pointer(enc, pAllocator))
        assert(false);
    if (vn_encode_simple_pointer(enc, pBuffer))
        vn_encode_VkBuffer(enc, pBuffer);
}
```

Header = exactly two dwords, nothing else:

| DW | width | contents |
|---|---|---|
| 0 | 4 | `VkCommandTypeEXT` as `int32_t` (`vkCreateBuffer` = **50**; `vkCreateInstance` = 0) |
| 1 | 4 | `VkCommandFlagsEXT` as `uint32_t` — only bit defined: `VK_COMMAND_GENERATE_REPLY_BIT_EXT = 0x1` |

No length field, no sequence number, no magic. From the spec: *"The length of a command is
implicitly defined by the command type and not explicitly serialized. It is impossible to
skip an unrecognized command type during deserialization."*

Note `vn_sizeof_*` hard-codes `cmd_flags = 0` — that's fine because `VkFlags` is a fixed
4 bytes regardless of value.

`VkCommandTypeEXT` values are a dense enum in `vn_protocol_driver_defines.h`
(`VK_COMMAND_TYPE_vkCreateInstance_EXT = 0` … `VK_COMMAND_TYPE_vkCreateBuffer_EXT = 50` …).
The numbering is protocol ABI and is generated from vk.xml ordering, so it must be copied
verbatim from `vn_protocol_driver_defines.h`, not re-derived.

Also relevant: `#define VN_SUBMIT_LOCAL_CMD_SIZE 256` — the driver encodes into a 256-byte
stack buffer when `vn_sizeof_*` fits, else mallocs:

```c
static inline void vn_submit_vkCreateBuffer(struct vn_ring *vn_ring, VkCommandFlagsEXT cmd_flags, VkDevice device, const VkBufferCreateInfo* pCreateInfo, const VkAllocationCallbacks* pAllocator, VkBuffer* pBuffer, struct vn_ring_submit_command *submit)
{
    uint8_t local_cmd_data[VN_SUBMIT_LOCAL_CMD_SIZE];
    void *cmd_data = local_cmd_data;
    size_t cmd_size = vn_sizeof_vkCreateBuffer(device, pCreateInfo, pAllocator, pBuffer);
    if (cmd_size > sizeof(local_cmd_data)) {
        cmd_data = malloc(cmd_size);
        if (!cmd_data)
            cmd_size = 0;
    }
    const size_t reply_size = cmd_flags & VK_COMMAND_GENERATE_REPLY_BIT_EXT ? vn_sizeof_vkCreateBuffer_reply(device, pCreateInfo, pAllocator, pBuffer) : 0;

    struct vn_cs_encoder *enc = vn_ring_submit_command_init(vn_ring, submit, cmd_data, cmd_size, reply_size);
    if (cmd_size) {
        vn_encode_vkCreateBuffer(enc, cmd_flags, device, pCreateInfo, pAllocator, pBuffer);
        vn_ring_submit_command(vn_ring, submit);
        if (cmd_data != local_cmd_data)
            free(cmd_data);
    }
}
```

This is why `vn_sizeof_*` must be *exact*, not an upper bound — the encoder buffer is sized
from it and `vn_cs_encoder_write` asserts `size <= enc->end - enc->cur`.

---

## J. The reply

```c
static inline size_t vn_sizeof_vkCreateBuffer_reply(VkDevice device, const VkBufferCreateInfo* pCreateInfo, const VkAllocationCallbacks* pAllocator, VkBuffer* pBuffer)
{
    const VkCommandTypeEXT cmd_type = VK_COMMAND_TYPE_vkCreateBuffer_EXT;
    size_t cmd_size = vn_sizeof_VkCommandTypeEXT(&cmd_type);

    VkResult ret;
    cmd_size += vn_sizeof_VkResult(&ret);
    /* skip device */
    /* skip pCreateInfo */
    /* skip pAllocator */
    cmd_size += vn_sizeof_simple_pointer(pBuffer);
    if (pBuffer)
        cmd_size += vn_sizeof_VkBuffer(pBuffer);

    return cmd_size;
}

static inline VkResult vn_decode_vkCreateBuffer_reply(struct vn_cs_decoder *dec, VkDevice device, const VkBufferCreateInfo* pCreateInfo, const VkAllocationCallbacks* pAllocator, VkBuffer* pBuffer)
{
    VkCommandTypeEXT command_type;
    vn_decode_VkCommandTypeEXT(dec, &command_type);
    assert(command_type == VK_COMMAND_TYPE_vkCreateBuffer_EXT);

    VkResult ret;
    vn_decode_VkResult(dec, &ret);
    /* skip device */
    /* skip pCreateInfo */
    /* skip pAllocator */
    if (vn_decode_simple_pointer(dec)) {
        vn_decode_VkBuffer(dec, pBuffer);
    } else {
        pBuffer = NULL;
    }

    return ret;
}
```

Renderer side (`vn_protocol_renderer_buffer.h`):

```c
static inline void vn_encode_vkCreateBuffer_reply(struct vn_cs_encoder *enc, const struct vn_command_vkCreateBuffer *args)
{
    vn_encode_VkCommandTypeEXT(enc, &(VkCommandTypeEXT){VK_COMMAND_TYPE_vkCreateBuffer_EXT});

    vn_encode_VkResult(enc, &args->ret);
    /* skip args->device */
    /* skip args->pCreateInfo */
    /* skip args->pAllocator */
    if (vn_encode_simple_pointer(enc, args->pBuffer))
        vn_encode_VkBuffer(enc, args->pBuffer);
}
```

Reply layout:

| DW | width | contents |
|---|---|---|
| 0 | 4 | `VkCommandTypeEXT` (echoed, **no** flags dword) |
| 1 | 4 | return value (`VkResult` as `int32_t`), only if the command returns something |
| 2… | | non-`const` pointer/out parameters, in declaration order |

`const` in-params are skipped entirely. There is no length prefix and no error framing
other than `VkResult`.

### How the reply stream is framed (transport level)

The reply is **not** in the same stream. `vn_ring.c`:

```c
static inline void
vn_ring_set_reply_shmem_locked(struct vn_ring *ring,
                               struct vn_renderer_shmem *shmem,
                               size_t offset,
                               size_t size)
{

   uint32_t set_reply_command_stream_data[16];
   struct vn_cs_encoder local_enc = VN_CS_ENCODER_INITIALIZER_LOCAL(
      set_reply_command_stream_data, sizeof(set_reply_command_stream_data));
   const struct VkCommandStreamDescriptionMESA stream = {
      .resourceId = shmem->res_id,
      .offset = offset,
      .size = size,
   };
   vn_encode_vkSetReplyCommandStreamMESA(&local_enc, 0, &stream);
   vn_cs_encoder_commit(&local_enc);
   vn_ring_submit_locked(ring, &local_enc, NULL, NULL);
}

void
vn_ring_submit_command(struct vn_ring *ring,
                       struct vn_ring_submit_command *submit)
{
   assert(!vn_cs_encoder_is_empty(&submit->command));

   vn_cs_encoder_commit(&submit->command);

   size_t reply_offset = 0;
   if (submit->reply_size) {
      submit->reply_shmem = vn_instance_reply_shmem_alloc(
         ring->instance, submit->reply_size, &reply_offset);
      ...
   }

   mtx_lock(&ring->mutex);
   if (submit->reply_size) {
      vn_ring_set_reply_shmem_locked(ring, submit->reply_shmem, reply_offset,
                                     submit->reply_size);
   }
   submit->ring_seqno_valid =
      VK_SUCCESS == vn_ring_submit_locked(ring, &submit->command,
                                          submit->reply_shmem,
                                          &submit->ring_seqno);
   mtx_unlock(&ring->mutex);

   if (submit->reply_size) {
      if (likely(submit->ring_seqno_valid)) {
         void *reply_ptr = submit->reply_shmem->mmap_ptr + reply_offset;
         submit->reply =
            VN_CS_DECODER_INITIALIZER(reply_ptr, submit->reply_size);
         vn_ring_wait_seqno(ring, submit->ring_seqno);
      } else { ... }
   }
}
```

So the sequence for a synchronous call is:

1. Guest computes `reply_size = vn_sizeof_<cmd>_reply(...)` — an *exact* size.
2. Guest allocates a shmem region of that size and sends
   `vkSetReplyCommandStreamMESA{ resourceId, offset, size }` down the ring.
3. Guest sends the actual command with `VK_COMMAND_GENERATE_REPLY_BIT_EXT` set.
4. Renderer writes the reply into that region
   (`vkr_cs_encoder_write` refuses to overrun: it calls `vkr_cs_encoder_set_fatal` if
   `size > enc->end - enc->cur`).
5. Guest waits for the ring seqno, then decodes from the mapped region with a
   `vn_cs_decoder` bounded by exactly `reply_size`.

`VkCommandStreamDescriptionMESA` (`vn_protocol_driver_defines.h`):

```c
typedef struct VkCommandStreamDescriptionMESA {
    uint32_t resourceId;
    size_t offset;
    size_t size;
} VkCommandStreamDescriptionMESA;
```

The `vn_call_*` wrapper ties it together:

```c
static inline VkResult vn_call_vkCreateBuffer(struct vn_ring *vn_ring, VkDevice device, const VkBufferCreateInfo* pCreateInfo, const VkAllocationCallbacks* pAllocator, VkBuffer* pBuffer)
{
    VN_TRACE_FUNC();

    struct vn_ring_submit_command submit;
    vn_submit_vkCreateBuffer(vn_ring, VK_COMMAND_GENERATE_REPLY_BIT_EXT, device, pCreateInfo, pAllocator, pBuffer, &submit);
    struct vn_cs_decoder *dec = vn_ring_get_command_reply(vn_ring, &submit);
    if (dec) {
        const VkResult ret = vn_decode_vkCreateBuffer_reply(dec, device, pCreateInfo, pAllocator, pBuffer);
        vn_ring_free_command_reply(vn_ring, &submit);
        return ret;
    } else {
        return VK_ERROR_OUT_OF_HOST_MEMORY;
    }
}
```

`vn_async_*` is the same minus the reply bit and the decode.

---

## K. `_partial` variants

```c
static inline size_t
vn_sizeof_VkPhysicalDeviceProperties_partial(const VkPhysicalDeviceProperties *val)
{
    size_t size = 0;
    /* skip val->apiVersion */
    /* skip val->driverVersion */
    /* skip val->vendorID */
    /* skip val->deviceID */
    /* skip val->deviceType */
    /* skip val->deviceName */
    /* skip val->pipelineCacheUUID */
    size += vn_sizeof_VkPhysicalDeviceLimits_partial(&val->limits);
    size += vn_sizeof_VkPhysicalDeviceSparseProperties_partial(&val->sparseProperties);
    return size;
}

static inline void
vn_encode_VkPhysicalDeviceProperties_partial(struct vn_cs_encoder *enc, const VkPhysicalDeviceProperties *val)
{
    /* skip val->apiVersion */
    /* skip val->driverVersion */
    /* skip val->vendorID */
    /* skip val->deviceID */
    /* skip val->deviceType */
    /* skip val->deviceName */
    /* skip val->pipelineCacheUUID */
    vn_encode_VkPhysicalDeviceLimits_partial(enc, &val->limits);
    vn_encode_VkPhysicalDeviceSparseProperties_partial(enc, &val->sparseProperties);
}
```

For comparison, the full versions:

```c
static inline size_t
vn_sizeof_VkPhysicalDeviceProperties(const VkPhysicalDeviceProperties *val)
{
    size_t size = 0;
    size += vn_sizeof_uint32_t(&val->apiVersion);
    size += vn_sizeof_uint32_t(&val->driverVersion);
    size += vn_sizeof_uint32_t(&val->vendorID);
    size += vn_sizeof_uint32_t(&val->deviceID);
    size += vn_sizeof_VkPhysicalDeviceType(&val->deviceType);
    size += vn_sizeof_array_size(VK_MAX_PHYSICAL_DEVICE_NAME_SIZE);
    size += vn_sizeof_char_array(val->deviceName, VK_MAX_PHYSICAL_DEVICE_NAME_SIZE);
    size += vn_sizeof_array_size(VK_UUID_SIZE);
    size += vn_sizeof_uint8_t_array(val->pipelineCacheUUID, VK_UUID_SIZE);
    size += vn_sizeof_VkPhysicalDeviceLimits(&val->limits);
    size += vn_sizeof_VkPhysicalDeviceSparseProperties(&val->sparseProperties);
    return size;
}
```

And a leaf where *everything* is skipped, so the partial form encodes **zero bytes**:

```c
static inline void
vn_encode_VkPhysicalDeviceSparseProperties_partial(struct vn_cs_encoder *enc, const VkPhysicalDeviceSparseProperties *val)
{
    /* skip val->residencyStandard2DBlockShape */
    /* skip val->residencyStandard2DMultisampleBlockShape */
    /* skip val->residencyStandard3DBlockShape */
    /* skip val->residencyAlignedMipSize */
    /* skip val->residencyNonResidentStrict */
}
```

**What "partial" means:** it is the encoding used when the struct is a *pure output*
parameter travelling **guest → host**. The guest has to send *something* so the host knows
the shape of the request, but the payload fields are uninitialized garbage in the guest's
memory. So the partial encoder emits only the fields that are meaningfully initialized on
an out-parameter. Per the spec:

> When a command parameter is a non-const pointer/array, the value(s) pointed to
> is assumed to be uninitialized except for
>
>  - `sType` of a struct
>  - `pNext` of a struct
>  - object handles
>  - `uint32_t` indicating the size of another command parameter
>
> A special rule must be followed to serialize only the initialized parts.

So the fields kept in a `_partial` are exactly: `sType`, `pNext` (via
`_pnext_partial`, which recurses with `_self_partial` bodies), object handles, and length
fields. Everything else is `/* skip */`. Nothing is zeroed — the bytes are simply **not
present on the wire at all** (the reply then carries the full struct back).

Usage:

```c
static inline size_t vn_sizeof_vkGetPhysicalDeviceProperties(VkPhysicalDevice physicalDevice, VkPhysicalDeviceProperties* pProperties)
{
    ...
    cmd_size += vn_sizeof_simple_pointer(pProperties);
    if (pProperties)
        cmd_size += vn_sizeof_VkPhysicalDeviceProperties_partial(pProperties);
    ...
}
```

`VkPhysicalDeviceProperties2_partial` shows the chained case — it keeps `sType`, walks
`_pnext_partial`, and calls `_self_partial`:

```c
static inline void
vn_encode_VkPhysicalDeviceProperties2_partial(struct vn_cs_encoder *enc, const VkPhysicalDeviceProperties2 *val)
{
    assert(val->sType == VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2);
    vn_encode_VkStructureType(enc, &(VkStructureType){ VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2 });
    vn_encode_VkPhysicalDeviceProperties2_pnext_partial(enc, val->pNext);
    vn_encode_VkPhysicalDeviceProperties2_self_partial(enc, val);
}
```

There is a corresponding renderer-side `vn_decode_VkFoo_partial_temp`, so the partial form
is a real, symmetric wire encoding — a distinct "shape" per struct that both sides must
agree on. **An implementation must generate both the full and the partial encoder for every
struct that can appear as an out-parameter.**

---

## L. Output-array "clamping" on replies

**There is no clamp in the generated code.** I grepped `vn_protocol_driver_device.h`,
`_transport.h`, `_pipeline.h`, `_pipeline_cache.h`, `_instance.h` for `MIN(`/`MIN2(`/
`clamp` and found zero hits in serialization paths. Reporting this explicitly since the
question presumed a clamp exists.

What Venus does instead is order the reply so that the count is decoded **before** the
array, and then require exact agreement:

```c
static inline VkResult vn_decode_vkEnumeratePhysicalDevices_reply(struct vn_cs_decoder *dec, VkInstance instance, uint32_t* pPhysicalDeviceCount, VkPhysicalDevice* pPhysicalDevices)
{
    VkCommandTypeEXT command_type;
    vn_decode_VkCommandTypeEXT(dec, &command_type);
    assert(command_type == VK_COMMAND_TYPE_vkEnumeratePhysicalDevices_EXT);

    VkResult ret;
    vn_decode_VkResult(dec, &ret);
    /* skip instance */
    if (vn_decode_simple_pointer(dec)) {
        vn_decode_uint32_t(dec, pPhysicalDeviceCount);
    } else {
        pPhysicalDeviceCount = NULL;
    }
    if (vn_peek_array_size(dec)) {
        const uint32_t iter_count = vn_decode_array_size(dec, (pPhysicalDeviceCount ? *pPhysicalDeviceCount : 0));
        for (uint32_t i = 0; i < iter_count; i++)
            vn_decode_VkPhysicalDevice(dec, &pPhysicalDevices[i]);
    } else {
        vn_decode_array_size_unchecked(dec);
        pPhysicalDevices = NULL;
    }

    return ret;
}
```

`*pPhysicalDeviceCount` has **already been overwritten with the host's returned count** by
the time `vn_decode_array_size(dec, *pPhysicalDeviceCount)` runs, so the expected value is
the *post-call* count, not the caller's capacity. The renderer encodes exactly that many:

```c
static inline void vn_encode_vkEnumerateInstanceExtensionProperties_reply(struct vn_cs_encoder *enc, const struct vn_command_vkEnumerateInstanceExtensionProperties *args)
{
    vn_encode_VkCommandTypeEXT(enc, &(VkCommandTypeEXT){VK_COMMAND_TYPE_vkEnumerateInstanceExtensionProperties_EXT});

    vn_encode_VkResult(enc, &args->ret);
    /* skip args->pLayerName */
    if (vn_encode_simple_pointer(enc, args->pPropertyCount))
        vn_encode_uint32_t(enc, args->pPropertyCount);
    if (args->pProperties) {
        vn_encode_array_size(enc, (args->pPropertyCount ? *args->pPropertyCount : 0));
        for (uint32_t i = 0; i < (args->pPropertyCount ? *args->pPropertyCount : 0); i++)
            vn_encode_VkExtensionProperties(enc, &args->pProperties[i]);
    } else {
        vn_encode_array_size(enc, 0);
    }
}
```

Guarantees that make this safe:

- `reply_size` was computed with the *pre-call* count (the caller's capacity), so a smaller
  post-call count always fits in the reply shmem.
- `VK_INCOMPLETE` semantics mean the returned count is never larger than what the guest
  requested, so the array never overruns the caller's buffer.
- If either side disagrees, `vn_decode_array_size` → `vn_cs_decoder_set_fatal` → `abort()`
  on the guest, `vkr_cs_decoder_set_fatal` (context kill) on the host.

Same pattern for a blob (`vn_decode_vkGetPipelineCacheData_reply`):

```c
    if (vn_decode_simple_pointer(dec)) {
        vn_decode_size_t(dec, pDataSize);
    } else {
        pDataSize = NULL;
    }
    if (vn_peek_array_size(dec)) {
        const size_t array_size = vn_decode_array_size(dec, (pDataSize ? *pDataSize : 0));
        vn_decode_blob_array(dec, pData, array_size);
    } else {
        vn_decode_array_size_unchecked(dec);
        pData = NULL;
    }
```

The nearest thing to a clamp on the *renderer* side is the bounds check in the encoder
(`virgl_vkr_cs.h`), which turns an overrun into a fatal error rather than truncating:

```c
static inline void
vkr_cs_encoder_write(struct vkr_cs_encoder *enc,
                     size_t size,
                     const void *val,
                     size_t val_size)
{
   assert(val_size <= size);

   if (unlikely(size > (size_t)(enc->end - enc->cur))) {
      vkr_log("failed to write the reply stream");
      vkr_cs_encoder_set_fatal(enc);
      return;
   }

   /* we should not rely on the compiler to optimize away memcpy... */
   if (enc->cur != val)
      memcpy(enc->cur, val, val_size);
   enc->cur += size;
}
```

**A Rust reimplementation should therefore treat the reply's array size as authoritative but
validate it against the decoded count, and error (not clamp) on mismatch.**

---

## M. `vkCreateBuffer` byte-by-byte

Assumptions for the concrete offsets: `pCreateInfo != NULL`, no recognized `pNext` link,
`sharingMode != VK_SHARING_MODE_CONCURRENT`, `pAllocator == NULL`, `pBuffer != NULL`.
All multi-byte values little-endian.

### Command

| off | width | field | value |
|----:|------:|---|---|
| 0 | 4 | `VkCommandTypeEXT` (`int32`) | `50` (`VK_COMMAND_TYPE_vkCreateBuffer_EXT`) |
| 4 | 4 | `VkCommandFlagsEXT` (`uint32`) | `1` for a sync call (`VK_COMMAND_GENERATE_REPLY_BIT_EXT`), `0` for async |
| 8 | 8 | `device` handle (`uint64`) | device object id |
| 16 | 8 | simple_pointer(`pCreateInfo`) (`uint64`) | `1` |
| **`VkBufferCreateInfo` begins** | | | |
| 24 | 4 | `sType` (`int32`) | `VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO` = 12 |
| 28 | 8 | pNext terminator (`uint64`) | `0` |
| 36 | 4 | `flags` (`VkFlags`/`uint32`) | |
| 40 | 8 | `size` (`VkDeviceSize`/`uint64`) | |
| 48 | 4 | `usage` (`VkFlags`/`uint32`) | |
| 52 | 4 | `sharingMode` (enum→`int32`) | `0` = EXCLUSIVE |
| 56 | 4 | `queueFamilyIndexCount` (`uint32`) | |
| 60 | 8 | array_size for `pQueueFamilyIndices` (`uint64`) | `0` (EXCLUSIVE branch) |
| **`VkBufferCreateInfo` ends at 68** | | | |
| 68 | 8 | simple_pointer(`pAllocator`) (`uint64`) | `0` (non-NULL trips `assert(false)`) |
| 76 | 8 | simple_pointer(`pBuffer`) (`uint64`) | `1` |
| 84 | 8 | `pBuffer` handle id (`uint64`) | guest-allocated object id for the new buffer |
| | | **total** | **92 bytes** |

Note the stream is only 4-byte aligned, so the 8-byte fields at offsets 28, 60, 68, 76, 84
are *not* 8-byte aligned. There is no natural alignment and no inserted padding: a decoder
must do unaligned 64-bit reads (or byte-wise reads).

Variants:

- `sharingMode == VK_SHARING_MODE_CONCURRENT`: offset 60 holds
  `queueFamilyIndexCount` as `uint64`, followed by `4 * queueFamilyIndexCount` bytes of
  packed `uint32`s at offset 68; everything after shifts by `4*n`.
- One recognized pNext link, e.g. `VkExternalMemoryBufferCreateInfo`: at offset 28 write
  `uint64 1`, then `int32 sType`, then the recursive pNext (here `uint64 0`), then that
  struct's `_self` (`uint32 handleTypes`) — 8+4+8+4 = 24 bytes replacing the 8-byte
  terminator, so `flags` moves from 36 to 52.
- `pBuffer == NULL`: offset 76 holds `0` and the trailing 8 bytes are absent (84 total).

### Reply (`reply_size` = 24)

| off | width | field |
|----:|------:|---|
| 0 | 4 | `VkCommandTypeEXT` = `50` |
| 4 | 4 | `VkResult` (`int32`) |
| 8 | 8 | simple_pointer(`pBuffer`) = `1` |
| 16 | 8 | `pBuffer` handle id (`uint64`) — host-assigned |
| | | **total 24 bytes** |

---

## Extras worth having in the codegen

### Unions

`venus-protocol-templates/types_union.h`: a union is a **`uint32_t` tag** (4 bytes) followed
by the selected member. If the union has a real selector (`ty.is_valid_union()`) the tag is
the selector's own type; otherwise the generator hardcodes a default tag:

```python
    UNION_DEFAULT_TAGS = {
        'VkClearColorValue': 2,
        'VkClearValue': 0,
        'VkDeviceOrHostAddressKHR': 0,
        'VkDeviceOrHostAddressConstKHR': 0,
        'VkPipelineExecutableStatisticValueKHR': 2,
    }
```

emitted as `static const uint32_t tag = N; /* union with default tag */`. On decode an
unknown tag is fatal.

### Extension gating

`vn_cs_renderer_protocol_has_extension(<vk.xml ext number>)` / `..._has_api_version(...)`
(`mesa_vn_cs.h`, backed by `vn_protocol_driver_info.h`) gate individual pNext cases. The
wire format is therefore **negotiated**: the same `VkBufferCreateInfo` serializes
differently depending on what the renderer advertised. A reimplementation must replicate
the extension-number table from `vn_protocol_driver_info.h` and the same gating conditions,
or it will desync.

### Command-stream / transport level

`vn_protocol_driver_transport.h` defines the MESA-private commands used to drive the ring:
`vkSetReplyCommandStreamMESA`, `vkExecuteCommandStreamsMESA`, `vkCreateRingMESA`,
`vkSubmitVirtqueueSeqnoMESA`, etc. These are encoded with the exact same rules as ordinary
Vulkan commands.

### Files to mine when generating

- `vn_protocol.py` — `PRIMITIVE_TYPES`, `UNION_DEFAULT_TAGS`, `COMMAND_BLOCK_LIST`, and the
  `Gen.*` methods that decide when a member is a blob / string / array / partial.
- `venus-protocol-templates/types_scalar.h`, `types_struct.h`, `types_chain.h`,
  `types_handle.h`, `types_union.h`, `types_command.h`, `types_custom.h` — the normative
  emission rules.
- `vn_protocol_driver_defines.h` — the `VkCommandTypeEXT` numbering (ABI!).
