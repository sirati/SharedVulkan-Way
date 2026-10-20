/* Exercises actual persistent metadata/locking, including a distinct process. */
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include "hl_seals.h"
#include "wc_hyperlinux.h"
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
static int create_file(void) {
    char path[] = "/tmp/wc-hl-seal-test-XXXXXX";
    int fd = mkstemp(path); assert(fd >= 0); unlink(path);
    assert(ftruncate(fd, 8192) == 0);
    assert(hl_wc_seal_init(fd, 1) == 0);
    return fd;
}
static int operation(int fd, int op, uint64_t arg) {
    hl_wc_seal_guard g = { .lockfd = -1 };
    int r = hl_wc_seal_begin(fd, op, arg, &g), e = errno;
    hl_wc_seal_end(&g); errno = e;
    return r;
}
int main(void) {
    int fd = create_file(), alias = dup(fd);
    hl_wc_seal_guard g = { .lockfd = -1 };
    uint64_t id;
    assert(hl_wc_seal_begin(fd, 0, 0, &g) == 0);
    assert(hl_wc_seal_track(&g, 4096, 4096, 1, &id) == 0 && id != 0);
    hl_wc_seal_end(&g);
    assert(hl_wc_seal_fcntl(alias, 1033, 8) == -EBUSY);
    assert(hl_wc_seal_fcntl(fd, 1033, 2|4|16) == 0);
    assert(hl_wc_seal_fcntl(alias, 1034, 0) == (2|4|16));
    assert(operation(alias, HL_WC_SEAL_WRITE, 0) < 0 && errno == EPERM);
    assert(hl_wc_write(alias, "X", 1) < 0 && errno == EPERM);
    assert(hl_wc_pwrite(alias, "X", 1, 0) < 0 && errno == EPERM);
    assert(operation(alias, HL_WC_SEAL_MAP, 1) < 0 && errno == EPERM);
    assert(operation(alias, HL_WC_SEAL_MAP, 0) == 0);
    assert(operation(alias, HL_WC_SEAL_RESIZE, 4096) < 0 && errno == EPERM);
    assert(operation(alias, HL_WC_SEAL_RESIZE, 16384) < 0 && errno == EPERM);
    pid_t pid = fork(); assert(pid >= 0);
    if (!pid) {
        assert(hl_wc_seal_fcntl(alias, 1034, 0) == (2|4|16));
        assert(operation(alias, HL_WC_SEAL_WRITE, 0) < 0 && errno == EPERM);
        assert(hl_wc_pwrite(alias, "X", 1, 0) < 0 && errno == EPERM);
        _exit(0);
    }
    int status; assert(waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    hl_wc_seal_untrack(fd, id);
    assert(hl_wc_seal_fcntl(fd, 1033, 8|1) == 0);
    assert(hl_wc_seal_fcntl(alias, 1033, 0) == -EPERM);
    close(fd); close(alias);
    /* A token cannot be registered while a prior mapping can rewrite header. */
    fd = create_file();
    unsigned char token[48] = {0};
    memcpy(token, "SVDMABF1", 8); token[8]=1; token[12]=48; token[17]=32;
    memset(token+24, 7, 16); token[40]=4; memcpy(token+44, "/okx", 4);
    assert(pwrite(fd, token, sizeof(token), 0) == sizeof(token));
    assert(hl_wc_seal_begin(fd, 0, 0, &g) == 0);
    assert(hl_wc_seal_track(&g, 0, 4096, 1, &id) == 0);
    hl_wc_seal_end(&g);
    assert(hl_wc_seal_fcntl(fd, 1033, 1|2|4|16) == -EBUSY);
    hl_wc_seal_untrack(fd, id);
    assert(hl_wc_seal_fcntl(fd, 1033, 1|2|4|16) == 0);
    unsigned char snapshot[448];
    assert(hl_wc_token_snapshot(fd, snapshot, sizeof(snapshot)) == sizeof(token));
    assert(memcmp(snapshot, token, sizeof(token)) == 0);
    /* Simulate a native writer outside the guest ABI. Import must reject it. */
    assert(pwrite(fd, "X", 1, 44) == 1);
    assert(hl_wc_token_snapshot(fd, snapshot, sizeof(snapshot)) == -EINVAL);
    close(fd);
    puts("seal aliases, fork, resize, writable maps and token mutation rejection passed");
}
