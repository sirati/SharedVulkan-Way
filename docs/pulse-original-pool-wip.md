Inactive original-pool protocol checkpoint

This work is incomplete and must not replace the accepted strict provider.
32f8590a is the strict refusal foundation. f463117b is the immutable inactive
bounded backend snapshot. The protocol topic adds an unused provider expression,
an integration patch and native ctypes fixture sources. No packaged session or
provider enables or selects this protocol implementation.

The proposed supported domain is one explicitly trusted native UID with complete
O_RDWR access to each original backing pool. Peer UID comes from native kernel
credentials; client declarations/cookie possession do not replace that check.
No rights attenuation, guest multiuser isolation or cross-user sharing is claimed.
The accepted HyperLinux patch reports native UID/EUID/GID and retains stubbed
identity setters. Its VM-wide single-UID/mapper admission gate must be confirmed
before activation. Native producer death was tested; HyperLinux death was not.

Both library and listener require explicit capability opt-in. A client requires
PULSE_ORIGINAL_POOL_LEASES=native-uid-whole-pool-v1. Listener option is
original-pool-leases=yes; it implies strict mode and disables SRB. Registration
and block references must share one ordered socket parser, with no migration or
reconnect reuse. Registration failure must stop all buffered following frames.
That failure-dispatch property still needs a focused runtime test.

Source-owned exporter leases reserve whole-pool quarantine budget before auth.
Broker imports orphan on source close, retaining original segment mappings until
references drain. Source revoke under negotiated mode is a transport failure,
never localization. The immutable foreign-origin guard remains in place.

A destination ledger permits two original aliases. Each binds source pool and
segment identity, fstat dev/inode/size/UID/access, retained original FD, segment
anchor and persistent exporter. Alias IDs collision-check every registered ID.
Every published capability block ID maps to its exact exporter. Unknown IDs,
collisions, quota exhaustion or failed export cause transport failure, not PCM
payload fallback. The process quarantine limit is 128 MiB. Receiver registrations
are bounded separately. These limits intentionally fail closed.

Destination disconnect is not consumption completion. Outstanding forwarded
export refs are quarantined too, preserving upstream producer leases even if the
recorder retains application references after socket close. Normal RELEASE drains
refs; uncertain close retains a bounded reservation until provider process exit.
Library unload/fork residency and broader concurrency still require proof.

No-copy acceptance must compare original backing identity and offset for every
forwarded producer frame, not just shared byte totals. Mixer/resampler fresh
copies are outside immutable same-object ancestry tracking. Use matching sample
spec/channel map, unity volume and one active null-sink input; account separately
for native-owned startup silence. No full Linux/native/Linux forwarding proof
exists for this checkpoint.

FIFO diagnosis: the accepted 1b race fix intentionally permits readable EOF with
POLLIN and read()==0, without requiring POLLHUP. The older recording startup
fixture demanded HUP and failed before audio startup. New evidence preserves that
failure and aligns only the new fixture with the accepted readable-EOF semantics.
Recording compatibility against the strict provider has not yet been re-proved.

Before activation review/test registration failure plus buffered next frame,
destination/source close with live refs, producer death, alias/source-ID and
exporter-ID collisions, quota refusal with zero payload, and same-UID admission.
