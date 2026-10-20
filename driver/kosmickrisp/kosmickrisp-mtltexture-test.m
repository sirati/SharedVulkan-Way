/* Headless acceptance test for KosmicKrisp MTLTexture import
 * (VK_EXT_external_memory_metal, VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLTEXTURE_BIT_EXT).
 *
 * For each case: create the VkImage, query its layout, create an IOSurface
 * with that row pitch, wrap it in an MTLTexture, import that as the image's
 * dedicated memory, clear with vkCmdClearColorImage, then with a render pass
 * clear plus a vkCmdClearAttachments sub-rectangle, and check the IOSurface
 * pixels on the CPU after each step.
 *
 * Run with VK_DRIVER_FILES=<icd.json> DYLD_LIBRARY_PATH=<vulkan-loader/lib>.
 */
#define VK_NO_PROTOTYPES
#define VK_USE_PLATFORM_METAL_EXT
#include <vulkan/vulkan.h>

#import <CoreFoundation/CoreFoundation.h>
#import <Foundation/Foundation.h>
#import <IOSurface/IOSurface.h>
#import <Metal/Metal.h>

#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static PFN_vkGetInstanceProcAddr gipa;
static VkInstance inst;
static VkPhysicalDevice pdev;
static VkDevice dev;
static VkQueue queue;
static VkCommandPool pool;
static id<MTLDevice> mtl;
static int failures;

#define LOAD_I(name) PFN_##name name = (PFN_##name)gipa(inst, #name)
#define LOAD_D(name) PFN_##name name = (PFN_##name)gipa(inst, #name)

#define CHECK(call)                                                            \
   do {                                                                        \
      VkResult r_ = (call);                                                    \
      printf("  %-58s -> %d\n", #call, r_);                                    \
      if (r_ != VK_SUCCESS) {                                                  \
         failures++;                                                           \
         return 1;                                                             \
      }                                                                        \
   } while (0)

static uint32_t
align_u32(uint32_t v, uint32_t a)
{
   return (v + a - 1) / a * a;
}

static void
expect_bytes(const float c[4], bool bgra, uint8_t out[4])
{
   uint8_t rgba[4];
   for (int i = 0; i < 4; i++)
      rgba[i] = (uint8_t)(c[i] * 255.0f + 0.5f);
   if (bgra) {
      out[0] = rgba[2];
      out[1] = rgba[1];
      out[2] = rgba[0];
      out[3] = rgba[3];
   } else {
      memcpy(out, rgba, 4);
   }
}

static bool
near(uint8_t a, uint8_t b)
{
   return a > b ? a - b <= 1 : b - a <= 1;
}

/* Check every pixel; inside rect (if given) expect `in`, elsewhere `out`. */
static int
check_surface(IOSurfaceRef surf, uint32_t w, uint32_t h, uint32_t pitch,
              const uint8_t out[4], const uint8_t in[4], const VkRect2D *rect)
{
   IOSurfaceLock(surf, kIOSurfaceLockReadOnly, NULL);
   const uint8_t *base = IOSurfaceGetBaseAddress(surf);
   uint32_t bad = 0, first_x = 0, first_y = 0;
   uint8_t got[4] = {0}, want[4] = {0};
   for (uint32_t y = 0; y < h; y++) {
      for (uint32_t x = 0; x < w; x++) {
         const uint8_t *p = base + (size_t)y * pitch + x * 4;
         bool inside = rect && x >= (uint32_t)rect->offset.x &&
                       x < rect->offset.x + rect->extent.width &&
                       y >= (uint32_t)rect->offset.y &&
                       y < rect->offset.y + rect->extent.height;
         const uint8_t *e = inside ? in : out;
         if (!near(p[0], e[0]) || !near(p[1], e[1]) || !near(p[2], e[2]) ||
             !near(p[3], e[3])) {
            if (bad++ == 0) {
               first_x = x;
               first_y = y;
               memcpy(got, p, 4);
               memcpy(want, e, 4);
            }
         }
      }
   }
   const uint8_t *p0 = base;
   printf("  pixel(0,0) = %02x %02x %02x %02x", p0[0], p0[1], p0[2], p0[3]);
   if (rect) {
      const uint8_t *pc =
         base + (size_t)(rect->offset.y + 1) * pitch + (rect->offset.x + 1) * 4;
      printf(", pixel(%d,%d) = %02x %02x %02x %02x", rect->offset.x + 1,
             rect->offset.y + 1, pc[0], pc[1], pc[2], pc[3]);
   }
   printf("\n");
   IOSurfaceUnlock(surf, kIOSurfaceLockReadOnly, NULL);
   if (bad) {
      printf("  FAIL: %u/%u pixels wrong, first at (%u,%u): got %02x %02x %02x "
             "%02x want %02x %02x %02x %02x\n",
             bad, w * h, first_x, first_y, got[0], got[1], got[2], got[3],
             want[0], want[1], want[2], want[3]);
      failures++;
      return 1;
   }
   printf("  OK: all %u pixels match\n", w * h);
   return 0;
}

static int
submit_and_wait(VkCommandBuffer cmd)
{
   LOAD_D(vkEndCommandBuffer);
   LOAD_D(vkQueueSubmit);
   LOAD_D(vkCreateFence);
   LOAD_D(vkWaitForFences);
   LOAD_D(vkDestroyFence);
   VkFence fence;
   CHECK(vkEndCommandBuffer(cmd));
   CHECK(vkCreateFence(dev, &(VkFenceCreateInfo){VK_STRUCTURE_TYPE_FENCE_CREATE_INFO}, NULL, &fence));
   VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                      .commandBufferCount = 1,
                      .pCommandBuffers = &cmd};
   CHECK(vkQueueSubmit(queue, 1, &si, fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 5000000000ull));
   vkDestroyFence(dev, fence, NULL);
   return 0;
}

