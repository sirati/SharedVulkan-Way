/* Tests the actual driver bounds helpers without pretending to emulate WDK. */
#include "RangeBounds.h"
#include <assert.h>
#include <stdint.h>
int main(void)
{
    const uint64_t page = 4096;
    const uint64_t control = 128 * 1024;
    const uint64_t payload = 16 * 1024 * 1024;
    assert(sizeof(WC_RANGE_U64) == 8);
    assert(wc_range_window_ok(control, payload, control + payload, page));
    assert(!wc_range_window_ok(control, payload + page, control + payload, page));
    assert(!wc_range_window_ok(UINT64_MAX - 4095, 4096, UINT64_MAX, page));
    assert(!wc_range_window_ok(1, page, payload, page));
    assert(!wc_range_window_ok(control, page - 1, payload, page));
    assert(!wc_range_inside(0, page, control, payload));
    assert(!wc_range_inside(control - page, 2 * page, control, payload));
    assert(wc_range_inside(control, payload, control, payload));
    assert(!wc_range_inside(control, payload + page, control, payload));
    assert(!wc_range_inside(UINT64_MAX - 7, 16, control, payload));
    assert(!wc_range_inside(UINT64_MAX, UINT64_MAX, 0, payload));
    assert(wc_range_physical_ok(0x80000000ULL, control, payload, page));
    assert(!wc_range_physical_ok(UINT64_MAX, 0, page, page));
    assert(!wc_range_physical_ok(0x8000000000000000ULL, 0, page, page));
    assert(!wc_range_physical_ok(0x7ffffffffffff000ULL, page, page, page));
    assert(!wc_range_physical_ok(0x7ffffffffffff000ULL, 0, page + 1, page));
    assert(wc_range_physical_ok(0x7ffffffffffff000ULL, 0, page, page));
    assert(!wc_range_physical_ok(1, 0, page, page));
    assert(!wc_range_physical_ok(0, 0, 0, page));
    return 0;
}
