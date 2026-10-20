from pathlib import Path
import sys
src,out=map(Path,sys.argv[1:]);out.mkdir(parents=True,exist_ok=True)
here=Path(__file__).parent
s=(src/'src/syscall.h').read_text();a=s.index('#define FD_TABLE_SIZE');b=s.index('/* ---------- API ---------- */',a)
(out/'portable.h').write_text('#define SYSCALL_H\n#include <stdint.h>\n'+s[a:b]+'''\n#define LINUX_O_CLOEXEC 02000000
#define LINUX_EBADF 9
#define LINUX_EINVAL 22
#define LINUX_ENOMEM 12
#define LINUX_EMFILE 24
#define LINUX_ENOTDIR 20
''')
base=(here/'fd-authority-check.c').read_text().split('int main(void)')[0]
base=base.replace('void hl_vdir_release(void *p) { (void)p; }', 'void hl_vdir_release(void *p) { if (--*(int *)p == 0) free(p); }').replace('void hl_vdir_retain(void *p) { (void)p; }', 'void hl_vdir_retain(void *p) { ++*(int *)p; }')
v=(src/'src/vfs.c').read_text();v=v[v.index('typedef struct {',v.index('/* Map a guest directory fd')):v.index('\nint hl_vfs_resolve_at')]
preamble='''
#include "hl_fd_lifecycle.h"
#include <assert.h>
#include <sys/stat.h>
#define HL_VFS_PATH_MAX 4096
#ifndef F_GETPATH
#define F_GETPATH 9999
#endif
static int hl_vfs_host_to_guest(const char *a,char *b,size_t n){snprintf(b,n,"%s",a);return 0;}
const char *hl_vdir_path(const void*p){return p ? (const char *)p+sizeof(int) : NULL;}
'''
main=r'''
int main(void) {
 bitmap_init();
 char a[]="/tmp/hl-vfs-a-XXXXXX",b[]="/tmp/hl-vfs-b-XXXXXX";
 assert(mkdtemp(a)&&mkdtemp(b));
 int old=open(a,O_RDONLY),other=open(b,O_RDONLY);assert(old>=0&&other>=0);
 int g=fd_alloc(FD_DIR,old);vfs_dirfd_snapshot_t snap;
 assert(vfs_dirfd_snapshot(g,&snap)==0);
 /* Close and replace the original native number as well as the guest slot. */
 close(old);assert(dup2(other,old)==old);fd_table[g].host_fd=old;
 struct stat held,reused;assert(fstat(snap.ref.host_fd,&held)==0);assert(fstat(old,&reused)==0);
 assert(held.st_ino!=reused.st_ino);
 int cwd=open(".",O_RDONLY);assert(fchdir(snap.ref.host_fd)==0);
 struct stat current;assert(stat(".",&current)==0&&current.st_ino==held.st_ino);
 assert(fchdir(cwd)==0);close(cwd);
#ifdef __APPLE__
 char nativepath[4096];assert(vfs_dirfd_path(&snap,nativepath,sizeof(nativepath))==0);
 struct stat resolved;assert(stat(nativepath,&resolved)==0&&resolved.st_ino==held.st_ino);
#endif
 vfs_dirfd_drop(&snap);
 hl_open_file_t *of=hl_open_file_create(FD_REGULAR,NULL,0,NULL);assert(of);
 of->guest_path_hint=strdup(a);fd_table[g].of=of;
 assert(vfs_dirfd_snapshot(g,&snap)==0);
 of->guest_path_hint[0]='X';char path[4096];assert(vfs_dirfd_path(&snap,path,sizeof(path))==0);
 assert(strcmp(path,a)==0);vfs_dirfd_drop(&snap);fd_table[g].of=NULL;hl_open_file_release(of);
 void *vd=malloc(sizeof(int)+16);*(int *)vd=1;strcpy((char *)vd+sizeof(int),"/synthetic");
 fd_table[g].type=FD_VIRTUAL_DIR;fd_table[g].host_fd=-1;fd_table[g].dir=vd;
 assert(vfs_dirfd_snapshot(g,&snap)==0);hl_vdir_release(vd);fd_table[g].dir=NULL;
 assert(vfs_dirfd_path(&snap,path,sizeof(path))==0&&strcmp(path,"/synthetic")==0);
 vfs_dirfd_drop(&snap);
 fd_table[g].type=FD_CLOSED;assert(vfs_dirfd_snapshot(g,&snap)==-LINUX_EBADF);
 assert(vfs_dirfd_snapshot(-1,&snap)==-LINUX_EBADF);
 assert(vfs_dirfd_snapshot(FD_TABLE_SIZE,&snap)==-LINUX_EBADF);
 close(old);close(other);rmdir(a);rmdir(b);
 puts("PASS VFS exact native identity, owned hint, fchdir after close/reuse, invalid guest descriptors");return 0;
}
'''
(out/'vfs-check.c').write_text(base+preamble+v+main)