static void
query_format(VkFormat format, VkImageTiling tiling, VkImageUsageFlags usage)
{
   LOAD_I(vkGetPhysicalDeviceImageFormatProperties2);
   VkPhysicalDeviceExternalImageFormatInfo ext = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLTEXTURE_BIT_EXT,
   };
   VkPhysicalDeviceImageFormatInfo2 info = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
      .pNext = &ext,
      .format = format,
      .type = VK_IMAGE_TYPE_2D,
      .tiling = tiling,
      .usage = usage,
   };
   VkExternalImageFormatProperties eprops = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES};
   VkImageFormatProperties2 props = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2, .pNext = &eprops};
   VkResult r = vkGetPhysicalDeviceImageFormatProperties2(pdev, &info, &props);
   const VkExternalMemoryProperties *m = &eprops.externalMemoryProperties;
   printf("  format %3d %-7s -> %d, features 0x%x (importable %d, dedicated_only "
          "%d), compatible 0x%x, maxMip %u, maxLayers %u, maxExtent %ux%u\n",
          format, tiling == VK_IMAGE_TILING_LINEAR ? "LINEAR" : "OPTIMAL", r,
          m->externalMemoryFeatures,
          !!(m->externalMemoryFeatures & VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT),
          !!(m->externalMemoryFeatures & VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT),
          m->compatibleHandleTypes, props.imageFormatProperties.maxMipLevels,
          props.imageFormatProperties.maxArrayLayers,
          props.imageFormatProperties.maxExtent.width,
          props.imageFormatProperties.maxExtent.height);
   if (r != VK_SUCCESS ||
       !(m->externalMemoryFeatures & VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT) ||
       !(m->externalMemoryFeatures & VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT))
      failures++;
}

