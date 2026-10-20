#include "WCIVSHMEM.hpp"
#include <libkern/OSByteOrder.h>
#include <libkern/OSAtomic.h>
#include <libkern/libkern.h>
#include <mach/mach_time.h>
#include <mach/vm_param.h>
#include <IOKit/IOLib.h>

OSDefineMetaClassAndStructors(WCIVSHMEM, IOService)
OSDefineMetaClassAndStructors(WCIVSHMEMClient, IOUserClient)

bool WCIVSHMEM::start(IOService *provider) {
    if (!IOService::start(provider)) return false;
    lock = IOLockAlloc();
    if (!lock) return false;
    pci = OSDynamicCast(IOPCIDevice, provider);
    if (!pci) { cleanup(); return false; }
    pci->retain();
    pciOpen = pci->open(this);
    if (!pciOpen) { cleanup(); return false; }
    if (pci->configRead16(kIOPCIConfigVendorID) != 0x1af4 ||
        pci->configRead16(kIOPCIConfigDeviceID) != 0x1110) { cleanup(); return false; }
    pci->setMemoryEnable(true);
    pci->setBusMasterEnable(false);
    // Public IOKit API, with the actual BAR register rather than memory index.
    shared = pci->getDeviceMemoryWithRegister(kIOPCIConfigBaseAddress2);
    if (!shared || !shared->getLength()) { shared = nullptr; cleanup(); return false; }
    shared->retain();
    registerMap = pci->mapDeviceMemoryWithRegister(kIOPCIConfigBaseAddress0);
    if (registerMap && registerMap->getLength() >= 16 && pci->configRead8(kIOPCIConfigRevisionID) == 1) {
        UInt8 capability = 0;
        if (pci->findPCICapability(0x11, &capability) && capability) {
            vectors = (pci->configRead16(capability + 2) & 0x7ff) + 1;
            int type = 0;
            if (pci->getInterruptType(0, &type) != kIOReturnSuccess || !(type & kIOInterruptTypePCIMessaged)) vectors = 0;
        }
    }
    if (vectors) {
        loop = IOWorkLoop::workLoop();
        irq = IOInterruptEventSource::interruptEventSource(this, interrupt, pci, 0);
        if (!loop || !irq || loop->addEventSource(irq) != kIOReturnSuccess) { cleanup(); return false; }
        irqAdded = true;
        irq->enable();
    }
    IOLockLock(lock);
    stopping = false;
    IOLockUnlock(lock);
    registerService();
    return true;
}
void WCIVSHMEM::cleanup() {
    if (!lock) return;
    // Lock order: a userclient may hold its lock while taking this device
    // lock. Never call a userclient or wait for the workloop with it held.
    // Detach all resources after in-flight accessors have left this lock.
    IOLockLock(lock);
    stopping = true;
    auto oldIRQ = irq; irq = nullptr;
    auto oldLoop = loop; loop = nullptr;
    bool removeIRQ = irqAdded; irqAdded = false;
    auto oldRegisters = registerMap; registerMap = nullptr;
    auto oldShared = shared; shared = nullptr;
    auto oldPCI = pci; pci = nullptr;
    bool closePCI = pciOpen; pciOpen = false;
    auto owner = bellOwner; bellOwner = nullptr;
    vectors = 0;
    IOLockUnlock(lock);
    // A running interrupt callback may still hold an independently retained
    // client. It sees stopping, or notify() sees that client's closed state.
    if (oldIRQ) {
        oldIRQ->disable();
        if (removeIRQ && oldLoop) oldLoop->removeEventSource(oldIRQ);
        oldIRQ->release();
    }
    if (oldLoop) oldLoop->release();
    if (owner) { owner->providerStopped(); owner->release(); }
    if (oldRegisters) oldRegisters->release();
    // Acquired subdescriptors/maps retain their own parent descriptor.
    if (oldShared) oldShared->release();
    if (oldPCI) {
        if (closePCI) oldPCI->close(this);
        oldPCI->release();
    }
}
void WCIVSHMEM::stop(IOService *provider) { cleanup(); IOService::stop(provider); }
void WCIVSHMEM::free() {
    cleanup();
    if (lock) { IOLockFree(lock); lock = nullptr; }
    IOService::free();
}
IOReturn WCIVSHMEM::newUserClient(task_t task, void *security, UInt32 type,
    OSDictionary *properties, IOUserClient **handler) {
    *handler = nullptr;
    if (type != WC_PCI_OPEN_TYPE) return kIOReturnUnsupported;
    // Administrator here is the kernel task-security-token uid == 0 test.
    if (IOUserClient::clientHasPrivilege(task, kIOClientPrivilegeAdministrator) != kIOReturnSuccess)
        return kIOReturnNotPrivileged;
    uint64_t info[5];
    if (getInfo(info) != kIOReturnSuccess) return kIOReturnNoDevice;
    auto client = new WCIVSHMEMClient;
    if (!client) return kIOReturnNoMemory;
    if (!client->initWithTask(task, security, type, properties) || !client->attach(this)) {
        client->release(); return kIOReturnError;
    }
    if (!client->start(this)) {
        client->detach(this); client->release(); return kIOReturnError;
    }
    *handler = client;
    return kIOReturnSuccess;
}
IOReturn WCIVSHMEM::getInfo(uint64_t out[5]) {
    IOLockLock(lock);
    if (stopping) { IOLockUnlock(lock); return kIOReturnNoDevice; }
    out[0] = WC_PCI_ABI; out[1] = shared->getLength(); out[2] = PAGE_SIZE;
    out[3] = vectors; out[4] = UINT64_MAX;
    if (vectors && registerMap) {
        auto base = reinterpret_cast<volatile void *>(registerMap->getVirtualAddress());
        out[4] = OSReadLittleInt32(base, 8);
    }
    IOLockUnlock(lock);
    return kIOReturnSuccess;
}
IOReturn WCIVSHMEM::copySlice(uint64_t off, uint64_t len, IOMemoryDescriptor **out) {
    IOLockLock(lock);
    if (stopping) { IOLockUnlock(lock); return kIOReturnNoDevice; }
    if (!wc_pci::slice(off, len, shared->getLength(), PAGE_SIZE)) {
        IOLockUnlock(lock); return kIOReturnBadArgument;
    }
    auto sub = IOSubMemoryDescriptor::withSubRange(shared, off, len, kIODirectionInOut);
    IOLockUnlock(lock);
    if (!sub) return kIOReturnNoMemory;
    *out = sub;
    return kIOReturnSuccess;
}
IOReturn WCIVSHMEM::retainClientSlice(IOMemoryDescriptor *selected, IOMemoryDescriptor **out) {
    IOLockLock(lock);
    if (stopping) { IOLockUnlock(lock); return kIOReturnNoDevice; }
    selected->retain();
    *out = selected;
    IOLockUnlock(lock);
    return kIOReturnSuccess;
}
IOReturn WCIVSHMEM::ring(uint64_t peer, uint64_t vector) {
    IOLockLock(lock);
    if (stopping) { IOLockUnlock(lock); return kIOReturnNoDevice; }
    if (!registerMap || !wc_pci::bell(peer, vector, vectors)) {
        IOLockUnlock(lock); return kIOReturnBadArgument;
    }
    auto base = reinterpret_cast<volatile void *>(registerMap->getVirtualAddress());
    __sync_synchronize(); // publish shared-ring writes before the MMIO doorbell
    OSWriteLittleInt32(base, 12, static_cast<uint32_t>((peer << 16) | vector));
    IOLockUnlock(lock);
    return kIOReturnSuccess;
}
IOReturn WCIVSHMEM::claimBell(WCIVSHMEMClient *client) {
    IOLockLock(lock);
    if (stopping) { IOLockUnlock(lock); return kIOReturnNoDevice; }
    if (!irq) { IOLockUnlock(lock); return kIOReturnUnsupported; }
    IOReturn ret = kIOReturnExclusiveAccess;
    if (!bellOwner || bellOwner == client) {
        if (!bellOwner) { client->retain(); bellOwner = client; }
        ret = kIOReturnSuccess;
    }
    IOLockUnlock(lock);
    return ret;
}
void WCIVSHMEM::releaseBell(WCIVSHMEMClient *client) {
    IOLockLock(lock);
    bool release = bellOwner == client;
    if (release) bellOwner = nullptr;
    IOLockUnlock(lock);
    if (release) client->release();
}
void WCIVSHMEM::interrupt(OSObject *owner, IOInterruptEventSource *, int count) {
    auto self = OSDynamicCast(WCIVSHMEM, owner);
    if (!self) return;
    IOLockLock(self->lock);
    auto client = self->stopping ? nullptr : self->bellOwner;
    if (client) client->retain();
    IOLockUnlock(self->lock);
    if (client) { client->notify(count > 0 ? count : 1); client->release(); }
}

