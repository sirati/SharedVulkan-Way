/* Pure bounds checks shared by the driver and its portable adversarial tests.
 * Waycompose addition, under LICENSE. No kernel or operating system inputs. */
#pragma once
typedef unsigned long long WC_RANGE_U64;
static int wc_range_window_ok(WC_RANGE_U64 offset, WC_RANGE_U64 length,
                              WC_RANGE_U64 bytes, WC_RANGE_U64 page)
{
    return page != 0 && offset % page == 0 && length % page == 0 &&
        offset <= bytes && length <= bytes - offset;
}
static int wc_range_inside(WC_RANGE_U64 offset, WC_RANGE_U64 length,
                           WC_RANGE_U64 start, WC_RANGE_U64 size)
{
    return offset >= start && offset - start <= size && length <= size - (offset - start);
}
static int wc_range_physical_ok(WC_RANGE_U64 base, WC_RANGE_U64 offset,
                                WC_RANGE_U64 length, WC_RANGE_U64 page)
{
    const WC_RANGE_U64 maximum = 0x7fffffffffffffffULL;
    return page != 0 && base <= maximum && base % page == 0 && length != 0 &&
        offset <= maximum - base && length - 1 <= maximum - base - offset;
}