static int
run_case(VkFormat format, VkImageTiling tiling, uint32_t w, uint32_t h)
{
   LOAD_D(vkCreateImage);
   LOAD_D(vkDestroyImage);
   LOAD_D(vkGetImageSubresourceLayout);
   LOAD_D(vkGetImageMemoryRequirements2);
   LOAD_D(vkGetMemoryMetalHandlePropertiesEXT);
   LOAD_D(vkAllocateMemory);
   LOAD_D(vkFreeMemory);
   LOAD_D(vkMapMemory);
   LOAD_D(vkBindImageMemory);
   LOAD_D(vkAllocateCommandBuffers);
   LOAD_D(vkFreeCommandBuffers);
   LOAD_D(vkBeginCommandBuffer);
   LOAD_D(vkCmdPipelineBarrier);
   LOAD_D(vkCmdClearColorImage);
   LOAD_D(vkCreateImageView);
   LOAD_D(vkDestroyImageView);
   LOAD_D(vkCreateRenderPass);
   LOAD_D(vkDestroyRenderPass);
   LOAD_D(vkCreateFramebuffer);
   LOAD_D(vkDestroyFramebuffer);
   LOAD_D(vkCmdBeginRenderPass);
   LOAD_D(vkCmdClearAttachments);
   LOAD_D(vkCmdEndRenderPass);

   const bool bgra = format == VK_FORMAT_B8G8R8A8_UNORM;
   const bool linear = tiling == VK_IMAGE_TILING_LINEAR;
   printf("\n== case %s %s %ux%u\n", bgra ? "B8G8R8A8_UNORM" : "R8G8B8A8_UNORM",
          linear ? "LINEAR" : "OPTIMAL", w, h);

   const VkImageUsageFlags usage =
      VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_SAMPLED_BIT |
      VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT;

   /* 1. VkImage */
   VkExternalMemoryImageCreateInfo ext_ci = {
      .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
      .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLTEXTURE_BIT_EXT,
   };
   VkImageCreateInfo ici = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
      .pNext = &ext_ci,
      .imageType = VK_IMAGE_TYPE_2D,
      .format = format,
      .extent = {w, h, 1},
      .mipLevels = 1,
      .arrayLayers = 1,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .tiling = tiling,
      .usage = usage,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
   };
   VkImage image;
   CHECK(vkCreateImage(dev, &ici, NULL, &image));

   /* 2. Row pitch: from the driver for LINEAR, align(w*4, 64) otherwise */
   uint32_t pitch;
   if (linear) {
      VkImageSubresource sub = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0};
      VkSubresourceLayout lay;
      vkGetImageSubresourceLayout(dev, image, &sub, &lay);
      printf("  vkGetImageSubresourceLayout: offset %llu size %llu rowPitch %llu "
             "(w*4 = %u)\n",
             (unsigned long long)lay.offset, (unsigned long long)lay.size,
             (unsigned long long)lay.rowPitch, w * 4);
      pitch = (uint32_t)lay.rowPitch;
   } else {
      pitch = align_u32(w * 4, 64);
      printf("  OPTIMAL: using IOSurface rowPitch align(w*4, 64) = %u\n", pitch);
   }

   /* 3. IOSurface with that pitch */
   const OSType fourcc = bgra ? 'BGRA' : 'RGBA';
   NSDictionary *props = @{
      (id)kIOSurfaceWidth : @(w),
      (id)kIOSurfaceHeight : @(h),
      (id)kIOSurfaceBytesPerElement : @4,
      (id)kIOSurfaceBytesPerRow : @(pitch),
      (id)kIOSurfacePixelFormat : @(fourcc),
   };
   IOSurfaceRef surf = IOSurfaceCreate((CFDictionaryRef)props);
   if (!surf) {
      printf("  FAIL: IOSurfaceCreate\n");
      failures++;
      return 1;
   }
   printf("  IOSurface %ux%u: bytesPerRow requested %u, actual %zu, "
          "IOSurfaceGetPropertyAlignment(BytesPerRow) %zu, "
          "minimumLinearTextureAlignment %lu\n",
          w, h, pitch, IOSurfaceGetBytesPerRow(surf),
          IOSurfaceGetPropertyAlignment(kIOSurfaceBytesPerRow),
          (unsigned long)[mtl minimumLinearTextureAlignmentForPixelFormat:
                                 bgra ? MTLPixelFormatBGRA8Unorm
                                      : MTLPixelFormatRGBA8Unorm]);
   if (IOSurfaceGetBytesPerRow(surf) != pitch) {
      printf("  FAIL: IOSurface did not keep the requested pitch\n");
      failures++;
   }
   pitch = (uint32_t)IOSurfaceGetBytesPerRow(surf);

   /* 4. MTLTexture over the IOSurface */
   MTLTextureDescriptor *td = [MTLTextureDescriptor
      texture2DDescriptorWithPixelFormat:bgra ? MTLPixelFormatBGRA8Unorm
                                              : MTLPixelFormatRGBA8Unorm
                                   width:w
                                  height:h
                               mipmapped:NO];
   td.usage = MTLTextureUsageShaderRead | MTLTextureUsageShaderWrite |
              MTLTextureUsageRenderTarget | MTLTextureUsagePixelFormatView;
   td.storageMode = MTLStorageModeShared;
   id<MTLTexture> tex = [mtl newTextureWithDescriptor:td iosurface:surf plane:0];
   printf("  newTextureWithDescriptor:iosurface:plane: -> %p (%lux%lu)\n",
          (void *)tex, (unsigned long)tex.width, (unsigned long)tex.height);
   if (!tex) {
      failures++;
      return 1;
   }

   /* 5. Handle properties and requirements */
   VkMemoryMetalHandlePropertiesEXT hp = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_METAL_HANDLE_PROPERTIES_EXT};
   CHECK(vkGetMemoryMetalHandlePropertiesEXT(
      dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLTEXTURE_BIT_EXT, (void *)tex, &hp));
   VkMemoryDedicatedRequirements dreq = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS};
   VkMemoryRequirements2 req = {.sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2,
                                .pNext = &dreq};
   vkGetImageMemoryRequirements2(
      dev,
      &(VkImageMemoryRequirementsInfo2){
         VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2, NULL, image},
      &req);
   printf("  memoryTypeBits handle 0x%x image 0x%x, size %llu, "
          "requiresDedicated %d prefersDedicated %d\n",
          hp.memoryTypeBits, req.memoryRequirements.memoryTypeBits,
          (unsigned long long)req.memoryRequirements.size,
          dreq.requiresDedicatedAllocation, dreq.prefersDedicatedAllocation);
   if (!dreq.requiresDedicatedAllocation)
      failures++;
   uint32_t bits = hp.memoryTypeBits & req.memoryRequirements.memoryTypeBits;
   if (!bits) {
      printf("  FAIL: no memory type\n");
      failures++;
      return 1;
   }

   /* 6. Import + bind */
   VkMemoryDedicatedAllocateInfo ded = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
      .image = image,
   };
   VkImportMemoryMetalHandleInfoEXT imp = {
      .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_METAL_HANDLE_INFO_EXT,
      .pNext = &ded,
      .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLTEXTURE_BIT_EXT,
      .handle = (void *)tex,
   };
   VkMemoryAllocateInfo mai = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .pNext = &imp,
      .allocationSize = req.memoryRequirements.size,
      .memoryTypeIndex = (uint32_t)__builtin_ctz(bits),
   };
   VkDeviceMemory mem;
   CHECK(vkAllocateMemory(dev, &mai, NULL, &mem));
   CHECK(vkBindImageMemory(dev, image, mem, 0));
   void *ptr = NULL;
   VkResult mr = vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, &ptr);
   printf("  vkMapMemory on imported memory -> %d (expected %d, no CPU view)\n",
          mr, VK_ERROR_MEMORY_MAP_FAILED);

   VkCommandBuffer cmd;
   VkCommandBufferAllocateInfo cai = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1,
   };
   VkCommandBufferBeginInfo cbi = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
      .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
   };
   const VkImageSubresourceRange range = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};

   /* 7. vkCmdClearColorImage */
   const float c1[4] = {0.2f, 0.4f, 0.6f, 1.0f};
   CHECK(vkAllocateCommandBuffers(dev, &cai, &cmd));
   CHECK(vkBeginCommandBuffer(cmd, &cbi));
   VkImageMemoryBarrier b = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
      .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
      .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .image = image,
      .subresourceRange = range,
   };
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1,
                        &b);
   vkCmdClearColorImage(cmd, image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                        &(VkClearColorValue){.float32 = {c1[0], c1[1], c1[2], c1[3]}},
                        1, &range);
   b.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
   b.dstAccessMask = VK_ACCESS_HOST_READ_BIT;
   b.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
   b.newLayout = VK_IMAGE_LAYOUT_GENERAL;
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
                        VK_PIPELINE_STAGE_HOST_BIT, 0, 0, NULL, 0, NULL, 1, &b);
   if (submit_and_wait(cmd))
      return 1;
   uint8_t e1[4];
   expect_bytes(c1, bgra, e1);
   printf("  vkCmdClearColorImage(0.2,0.4,0.6,1.0): expect %02x %02x %02x %02x\n",
          e1[0], e1[1], e1[2], e1[3]);
   check_surface(surf, w, h, pitch, e1, NULL, NULL);
   vkFreeCommandBuffers(dev, pool, 1, &cmd);

   /* 8. Render pass clear + vkCmdClearAttachments sub-rectangle */
   VkImageView view;
   VkImageViewCreateInfo vci = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = image,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = format,
      .subresourceRange = range,
   };
   CHECK(vkCreateImageView(dev, &vci, NULL, &view));
   VkAttachmentDescription att = {
      .format = format,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
      .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .finalLayout = VK_IMAGE_LAYOUT_GENERAL,
   };
   VkAttachmentReference aref = {0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
   VkSubpassDescription sp = {
      .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
      .colorAttachmentCount = 1,
      .pColorAttachments = &aref,
   };
   VkSubpassDependency dep = {
      .srcSubpass = 0,
      .dstSubpass = VK_SUBPASS_EXTERNAL,
      .srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
      .dstStageMask = VK_PIPELINE_STAGE_HOST_BIT,
      .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
      .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
   };
   VkRenderPassCreateInfo rpci = {
      .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
      .attachmentCount = 1,
      .pAttachments = &att,
      .subpassCount = 1,
      .pSubpasses = &sp,
      .dependencyCount = 1,
      .pDependencies = &dep,
   };
   VkRenderPass rp;
   CHECK(vkCreateRenderPass(dev, &rpci, NULL, &rp));
   VkFramebuffer fb;
   VkFramebufferCreateInfo fci = {
      .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
      .renderPass = rp,
      .attachmentCount = 1,
      .pAttachments = &view,
      .width = w,
      .height = h,
      .layers = 1,
   };
   CHECK(vkCreateFramebuffer(dev, &fci, NULL, &fb));

   const float c2[4] = {0.8f, 0.1f, 0.3f, 1.0f};
   const float c3[4] = {0.0f, 1.0f, 0.5f, 1.0f};
   VkClearValue cv = {.color.float32 = {c2[0], c2[1], c2[2], c2[3]}};
   VkRect2D sub = {{(int32_t)(w / 4), (int32_t)(h / 4)}, {w / 2, h / 2}};
   CHECK(vkAllocateCommandBuffers(dev, &cai, &cmd));
   CHECK(vkBeginCommandBuffer(cmd, &cbi));
   VkRenderPassBeginInfo rpbi = {
      .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
      .renderPass = rp,
      .framebuffer = fb,
      .renderArea = {{0, 0}, {w, h}},
      .clearValueCount = 1,
      .pClearValues = &cv,
   };
   vkCmdBeginRenderPass(cmd, &rpbi, VK_SUBPASS_CONTENTS_INLINE);
   VkClearAttachment ca = {
      .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
      .colorAttachment = 0,
      .clearValue.color.float32 = {c3[0], c3[1], c3[2], c3[3]},
   };
   VkClearRect cr = {sub, 0, 1};
   vkCmdClearAttachments(cmd, 1, &ca, 1, &cr);
   vkCmdEndRenderPass(cmd);
   if (submit_and_wait(cmd))
      return 1;
   uint8_t e2[4], e3[4];
   expect_bytes(c2, bgra, e2);
   expect_bytes(c3, bgra, e3);
   printf("  render pass clear(0.8,0.1,0.3,1.0) + ClearAttachments(0,1,0.5,1) at "
          "(%d,%d %ux%u): expect %02x %02x %02x %02x / %02x %02x %02x %02x\n",
          sub.offset.x, sub.offset.y, sub.extent.width, sub.extent.height, e2[0],
          e2[1], e2[2], e2[3], e3[0], e3[1], e3[2], e3[3]);
   check_surface(surf, w, h, pitch, e2, e3, &sub);
   vkFreeCommandBuffers(dev, pool, 1, &cmd);

   vkDestroyFramebuffer(dev, fb, NULL);
   vkDestroyRenderPass(dev, rp, NULL);
   vkDestroyImageView(dev, view, NULL);
   vkDestroyImage(dev, image, NULL);
   vkFreeMemory(dev, mem, NULL);
   [tex release];
   CFRelease(surf);
   return 0;
}

