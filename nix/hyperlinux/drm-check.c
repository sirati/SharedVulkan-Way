#include "wc_hyperlinux.h"
#include <assert.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
static unsigned char memory[4096];
static int32_t read_memory(void *ctx, uint64_t addr, uint8_t *out, uint64_t len) {
    (void)ctx;
    if (!addr || addr >= sizeof(memory) || len > sizeof(memory)-addr) return -EFAULT;
    memcpy(out, memory+addr, len); return 0;
}
static int32_t write_memory(void *ctx, uint64_t addr, const uint8_t *data, uint64_t len) {
    (void)ctx;
    if (!addr || addr >= sizeof(memory) || len > sizeof(memory)-addr) return -EFAULT;
    memcpy(memory+addr, data, len); return 0;
}
static int32_t token(void *ctx, int32_t fd, uint8_t *out, uint64_t cap) {
    (void)ctx;
    if (fd != 123 || cap < 48) return -EBADF;
    memset(out, 0, 48); memcpy(out, "SVDMABF1", 8);
    out[8]=1; out[12]=48; out[17]=32; memset(out+24, 7, 16); out[40]=4;
    memcpy(out+44, "/okx", 4); return 48;
}
static int32_t relay(void *ctx, int32_t fd, const uint8_t *name, uint64_t len) {
    (void)ctx;
    return fd == 124 && len == 8 && memcmp(name, "sv-gpu:x", 8) == 0 ? 0 : -ENOENT;
}
static void put64(unsigned off, uint64_t value) { memcpy(memory+off, &value, 8); }
static void put32(unsigned off, uint32_t value) { memcpy(memory+off, &value, 4); }
static uint64_t get64(unsigned off) { uint64_t value; memcpy(&value, memory+off, 8); return value; }
static uint32_t get32(unsigned off) { uint32_t value; memcpy(&value, memory+off, 4); return value; }
int main(void) {
    wc_hl_drm *drm = 0;
    assert(wc_hl_drm_open(1000, 4321, &drm) == 0 && drm);
    wc_hl_drm_callbacks cb = { 0, read_memory, write_memory, token, relay };
    put64(64+16, 8); put64(64+24, 256);
    put64(64+32, 8); put64(64+40, 272);
    put64(64+48, 32); put64(64+56, 288);
    assert(wc_hl_drm_ioctl(drm, 1000, 4321, 0xc0406400, 64, cb) == 0);
    assert(get32(64) == 1 && get64(64+16) == 5 && memcmp(memory+256, "svdrm", 5) == 0);
    assert(wc_hl_drm_ioctl(drm, 1000, 4321, 0xc0406400, 4092, cb) == -EFAULT);
    memset(memory+64, 0, 128); put64(64, 5);
    assert(wc_hl_drm_ioctl(drm, 1000, 4321, 0xc010640c, 64, cb) == 0 && get64(72) == 3);
    put32(64+8, 123);
    assert(wc_hl_drm_ioctl(drm, 1000, 4321, 0xc00c642e, 64, cb) == 0 && get32(64) == 1);
    assert(wc_hl_drm_ioctl(drm, 1000, 4321, 0xc00c642e, 64, cb) == 0 && get32(64) == 1);
    put32(68, 0);
    assert(wc_hl_drm_ioctl(drm, 1000, 4321, 0x40086409, 64, cb) == 0);
    assert(wc_hl_drm_ioctl(drm, 1000, 4321, 0x40086409, 64, cb) == -EINVAL);
    put32(64, 124); memset(memory+68, 0, 64); memcpy(memory+68, "sv-gpu:x", 8);
    assert(wc_hl_drm_ioctl(drm, 1000, 4321, 0x40446440, 64, cb) == 0);
    assert(wc_hl_drm_ioctl(drm, 1000, 4321, 0xffffffff, 64, cb) == -ENOTTY);
    wc_hl_drm_release(drm);
    puts("embedded C ABI DRM version, bounds, caps, PRIME handles and relay passed");
}
