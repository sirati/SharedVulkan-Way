#define VK_NO_PROTOTYPES
#include "nv12-sample.h"
#include "nv12_fixture_spv.h"
#include <stdlib.h>
#include <string.h>

#define FUNCTIONS(F)                                                           \
   F(CreateSamplerYcbcrConversion)                                             \
   F(DestroySamplerYcbcrConversion)                                            \
   F(CreateSampler) F(DestroySampler) F(CreateImageView) F(DestroyImageView)   \
       F(CreateBuffer) F(DestroyBuffer) F(GetBufferMemoryRequirements)         \
           F(AllocateMemory) F(FreeMemory) F(BindBufferMemory) F(MapMemory)    \
               F(UnmapMemory) F(CreateDescriptorSetLayout) F(                  \
                   DestroyDescriptorSetLayout) F(CreatePipelineLayout)         \
                   F(DestroyPipelineLayout) F(CreateShaderModule) F(           \
                       DestroyShaderModule) F(CreateComputePipelines)          \
                       F(DestroyPipeline) F(CreateDescriptorPool) F(           \
                           DestroyDescriptorPool) F(AllocateDescriptorSets)    \
                           F(UpdateDescriptorSets) F(CreateCommandPool) F(     \
                               DestroyCommandPool) F(AllocateCommandBuffers)   \
                               F(BeginCommandBuffer) F(CmdPipelineBarrier)     \
                                   F(CmdBindPipeline) F(CmdBindDescriptorSets) \
                                       F(CmdDispatch) F(EndCommandBuffer) F(   \
                                           CreateFence) F(DestroyFence)        \
                                           F(QueueSubmit) F(WaitForFences) F(  \
                                               GetFenceStatus)                 \
                                               F(InvalidateMappedMemoryRanges)
struct kk_nv12_fixture_job {
   VkDevice device;
#define DECLARE(n) PFN_vk##n n;
   FUNCTIONS(DECLARE)
#undef DECLARE
   VkSamplerYcbcrConversion conversion;
   VkSampler sampler;
   VkImageView view;
   VkBuffer buffer;
   VkDeviceMemory memory;
   void *mapped;
   VkDescriptorSetLayout set_layout;
   VkPipelineLayout pipeline_layout;
   VkShaderModule shader;
   VkPipeline pipeline;
   VkDescriptorPool descriptor_pool;
   VkCommandPool command_pool;
   VkFence fence;
};
static void cleanup(struct kk_nv12_fixture_job *j) {
   VkDevice d = j->device;
   if (j->fence)
      j->DestroyFence(d, j->fence, NULL);
   if (j->command_pool)
      j->DestroyCommandPool(d, j->command_pool, NULL);
   if (j->descriptor_pool)
      j->DestroyDescriptorPool(d, j->descriptor_pool, NULL);
   if (j->pipeline)
      j->DestroyPipeline(d, j->pipeline, NULL);
   if (j->shader)
      j->DestroyShaderModule(d, j->shader, NULL);
   if (j->pipeline_layout)
      j->DestroyPipelineLayout(d, j->pipeline_layout, NULL);
   if (j->set_layout)
      j->DestroyDescriptorSetLayout(d, j->set_layout, NULL);
   if (j->view)
      j->DestroyImageView(d, j->view, NULL);
   if (j->sampler)
      j->DestroySampler(d, j->sampler, NULL);
   if (j->conversion)
      j->DestroySamplerYcbcrConversion(d, j->conversion, NULL);
   if (j->mapped)
      j->UnmapMemory(d, j->memory);
   if (j->buffer)
      j->DestroyBuffer(d, j->buffer, NULL);
   if (j->memory)
      j->FreeMemory(d, j->memory, NULL);
   free(j);
}
#define TRY(call)                                                              \
   do {                                                                        \
      result = (call);                                                         \
      if (result != VK_SUCCESS)                                                \
         goto fail;                                                            \
   } while (0)
