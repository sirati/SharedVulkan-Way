/* Waycompose addition to virtio-win ivshmem. See LICENSE and README.md. */
#pragma once

#define IVSHMEM_RANGE_VERSION 1
#define IVSHMEM_RANGE_WRITE 1
#define IOCTL_IVSHMEM_RANGE_QUERY CTL_CODE(FILE_DEVICE_UNKNOWN, 0x820, METHOD_BUFFERED, FILE_READ_ACCESS)
#define IOCTL_IVSHMEM_RANGE_MAP CTL_CODE(FILE_DEVICE_UNKNOWN, 0x821, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS)
#define IOCTL_IVSHMEM_RANGE_CONFIGURE CTL_CODE(FILE_DEVICE_UNKNOWN, 0x823, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS)
#define IOCTL_IVSHMEM_RANGE_UNMAP CTL_CODE(FILE_DEVICE_UNKNOWN, 0x822, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS)

/* Fixed-width ABI, identical for 32/64-bit callers. No PID or raw device handle
 * is ever exported. TargetHandle is a handle in the SYSTEM broker process. */
typedef struct {
    UINT32 Version;
    UINT32 Flags;
    UINT64 TargetHandle;
    UINT64 Generation;
    UINT64 Offset;
    UINT64 Length;
} IVSHMEM_RANGE_REQUEST;
typedef struct {
    UINT32 Version;
    UINT32 Reserved;
    UINT64 Generation;
    UINT64 BarBytes;
} IVSHMEM_RANGE_CAPS;
typedef struct {
    UINT64 Generation;
    UINT64 Mapping;
    UINT64 Address;
    UINT64 Length;
} IVSHMEM_RANGE_REPLY;
typedef struct {
    UINT32 Version;
    UINT32 Reserved;
    UINT64 Generation;
    UINT64 Mapping;
} IVSHMEM_RANGE_RELEASE;

/* One immutable policy per hardware generation. Header/control/ring pages
 * are outside both windows and cannot be mapped into desktop processes. */
typedef struct {
    UINT32 Version;
    UINT32 Reserved;
    UINT64 Generation;
    UINT64 WriteOffset;
    UINT64 WriteLength;
    UINT64 ReadOffset;
    UINT64 ReadLength;
} IVSHMEM_RANGE_POLICY;

typedef struct {
    KMUTEX Lock;
    LIST_ENTRY Maps;
    UINT64 Generation;
    UINT64 NextMapping;
    BOOLEAN Ready;
    BOOLEAN Configured;
    IVSHMEM_RANGE_POLICY Policy;
} IVSHMEM_RANGE_STATE;

/* Must run in the submitting process, before the request enters a queue. */
EVT_WDF_IO_IN_CALLER_CONTEXT IVSHMEMRangeCallerContext;
VOID IVSHMEMRangeInitialize(WDFDEVICE Device);
VOID IVSHMEMRangeReady(WDFDEVICE Device);
VOID IVSHMEMRangeDrain(WDFDEVICE Device);
