/* Test-only joined NV12 sampling. No image import or production protocol. */
#ifndef KK_NV12_FIXTURE_H
#define KK_NV12_FIXTURE_H
#include <vulkan/vulkan.h>
#ifdef __cplusplus
extern "C" {
#endif
struct kk_nv12_fixture_job;
struct kk_nv12_fixture_info {
   PFN_vkGetDeviceProcAddr get_device_proc_addr;
   VkDevice device;
   VkQueue queue;
   uint32_t queue_family;
   const VkPhysicalDeviceMemoryProperties *memory_properties;
   VkImage image;
   VkImageLayout old_layout;
   VkPipelineStageFlags src_stage;
   VkAccessFlags src_access;
   VkSamplerYcbcrModelConversion model;
   VkSamplerYcbcrRange range;
   VkChromaLocation x_chroma;
   VkChromaLocation y_chroma;
};
/* The supplied bound image must be NV12, sampled, 2D, one mip/layer/sample.
 * Its producer must have completed and this queue must own it. Caller retains
 * image/device/queue and the decoder picture until read/destroy succeeds.
 * Caller externally synchronizes queue access during start. If a submission
 * fails, start returns its error with a non-NULL job; retain job and image
 * until completion or terminate the isolated test process without tearing
 * down resources that may still be in use. Earlier errors return a NULL job.
 * The image transitions to SHADER_READ_ONLY_OPTIMAL and stays in that layout.
 * memory_properties is used only during start. No caller pointer is retained.
 */
VkResult kk_nv12_fixture_start(const struct kk_nv12_fixture_info *info,
                               struct kk_nv12_fixture_job **out_job);
/* Samples a 4x4 normalized grid, returning 16 RGBA float pixels, row-major.
 * A timeout/error leaves the job alive. Retry or retain it until completion.
 * The caller checks pixels against its own decoded-frame oracle.
 */
VkResult kk_nv12_fixture_read(struct kk_nv12_fixture_job *job,
                              uint64_t timeout_ns, float rgba[64]);
/* Returns NOT_READY without freeing pending resources. Never destroys the
 * caller's image/device/queue. A successful destroy consumes the job.
 */
VkResult kk_nv12_fixture_destroy(struct kk_nv12_fixture_job *job);
#ifdef __cplusplus
}
#endif
#endif
