/* Headless joined NV12 import and BT.601 sampler conversion fixture.
 * Samples a padded two-plane IOSurface through one VkImage, checks every
 * output pixel after a GPU fence, and exercises full and narrow ranges.
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

#include "nv12_spv.h"
#include <math.h>
static int run_memory_contract(void) {
   LOAD_D(vkGetMemoryMetalHandlePropertiesEXT);
   LOAD_D(vkAllocateMemory);
   LOAD_D(vkMapMemory);
   LOAD_D(vkUnmapMemory);
   LOAD_D(vkFreeMemory);
   LOAD_I(vkGetPhysicalDeviceMemoryProperties);
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pdev, &mp);
   uint32_t hosts = 0;
   for (unsigned i = 0; i < mp.memoryTypeCount; i++)
      if (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)
         hosts |= 1u << i;
   if (!hosts)
      return 1;
   MTLHeapDescriptor *desc = [[MTLHeapDescriptor alloc] init];
   desc.size = 65536;
   desc.storageMode = MTLStorageModeShared;
   desc.type = MTLHeapTypePlacement;
   desc.hazardTrackingMode = MTLHazardTrackingModeUntracked;
   id<MTLHeap> heap = [mtl newHeapWithDescriptor:desc];
   [desc release];
   if (!heap)
      return 1;
   VkMemoryMetalHandlePropertiesEXT props = {
       .sType = VK_STRUCTURE_TYPE_MEMORY_METAL_HANDLE_PROPERTIES_EXT};
   CHECK(vkGetMemoryMetalHandlePropertiesEXT(
       dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLHEAP_BIT_EXT, heap, &props));
   if ((props.memoryTypeBits & hosts) != hosts)
      return 1;
   for (unsigned imported = 0; imported < 2; imported++) {
      VkImportMemoryMetalHandleInfoEXT info = {
          .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_METAL_HANDLE_INFO_EXT,
          .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLHEAP_BIT_EXT,
          .handle = heap};
      VkDeviceMemory memory;
      CHECK(
          vkAllocateMemory(dev,
                           &(VkMemoryAllocateInfo){
                               .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                               .pNext = imported ? &info : NULL,
                               .allocationSize = 4096,
                               .memoryTypeIndex = __builtin_ctz(hosts)},
                           NULL, &memory));
      void *mapped = NULL;
      CHECK(vkMapMemory(dev, memory, 0, VK_WHOLE_SIZE, 0, &mapped));
      if (!mapped)
         return 1;
      memset(mapped, 0x5a, 4096);
      vkUnmapMemory(dev, memory);
      vkFreeMemory(dev, memory, NULL);
   }
   [heap release];
   printf("ordinary and MTLHEAP HOST_VISIBLE allocations remain mappable\n");
   return 0;
}

static int run_nv12(bool narrow) {
#define D(x) LOAD_D(x)
   D(vkCreateImage);
   D(vkGetImageMemoryRequirements);
   D(vkAllocateMemory);
   D(vkBindImageMemory);
   D(vkGetMemoryMetalHandlePropertiesEXT);
   D(vkCreateSamplerYcbcrConversion);
   D(vkCreateSampler);
   D(vkCreateImageView);
   D(vkCreateBuffer);
   D(vkGetBufferMemoryRequirements);
   D(vkBindBufferMemory);
   D(vkMapMemory);
   D(vkCreateDescriptorSetLayout);
   D(vkCreatePipelineLayout);
   D(vkCreateShaderModule);
   D(vkCreateComputePipelines);
   D(vkCreateDescriptorPool);
   D(vkAllocateDescriptorSets);
   D(vkUpdateDescriptorSets);
   D(vkAllocateCommandBuffers);
   D(vkBeginCommandBuffer);
   D(vkCmdPipelineBarrier);
   D(vkCmdBindPipeline);
   D(vkCmdBindDescriptorSets);
   D(vkCmdDispatch);
   D(vkEndCommandBuffer);
   D(vkCreateFence);
   D(vkQueueSubmit);
   D(vkWaitForFences);
   D(vkInvalidateMappedMemoryRanges);
   LOAD_I(vkGetPhysicalDeviceMemoryProperties);
   const VkFormat fmt = VK_FORMAT_G8_B8R8_2PLANE_420_UNORM;
   LOAD_I(vkGetPhysicalDeviceImageFormatProperties2);
   VkPhysicalDeviceExternalImageFormatInfo external_query = {
       .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
       .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLTEXTURE_BIT_EXT};
   VkPhysicalDeviceImageFormatInfo2 query = {
       .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
       .pNext = &external_query,
       .format = fmt,
       .type = VK_IMAGE_TYPE_2D,
       .tiling = VK_IMAGE_TILING_LINEAR,
       .usage = VK_IMAGE_USAGE_SAMPLED_BIT};
   VkExternalImageFormatProperties external_props = {
       .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES};
   VkImageFormatProperties2 props = {
       .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2,
       .pNext = &external_props};
   CHECK(vkGetPhysicalDeviceImageFormatProperties2(pdev, &query, &props));
   if (props.imageFormatProperties.maxMipLevels != 1 ||
       props.imageFormatProperties.maxArrayLayers != 1 ||
       !(external_props.externalMemoryProperties.externalMemoryFeatures &
         VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT))
      return 1;
   query.usage |= VK_IMAGE_USAGE_TRANSFER_DST_BIT;
   if (vkGetPhysicalDeviceImageFormatProperties2(pdev, &query, &props) !=
       VK_ERROR_FORMAT_NOT_SUPPORTED)
      return 1;
   query.usage = VK_IMAGE_USAGE_SAMPLED_BIT;
   query.flags = VK_IMAGE_CREATE_DISJOINT_BIT;
   if (vkGetPhysicalDeviceImageFormatProperties2(pdev, &query, &props) !=
       VK_ERROR_FORMAT_NOT_SUPPORTED)
      return 1;
   NSDictionary *plane0 = @{
      (id)kIOSurfacePlaneWidth : @4,
      (id)kIOSurfacePlaneHeight : @4,
      (id)kIOSurfacePlaneBytesPerElement : @1,
      (id)kIOSurfacePlaneBytesPerRow : @64,
      (id)kIOSurfacePlaneOffset : @0,
      (id)kIOSurfacePlaneSize : @256
   };
   NSDictionary *plane1 = @{
      (id)kIOSurfacePlaneWidth : @2,
      (id)kIOSurfacePlaneHeight : @2,
      (id)kIOSurfacePlaneBytesPerElement : @2,
      (id)kIOSurfacePlaneBytesPerRow : @64,
      (id)kIOSurfacePlaneOffset : @256,
      (id)kIOSurfacePlaneSize : @128
   };
   IOSurfaceRef surface = IOSurfaceCreate((CFDictionaryRef) @{
      (id)kIOSurfaceWidth : @4,
      (id)kIOSurfaceHeight : @4,
      (id)kIOSurfacePixelFormat : @(narrow ? 0x34323076 : 0x34323066),
      (id)kIOSurfaceAllocSize : @384,
      (id)kIOSurfacePlaneInfo : @[ plane0, plane1 ]
   });
   if (!surface)
      return 1;
   IOSurfaceLock(surface, 0, NULL);
   for (int y = 0; y < 4; y++)
      for (int x = 0; x < 4; x++)
         *((uint8_t *)IOSurfaceGetBaseAddressOfPlane(surface, 0) +
           y * IOSurfaceGetBytesPerRowOfPlane(surface, 0) + x) =
             80 + y * 20 + x * 7;
   for (int y = 0; y < 2; y++)
      for (int x = 0; x < 2; x++) {
         uint8_t *p = (uint8_t *)IOSurfaceGetBaseAddressOfPlane(surface, 1) +
                      y * IOSurfaceGetBytesPerRowOfPlane(surface, 1) + x * 2;
         p[0] = 40 + x * 48;
         p[1] = 140 + y * 40;
      }
   IOSurfaceUnlock(surface, 0, NULL);
   MTLTextureDescriptor *td = [MTLTextureDescriptor
       texture2DDescriptorWithPixelFormat:MTLPixelFormatR8Unorm
                                    width:4
                                   height:4
                                mipmapped:NO];
   td.storageMode = MTLStorageModeShared;
   td.usage = MTLTextureUsageShaderRead | MTLTextureUsagePixelFormatView;
   id<MTLTexture> tex = [mtl newTextureWithDescriptor:td
                                            iosurface:surface
                                                plane:0];
   if (!tex)
      return 1;
   VkExternalMemoryImageCreateInfo external = {
       .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
       .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLTEXTURE_BIT_EXT};
   VkImage image;
   CHECK(vkCreateImage(
       dev,
       &(VkImageCreateInfo){.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                            .pNext = &external,
                            .imageType = VK_IMAGE_TYPE_2D,
                            .format = fmt,
                            .extent = {4, 4, 1},
                            .mipLevels = 1,
                            .arrayLayers = 1,
                            .samples = VK_SAMPLE_COUNT_1_BIT,
                            .tiling = VK_IMAGE_TILING_LINEAR,
                            .usage = VK_IMAGE_USAGE_SAMPLED_BIT,
                            .initialLayout = VK_IMAGE_LAYOUT_PREINITIALIZED},
       NULL, &image));
   VkMemoryRequirements req;
   vkGetImageMemoryRequirements(dev, image, &req);
   VkMemoryMetalHandlePropertiesEXT hp = {
       .sType = VK_STRUCTURE_TYPE_MEMORY_METAL_HANDLE_PROPERTIES_EXT};
   CHECK(vkGetMemoryMetalHandlePropertiesEXT(
       dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLTEXTURE_BIT_EXT, tex, &hp));
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pdev, &mp);
   uint32_t host_types = 0;
   for (unsigned i = 0; i < mp.memoryTypeCount; i++)
      if (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT)
         host_types |= 1u << i;
   if (!hp.memoryTypeBits || !req.memoryTypeBits ||
       (hp.memoryTypeBits & host_types) || (req.memoryTypeBits & host_types) ||
       !(req.memoryTypeBits & hp.memoryTypeBits))
      return 1;
   uint32_t type = __builtin_ctz(req.memoryTypeBits & hp.memoryTypeBits);
   VkMemoryDedicatedAllocateInfo dedicated = {
       .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
       .image = image};
   VkImportMemoryMetalHandleInfoEXT import = {
       .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_METAL_HANDLE_INFO_EXT,
       .pNext = &dedicated,
       .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLTEXTURE_BIT_EXT,
       .handle = tex};
   for (unsigned i = 0; i < mp.memoryTypeCount; i++) {
      if (!(host_types & (1u << i)))
         continue;
      VkDeviceMemory incompatible = VK_NULL_HANDLE;
      VkResult rejected =
          vkAllocateMemory(dev,
                           &(VkMemoryAllocateInfo){
                               .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                               .pNext = &import,
                               .allocationSize = req.size,
                               .memoryTypeIndex = i},
                           NULL, &incompatible);
      if (rejected != VK_ERROR_INVALID_EXTERNAL_HANDLE) {
         printf("HOST_VISIBLE texture import type %u returned %d\n", i,
                rejected);
         return 1;
      }
   }
   printf("MTLTEXTURE properties/requirements exclude HOST_VISIBLE; "
          "incompatible imports refused\n");
   MTLTextureDescriptor *wrong_desc = [MTLTextureDescriptor
       texture2DDescriptorWithPixelFormat:MTLPixelFormatRG8Unorm
                                    width:2
                                   height:2
                                mipmapped:NO];
   wrong_desc.storageMode = MTLStorageModeShared;
   wrong_desc.usage =
       MTLTextureUsageShaderRead | MTLTextureUsagePixelFormatView;
   id<MTLTexture> wrong = [mtl newTextureWithDescriptor:wrong_desc
                                              iosurface:surface
                                                  plane:1];
   if (!wrong)
      return 1;
   import.handle = wrong;
   VkDeviceMemory refused = VK_NULL_HANDLE;
   VkResult rejected = vkAllocateMemory(
       dev,
       &(VkMemoryAllocateInfo){.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                               .pNext = &import,
                               .allocationSize = req.size,
                               .memoryTypeIndex = type},
       NULL, &refused);
   [wrong release];
   import.handle = tex;
   if (rejected != VK_ERROR_INVALID_EXTERNAL_HANDLE) {
      printf("wrong-plane import returned %d\n", rejected);
      return 1;
   }
   VkDeviceMemory mem;
   CHECK(vkAllocateMemory(
       dev,
       &(VkMemoryAllocateInfo){.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                               .pNext = &import,
                               .allocationSize = req.size,
                               .memoryTypeIndex = type},
       NULL, &mem));
   CHECK(vkBindImageMemory(dev, image, mem, 0));
   VkMemoryRequirements after_bind;
   vkGetImageMemoryRequirements(dev, image, &after_bind);
   if (after_bind.size != req.size || after_bind.alignment != req.alignment ||
       after_bind.memoryTypeBits != req.memoryTypeBits)
      return 1;
   D(vkGetImageSubresourceLayout);
   for (unsigned p = 0; p < 2; p++) {
      VkImageSubresource sub = {.aspectMask = p ? VK_IMAGE_ASPECT_PLANE_1_BIT
                                                : VK_IMAGE_ASPECT_PLANE_0_BIT};
      VkSubresourceLayout layout;
      vkGetImageSubresourceLayout(dev, image, &sub, &layout);
      size_t offset = (uintptr_t)IOSurfaceGetBaseAddressOfPlane(surface, p) -
                      (uintptr_t)IOSurfaceGetBaseAddress(surface);
      size_t pitch = IOSurfaceGetBytesPerRowOfPlane(surface, p);
      if (layout.offset != offset || layout.rowPitch != pitch ||
          layout.size != pitch * IOSurfaceGetHeightOfPlane(surface, p))
         return 1;
   }

   /* The driver must own both textures and their IOSurface after the caller
    * releases them. */
   [tex release];
   CFRelease(surface);
   VkSamplerYcbcrConversion conversion;
   CHECK(vkCreateSamplerYcbcrConversion(
       dev,
       &(VkSamplerYcbcrConversionCreateInfo){
           .sType = VK_STRUCTURE_TYPE_SAMPLER_YCBCR_CONVERSION_CREATE_INFO,
           .format = fmt,
           .ycbcrModel = VK_SAMPLER_YCBCR_MODEL_CONVERSION_YCBCR_601,
           .ycbcrRange = narrow ? VK_SAMPLER_YCBCR_RANGE_ITU_NARROW
                                : VK_SAMPLER_YCBCR_RANGE_ITU_FULL,
           .components = {VK_COMPONENT_SWIZZLE_IDENTITY,
                          VK_COMPONENT_SWIZZLE_IDENTITY,
                          VK_COMPONENT_SWIZZLE_IDENTITY,
                          VK_COMPONENT_SWIZZLE_IDENTITY},
           .xChromaOffset = VK_CHROMA_LOCATION_MIDPOINT,
           .yChromaOffset = VK_CHROMA_LOCATION_MIDPOINT,
           .chromaFilter = VK_FILTER_NEAREST},
       NULL, &conversion));
   VkSamplerYcbcrConversionInfo ci = {
       .sType = VK_STRUCTURE_TYPE_SAMPLER_YCBCR_CONVERSION_INFO,
       .conversion = conversion};
   VkSampler sampler;
   CHECK(vkCreateSampler(
       dev,
       &(VkSamplerCreateInfo){
           .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
           .pNext = &ci,
           .magFilter = VK_FILTER_NEAREST,
           .minFilter = VK_FILTER_NEAREST,
           .mipmapMode = VK_SAMPLER_MIPMAP_MODE_NEAREST,
           .addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
           .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
           .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
           .maxLod = 0},
       NULL, &sampler));
   VkImageView view;
   CHECK(vkCreateImageView(
       dev,
       &(VkImageViewCreateInfo){
           .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
           .pNext = &ci,
           .image = image,
           .viewType = VK_IMAGE_VIEW_TYPE_2D,
           .format = fmt,
           .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}},
       NULL, &view));
   VkBuffer out;
   CHECK(vkCreateBuffer(
       dev,
       &(VkBufferCreateInfo){.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                             .size = 256,
                             .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT},
       NULL, &out));
   vkGetBufferMemoryRequirements(dev, out, &req);
   for (type = 0; type < mp.memoryTypeCount; type++)
      if ((req.memoryTypeBits & (1u << type)) &&
          (mp.memoryTypes[type].propertyFlags &
           VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT))
         break;
   VkDeviceMemory om;
   CHECK(vkAllocateMemory(
       dev,
       &(VkMemoryAllocateInfo){.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                               .allocationSize = req.size,
                               .memoryTypeIndex = type},
       NULL, &om));
   CHECK(vkBindBufferMemory(dev, out, om, 0));
   float *pixels;
   CHECK(vkMapMemory(dev, om, 0, VK_WHOLE_SIZE, 0, (void **)&pixels));
   VkDescriptorSetLayoutBinding bindings[2] = {
       {.binding = 0,
        .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
        .descriptorCount = 1,
        .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
        .pImmutableSamplers = &sampler},
       {.binding = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
        .descriptorCount = 1,
        .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT}};
   VkDescriptorSetLayout sl;
   CHECK(vkCreateDescriptorSetLayout(
       dev,
       &(VkDescriptorSetLayoutCreateInfo){
           .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
           .bindingCount = 2,
           .pBindings = bindings},
       NULL, &sl));
   VkPipelineLayout pl;
   CHECK(vkCreatePipelineLayout(
       dev,
       &(VkPipelineLayoutCreateInfo){
           .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
           .setLayoutCount = 1,
           .pSetLayouts = &sl},
       NULL, &pl));
   VkShaderModule shader;
   CHECK(vkCreateShaderModule(
       dev,
       &(VkShaderModuleCreateInfo){
           .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
           .codeSize = sizeof(nv12_spv),
           .pCode = nv12_spv},
       NULL, &shader));
   VkPipeline pipeline;
   CHECK(vkCreateComputePipelines(
       dev, VK_NULL_HANDLE, 1,
       &(VkComputePipelineCreateInfo){
           .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
           .stage = {.sType =
                         VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                     .stage = VK_SHADER_STAGE_COMPUTE_BIT,
                     .module = shader,
                     .pName = "main"},
           .layout = pl},
       NULL, &pipeline));
   VkDescriptorPoolSize sizes[2] = {
       {VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 2},
       {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1}};
   VkDescriptorPool dp;
   CHECK(vkCreateDescriptorPool(
       dev,
       &(VkDescriptorPoolCreateInfo){
           .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
           .maxSets = 1,
           .poolSizeCount = 2,
           .pPoolSizes = sizes},
       NULL, &dp));
   VkDescriptorSet ds;
   CHECK(vkAllocateDescriptorSets(
       dev,
       &(VkDescriptorSetAllocateInfo){
           .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
           .descriptorPool = dp,
           .descriptorSetCount = 1,
           .pSetLayouts = &sl},
       &ds));
   VkDescriptorImageInfo ii = {.imageView = view,
                               .imageLayout =
                                   VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL};
   VkDescriptorBufferInfo bi = {out, 0, 256};
   VkWriteDescriptorSet writes[2] = {
       {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
        .dstSet = ds,
        .dstBinding = 0,
        .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
        .pImageInfo = &ii},
       {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
        .dstSet = ds,
        .dstBinding = 1,
        .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
        .pBufferInfo = &bi}};
   vkUpdateDescriptorSets(dev, 2, writes, 0, NULL);
   VkCommandBuffer cmd;
   CHECK(vkAllocateCommandBuffers(
       dev,
       &(VkCommandBufferAllocateInfo){
           .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
           .commandPool = pool,
           .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
           .commandBufferCount = 1},
       &cmd));
   CHECK(vkBeginCommandBuffer(
       cmd, &(VkCommandBufferBeginInfo){
                .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO}));
   VkImageMemoryBarrier ib = {
       .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
       .srcAccessMask = VK_ACCESS_HOST_WRITE_BIT,
       .dstAccessMask = VK_ACCESS_SHADER_READ_BIT,
       .oldLayout = VK_IMAGE_LAYOUT_PREINITIALIZED,
       .newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
       .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
       .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
       .image = image,
       .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_HOST_BIT,
                        VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0, 0, NULL, 0,
                        NULL, 1, &ib);
   vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline);
   vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds,
                           0, NULL);
   vkCmdDispatch(cmd, 4, 4, 1);
   VkMemoryBarrier mb = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
                         .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
                         .dstAccessMask = VK_ACCESS_HOST_READ_BIT};
   vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                        VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &mb, 0, NULL, 0,
                        NULL);
   CHECK(vkEndCommandBuffer(cmd));
   VkFence fence;
   CHECK(vkCreateFence(
       dev, &(VkFenceCreateInfo){.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO},
       NULL, &fence));
   CHECK(vkQueueSubmit(queue, 1,
                       &(VkSubmitInfo){.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                                       .commandBufferCount = 1,
                                       .pCommandBuffers = &cmd},
                       fence));
   CHECK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 10000000000ull));
   CHECK(vkInvalidateMappedMemoryRanges(
       dev, 1,
       &(VkMappedMemoryRange){.sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
                              .memory = om,
                              .size = VK_WHOLE_SIZE}));
   for (int i = 0; i < 16; i++) {
      float y = (80 + (i / 4) * 20 + (i % 4) * 7 - (narrow ? 16 : 0)) /
                (narrow ? 219.0 : 255.0);
      float cb = (40 + ((i % 4) / 2) * 48 - 128) / (narrow ? 224.0 : 255.0);
      float cr = (140 + ((i / 4) / 2) * 40 - 128) / (narrow ? 224.0 : 255.0);
      float expected[4] = {y + 1.402 * cr, y - 0.344136 * cb - 0.714136 * cr,
                           y + 1.772 * cb, 1};
      for (int c = 0; c < 4; c++)
         if (!isfinite(pixels[i * 4 + c]) ||
             fabsf(pixels[i * 4 + c] - expected[c]) > 0.015) {
            printf("pixel %d channel %d got %f expected %f\n", i, c,
                   pixels[i * 4 + c], expected[c]);
            return 1;
         }
   }
   printf("joined NV12 %s YCbCr sampling passed all 16 pixels after fence; "
          "first=%f,%f,%f,%f\n",
          narrow ? "narrow" : "full", pixels[0], pixels[1], pixels[2],
          pixels[3]);
   D(vkUnmapMemory);
   D(vkDestroyFence);
   D(vkFreeCommandBuffers);
   D(vkDestroyDescriptorPool);
   D(vkDestroyPipeline);
   D(vkDestroyShaderModule);
   D(vkDestroyPipelineLayout);
   D(vkDestroyDescriptorSetLayout);
   D(vkDestroyImageView);
   D(vkDestroySampler);
   D(vkDestroySamplerYcbcrConversion);
   D(vkDestroyImage);
   D(vkDestroyBuffer);
   D(vkFreeMemory);
   vkUnmapMemory(dev, om);
   vkDestroyFence(dev, fence, NULL);
   vkFreeCommandBuffers(dev, pool, 1, &cmd);
   vkDestroyDescriptorPool(dev, dp, NULL);
   vkDestroyPipeline(dev, pipeline, NULL);
   vkDestroyShaderModule(dev, shader, NULL);
   vkDestroyPipelineLayout(dev, pl, NULL);
   vkDestroyDescriptorSetLayout(dev, sl, NULL);
   vkDestroyImageView(dev, view, NULL);
   vkDestroySampler(dev, sampler, NULL);
   vkDestroySamplerYcbcrConversion(dev, conversion, NULL);
   vkDestroyImage(dev, image, NULL);
   vkFreeMemory(dev, mem, NULL);
   vkDestroyBuffer(dev, out, NULL);
   vkFreeMemory(dev, om, NULL);
   return 0;
}
int main(void) {
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
          .flags = portability
                       ? VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR
                       : 0,
          .pApplicationInfo =
              &(VkApplicationInfo){.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
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
          .pNext =
              &(VkPhysicalDeviceSamplerYcbcrConversionFeatures){
                  .sType =
                      VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SAMPLER_YCBCR_CONVERSION_FEATURES,
                  .samplerYcbcrConversion = VK_TRUE},
          .queueCreateInfoCount = 1,
          .pQueueCreateInfos =
              &(VkDeviceQueueCreateInfo){
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

      CHECK(run_memory_contract());
      CHECK(run_nv12(false));
      CHECK(run_nv12(true));
      LOAD_D(vkDestroyCommandPool);
      LOAD_D(vkDestroyDevice);
      LOAD_I(vkDestroyInstance);
      vkDestroyCommandPool(dev, pool, NULL);
      vkDestroyDevice(dev, NULL);
      vkDestroyInstance(inst, NULL);
      [mtl release];
      return 0;
   }
}
