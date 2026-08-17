# SPECS

Semi-structured specifications and decision log for the project as a whole. Per-directory
SPECS.md files (where they exist) cover that subsystem in more depth; this file covers
cross-cutting decisions that don't belong to a single directory.

## Device identity: fingerprinting and IP migration

### Problem

Every device (KEBA wallbox, MikroTik router/APs, Sonnen battery, myStrom switches) is keyed in
the `Devices` table by its IP address (`ip TEXT NOT NULL UNIQUE`). When a device's IP changes —
a new DHCP lease, a static IP reassignment — discovery has no way to tell that this is the same
physical device at a new address. It registers a second row at the new IP, and the old row is
left behind: permanently unreachable, but still shown in the UI (as long as its poll loop keeps
running or it has a user-assigned label) and still holding whatever credentials/history/label
were attached to it.

Concretely: the KEBA wallbox's IP changed, and this played out exactly as above — a second
`Devices` row appeared at the new address, the old row's poll loop kept failing forever, and the
dead IP stayed visible in the UI as a device that can't be connected to.

### Decision: MAC address as the stable fingerprint

Considered fetching a per-device serial number (e.g. KEBA's `Serial` field, already present in
its UDP report replies) as the stable identity. Rejected as the primary mechanism because it's
device-type-specific — every device type would need its own identity-extraction protocol work,
and the two devices with the most to lose from an IP change (MikroTik, Sonnen — both hold
credentials that would otherwise need re-entering) need authentication to query anything at all,
which may not be available yet at discovery time.

Chose the LAN MAC address instead, read from the local kernel ARP cache (`/proc/net/arp`, which
`devices::scan_all_networks` was already parsing for ping targets). This requires no new
per-device protocol work, needs no credentials, and covers all device types uniformly since
they're all on the same LAN segment as this host. A device's MAC survives its IP changing, and a
device without a resolvable ARP entry simply has no fingerprint (`None`) — it falls back to the
IP-only behavior that already existed, unaffected.

The `Devices` table gained a nullable `fingerprint` column with a `UNIQUE(type, fingerprint)`
index (NULLs don't collide with each other in SQLite, so unfingerprinted rows are unaffected).

### Reconciliation behavior (`db::upsert_device`)

On every discovery cycle, each detected device is saved with its current IP and (if resolvable)
its MAC. If a row of the same device type already exists with the same fingerprint at a
*different* IP, that row is the same physical device — its `ip` column is updated in place,
preserving its id, credentials, label and poll interval. No new row is created.

If a row already exists at the *destination* IP (e.g. the device was auto-discovered as a "new"
device during the window before its fingerprint was known), that row's `RawDeviceMeasurements`,
`Energy`, `EnergyStorage` and `SwitchTimers` rows are reassigned onto the fingerprinted row's id,
and the now-empty duplicate is deleted. The fingerprinted row always survives the merge, since the
fingerprint is what proves it's the same device; the mechanism is fully automatic (no
confirmation prompt) since this is the whole point of the feature — a device moving IPs should
require no manual intervention.

### Poll loop cleanup

A poll loop captures its target IP once at spawn time and has no way to observe that its
device's DB row later moved elsewhere. Each poll loop (KEBA, MikroTik, Sonnen, myStrom) now
checks, on the first tick it goes `Lost` (not every failed tick — that would be one extra query
per device per poll interval for no benefit), whether its device id's IP in the DB still matches
the address it's polling (`db::device_moved`). If not, the loop clears its entries from the
in-memory state maps (`conn_status`, `last_error`, the per-type readings map, `polled_ips`) and
exits — a fresh loop for the new address is already running, spawned by the discovery cycle that
performed the migration. This is what makes the dead IP actually disappear from the UI, rather
than merely stopping it from accumulating new duplicate rows going forward.

### Shared poll-failure handling (2026-08-17)

The per-device poll-loop cleanup described above was originally implemented four
times — once in each of the KEBA, MikroTik, Sonnen and myStrom poll loops — as
blocks that were identical except for which readings map they cleared. Every
change to the failure policy therefore had to be made in four places, and the
KEBA copy had already drifted (it cleared two maps where the others cleared one).

Consolidated into `devices::handle_poll_failure`, which owns the whole error
path: increment-to-`Lost` transition, the one-shot `db::device_moved` check on
the tick the device goes `Lost`, and recording the failure for display. It
returns a bool rather than taking a callback, so each loop still decides
literally whether to `return` — control flow stays visible at the call site.

State cleanup moved to `App::forget_device`, which clears *all* per-type readings
maps rather than only the caller's. An IP hosts exactly one device, so the extra
removals are no-ops; doing it uniformly means adding a new device type cannot
leave a stale row on screen by forgetting to extend one caller. (It deliberately
does not clear every `HashMap<IpAddr, _>` on `App` — see REVIEW.md.)

The `failures == 4` / `failures <= 3` literals became a single
`LOST_AFTER_FAILURES` constant, which makes the relationship between the two
explicit: the migration check fires on exactly the tick the device transitions
to `Lost`, not before and not repeatedly.

### Single DB timestamp format (2026-08-17)

`"%Y-%m-%d %H:%M:%S"` was written out in five places: a private `ts` helper
duplicated verbatim in three device modules, an inline `format` call in the
MikroTik loop, and the `parse_from_str` in `db.rs` that reads those values back.

A device type formatting its timestamps differently would not fail on write —
the columns are `TEXT` — it would silently fail to parse on read. Write and read
now share one `devices::DB_TIMESTAMP_FMT` constant and one `devices::ts` helper,
so the two halves cannot drift apart.

The display timestamp in `tui/render.rs` deliberately still formats its own
string: it uses `Local::now()` for the user's benefit and is not a DB value.

### One-time backfill (2026-08-14)

The KEBA duplicate already present in the live database (id 3256 at the old IP, holding
continuous history from 2026-07-03; id 156986 at the new IP, holding history since the IP changed
around 2026-08-09) was merged by hand: measurement rows from 156986 reassigned onto 3256, 156986
deleted, 3256's `ip` updated to the current address. Done with the live `dom` process stopped
(avoids a race between the merge and an in-flight poll writing to the row being deleted) and a
checkpointed backup of `db.sqlite` taken first.
