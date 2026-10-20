/* Waycompose addition to virtio-win ivshmem. See LICENSE and README.md. */
#include "driver.h"
#include "Range.h"
#include "RangeBounds.h"

#define RANGE_TAG 'rIvW'

typedef struct {
    LIST_ENTRY Entry;
    LONG References;
    WDFDEVICE Device;
    PEPROCESS Process;
    PMDL Mdl;
    PVOID Address;
    UINT64 Id;
    KEVENT Stop;
    HANDLE Thread;
    BOOLEAN Linked;
} RANGE_MAP;

static VOID lock(IVSHMEM_RANGE_STATE *s)
{
    KeWaitForSingleObject(&s->Lock, Executive, KernelMode, FALSE, NULL);
}
static VOID unlock(IVSHMEM_RANGE_STATE *s) { KeReleaseMutex(&s->Lock, FALSE); }
static VOID release(RANGE_MAP *m)
{
    if (InterlockedDecrement(&m->References) == 0) {
        if (m->Thread != NULL) ZwClose(m->Thread);
        ObDereferenceObject(m->Process);
        WdfObjectDereference(m->Device);
        ExFreePoolWithTag(m, RANGE_TAG);
    }
}

/* Primary-token identity of the actual requesting process, not an input SID
 * and not a possibly impersonated token on a framework worker thread. */
static BOOLEAN system_caller(VOID)
{
    PACCESS_TOKEN token = PsReferencePrimaryToken(PsGetCurrentProcess());
    PTOKEN_USER user = NULL;
    NTSTATUS status = SeQueryInformationToken(token, TokenUser, (PVOID *)&user);
    BOOLEAN ok = NT_SUCCESS(status) && user != NULL &&
        RtlEqualSid(user->User.Sid, SeExports->SeLocalSystemSid);
    if (user != NULL) ExFreePool(user);
    PsDereferencePrimaryToken(token);
    return ok;
}

/* An unmap is performed while the retained target still owns an address
 * space. If exit has begun, wait for the kernel process object instead. The
 * OS removes that process's user mappings during address-space teardown. */
static VOID unmap_or_wait(RANGE_MAP *m)
{
    NTSTATUS status = PsAcquireProcessExitSynchronization(m->Process);
    if (NT_SUCCESS(status)) {
        KAPC_STATE apc;
        KeStackAttachProcess(m->Process, &apc);
        MmUnmapLockedPages(m->Address, m->Mdl);
        KeUnstackDetachProcess(&apc);
        PsReleaseProcessExitSynchronization(m->Process);
    } else {
        KeWaitForSingleObject(m->Process, Executive, KernelMode, FALSE, NULL);
    }
}

static VOID map_worker(PVOID context)
{
    RANGE_MAP *m = context;
    PVOID objects[2] = { &m->Stop, m->Process };
    NTSTATUS status = KeWaitForMultipleObjects(2, objects, WaitAny, Executive,
                                              KernelMode, FALSE, NULL, NULL);
    /* No timeout or client-writable word is used as a death indication. */
    if (status != STATUS_WAIT_0 + 1) unmap_or_wait(m);
    IoFreeMdl(m->Mdl);
    m->Mdl = NULL;
    /* The table retains the record and kernel thread HANDLE. Control/drain
     * waits for actual thread termination before freeing it or releasing the
     * final device reference, so the driver cannot unload under this code. */
    PsTerminateSystemThread(STATUS_SUCCESS);
}

/* Called with the state lock and one lookup reference, returns with the lock.
 * A second concurrent unmap may also wait, but only one removes table ownership. */
static VOID stop_map(IVSHMEM_RANGE_STATE *s, RANGE_MAP *m)
{
    KeSetEvent(&m->Stop, IO_NO_INCREMENT, FALSE);
    unlock(s);
    ZwWaitForSingleObject(m->Thread, FALSE, NULL);
    lock(s);
    if (m->Linked) {
        RemoveEntryList(&m->Entry);
        m->Linked = FALSE;
        release(m); /* table reference */
    }
    release(m); /* lookup reference */
}

static VOID reap_maps(IVSHMEM_RANGE_STATE *s)
{
    LIST_ENTRY *entry = s->Maps.Flink;
    LARGE_INTEGER zero;
    zero.QuadPart = 0;
    while (entry != &s->Maps) {
        RANGE_MAP *m = CONTAINING_RECORD(entry, RANGE_MAP, Entry);
        entry = entry->Flink;
        if (ZwWaitForSingleObject(m->Thread, FALSE, &zero) == STATUS_SUCCESS) {
            RemoveEntryList(&m->Entry);
            m->Linked = FALSE;
            release(m);
        }
    }
}

