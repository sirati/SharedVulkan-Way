/* SPDX-License-Identifier: MIT
 * Runtime contract test, separate from the unchanged application proof.
 */
#include "wc-local-client.h"
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
extern int os_create_anonymous_file(off_t);
extern size_t wc_native_fd_word_count(int);
extern int wc_native_fd_words(int, uint32_t *, size_t);
extern int wc_native_fd_from_words(const uint32_t *, size_t);
int main(void) {
  uint32_t before[3], after[3];
  assert(wc_local_counts(before) == 0);
  int owner = os_create_anonymous_file(65536);
  assert(owner >= 0);
  uint8_t *pixels =
      mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_SHARED, owner, 0);
  assert(pixels != MAP_FAILED);
  pixels[0] = 0x73;
  pixels[65535] = 0xa1;
  int alias = dup(owner);
  assert(alias >= 0);
  assert(close(owner) == 0);
  size_t count = wc_native_fd_word_count(alias);
  assert(count > 1 && count <= 1025);
  uint32_t *words = calloc(count, 4);
  assert(words);
  assert(wc_native_fd_words(alias, words, count) == 0);
  assert(close(alias) == 0);
  int consumer = wc_native_fd_from_words(words, count);
  assert(consumer >= 0);
  free(words);
  const uint8_t *read = mmap(NULL, 65536, PROT_READ, MAP_SHARED, consumer, 0);
  assert(read != MAP_FAILED);
  assert(read[0] == 0x73 && read[65535] == 0xa1);
  assert(mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_SHARED, consumer, 0) ==
         MAP_FAILED);
  assert(munmap(pixels, 65536) == 0);
  assert(close(consumer) == 0);
  assert(read[0] == 0x73 && read[65535] == 0xa1);
  assert(munmap((void *)read, 65536) == 0);
  assert(wc_local_counts(after) == 0);
  assert(before[0] == after[0] && before[1] == after[1] &&
         before[2] == after[2]);
  puts("LOCAL_SECTION_POSIX_OK bytes=65536 named_section=1 aliases=1 "
       "retained_views=1 read_only_import=1 zero_retained=1");
  return 0;
}
