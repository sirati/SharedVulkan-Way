#include "elf.h"
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#define SIZE (32ULL*1024*1024)
static void image(const char *path, uint64_t addr) {
    elf64_ehdr_t h={0}; memcpy(h.e_ident,"\177ELF",4);
    h.e_ident[EI_CLASS]=ELFCLASS64; h.e_ident[EI_DATA]=ELFDATA2LSB;
    h.e_type=ET_EXEC; h.e_machine=EM_AARCH64; h.e_phoff=sizeof(h);
    h.e_phentsize=sizeof(elf64_phdr_t); h.e_phnum=1;
    elf64_phdr_t p={0}; p.p_type=PT_LOAD; p.p_vaddr=addr; p.p_memsz=4096;
    FILE *f=fopen(path,"wb"); assert(f); assert(fwrite(&h,sizeof(h),1,f)==1);
    assert(fwrite(&p,sizeof(p),1,f)==1); assert(!fclose(f));
}
int main(void) {
    char path[]="/tmp/hl-elf-control-XXXXXX"; int fd=mkstemp(path); assert(fd>=0); close(fd);
    unsigned char *mem=malloc(SIZE); assert(mem); elf_info_t i;
    image(path,0x400000); assert(!elf_load(path,&i)); memset(mem,0xa5,SIZE);
    assert(!elf_map_segments(&i,path,mem,SIZE,0)); assert(mem[0x400000]==0);
    assert(mem[0x3fffff]==0xa5); assert(mem[SIZE-16*1024*1024]==0xa5);
    uint64_t bad[]={0x100000,SIZE-16*1024*1024, SIZE-16*1024*1024-1};
    for(unsigned k=0;k<sizeof(bad)/sizeof(*bad);k++) {
        image(path,bad[k]); assert(!elf_load(path,&i)); memset(mem,0xa5,SIZE);
        assert(elf_map_segments(&i,path,mem,SIZE,0)<0);
        for(size_t j=0;j<SIZE;j++) assert(mem[j]==0xa5);
    }
    image(path,0x400000); assert(!elf_load(path,&i));
    i.phdr_gpa=0x200000; memset(mem,0xa5,SIZE);
    assert(elf_map_segments(&i,path,mem,SIZE,0)<0);
    for(size_t j=0;j<SIZE;j++) assert(mem[j]==0xa5);
    image(path,0x400000); assert(!elf_load(path,&i));
    i.num_segments=2; i.segments[1]=i.segments[0]; i.segments[1].gpa=0x200000;
    assert(elf_map_segments(&i,path,mem,SIZE,0)<0);
    for(size_t j=0;j<SIZE;j++) assert(mem[j]==0xa5);
    image(path,0x400000); assert(!elf_load(path,&i));
    FILE *f=fopen(path,"r+b"); assert(f); elf64_ehdr_t h;
    assert(fread(&h,sizeof(h),1,f)==1); h.e_phentsize=1;
    rewind(f); assert(fwrite(&h,sizeof(h),1,f)==1); fclose(f);
    assert(elf_map_segments(&i,path,mem,SIZE,0)<0);
    for(size_t j=0;j<SIZE;j++) assert(mem[j]==0xa5);
    image(path,0x400000); assert(!elf_load(path,&i)); image(path,0x100000);
    memset(mem,0xa5,SIZE); assert(elf_map_segments(&i,path,mem,SIZE,0)<0);
    for(size_t j=0;j<SIZE;j++) assert(mem[j]==0xa5);
    image(path,0x800000000000ULL); assert(!elf_load(path,&i));
    assert(!elf_map_segments(&i,path,mem,SIZE,0x400000-0x800000000000ULL));
    unlink(path); free(mem); puts("ELF protected backing and snapshot regressions passed");
}