bool WCIVSHMEMClient::initWithTask(task_t task, void *security, UInt32 type, OSDictionary *properties) {
    if (type != WC_PCI_OPEN_TYPE || clientHasPrivilege(task, kIOClientPrivilegeAdministrator) != kIOReturnSuccess)
        return false;
    if (!IOUserClient::initWithTask(task, security, type, properties)) return false;
    lock = IOLockAlloc();
    return lock != nullptr;
}
bool WCIVSHMEMClient::start(IOService *provider) {
    if (!IOUserClient::start(provider)) return false;
    device = OSDynamicCast(WCIVSHMEM, provider);
    if (!device) return false;
    device->retain();
    return true;
}
void WCIVSHMEMClient::closeState() {
    if (!lock) return;
    IOLockLock(lock);
    closed = true;
    if (armed) { releaseAsyncReference64(async); armed = false; }
    bool release = bellClaimed;
    bellClaimed = false;
    IOLockUnlock(lock);
    if (release && device) device->releaseBell(this);
}
void WCIVSHMEMClient::stop(IOService *provider) { closeState(); IOUserClient::stop(provider); }
IOReturn WCIVSHMEMClient::clientClose() {
    closeState();
    terminate();
    return kIOReturnSuccess;
}
IOReturn WCIVSHMEMClient::clientDied() { return clientClose(); }
void WCIVSHMEMClient::free() {
    if (slice) { slice->release(); slice = nullptr; }
    if (device) { device->release(); device = nullptr; }
    if (lock) { IOLockFree(lock); lock = nullptr; }
    IOUserClient::free();
}
IOReturn WCIVSHMEMClient::dispatch(OSObject *target, void *reference, IOExternalMethodArguments *args) {
    auto self = OSDynamicCast(WCIVSHMEMClient, target);
    if (!self) return kIOReturnBadArgument;
    IOLockLock(self->lock);
    IOReturn ret = self->closed ? kIOReturnNotOpen : self->method(static_cast<uint32_t>(reinterpret_cast<uintptr_t>(reference)), args);
    IOLockUnlock(self->lock);
    return ret;
}
IOReturn WCIVSHMEMClient::externalMethod(uint32_t selector, IOExternalMethodArguments *args,
    IOExternalMethodDispatch *, OSObject *, void *) {
    static const IOExternalMethodDispatch methods[] = {
        {dispatch, 0, 0, 5, 0}, {dispatch, 2, 0, 0, 0},
        {dispatch, 2, 0, 0, 0}, {dispatch, 1, 0, 0, 0}
    };
    if (selector >= sizeof(methods) / sizeof(methods[0])) return kIOReturnUnsupported;
    if (!args || (selector != WC_PCI_ARM && args->asyncWakePort != MACH_PORT_NULL)) return kIOReturnBadArgument;
    return IOUserClient::externalMethod(selector, args, const_cast<IOExternalMethodDispatch *>(&methods[selector]),
        this, reinterpret_cast<void *>(static_cast<uintptr_t>(selector)));
}
IOReturn WCIVSHMEMClient::method(uint32_t selector, IOExternalMethodArguments *args) {
    switch (selector) {
    case WC_PCI_INFO:
        return device->getInfo(args->scalarOutput);
    case WC_PCI_SELECT:
        if (slice) return kIOReturnExclusiveAccess;
        return device->copySlice(args->scalarInput[0], args->scalarInput[1], &slice);
    case WC_PCI_RING:
        return device->ring(args->scalarInput[0], args->scalarInput[1]);
    case WC_PCI_ARM: {
        if (armed || args->asyncWakePort == MACH_PORT_NULL || args->asyncReferenceCount != kOSAsyncRef64Count)
            return kIOReturnBadArgument;
        if (!bellClaimed) {
            auto ret = device->claimBell(this);
            if (ret != kIOReturnSuccess) return ret;
            bellClaimed = true;
        }
        // Preserve the kernel-supplied 64-bit/port flags. Own the transferred
        // async wake right until completion or close, then release it once.
        bcopy(args->asyncReference, async, sizeof(async));
        armed = true;
        if (args->scalarInput[0] != sequence) {
            io_user_reference_t data[] = {sequence, mach_absolute_time()};
            auto ret = sendAsyncResult64(async, kIOReturnSuccess, data, 2);
            releaseAsyncReference64(async); armed = false;
            return ret;
        }
        return kIOReturnSuccess;
    }
    default: return kIOReturnUnsupported;
    }
}
IOReturn WCIVSHMEMClient::clientMemoryForType(UInt32 type, IOOptionBits *options, IOMemoryDescriptor **memory) {
    IOLockLock(lock);
    IOReturn ret = kIOReturnNotFound;
    if (!closed && type == WC_PCI_MEMORY && slice) {
        ret = device->retainClientSlice(slice, memory);
        if (ret == kIOReturnSuccess) *options = kIOMapDefaultCache;
    }
    IOLockUnlock(lock);
    return ret;
}
void WCIVSHMEMClient::notify(uint64_t count) {
    IOLockLock(lock);
    sequence += count;
    if (!closed && armed) {
        io_user_reference_t data[] = {sequence, mach_absolute_time()};
        sendAsyncResult64(async, kIOReturnSuccess, data, 2);
        releaseAsyncReference64(async); armed = false;
    }
    IOLockUnlock(lock);
}

void WCIVSHMEMClient::providerStopped() {
    IOLockLock(lock);
    closed = true;
    // cleanup() detached and now owns the bellOwner retain. Do not release it
    // again through releaseBell, including when clientClose races this call.
    bellClaimed = false;
    if (armed) {
        sendAsyncResult64(async, kIOReturnNoDevice, nullptr, 0);
        releaseAsyncReference64(async); armed = false;
    }
    IOLockUnlock(lock);
}
