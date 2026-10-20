#ifndef WC_PCI_ABI_H
#define WC_PCI_ABI_H
#include <stdint.h>
// Scalar-only IOConnect ABI. Structures and host pointers never cross this ABI.
enum { WC_PCI_ABI = 1, WC_PCI_OPEN_TYPE = 0x57435043,
       WC_PCI_INFO = 0, WC_PCI_SELECT = 1, WC_PCI_RING = 2, WC_PCI_ARM = 3,
       WC_PCI_MEMORY = 1, WC_PCI_SHARED_BAR = 2, WC_PCI_REG_BAR = 0 };
// INFO: [] -> [abi, BAR2 bytes, guest page bytes, bell vectors, peer id].
// SELECT: [offset, length] -> []; immutable per connection, page aligned.
// RING: [peer id, vector] -> []; no arbitrary MMIO/config access.
// ARM (async): [last count] -> []; completion scalars [count, timestamp].
// Exactly one bell connection per device. Rescan shared rings after every wake/open.
// IOConnectMapMemory64(WC_PCI_MEMORY) maps only the selected slice.
#ifdef __cplusplus
namespace wc_pci {
constexpr bool slice(uint64_t off, uint64_t len, uint64_t size, uint64_t page) {
    return page && !(page & (page - 1)) && len && !(off % page) &&
           !(len % page) && off <= size && len <= size - off;
}
constexpr bool bell(uint64_t peer, uint64_t vector, uint64_t vectors) {
    return peer <= 0xffff && vector < vectors && vector <= 0xffff;
}
}
#endif
#endif
