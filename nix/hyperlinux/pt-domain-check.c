/* Uses the production PT walker and domain lock. A reader deliberately stops
 * after fetching an L3 pointer while an unmap/reuse writer tries to proceed. */
#include "hl_pt_access.h"
#include "hl_pt_domain.h"
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
static uint64_t tables[4][512];
static pthread_mutex_t gate = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;
static int park_reader = 1, reader_inside, writer_started, release_reader;
static const uint64_t rw_leaf = 0x800000 | 3 | (1 << 10) | (1 << 6);
static const uint64_t ro_leaf = 0x900000 | 3 | (1 << 10) | (3 << 6);
static const uint64_t poison_leaf = 0xc00000 | 3 | (1 << 10) | (1 << 6);
static const uint64_t *fetch(const void *unused, uint64_t ipa) {
    (void)unused;
    if (ipa < 0x1000 || ipa >= 0x5000 || (ipa & 4095)) return NULL;
    if (ipa == 0x4000) {
        pthread_mutex_lock(&gate);
        if (park_reader) {
            park_reader = 0; reader_inside = 1;
            pthread_cond_broadcast(&changed);
            while (!release_reader) pthread_cond_wait(&changed, &gate);
        }
        pthread_mutex_unlock(&gate);
    }
    return tables[ipa / 4096 - 1];
}
static int translate(int write, uint64_t *ipa) {
    uint64_t available;
    return hl_pt_access_translate(NULL, fetch, 0x1000, 0x401123,
                                   write, 0, ipa, &available);
}
static void reused_table_reset(void) {
    HL_PT_WRITE_SCOPE; /* Test the same nested writer pattern as split/alloc. */
    memset(tables[3], 0, sizeof(tables[3]));
    tables[3][1] = rw_leaf;
}
static void publish_rw(void) {
    HL_PT_WRITE_SCOPE;
    reused_table_reset();
    tables[2][2] = 0x4003;
}
static void protect_readonly(void) {
    HL_PT_WRITE_SCOPE;
    tables[3][1] = ro_leaf;
}
static void unmap_and_reuse(void) {
    HL_PT_WRITE_SCOPE;
    tables[2][2] = 0;
    /* Reuse the old L3 storage for an unrelated valid table while its old
     * parent is invalid. A stale L3 pointer must never observe this leaf. */
    tables[3][1] = poison_leaf;
}
static void *first_reader(void *unused) {
    (void)unused;
    HL_PT_READ_SCOPE;
    uint64_t ipa;
    assert(!translate(1, &ipa) && ipa == 0x800123);
    return NULL;
}
static void *first_writer(void *unused) {
    (void)unused;
    pthread_mutex_lock(&gate); writer_started = 1;
    pthread_cond_broadcast(&changed); pthread_mutex_unlock(&gate);
    unmap_and_reuse();
    return NULL;
}
static void *stress_reader(void *unused) {
    (void)unused;
    for (int i = 0; i < 25000; i++) {
        HL_PT_READ_SCOPE;
        uint64_t read_ipa, write_ipa;
        int read_result = translate(0, &read_ipa);
        int write_result = translate(1, &write_ipa);
        if (!read_result) assert(read_ipa == 0x800123 || read_ipa == 0x900123);
        if (!write_result) assert(write_ipa == 0x800123 && !read_result);
        if (read_result) assert(write_result);
    }
    return NULL;
}
static void *stress_writer(void *unused) {
    (void)unused;
    for (int i = 0; i < 5000; i++) {
        publish_rw(); protect_readonly(); unmap_and_reuse();
    }
    return NULL;
}
int main(void) {
    tables[0][0] = 0x2003; tables[1][0] = 0x3003;
    publish_rw();
    pthread_t reader, writer;
    assert(!pthread_create(&reader, NULL, first_reader, NULL));
    pthread_mutex_lock(&gate);
    while (!reader_inside) pthread_cond_wait(&changed, &gate);
    pthread_mutex_unlock(&gate);
    assert(!pthread_create(&writer, NULL, first_writer, NULL));
    pthread_mutex_lock(&gate);
    while (!writer_started) pthread_cond_wait(&changed, &gate);
    assert(pthread_rwlock_trywrlock(&hl_pt_domain_lock) == EBUSY);
    release_reader = 1; pthread_cond_broadcast(&changed);
    pthread_mutex_unlock(&gate);
    assert(!pthread_join(reader, NULL) && !pthread_join(writer, NULL));
    { HL_PT_READ_SCOPE; uint64_t ipa; assert(translate(0, &ipa) < 0); }
    publish_rw();
    pthread_t readers[2];
    for (int i = 0; i < 2; i++) assert(!pthread_create(&readers[i], NULL, stress_reader, NULL));
    assert(!pthread_create(&writer, NULL, stress_writer, NULL));
    for (int i = 0; i < 2; i++) assert(!pthread_join(readers[i], NULL));
    assert(!pthread_join(writer, NULL));
    puts("pt-domain-check: parked L3 reader excludes reclamation, nested writers and 50000 concurrent permission/unmap/reuse snapshots passed");
}
