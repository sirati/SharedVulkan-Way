#!/usr/bin/env python3
"""Cross-build LP64 Cygwin local libwayland without a broker or VM BAR."""
import argparse
import sys
sys.dont_write_bytecode = True
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess

REPO=Path(__file__).resolve().parent.parent
spec=importlib.util.spec_from_file_location('mingw_build',REPO/'scripts/wc-mingw-build.py')
common=importlib.util.module_from_spec(spec)
spec.loader.exec_module(common)
run=common.run
fetch=common.common.fetch
digest=common.common.digest
pe_imports=common.common.pe_imports


def build(root):
    root=root.resolve()
    if not str(root).startswith('/mnt/datasets/'):
        raise RuntimeError('scratch must be under /mnt/datasets')
    root.mkdir(parents=True,exist_ok=True)
    if subprocess.run(['git','-C',root,'rev-parse','--is-inside-work-tree'],capture_output=True).returncode==0:
        raise RuntimeError('scratch must be outside git')
    pins=json.loads((REPO/'integrations/cygwin/sources.json').read_text())
    for name,item in pins.items():
        fetch(item['url'],root/name,item['sha256'],'sha256')
    sysroot=root/'sysroot'
    sysroot.mkdir(exist_ok=True)
    for item in json.loads((REPO/'integrations/cygwin/packages.json').read_text()).values():
        archive=root/Path(item['path']).name
        fetch('https://mirrors.kernel.org/sourceware/cygwin/'+item['path'],archive,item['sha512'],'sha512')
        run(['tar','-xf',archive,'-C',sysroot])
    source=root/'wayland-1.24.0'
    if source.exists():shutil.rmtree(source)
    run(['tar','-xf',root/'wayland-1.24.0.tar.xz','-C',root])
    output=root/'build';output.mkdir(exist_ok=True)
    gcc=sysroot/'usr/lib/gcc/x86_64-pc-cygwin/14'
    flags=[os.environ.get('WC_MACOS_CLANG','clang'),'--target=x86_64-pc-cygwin','--sysroot='+str(sysroot),'-isystem',str(sysroot/'usr/include'),'-B'+str(gcc)+'/',
           '-I'+str(output),'-I'+str(source/'src'),'-I'+str(REPO/'integrations/windows-local'),'-I'+str(sysroot/'usr/lib/libffi-3.8.0/include')]
    native=REPO/'target/x86_64-pc-windows-gnu/debug'
    run(['cargo','build','-p','wc-local-client','--target','x86_64-pc-windows-gnu'])
    def compile(file,name,extra=()):
        obj=output/(name+'.o');run([*flags,'-O2',*extra,'-c',file,'-o',obj]);return obj
    def link(objects,target,shared=False,libraries=(),import_library='libwayland-client.dll.a'):
        options=['--shared','--enable-auto-image-base','-e','_cygwin_dll_entry','--export-all-symbols','--out-implib='+str(output/import_library)] if shared else ['--subsystem','console','--tsaware']
        startup=[gcc/('crtbeginS.o' if shared else 'crtbegin.o')]
        if not shared:startup.insert(0,sysroot/'usr/lib/crt0.o')
        run([str(Path(os.environ.get('WC_MACOS_LLD_BIN',''))/'ld.lld'),'-m','i386pep',*options,'--dll-search-prefix=cyg','-o',target,*startup,*objects,'-L'+str(sysroot/'usr/lib'),'-L'+str(sysroot/'usr/lib/w32api'),'-L'+str(gcc),'-L'+str(output),*libraries,'-lcygwin','-lgcc_s','-lgcc','-ladvapi32','-lshell32','-luser32','-lkernel32',gcc/'crtend.o'])
    (output/'config.h').write_text('#define HAVE_ACCEPT4 1\n#define HAVE_BROKEN_MSG_CMSG_CLOEXEC 1\n#define PACKAGE_VERSION "1.24.0"\n')
    shutil.copy(output/'config.h',source/'config.h')
    version=(source/'src/wayland-version.h.in').read_text()
    for key,value in [('WAYLAND_VERSION','1.24.0'),('WAYLAND_VERSION_MAJOR','1'),('WAYLAND_VERSION_MINOR','24'),('WAYLAND_VERSION_MICRO','0')]:version=version.replace('@'+key+'@',value)
    (output/'wayland-version.h').write_text(version)
    for mode,extra,name in [('client-header',[],'wayland-client-protocol.h'),('client-header',['-c'],'wayland-client-protocol-core.h'),('public-code',[],'wayland-protocol.c')]:run(['wayland-scanner',mode,*extra,source/'protocol/wayland.xml',output/name])
    posix=compile(REPO/'integrations/cygwin-local/posix.c','posix',['-Wall','-Wextra','-Werror'])
    link([posix,native/'libwc_local_client.dll.a'],output/'cygwc-local-posix-0.dll',True,import_library='libwc-local-posix.dll.a')
    common.patch_descriptors(source)
    file=source/'src/connection.c'
    file.write_text('#include <stddef.h>\nextern size_t wc_native_fd_word_count(int);\nextern int wc_native_fd_words(int,unsigned int *,size_t);\nextern int wc_native_fd_from_words(const unsigned int *,size_t);\n'+file.read_text())
    file=source/'src/wayland-client.c';text=file.read_text();needle='fd = connect_to_socket(name);'
    if text.count(needle)!=1:raise RuntimeError('upstream connect hook changed')
    text=text.replace(needle,'fd = wc_cygwin_local_socket(name);').replace('\tclose(display->fd);','\twc_cygwin_ring_disconnect(display->fd);\n\tclose(display->fd);')
    file.write_text('extern int wc_cygwin_local_socket(const char *);\nextern void wc_cygwin_ring_disconnect(int);\n'+text)
    pump=(REPO/'integrations/cygwin/ring-transport.c').read_text().replace('__declspec(dllimport) ','')
    pump=pump.replace('void wc_cygwin_ring_disconnect(int fd)\n{','void wc_cygwin_ring_disconnect(int fd)\n{\n    if(local_child)return;\n    pthread_mutex_lock(&connect_lock);')
    pump=pump.replace('pthread_mutex_unlock(&owner_lock); return;','pthread_mutex_unlock(&owner_lock); pthread_mutex_unlock(&connect_lock); return;')
    pump=pump.replace('fprintf(stderr, "WC_CYGWIN_PUMPS_JOINED threads=2\\n");','fprintf(stderr, "WC_CYGWIN_PUMPS_JOINED threads=2\\n");\n    pthread_mutex_unlock(&connect_lock);')
    for operation in ['open','read','write','close']:pump=pump.replace('wc_bridge_'+operation,'wc_local_transport_'+operation)
    transport=(REPO/'integrations/cygwin-local/transport.c').read_text()+'\n'+pump+'''
int wc_cygwin_local_socket(const char *name) {
 pthread_mutex_lock(&connect_lock);
 pthread_mutex_lock(&owner_lock);int active=owner_fd!=-1;pthread_mutex_unlock(&owner_lock);
 if(active){pthread_mutex_unlock(&connect_lock);errno=EBUSY;return -1;}
 connecting_name=name;
 int fd=wc_cygwin_ring_socket(); connecting_name=NULL;
 pthread_mutex_unlock(&connect_lock); return fd;
}
'''
    (output/'local-transport.c').write_text(transport)
    objects=[compile(source/('src/'+name+'.c'),name,['-Dsendmsg=wc_cygwin_sendmsg'] if name=='connection' else []) for name in ['wayland-util','wayland-client','connection']]
    objects += [compile(output/'wayland-protocol.c','protocol'),compile(REPO/'integrations/cygwin/wayland-os.c','os'),compile(output/'local-transport.c','transport'),native/'libwc_local_client.dll.a']
    link(objects,output/'cygwayland-client-0.dll',True,['-lwc-local-posix','-lffi'])
    upstream=root/'weston-upstream'
    for directory in ['clients','shared','libweston']:(upstream/directory).mkdir(parents=True,exist_ok=True)
    for name,target in [('weston-info.c','clients/weston-info.c'),('simple-shm.c','clients/simple-shm.c'),('os-compatibility.h','shared/os-compatibility.h'),('zalloc.h','libweston/zalloc.h')]:shutil.copy(root/name,upstream/target)
    protocols=[]
    for name in ['xdg-shell','fullscreen-shell-unstable-v1']:
        run(['wayland-scanner','client-header',root/(name+'.xml'),output/(name+'-client-protocol.h')]);run(['wayland-scanner','public-code',root/(name+'.xml'),output/(name+'-protocol.c')]);protocols.append(compile(output/(name+'-protocol.c'),name))
    for name,extra in [('weston-info',[]),('simple-shm',protocols)]:
        obj=compile(upstream/('clients/'+name+'.c'),name,['-I'+str(upstream)]);link([obj,*extra],output/(name+'.exe'),libraries=['-lwc-local-posix','-lwayland-client'])
        if digest(upstream/('clients/'+name+'.c'))!=pins[name+'.c']['sha256']:raise RuntimeError('application source changed')
    abi=output/'abi.c'
    abi.write_text('#ifndef __CYGWIN__\n#error not Cygwin\n#endif\n#include <stdio.h>\n_Static_assert(sizeof(long)==8,"Cygwin LP64");\n_Static_assert(sizeof(void*)==8,"64bit pointers");\nint main(void){puts("Cygwin LP64 runtime");return 0;}\n')
    link([compile(abi,'abi')],output/'abi.exe')
    obj=compile(REPO/'integrations/windows-local/posix-probe.c','posix-probe')
    link([obj,native/'libwc_local_client.dll.a'],output/'posix-probe.exe',libraries=['-lwc-local-posix'])
    bundle=root/'bundle';bundle.mkdir(exist_ok=True)
    artifacts=[output/name for name in ['abi.exe','posix-probe.exe','weston-info.exe','simple-shm.exe','cygwayland-client-0.dll','cygwc-local-posix-0.dll']]+[native/'wc_local_client.dll',native/'wc-local-namespace.exe',*list((sysroot/'usr/bin').glob('*.dll'))]
    manifest={'sources':pins,'artifacts':{},'adapter_sources':{str(file.relative_to(REPO)):digest(file) for directory in ['crates/wc-local-client','integrations/cygwin-local','integrations/windows-local'] for file in (REPO/directory).rglob('*') if file.is_file()}}
    for item in artifacts:
        destination=bundle/item.name
        if destination.exists():destination.chmod(0o600)
        shutil.copyfile(item,destination);manifest['artifacts'][item.name]={'sha256':digest(item),'imports':pe_imports(item)}
    for name in ['simple-shm.exe','weston-info.exe','cygwayland-client-0.dll']:
        if 'cygwin1.dll' not in [x.lower() for x in manifest['artifacts'][name]['imports']]:raise RuntimeError('not a genuine Cygwin ABI executable')
    common.provenance(root,output,source,bundle,manifest)
    (bundle/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
    print('CYGWIN_LOCAL_ORDINARY_APP_BUILD_OK',bundle,flush=True)


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--scratch',required=True,type=Path);build(parser.parse_args().scratch)