VOID IVSHMEMRangeInitialize(WDFDEVICE Device)
{
    IVSHMEM_RANGE_STATE *s = &DeviceGetContext(Device)->range;
    KeInitializeMutex(&s->Lock, 0);
    InitializeListHead(&s->Maps);
    s->Generation = (UINT64)KeQueryPerformanceCounter(NULL).QuadPart;
    if (s->Generation == 0) s->Generation = 1;
    s->NextMapping = 1;
    s->Ready = FALSE;
    s->Configured = FALSE;
}
VOID IVSHMEMRangeReady(WDFDEVICE Device)
{
    IVSHMEM_RANGE_STATE *s = &DeviceGetContext(Device)->range;
    lock(s);
    if (s->Generation != MAXULONGLONG) {
        ++s->Generation;
        s->Configured = FALSE;
        s->Ready = TRUE;
    }
    unlock(s);
}
VOID IVSHMEMRangeDrain(WDFDEVICE Device)
{
    IVSHMEM_RANGE_STATE *s = &DeviceGetContext(Device)->range;
    lock(s);
    s->Ready = FALSE;
    while (!IsListEmpty(&s->Maps)) {
        RANGE_MAP *m = CONTAINING_RECORD(s->Maps.Flink, RANGE_MAP, Entry);
        InterlockedIncrement(&m->References);
        stop_map(s, m);
    }
    unlock(s);
}

static NTSTATUS configure(WDFDEVICE Device, WDFREQUEST Request, size_t input, size_t output)
{
    PDEVICE_CONTEXT d = DeviceGetContext(Device);
    IVSHMEM_RANGE_STATE *s = &d->range;
    IVSHMEM_RANGE_POLICY *p;
    NTSTATUS status;
    UINT64 bytes = d->shmemAddr.NumberOfBytes;
    if (input != sizeof(*p) || output != 0) return STATUS_INFO_LENGTH_MISMATCH;
    if (s->Configured || !IsListEmpty(&s->Maps)) return STATUS_DEVICE_BUSY;
    status = WdfRequestRetrieveInputBuffer(Request, sizeof(*p), (PVOID *)&p, NULL);
    if (!NT_SUCCESS(status)) return status;
    if (p->Version != IVSHMEM_RANGE_VERSION || p->Reserved != 0 || p->Generation != s->Generation ||
        p->WriteLength == 0 || !wc_range_window_ok(p->WriteOffset, p->WriteLength, bytes, PAGE_SIZE) ||
        !wc_range_window_ok(p->ReadOffset, p->ReadLength, bytes, PAGE_SIZE) ||
        (p->ReadLength != 0 && p->WriteOffset < p->ReadOffset + p->ReadLength &&
         p->ReadOffset < p->WriteOffset + p->WriteLength)) return STATUS_INVALID_PARAMETER;
    s->Policy = *p;
    s->Configured = TRUE;
    return STATUS_SUCCESS;
}

