#!/usr/bin/env bash
# Run inside the shared build cgroup and vm-tests.slice, as for other builds.
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")" && pwd)
: "${WC_PCI_CHECK_DIR:?set WC_PCI_CHECK_DIR to scratch under the datasets filesystem}"
mkdir -p "$WC_PCI_CHECK_DIR"
"${WC_PCI_NATIVE_CXX:-c++}" -std=c++17 -Wall -Wextra -Werror "$root/tests/ranges.cpp" -o "$WC_PCI_CHECK_DIR/ranges"
"$WC_PCI_CHECK_DIR/ranges"
# An ordinary macOS SDK is sufficient for the ABI cross-link, not the DEXT.
if [[ -n ${DARWIN_CXX:-} ]]; then
    "$DARWIN_CXX" -std=c++17 -fsyntax-only "$root/tests/mach-abi.cpp"
    "$DARWIN_CXX" -std=c++17 -Wall -Wextra -Werror "$root/tests/ranges.cpp" -o "$WC_PCI_CHECK_DIR/ranges-darwin"
fi
