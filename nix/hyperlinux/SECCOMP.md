# Guest seccomp

The runtime evaluates immutable classic BPF programs over the AArch64 Linux
`seccomp_data` ABI before dispatching a guest syscall to any backend. The filter
chain uses Linux action precedence and retains the newest filter's data on ties.
Filter installation requires per-thread, monotonic `no_new_privs`. TSYNC checks
that each thread in the caller's explicit thread group has an ancestor chain,
then changes all eligible threads atomically under the thread-table lock.

The runtime supports filter mode and the TSYNC flag. It rejects strict mode and
other installation flags. GET_ACTION_AVAIL reports supported KILL_THREAD,
KILL_PROCESS, TRAP, ERRNO, TRACE and ALLOW actions. TRACE has no seccomp ptrace
event subscriber and returns ENOSYS; USER_NOTIF has no listener and returns
ENOSYS. A returned LOG action permits the syscall, but logging is not advertised
by GET_ACTION_AVAIL. Unknown winning actions terminate the thread group.

Threads inherit the chain and no-new-privileges under the table lock before
publication. Fork transfers a bounded pointer-free snapshot over the existing
IPC stream. Single-thread exec retains the existing thread state. Exec with other live
threads fails with EOPNOTSUPP before changing memory or descriptors; the runtime
does not implement sibling retirement or CLONE_VM address-space unsharing. CLONE_VM groups have
separate thread-group identities. Security termination interrupts vCPUs and host
waits and marks pending same-group clone children before they can run. Killing
the main guest thread keeps the host VM alive until remaining workers exit.

The Linux reference check `seccomp-bpf-check.c` compares the portable verifier
and interpreter against kernel seccomp installation and actual denied syscalls.
The AArch64 `seccomp-client.c` has independent modes for NNP and fork/exec,
a denied write that must leave a nonblocking pipe empty, synchronous SIGSYS
metadata and saved X0, and TSYNC into an already-running thread.

This is not evidence that Firefox's default sandbox works. The audited upstream
runtime has no unshare/setns or capget/capset handlers. UID/GID setters return
stub success. User and mount namespace isolation, capability changes, procfs
sandbox reporting, host credential checks, and the application's sandbox setup
must be validated separately. Existing general exit_group and ordinary signals
still use the VM-wide legacy exit helper. Native guest probes and unchanged
Firefox/RDD sandbox reporting are required before sandbox acceptance.
