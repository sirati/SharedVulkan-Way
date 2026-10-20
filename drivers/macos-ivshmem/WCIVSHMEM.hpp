#ifndef WC_IVSHMEM_HPP
#define WC_IVSHMEM_HPP
#if !defined(__x86_64__)
#error WCIVSHMEM currently supports Intel macOS guests only
#endif
#include <IOKit/IOService.h>
#include <IOKit/IOUserClient.h>
#include <IOKit/pci/IOPCIDevice.h>
#include <IOKit/IOInterruptEventSource.h>
#include <IOKit/IOWorkLoop.h>
#include <IOKit/IOSubMemoryDescriptor.h>
#include <IOKit/IOLocks.h>
#include "abi.h"
class WCIVSHMEMClient;
class WCIVSHMEM : public IOService {
    OSDeclareDefaultStructors(WCIVSHMEM)
    IOPCIDevice *pci = nullptr;
    IOMemoryDescriptor *shared = nullptr;
    IOMemoryMap *registerMap = nullptr;
    IOWorkLoop *loop = nullptr;
    IOInterruptEventSource *irq = nullptr;
    IOLock *lock = nullptr;
    WCIVSHMEMClient *bellOwner = nullptr;
    uint64_t vectors = 0;
    bool pciOpen = false;
    bool irqAdded = false;
    bool stopping = true;
    static void interrupt(OSObject *, IOInterruptEventSource *, int);
    void cleanup();
public:
    bool start(IOService *) override;
    void stop(IOService *) override;
    void free() override;
    IOReturn newUserClient(task_t, void *, UInt32, OSDictionary *, IOUserClient **) override;
    IOReturn copySlice(uint64_t, uint64_t, IOMemoryDescriptor **);
    IOReturn ring(uint64_t, uint64_t);
    IOReturn claimBell(WCIVSHMEMClient *);
    void releaseBell(WCIVSHMEMClient *);
    IOReturn getInfo(uint64_t out[5]);
    IOReturn retainClientSlice(IOMemoryDescriptor *, IOMemoryDescriptor **);
};
class WCIVSHMEMClient : public IOUserClient {
    OSDeclareDefaultStructors(WCIVSHMEMClient)
    WCIVSHMEM *device = nullptr;
    IOMemoryDescriptor *slice = nullptr;
    IOLock *lock = nullptr;
    OSAsyncReference64 async = {};
    bool armed = false;
    bool closed = false;
    bool bellClaimed = false;
    uint64_t sequence = 0;
    static IOReturn dispatch(OSObject *, void *, IOExternalMethodArguments *);
    IOReturn method(uint32_t, IOExternalMethodArguments *);
    void closeState();
public:
    bool initWithTask(task_t, void *, UInt32, OSDictionary *) override;
    bool start(IOService *) override;
    void stop(IOService *) override;
    void free() override;
    IOReturn clientClose() override;
    IOReturn clientDied() override;
    IOReturn externalMethod(uint32_t, IOExternalMethodArguments *, IOExternalMethodDispatch *, OSObject *, void *) override;
    IOReturn clientMemoryForType(UInt32, IOOptionBits *, IOMemoryDescriptor **) override;
    void notify(uint64_t);
    void providerStopped();
};
#endif
