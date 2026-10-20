/* Diagnose RO registration using only original shared pages and public APIs.
 * This owns one empty HVF VM, never runs a vCPU and never maps a writable
 * alias of the readonly descriptor. The RW fd/map represents the producer. */
#include <Hypervisor/Hypervisor.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <assert.h>
#include <fcntl.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#define LEN 65536
#define IPA 0x1000000
static unsigned sequence;
static void *producer;
static void inspect(const char *name,void *view) {
    vm_region_basic_info_data_64_t info;
    mach_msg_type_number_t count=VM_REGION_BASIC_INFO_COUNT_64;
    mach_vm_address_t start=(mach_vm_address_t)(uintptr_t)view;
    mach_vm_size_t size=0;mach_port_t object=MACH_PORT_NULL;
    assert(mach_vm_region(mach_task_self(),&start,&size,VM_REGION_BASIC_INFO_64,
        (vm_region_info_t)&info,&count,&object)==KERN_SUCCESS);
    if(object)mach_port_deallocate(mach_task_self(),object);
    assert((info.protection & VM_PROT_READ) && !(info.protection & VM_PROT_WRITE));
    assert(!(info.max_protection & VM_PROT_WRITE));
    uint32_t value=0x51400000+(++sequence);
    memcpy(producer,&value,sizeof(value));
    assert(*(volatile uint32_t *)view==value);
    /* Admission diagnosis only. No vCPU exists, so the RX trial grants no
     * guest execution. This does not justify broader product permissions. */
    for(unsigned executable=0;executable<2;executable++) {
        hv_memory_flags_t flags=HV_MEMORY_READ | (executable ? HV_MEMORY_EXEC : 0);
        hv_return_t rc=hv_vm_map(view,IPA,LEN,flags);
        printf("RO_HVF name=%s current=%d maximum=%d shared=%d offset=%llu stage2=%s hv=0x%08x original_page_visible=1\n",
            name,info.protection,info.max_protection,info.shared,(unsigned long long)info.offset,
            executable ? "RX" : "R",(unsigned)rc);
        if(rc==HV_SUCCESS) assert(hv_vm_unmap(IPA,LEN)==HV_SUCCESS);
    }
}
int main(void) {
    char path[]="/tmp/hl-ro-hvf-XXXXXX";int rw=mkstemp(path);assert(rw>=0);assert(!ftruncate(rw,LEN));
    int ro=open(path,O_RDONLY);assert(ro>=0);assert(!unlink(path));
    producer=mmap(NULL,LEN,PROT_READ|PROT_WRITE,MAP_SHARED,rw,0);assert(producer!=MAP_FAILED);
    void *original=mmap(NULL,LEN,PROT_READ,MAP_SHARED,ro,0);assert(original!=MAP_FAILED);
    assert(hv_vm_create(NULL)==HV_SUCCESS);
    inspect("original-mmap",original);
    int locked=mlock(original,LEN);printf("RO_MLOCK result=%d errno=%d\n",locked,locked ? errno : 0);
    if(!locked){inspect("original-mlock",original);assert(!munlock(original,LEN));}
    mach_vm_address_t remapped=0;vm_prot_t current=0,maximum=0;
    kern_return_t rc=mach_vm_remap(mach_task_self(),&remapped,LEN,0,VM_FLAGS_ANYWHERE,
        mach_task_self(),(mach_vm_address_t)(uintptr_t)original,FALSE,&current,&maximum,VM_INHERIT_NONE);
    printf("RO_REMAP code=%d current=%d maximum=%d copy=0\n",rc,current,maximum);
    if(rc==KERN_SUCCESS) {
        assert(!(maximum & VM_PROT_WRITE));inspect("remap-shared",(void *)(uintptr_t)remapped);
        assert(mach_vm_deallocate(mach_task_self(),remapped,LEN)==KERN_SUCCESS);
    }
    memory_object_size_t entry_size=LEN;mach_port_t entry=MACH_PORT_NULL;
    rc=mach_make_memory_entry_64(mach_task_self(),&entry_size,(memory_object_offset_t)(uintptr_t)original,
        VM_PROT_READ,&entry,MACH_PORT_NULL);
    printf("RO_ENTRY code=%d size=%llu requested_protection=%d\n",rc,(unsigned long long)entry_size,VM_PROT_READ);
    if(rc==KERN_SUCCESS) {
        mach_vm_address_t mapped=0;
        rc=mach_vm_map(mach_task_self(),&mapped,LEN,0,VM_FLAGS_ANYWHERE,entry,0,FALSE,
            VM_PROT_READ,VM_PROT_READ,VM_INHERIT_NONE);
        printf("RO_ENTRY_MAP code=%d copy=0 current=1 maximum=1\n",rc);
        if(rc==KERN_SUCCESS) {
            inspect("readonly-entry",(void *)(uintptr_t)mapped);
            assert(mach_vm_deallocate(mach_task_self(),mapped,LEN)==KERN_SUCCESS);
        }
        mach_port_deallocate(mach_task_self(),entry);
    }
    assert(hv_vm_destroy()==HV_SUCCESS);
    assert(!munmap(original,LEN));assert(!munmap(producer,LEN));close(ro);close(rw);
    puts("readonly-hvf-check complete: all aliases retained original pages and readonly max protection");
}
