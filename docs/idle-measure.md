# Isolated control latency on the system layout

`scripts/wc-idle-measure.py` measures an existing dedicated `wcil` test
installation after the broker migration. It never deploys, replaces shared
services, changes power settings, restarts the VM, or kills the shared
compositor. SSH carries management commands and evidence retrieval. The
launcher times requests on the shared-memory control ring with guest QPC
stamps, so SSH latency is outside the measured round trips.

Copy `scripts/wc-idle-measure.example.json` outside git and replace every
placeholder with the deployment executor's approved values. Pin the exact
launcher, guest compositor, host proxy, host daemons, installed wc-session
script, and stock foot hashes. The compositor selected in the guest ini
must match its approved path. An already running shared compositor must
also have the approved image hash; the harness never replaces or kills it.
The source commit must contain the broker and original latency fixes.
There is no default Firefox executable or administrator profile.

Coordinate deployment with the broker owner. The dedicated install must be
listed in `wctestInstalls` and map the authenticated guest identity to
`wctest`. System `wc-bell-hub` and `wc-sessiond` must run as `waycompose`;
the hub must have `rings=`. Install only the test installation with
`WC_LAUNCH_HOST_LAYOUT=system`, `WC_LAUNCH_SESSION=wcil`,
`WC_LAUNCH_HOST_ROOT='sudokey run --'`, and `WC_LAUNCH_BROKER=key`. This mode provisions only this installation's
broker-held signing key and removes its local session.key; it never installs
or restarts the broker or changes device ACLs. `skip` is not valid for this
broker deployment. Use the key mode only after its provisioning/ownership
fix has passed the broker owner's Windows clone gate.
Leave shared-service and system-binary replacement options at `keep`.
`WC_LAUNCH_TEST_HOME` alone is not a deployment mechanism for app isolation.

The installed `/var/lib/waycompose/installs/wcil/test-home` must contain
one fresh directory under `/var/tmp/waycompose-tests/`. `wc-session` reads
this marker and sets HOME and all XDG base directories to its
`home/config/data/state/cache` subdirectories. The mapped account must own
that throwaway state. The cold-start measurement verifies the running
foot process's HOME/XDG values and UID. Alternatively deploy explicit
matching `env` entries in the `[foot]` apps.ini section as well. The harness
rejects other app environment overrides. Use an unmodified stock foot
store path with no arguments; no wrapper or altered sandbox settings.

Set `[session] linger-secs = 900` or greater in the dedicated guest ini
before measuring. The default 60 seconds is insufficient for a long
session with no application clients. Require a Wayland doorbell and
`debug-ssh-transport = off`. Set `installation = wcil` and leave
`session-key` unset. Preflight uses the administrative SSH management
channel to inspect only existence, length, owner and ACL metadata for
`ProgramData\waycompose\keys\wcil.key` and its parent directory. It never
reads or hashes signing-key bytes. Both ACLs must be protected and allow
only SYSTEM and Administrators, with SYSTEM or Administrators ownership.
The broker must be running and no session.key may remain in the user's
installation. Insufficient metadata access is a refusal, not proof that
the key is provisioned. Unidentified mapping entries for wcil are refused.
Each run also requires a fresh compositor-log signing identity matching the
configured Windows user, alongside the actual mapped wctest proxy. Only
new log bytes after the run's captured length count as signing evidence; a
stale prior signing line cannot satisfy this check. The initial length is
captured even when no compositor process is visible. The open handle's
Windows volume/file identity and a hash of that exact prefix must still
match when fresh bytes are read. Truncation, replacement
with a different prefix, or log rotation causes a refusal, including a
compositor restart that overwrites an old log. The harness does not erase
or rotate a shared log to recover from that refusal. An already running wctest session or dedicated
guest launcher causes a refusal, leaving it untouched. The watchdog checks
host-to-guest ping and guest SSH every 30 seconds. A failed health check
aborts management commands without retrying or resetting the guest.

Run preflight without starting a session:

```sh
python3 scripts/wc-idle-measure.py /mnt/datasets/private-idle.json preflight
```

Collect four batches in separate leases. Each batch takes at least 500
seconds and the default lease allows 12 minutes. Startup and cleanup have
150 seconds reserved. A batch can contain at most 750 samples. The normal
lease helper waits for guest inactivity, including TeamViewer activity.
Do not bypass it for cold-start app runs. A unique tag never overwrites
an existing evidence directory.

```sh
python3 scripts/wc-idle-measure.py /mnt/datasets/private-idle.json batch --tag one
python3 scripts/wc-idle-measure.py /mnt/datasets/private-idle.json batch --tag two
python3 scripts/wc-idle-measure.py /mnt/datasets/private-idle.json batch --tag three
python3 scripts/wc-idle-measure.py /mnt/datasets/private-idle.json batch --tag four
python3 scripts/wc-idle-measure.py /mnt/datasets/private-idle.json report
```

The report requires at least 2000 complete samples from one approved build.
It pools raw samples rather than averaging batch percentiles. `--minimum`
may raise the 2000-sample acceptance threshold but cannot lower it. Each
batch records a fingerprinted immutable manifest containing every binary
pin, the harness hash, install and mapped-user identity, guest ini hash,
ambient guest latency-tuning settings, host service configuration, mapping table,
apps.ini, gpu.env, and test-home. Reports reject missing, changed, or mixed
manifests, including different host builds with the same launcher. The
running proxy image and compositor images are checked against their pins. Every request
must have a paired send/reply and at least 950 ms between the prior reply
and its send. The guest requests a 1000 ms sleep. The tolerance accepts
clock rounding, not back-to-back traffic. Report all samples, including
scheduling or GPU-wake outliers. The mean must be below 200 microseconds;
a passing median alone does not meet the target. No cross-machine clock
fit is needed for same-guest RTT. GPU-wake attribution needs separate host
traces and calibrated clocks and is not claimed by this harness.

Measure two stock-foot clicks with another lease:

```sh
python3 scripts/wc-idle-measure.py /mnt/datasets/private-idle.json cold --tag clicks-one
```

This records a session-cold click followed by a session-warm click, both
using the launcher's CLICK-TO-WINDOW stamps. The shared compositor may
already be running. These are not process-cold or Firefox results. The
first foot remains open for the second click; the final scoped `--stop`
ends only this installation's session. Compare these timings with the
historical foot table only after checking the PulseAudio wait remains in
the approved build. No successful result manifest is written until scoped
cleanup, final health, and lease release succeed. Teardown checks all
matching units with a successful manager response; activating,
deactivating, or other pending states cannot count as stopped. Only
inactive, failed, or gone units satisfy that gate. Failed attempts retain
command evidence but are excluded from aggregate reports.

Run local regression checks without a VM:

```sh
python3 scripts/test-wc-idle-measure.py
```