static NTSTATUS map_range(WDFDEVICE Device, WDFREQUEST Request,
                          size_t InputLength, size_t OutputLength, size_t *written)
{
    PDEVICE_CONTEXT d = DeviceGetContext(Device);
    IVSHMEM_RANGE_STATE *s = &d->range;
    IVSHMEM_RANGE_REQUEST *input;
    IVSHMEM_RANGE_REQUEST in;
    IVSHMEM_RANGE_REPLY *out;
    RANGE_MAP *m = NULL;
    PEPROCESS process = NULL;
    NTSTATUS status;
    OBJECT_ATTRIBUTES threadAttributes;
    KAPC_STATE apc;
    MM_PHYSICAL_ADDRESS_LIST physical;
    ULONG priority = NormalPagePriority | MdlMappingNoExecute;
    if (InputLength != sizeof(in) || OutputLength != sizeof(*out)) return STATUS_INFO_LENGTH_MISMATCH;
    status = WdfRequestRetrieveInputBuffer(Request, sizeof(in), (PVOID *)&input, NULL);
    if (!NT_SUCCESS(status)) return status;
    in = *input; /* METHOD_BUFFERED input/output share one buffer. */
    status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*out), (PVOID *)&out, NULL);
    if (!NT_SUCCESS(status)) return status;
    if (in.Version != IVSHMEM_RANGE_VERSION || (in.Flags & ~IVSHMEM_RANGE_WRITE) != 0 ||
        in.Generation != s->Generation || in.Length == 0 ||
        in.Offset % PAGE_SIZE != 0 || in.Length % PAGE_SIZE != 0 ||
        in.Offset > d->shmemAddr.NumberOfBytes ||
        in.Length > d->shmemAddr.NumberOfBytes - in.Offset ||
        in.Length > MAXULONG || in.TargetHandle > MAXULONG_PTR) return STATUS_INVALID_PARAMETER;
    if (!s->Configured ||
        (!wc_range_inside(in.Offset, in.Length, s->Policy.WriteOffset, s->Policy.WriteLength) &&
         ((in.Flags & IVSHMEM_RANGE_WRITE) != 0 ||
          !wc_range_inside(in.Offset, in.Length, s->Policy.ReadOffset, s->Policy.ReadLength))))
        return STATUS_ACCESS_DENIED;
    /* The caller context resolves the SYSTEM broker's own process HANDLE.
     * No PID lookup, handle-table value from a desktop caller, or KernelMode
     * access-check bypass can select a target. */
    status = ObReferenceObjectByHandle((HANDLE)(ULONG_PTR)in.TargetHandle,
        PROCESS_VM_OPERATION | SYNCHRONIZE, *PsProcessType, UserMode, (PVOID *)&process, NULL);
    if (!NT_SUCCESS(status)) return status;
    m = ExAllocatePoolWithTag(NonPagedPoolNx, sizeof(*m), RANGE_TAG);
    if (m == NULL) { ObDereferenceObject(process); return STATUS_INSUFFICIENT_RESOURCES; }
    RtlZeroMemory(m, sizeof(*m));
    m->Process = process;
    m->Device = Device;
    WdfObjectReference(Device);
    m->References = 1; /* creation reference, transferred to table on success */
    KeInitializeEvent(&m->Stop, NotificationEvent, FALSE);

    physical = d->shmemAddr;
    if (!wc_range_physical_ok((UINT64)physical.PhysicalAddress.QuadPart,
                              in.Offset, in.Length, PAGE_SIZE)) {
        status = STATUS_INTEGER_OVERFLOW; goto fail;
    }
    physical.PhysicalAddress.QuadPart += in.Offset;
    physical.NumberOfBytes = in.Length;
    status = MmAllocateMdlForIoSpace(&physical, 1, &m->Mdl);
    if (!NT_SUCCESS(status)) goto fail;
    status = PsAcquireProcessExitSynchronization(process);
    if (!NT_SUCCESS(status)) goto fail;
    if ((in.Flags & IVSHMEM_RANGE_WRITE) == 0) priority |= MdlMappingNoWrite;
    KeStackAttachProcess(process, &apc);
    __try {
        m->Address = MmMapLockedPagesSpecifyCache(m->Mdl, UserMode, MmCached,
                                                  NULL, FALSE, priority);
        status = m->Address != NULL ? STATUS_SUCCESS : STATUS_INSUFFICIENT_RESOURCES;
    } __except (EXCEPTION_EXECUTE_HANDLER) { status = GetExceptionCode(); }
    KeUnstackDetachProcess(&apc);
    PsReleaseProcessExitSynchronization(process);
    if (!NT_SUCCESS(status)) goto fail;
    if (s->NextMapping == 0 || s->NextMapping == MAXULONGLONG) {
        status = STATUS_INTEGER_OVERFLOW; unmap_or_wait(m); goto fail;
    }
    m->Id = s->NextMapping++;
    InsertTailList(&s->Maps, &m->Entry);
    m->Linked = TRUE;
    InitializeObjectAttributes(&threadAttributes, NULL, OBJ_KERNEL_HANDLE, NULL, NULL);
    status = PsCreateSystemThread(&m->Thread, SYNCHRONIZE, &threadAttributes, NULL, NULL, map_worker, m);
    if (!NT_SUCCESS(status)) {
        RemoveEntryList(&m->Entry);
        m->Linked = FALSE;
        unmap_or_wait(m);
        goto fail;
    }
    /* Caller owns the state lock until output is copied, so the worker cannot
     * remove/free this record even if the target has already terminated. */
    out->Generation = s->Generation;
    out->Mapping = m->Id;
    out->Address = (UINT64)(ULONG_PTR)m->Address;
    out->Length = in.Length;
    *written = sizeof(*out);
    return STATUS_SUCCESS;
fail:
    if (m->Mdl != NULL) IoFreeMdl(m->Mdl);
    release(m);
    return status;
}

