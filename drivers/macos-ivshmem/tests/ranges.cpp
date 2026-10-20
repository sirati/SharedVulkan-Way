#include "../abi.h"
#include <assert.h>
int main() {
    using namespace wc_pci;
    assert(slice(0, 4096, 8192, 4096));
    assert(slice(4096, 4096, 8192, 4096));
    assert(!slice(8192, 4096, 8192, 4096));
    assert(!slice(UINT64_MAX - 4095, 8192, UINT64_MAX, 4096));
    assert(!slice(0, UINT64_MAX, UINT64_MAX, 4096));
    assert(!slice(1, 4096, 8192, 4096));
    assert(!slice(0, 0, 8192, 4096));
    assert(!slice(0, 4096, 8192, 0));
    assert(!slice(0, 4096, 8192, 3));
    assert(slice(16384, 16384, 32768, 16384));
    assert(!slice(4096, 16384, 32768, 16384));
    assert(bell(65535, 7, 8));
    assert(!bell(65536, 0, 8));
    assert(!bell(1, 8, 8));
    assert(!bell(1, 0, 0));
    assert(!bell(1, 65536, 65537));
}
