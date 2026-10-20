/* va-export-probe: what a VA-API driver hands out for a decode surface
 * (waycompose-design.md §112). Opens the render node, creates one NV12
 * surface of the size given, exports it as Firefox does
 * (vaExportSurfaceHandle, DRM_PRIME_2, separate layers, read-only) and prints
 * every object and layer. With `decode` it first decodes nothing but asks the
 * driver for an H.264 VLD config and context on that surface, which is what
 * makes Zink create its video image.
 *
 *   va-export-probe /dev/dri/renderD128 640 360 [decode]
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>

int main(int argc, char **argv) {
  if (argc < 4) { fprintf(stderr, "usage: %s NODE W H [decode]\n", argv[0]); return 2; }
  int w = atoi(argv[2]), h = atoi(argv[3]);
  int fd = open(argv[1], O_RDWR | O_CLOEXEC);
  if (fd < 0) { perror(argv[1]); return 1; }
  VADisplay dpy = vaGetDisplayDRM(fd);
  int maj, min;
  VAStatus s = vaInitialize(dpy, &maj, &min);
  if (s != VA_STATUS_SUCCESS) { printf("vaInitialize: %s\n", vaErrorStr(s)); return 1; }
  printf("vendor: %s\n", vaQueryVendorString(dpy));
  VASurfaceID surf;
  s = vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, w, h, &surf, 1, NULL, 0);
  if (s != VA_STATUS_SUCCESS) { printf("vaCreateSurfaces: %s\n", vaErrorStr(s)); return 1; }
  if (argc > 4 && !strcmp(argv[4], "decode")) {
    VAConfigID cfg; VAContextID ctx;
    s = vaCreateConfig(dpy, VAProfileH264High, VAEntrypointVLD, NULL, 0, &cfg);
    if (s != VA_STATUS_SUCCESS) { printf("vaCreateConfig: %s\n", vaErrorStr(s)); return 1; }
    s = vaCreateContext(dpy, cfg, w, h, VA_PROGRESSIVE, &surf, 1, &ctx);
    if (s != VA_STATUS_SUCCESS) { printf("vaCreateContext: %s\n", vaErrorStr(s)); return 1; }
    printf("decode context made\n");
  }
  VADRMPRIMESurfaceDescriptor d;
  memset(&d, 0, sizeof d);
  s = vaExportSurfaceHandle(dpy, surf, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                            VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS, &d);
  if (s != VA_STATUS_SUCCESS) { printf("vaExportSurfaceHandle: %s\n", vaErrorStr(s)); return 1; }
  printf("export: fourcc %.4s %ux%u, %u object(s), %u layer(s)\n", (char *)&d.fourcc, d.width, d.height,
         d.num_objects, d.num_layers);
  for (unsigned i = 0; i < d.num_objects; i++) {
    char magic[8] = {0};
    pread(d.objects[i].fd, magic, sizeof magic, 0);
    printf("  object %u: fd %d size %u modifier 0x%llx header %.8s\n", i, d.objects[i].fd, d.objects[i].size,
           (unsigned long long)d.objects[i].drm_format_modifier, magic);
  }
  for (unsigned i = 0; i < d.num_layers; i++)
    printf("  layer %u: format %.4s, %u plane(s): object %u offset %u pitch %u\n", i, (char *)&d.layers[i].drm_format,
           d.layers[i].num_planes, d.layers[i].object_index[0], d.layers[i].offset[0], d.layers[i].pitch[0]);
  printf("va-export-probe: ok\n");
  return 0;
}
