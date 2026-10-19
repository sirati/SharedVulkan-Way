# Handover: branch `windows-guest-ram` (§108, R43)

State at commit 73133db (master f0b85c3 and `broker` 1f48b47 merged in).
Do NOT merge into master: the whole-RAM-file stand-in (`.ram` run lists +
lease files that name QEMU's memfd path) must not be deployable.

## What exists (all committed, all gates green at a629224)
- Spec: docs/waycompose-design.md §108.1–108.17 (region table, kinds, life,
  death, policy, Windows substrate, probe locator, doorbell, keeper, test VM,
  stress, GPU/Wayland e2e, testhost plan, udmabuf broker plan, not-done,
  gates). docs/security-model.md R43.
- Code:
  - `sv-transport`: `regiontab` (table + `wake` module), `lpstamp`
    (stamps + probe), `winram` (large-page sections), `poolhdr` word 32,
    `poolcold` (REGION_TOKEN 0xFB dispatch, `poke_region_at`).
  - `sv-ram`: `policy`, `provider` (incl. foreign adoption), `keeper`
    (pipe server, unnamed sections, per-SID, DuplicateHandle), `winsub`,
    `winsys`, `peer` (host locate/map/answer; `publish_to` = the stand-in),
    `simram` (Linux stand-in guest RAM), `udma` (udmabuf parts), `testlayout`.
  - `sv-gpu-server/src/regions.rs`: RegionManager (grow on demand/ahead,
    retire idle/deep idle, retire_all on exit, doorbell ring + wake waits).
    `pool.rs`: grower hook, add/remove_region, lease_growing, tests.
  - `sv-icd/src/transport.rs`: guest-RAM region mapping from `.ram` + flock
    (STAND-IN), unmap to PROT_NONE at last lease.
  - `wc-proxy`: `ProvidedRegion::ram_window` (STAND-IN), barwin rule K>=2.
  - `wc-compositor`: `PoolRegion::lease_in`, `replace_pool_region`;
    `wc-handle::arena::unregister_borrowed_base`; sv-guest `GpuRegion`,
    `GpuSubRegion`, `ram` subcommands (keeper/test/canary/hog/jitter/stats).
  - `wc-arena-host ram-peer` (host peer CLI; doorbell with reconnect).
- Harnesses: scripts/wc-guestram-stress.sh (test VM), sv-pool-grow-e2e.sh,
  wc-multi-install.sh leg 9.
- Test VM: winvm instance `wcram` (stopped, overlay kept), start with
  `WINVM_QEMU=/mnt/datasets/builds/wcram/qemu-wrap.sh ~/winvm/bin/winvm up wcram --mem 6G --cpus 4`,
  then /mnt/datasets/builds/wcram/deploy.sh (keeper as SYSTEM task, host
  peer), bg.sh (canary/jitter/hog), watchdog.sh. user2 exists (batch logon).
- Evidence: /mnt/datasets/builds/evidence/windows-guest-ram/.

## Open tasks (from the coordinator, 2026-10-19 late)
1. Region doorbells: DONE in 48b90cc (guest rings 0xFB on announce/retire,
   waits on `regiontab::wake`; peer waits on the bell with a 250 ms beat and
   reconnects). Not yet re-measured on the test VM (the GPU process isn't
   run there; `ram test` already used the bell).
2. Residual risk (keeper + all providers killed at once): plan = a SYSTEM
   "holder" child that holds duplicates of every section handle and does
   nothing else; the keeper re-adopts from it after a restart
   (DuplicateHandle from the holder's process), and respawns it if it dies.
   Remaining risk: keeper + holder + providers all killed. In the wc-broker
   world this is the Windows service's region backend.
3. Allocation tails: DONE in 73133db (256 MiB steps, 64 MiB low water,
   every lease wakes the region manager; sv-pool-grow-e2e: leases that
   waited for a grow 10 -> 2, total 31 -> 5.8 ms; §108.6). Not re-measured on
   the Windows test VM (needs a Windows GPU process; the keeper latency by
   size is already in §108.7).
4. wc-broker integration (broker API at 1f48b47, merged):
   - Host: embed in the broker-host daemon (another agent builds it): a
     `sv_ram::brokered` module that runs the Peer loop and, per MAPPED region,
     makes udmabuf parts (`udma::create`) and calls
     `Broker::publish(User::uid(session uid), pool, epoch, pieces)`; on
     RETIRING calls `Broker::retire(id)`; answers UNMAPPED only after
     `on_drained(id, epoch)` (replace `Peer::mappers_gone`/`publish_to`).
   - Clients (ICD transport.rs `map_ram_region`, proxy `ram_window`): get the
     region by `wc_broker::client::Client::open(id)`, map each piece with
     `map_fixed` at base+offset, `close(id)` after unmapping.
   - Region id lookup: the protocol has no `find POOL`; ask the broker owner
     for one (pool name e.g. `gpu.<k>`), or publish a non-secret
     `{device}-{k}.id` (id + epoch) as an interim.
   - NixOS VM test on vmhost: broker as its own system user under systemd,
     nested QEMU guest providing the memfd (minimal RAM), clients as an
     ordinary user that cannot open /dev/udmabuf or the memfd and get only
     their regions. Then delete the `.ram`/lease-file stand-in.
   - Keeper: maps to wc-broker Windows backend: `alloc POOL BYTES large`,
     `free ID`, `adopt POOL`, `hello ROLE`; stamping = the region-init hook
     the broker-win sub-agent adds.
