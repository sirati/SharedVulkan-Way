# Both public stack and override integration consume these exact sources.
# Acceptance receipts must additionally pin the realized derivation inputs.
{
  patches = [ ./host-backed-mappings.patch ./initial-scripts.patch ./seccomp.patch ./external-guest-fault.patch ./peek-rights-guard.patch ./guest-status.patch ./seqpacket.patch ./async-signal-context.patch ./deferred-signal-irq.patch ./primary-access-permissions.patch ./primary-pt-domain.patch ./mmap-native-authority.patch ./native-buffer-authority.patch ./user-state-guard.patch ./gdb-user-state.patch ./dczva-guard.patch ./fork-state-close.patch ./fd-authority.patch ./proc-fd-authority.patch ./seqpoll.patch ];
  sources = [
    ./hl_shared.c
    ./hl_shared.h
    ./hl_pt_access.h
    ./hl_guest_iov.h
    ./hl_user_regs.h
    ./hl_pt_domain.h
    ./hl_seals.c
    ./hl_seals.h
    ./hl_drm.c
    ./hl_drm.h
    ./hl_socket.c
    ./hl_socket.h
    ./hl_seqpoll.h
    ./hl_seqpacket.c
    ./hl_seqpacket.h
    ./hl_script.h
    ./hl_copy.c
    ./hl_copy.h
    ./hl_atomic.c
    ./hl_atomic.h
    ./hl_seccomp.c
    ./hl_seccomp.h
    ./hl_status.c
    ./hl_status.h
    ./hl_seccomp_bpf.c
    ./hl_seccomp_bpf.h
    ../../include/wc_hyperlinux.h
  ];
}