int
main(void)
{
   @autoreleasepool {
      void *lib = dlopen("libvulkan.1.dylib", RTLD_NOW | RTLD_LOCAL);
      if (!lib) {
         printf("dlopen libvulkan.1.dylib: %s\n", dlerror());
         return 2;
      }
      gipa = (PFN_vkGetInstanceProcAddr)dlsym(lib, "vkGetInstanceProcAddr");
      mtl = MTLCreateSystemDefaultDevice();
      printf("Metal device: %s\n", mtl.name.UTF8String);

      LOAD_I(vkCreateInstance);
      LOAD_I(vkEnumerateInstanceExtensionProperties);
      uint32_t n = 0;
      vkEnumerateInstanceExtensionProperties(NULL, &n, NULL);
      VkExtensionProperties *iexts = calloc(n, sizeof(*iexts));
      vkEnumerateInstanceExtensionProperties(NULL, &n, iexts);
      bool portability = false;
      for (uint32_t i = 0; i < n; i++)
         portability |= !strcmp(iexts[i].extensionName,
                                VK_KHR_PORTABILITY_ENUMERATION_EXTENSION_NAME);
      const char *inst_ext = VK_KHR_PORTABILITY_ENUMERATION_EXTENSION_NAME;
      VkInstanceCreateInfo ici = {
         .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
         .flags = portability ? VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR : 0,
         .pApplicationInfo = &(VkApplicationInfo){
            .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
            .apiVersion = VK_API_VERSION_1_3},
         .enabledExtensionCount = portability ? 1 : 0,
         .ppEnabledExtensionNames = &inst_ext,
      };
      CHECK(vkCreateInstance(&ici, NULL, &inst));

      LOAD_I(vkEnumeratePhysicalDevices);
      LOAD_I(vkGetPhysicalDeviceProperties);
      LOAD_I(vkEnumerateDeviceExtensionProperties);
      LOAD_I(vkCreateDevice);
      n = 1;
      vkEnumeratePhysicalDevices(inst, &n, &pdev);
      VkPhysicalDeviceProperties pp;
      vkGetPhysicalDeviceProperties(pdev, &pp);
      printf("Vulkan device: %s, driver version 0x%x\n", pp.deviceName,
             pp.driverVersion);

      vkEnumerateDeviceExtensionProperties(pdev, NULL, &n, NULL);
      VkExtensionProperties *dexts = calloc(n, sizeof(*dexts));
      vkEnumerateDeviceExtensionProperties(pdev, NULL, &n, dexts);
      const char *exts[2] = {VK_EXT_EXTERNAL_MEMORY_METAL_EXTENSION_NAME};
      uint32_t ext_count = 1;
      bool has_metal = false;
      for (uint32_t i = 0; i < n; i++) {
         if (!strcmp(dexts[i].extensionName, "VK_KHR_portability_subset"))
            exts[ext_count++] = "VK_KHR_portability_subset";
         has_metal |= !strcmp(dexts[i].extensionName,
                              VK_EXT_EXTERNAL_MEMORY_METAL_EXTENSION_NAME);
      }
      printf("VK_EXT_external_memory_metal advertised: %d\n", has_metal);
      VkDeviceCreateInfo dci = {
         .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
         .queueCreateInfoCount = 1,
         .pQueueCreateInfos = &(VkDeviceQueueCreateInfo){
            .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
            .queueFamilyIndex = 0,
            .queueCount = 1,
            .pQueuePriorities = &(float){1.0f}},
         .enabledExtensionCount = ext_count,
         .ppEnabledExtensionNames = exts,
      };
      CHECK(vkCreateDevice(pdev, &dci, NULL, &dev));
      LOAD_D(vkGetDeviceQueue);
      LOAD_D(vkCreateCommandPool);
      vkGetDeviceQueue(dev, 0, 0, &queue);
      CHECK(vkCreateCommandPool(
         dev,
         &(VkCommandPoolCreateInfo){
            .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
            .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT},
         NULL, &pool));

      printf("\n== vkGetPhysicalDeviceImageFormatProperties2, MTLTEXTURE, 2D, "
             "COLOR_ATTACHMENT|SAMPLED|TRANSFER_SRC|TRANSFER_DST\n");
      const VkImageUsageFlags usage =
         VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_SAMPLED_BIT |
         VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT;
      const VkFormat fmts[] = {VK_FORMAT_B8G8R8A8_UNORM, VK_FORMAT_B8G8R8A8_SRGB,
                               VK_FORMAT_R8G8B8A8_UNORM, VK_FORMAT_R8G8B8A8_SRGB};
      for (unsigned i = 0; i < 4; i++) {
         query_format(fmts[i], VK_IMAGE_TILING_LINEAR, usage);
         query_format(fmts[i], VK_IMAGE_TILING_OPTIMAL, usage);
      }

      run_case(VK_FORMAT_B8G8R8A8_UNORM, VK_IMAGE_TILING_LINEAR, 256, 256);
      run_case(VK_FORMAT_B8G8R8A8_UNORM, VK_IMAGE_TILING_LINEAR, 100, 75);
      run_case(VK_FORMAT_B8G8R8A8_UNORM, VK_IMAGE_TILING_LINEAR, 101, 77);
      run_case(VK_FORMAT_R8G8B8A8_UNORM, VK_IMAGE_TILING_LINEAR, 256, 256);
      run_case(VK_FORMAT_B8G8R8A8_UNORM, VK_IMAGE_TILING_OPTIMAL, 256, 256);

      LOAD_D(vkDestroyCommandPool);
      LOAD_D(vkDestroyDevice);
      LOAD_I(vkDestroyInstance);
      vkDestroyCommandPool(dev, pool, NULL);
      vkDestroyDevice(dev, NULL);
      vkDestroyInstance(inst, NULL);
      [mtl release];
      printf("\n%s (%d failures)\n", failures ? "FAILED" : "PASSED", failures);
      return failures ? 1 : 0;
   }
}
