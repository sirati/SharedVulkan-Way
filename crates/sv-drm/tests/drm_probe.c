/* What libdrm makes of a device number: run inside `sv-drm ns`.
 *   drm_probe MAJ MIN
 * Prints the drmGetDeviceFromDevId result; exit 0 iff it is a render node on
 * bus faux named "svdrm". Build: cc drm_probe.c $(pkg-config --cflags --libs libdrm)
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sysmacros.h>
#include <xf86drm.h>

int main(int argc, char **argv)
{
    if (argc != 3) {
        fprintf(stderr, "usage: %s MAJ MIN\n", argv[0]);
        return 2;
    }
    unsigned maj = strtoul(argv[1], NULL, 0), min = strtoul(argv[2], NULL, 0);
    drmDevicePtr dev = NULL;
    int r = drmGetDeviceFromDevId(makedev(maj, min), 0, &dev);
    printf("drmGetDeviceFromDevId(%u:%u) = %d (%s)\n", maj, min, r, r ? strerror(-r) : "ok");
    if (r || !dev) {
        printf("device = %p\n", (void *)dev);
        return 1;
    }
    printf("bustype = %d (DRM_BUS_FAUX = %d)\n", dev->bustype, DRM_BUS_FAUX);
    printf("available_nodes = %#x\n", dev->available_nodes);
    for (int i = 0; i < DRM_NODE_MAX; i++)
        if (dev->available_nodes & (1 << i))
            printf("nodes[%d] = %s\n", i, dev->nodes[i]);
    int ok = dev->bustype == DRM_BUS_FAUX && (dev->available_nodes & (1 << DRM_NODE_RENDER));
    if (dev->bustype == DRM_BUS_FAUX) {
        printf("faux name = %s\n", dev->businfo.faux->name);
        ok = ok && strcmp(dev->businfo.faux->name, "svdrm") == 0;
    }
    drmFreeDevice(&dev);
    printf("%s\n", ok ? "PASS" : "FAIL");
    return ok ? 0 : 1;
}