VkResult kk_nv12_fixture_start(const struct kk_nv12_fixture_info *i,
                               struct kk_nv12_fixture_job **out_job) {
   if (!out_job)
      return VK_ERROR_INITIALIZATION_FAILED;
   *out_job = NULL;
   if (!i || !i->get_device_proc_addr || !i->device || !i->queue || !i->image ||
       !i->memory_properties || !i->src_stage ||
       i->memory_properties->memoryTypeCount > VK_MAX_MEMORY_TYPES)
      return VK_ERROR_INITIALIZATION_FAILED;
   struct kk_nv12_fixture_job *j = calloc(1, sizeof(*j));
   if (!j)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   j->device = i->device;
#define LOAD(n)                                                                \
   j->n = (PFN_vk##n)i->get_device_proc_addr(i->device, "vk" #n);              \
   if (!j->n) {                                                                \
      free(j);                                                                 \
      return VK_ERROR_INITIALIZATION_FAILED;                                   \
   }
   FUNCTIONS(LOAD)
#undef LOAD
   VkResult result;
   TRY(j->CreateSamplerYcbcrConversion(
       j->device,
       &(VkSamplerYcbcrConversionCreateInfo){
           .sType = VK_STRUCTURE_TYPE_SAMPLER_YCBCR_CONVERSION_CREATE_INFO,
           .format = VK_FORMAT_G8_B8R8_2PLANE_420_UNORM,
           .ycbcrModel = i->model,
           .ycbcrRange = i->range,
           .xChromaOffset = i->x_chroma,
           .yChromaOffset = i->y_chroma,
           .chromaFilter = VK_FILTER_NEAREST},
       NULL, &j->conversion));
   VkSamplerYcbcrConversionInfo conversion = {
       .sType = VK_STRUCTURE_TYPE_SAMPLER_YCBCR_CONVERSION_INFO,
       .conversion = j->conversion};
   TRY(j->CreateSampler(
       j->device,
       &(VkSamplerCreateInfo){
           .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
           .pNext = &conversion,
           .magFilter = VK_FILTER_NEAREST,
           .minFilter = VK_FILTER_NEAREST,
           .mipmapMode = VK_SAMPLER_MIPMAP_MODE_NEAREST,
           .addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
           .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
           .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE},
       NULL, &j->sampler));
   TRY(j->CreateImageView(
       j->device,
       &(VkImageViewCreateInfo){
           .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
           .pNext = &conversion,
           .image = i->image,
           .viewType = VK_IMAGE_VIEW_TYPE_2D,
           .format = VK_FORMAT_G8_B8R8_2PLANE_420_UNORM,
           .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}},
       NULL, &j->view));
   TRY(j->CreateBuffer(
       j->device,
       &(VkBufferCreateInfo){.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                             .size = 256,
                             .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT},
       NULL, &j->buffer));
   VkMemoryRequirements req;
   j->GetBufferMemoryRequirements(j->device, j->buffer, &req);
   unsigned type;
   for (type = 0; type < i->memory_properties->memoryTypeCount; type++)
      if ((req.memoryTypeBits & (1u << type)) &&
          (i->memory_properties->memoryTypes[type].propertyFlags &
           VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT))
         break;
   if (type == i->memory_properties->memoryTypeCount) {
      result = VK_ERROR_FEATURE_NOT_PRESENT;
      goto fail;
   }
   TRY(j->AllocateMemory(
       j->device,
       &(VkMemoryAllocateInfo){.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                               .allocationSize = req.size,
                               .memoryTypeIndex = type},
       NULL, &j->memory));
   TRY(j->BindBufferMemory(j->device, j->buffer, j->memory, 0));
   TRY(j->MapMemory(j->device, j->memory, 0, VK_WHOLE_SIZE, 0, &j->mapped));
   VkDescriptorSetLayoutBinding bindings[2] = {
       {.binding = 0,
        .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
        .descriptorCount = 1,
        .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
        .pImmutableSamplers = &j->sampler},
       {.binding = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
        .descriptorCount = 1,
        .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT}};
   TRY(j->CreateDescriptorSetLayout(
       j->device,
       &(VkDescriptorSetLayoutCreateInfo){
           .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
           .bindingCount = 2,
           .pBindings = bindings},
       NULL, &j->set_layout));
   TRY(j->CreatePipelineLayout(
       j->device,
       &(VkPipelineLayoutCreateInfo){
           .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
           .setLayoutCount = 1,
           .pSetLayouts = &j->set_layout},
       NULL, &j->pipeline_layout));
   TRY(j->CreateShaderModule(
       j->device,
       &(VkShaderModuleCreateInfo){
           .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
           .codeSize = sizeof(kk_nv12_fixture_spv),
           .pCode = kk_nv12_fixture_spv},
       NULL, &j->shader));
   TRY(j->CreateComputePipelines(
       j->device, VK_NULL_HANDLE, 1,
       &(VkComputePipelineCreateInfo){
           .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
           .stage = {.sType =
                         VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                     .stage = VK_SHADER_STAGE_COMPUTE_BIT,
                     .module = j->shader,
                     .pName = "main"},
           .layout = j->pipeline_layout},
       NULL, &j->pipeline));
   VkDescriptorPoolSize sizes[2] = {
       {VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 2},
       {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1}};
   TRY(j->CreateDescriptorPool(
       j->device,
       &(VkDescriptorPoolCreateInfo){
           .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
           .maxSets = 1,
           .poolSizeCount = 2,
           .pPoolSizes = sizes},
       NULL, &j->descriptor_pool));
   VkDescriptorSet ds;
   TRY(j->AllocateDescriptorSets(
       j->device,
       &(VkDescriptorSetAllocateInfo){
           .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
           .descriptorPool = j->descriptor_pool,
           .descriptorSetCount = 1,
           .pSetLayouts = &j->set_layout},
       &ds));
   VkDescriptorImageInfo image = {.imageView = j->view,
                                  .imageLayout =
                                      VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL};
   VkDescriptorBufferInfo buffer = {j->buffer, 0, 256};
   VkWriteDescriptorSet writes[2] = {
       {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
        .dstSet = ds,
        .dstBinding = 0,
        .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
        .pImageInfo = &image},
       {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
        .dstSet = ds,
        .dstBinding = 1,
        .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
        .pBufferInfo = &buffer}};
   j->UpdateDescriptorSets(j->device, 2, writes, 0, NULL);
   TRY(j->CreateCommandPool(
       j->device,
       &(VkCommandPoolCreateInfo){
           .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
           .queueFamilyIndex = i->queue_family},
       NULL, &j->command_pool));
   VkCommandBuffer cmd;
   TRY(j->AllocateCommandBuffers(
       j->device,
       &(VkCommandBufferAllocateInfo){
           .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
           .commandPool = j->command_pool,
           .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
           .commandBufferCount = 1},
       &cmd));
   TRY(j->BeginCommandBuffer(
       cmd, &(VkCommandBufferBeginInfo){
                .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO}));
   VkImageMemoryBarrier ib = {
       .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
       .srcAccessMask = i->src_access,
       .dstAccessMask = VK_ACCESS_SHADER_READ_BIT,
       .oldLayout = i->old_layout,
       .newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
       .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
       .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
       .image = i->image,
       .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
   j->CmdPipelineBarrier(cmd, i->src_stage,
                         VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0, 0, NULL, 0,
                         NULL, 1, &ib);
   j->CmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, j->pipeline);
   j->CmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE,
                            j->pipeline_layout, 0, 1, &ds, 0, NULL);
   j->CmdDispatch(cmd, 4, 4, 1);
   VkMemoryBarrier mb = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
                         .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
                         .dstAccessMask = VK_ACCESS_HOST_READ_BIT};
   j->CmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                         VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &mb, 0, NULL, 0,
                         NULL);
   TRY(j->EndCommandBuffer(cmd));
   TRY(j->CreateFence(
       j->device,
       &(VkFenceCreateInfo){.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO}, NULL,
       &j->fence));
   result =
       j->QueueSubmit(i->queue, 1,
                      &(VkSubmitInfo){.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                                      .commandBufferCount = 1,
                                      .pCommandBuffers = &cmd},
                      j->fence);
   /* A failed submission may leave device execution state uncertain. Retain
    * every resource and expose the job instead of freeing possible GPU users.
    */
   *out_job = j;
   return result;
fail:
   cleanup(j);
   return result;
}
VkResult kk_nv12_fixture_read(struct kk_nv12_fixture_job *j, uint64_t timeout,
                              float rgba[64]) {
   if (!j || !rgba)
      return VK_ERROR_INITIALIZATION_FAILED;
   VkResult result =
       j->WaitForFences(j->device, 1, &j->fence, VK_TRUE, timeout);
   if (result != VK_SUCCESS)
      return result;
   result = j->InvalidateMappedMemoryRanges(
       j->device, 1,
       &(VkMappedMemoryRange){.sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
                              .memory = j->memory,
                              .size = VK_WHOLE_SIZE});
   if (result != VK_SUCCESS)
      return result;
   memcpy(rgba, j->mapped, 256);
   return VK_SUCCESS;
}
VkResult kk_nv12_fixture_destroy(struct kk_nv12_fixture_job *j) {
   if (!j)
      return VK_SUCCESS;
   VkResult result = j->GetFenceStatus(j->device, j->fence);
   if (result != VK_SUCCESS)
      return result;
   cleanup(j);
   return VK_SUCCESS;
}