VOID IVSHMEMRangeCallerContext(WDFDEVICE Device, WDFREQUEST Request)
{
    WDF_REQUEST_PARAMETERS parameters;
    PDEVICE_CONTEXT d = DeviceGetContext(Device);
    IVSHMEM_RANGE_STATE *s = &d->range;
    NTSTATUS status;
    size_t written = 0;
    ULONG code;
    WDF_REQUEST_PARAMETERS_INIT(&parameters);
    WdfRequestGetParameters(Request, &parameters);
    if (parameters.Type != WdfRequestTypeDeviceControl) {
        status = WdfDeviceEnqueueRequest(Device, Request);
        if (!NT_SUCCESS(status)) WdfRequestComplete(Request, status);
        return;
    }
    code = parameters.Parameters.DeviceIoControl.IoControlCode;
    /* All raw-device operations are SYSTEM-only, including legacy mmap. The
     * new service exports payload pointers, never a raw device handle. */
    if (KeGetCurrentIrql() != PASSIVE_LEVEL || !system_caller()) {
        WdfRequestComplete(Request, STATUS_ACCESS_DENIED);
        return;
    }
    if (code != IOCTL_IVSHMEM_RANGE_QUERY && code != IOCTL_IVSHMEM_RANGE_MAP && code != IOCTL_IVSHMEM_RANGE_UNMAP && code != IOCTL_IVSHMEM_RANGE_CONFIGURE && code != IOCTL_IVSHMEM_RANGE_POLICY_QUERY) {
        status = WdfDeviceEnqueueRequest(Device, Request);
        if (!NT_SUCCESS(status)) WdfRequestComplete(Request, status);
        return;
    }
    lock(s);
    reap_maps(s);
    status = STATUS_DEVICE_NOT_READY;
    if (s->Ready) {
        size_t input = parameters.Parameters.DeviceIoControl.InputBufferLength;
        size_t output = parameters.Parameters.DeviceIoControl.OutputBufferLength;
        if (code == IOCTL_IVSHMEM_RANGE_POLICY_QUERY) {
            IVSHMEM_RANGE_POLICY_STATUS *out;
            status = STATUS_INFO_LENGTH_MISMATCH;
            if (input == 0 && output == sizeof(*out)) {
                status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*out), (PVOID *)&out, NULL);
                if (NT_SUCCESS(status)) {
                    RtlZeroMemory(out, sizeof(*out));
                    out->Configured = s->Configured ? 1 : 0;
                    if (s->Configured) out->Policy = s->Policy;
                    else {
                        out->Policy.Version = IVSHMEM_RANGE_VERSION;
                        out->Policy.Generation = s->Generation;
                    }
                    written = sizeof(*out);
                }
            }
        } else if (code == IOCTL_IVSHMEM_RANGE_CONFIGURE) {
            status = configure(Device, Request, input, output);
        } else if (code == IOCTL_IVSHMEM_RANGE_MAP) {
            status = map_range(Device, Request, input, output, &written);
        } else if (code == IOCTL_IVSHMEM_RANGE_QUERY) {
            IVSHMEM_RANGE_CAPS *caps;
            status = STATUS_INFO_LENGTH_MISMATCH;
            if (input == 0 && output == sizeof(*caps)) {
                status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*caps), (PVOID *)&caps, NULL);
                if (NT_SUCCESS(status)) {
                    caps->Version = IVSHMEM_RANGE_VERSION;
                    caps->Reserved = 0;
                    caps->Generation = s->Generation;
                    caps->BarBytes = d->shmemAddr.NumberOfBytes;
                    written = sizeof(*caps);
                }
            }
        } else {
            IVSHMEM_RANGE_RELEASE *in;
            status = STATUS_INFO_LENGTH_MISMATCH;
            if (input == sizeof(*in) && output == 0) {
                status = WdfRequestRetrieveInputBuffer(Request, sizeof(*in), (PVOID *)&in, NULL);
                if (NT_SUCCESS(status)) {
                    status = STATUS_INVALID_PARAMETER;
                    if (in->Version == IVSHMEM_RANGE_VERSION && in->Reserved == 0 && in->Generation == s->Generation) {
                        LIST_ENTRY *entry;
                        status = STATUS_NOT_FOUND;
                        for (entry = s->Maps.Flink; entry != &s->Maps; entry = entry->Flink) {
                            RANGE_MAP *m = CONTAINING_RECORD(entry, RANGE_MAP, Entry);
                            if (m->Id == in->Mapping) {
                                InterlockedIncrement(&m->References);
                                stop_map(s, m);
                                unlock(s);
                                WdfRequestComplete(Request, STATUS_SUCCESS);
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
    unlock(s);
    WdfRequestCompleteWithInformation(Request, status, written);
}
