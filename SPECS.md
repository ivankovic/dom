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
device's DB row later moved elsewhere. Each poll loop (KEBA, MikroTik, Sonnen, myStrom)
therefore checks, on the first tick it goes `Lost` (not every failed tick — that would be one
extra query per device per poll interval for no benefit), whether its device id's IP in the DB
still matches the address it's polling (`db::device_moved`). If not, the loop clears the dead
address out of the in-memory state and exits — a fresh loop for the new address is already
running, spawned by the discovery cycle that performed the migration. This is what makes the
dead IP actually disappear from the UI, rather than merely stopping it from accumulating new
duplicate rows going forward.

The check and the cleanup are shared rather than reimplemented per device type — see
*Shared poll-failure handling* below, which also records what the cleanup clears.

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

State cleanup moved to `App::forget_device`, which clears *every* map keyed by IP
rather than only the caller's readings map. An IP hosts exactly one device, so the
extra removals are no-ops; doing it uniformly means adding a new device type cannot
leave a stale row on screen by forgetting to extend one caller. It was initially
limited to the maps the four loops had cleared between them, to keep the extraction
behaviour-preserving; it now also clears `ping_history`, `ping_configs`,
`device_energy_today`, `switch_auto_modes`, `switch_timers` and
`last_network_status`, since an address the device has left cannot have meaningful
state under any of them.

The `failures == 4` / `failures <= 3` literals became a single
`LOST_AFTER_FAILURES` constant, which makes the relationship between the two
explicit: the migration check fires on exactly the tick the device transitions
to `Lost`, not before and not repeatedly.

### One-time backfill (2026-08-14)
The KEBA duplicate already present in the live database (id 3256 at the old IP, holding
continuous history from 2026-07-03; id 156986 at the new IP, holding history since the IP changed
around 2026-08-09) was merged by hand: measurement rows from 156986 reassigned onto 3256, 156986
deleted, 3256's `ip` updated to the current address. Done with the live `dom` process stopped
(avoids a race between the merge and an in-flight poll writing to the row being deleted) and a
checkpointed backup of `db.sqlite` taken first.

## Single DB timestamp format (2026-08-17)
`"%Y-%m-%d %H:%M:%S"` was written out in five places: a private `ts` helper
duplicated verbatim in three device modules, an inline `format` call in the
MikroTik loop, and the `parse_from_str` in `db.rs` that reads those values back.

A device type formatting its timestamps differently would not fail on write —
the columns are `TEXT` — it would silently fail to parse on read. Write and read
now share one `devices::DB_TIMESTAMP_FMT` constant and one `devices::ts` helper,
so the two halves cannot drift apart.

The display timestamp in `tui/render.rs` deliberately still formats its own
string: it uses `Local::now()` for the user's benefit and is not a DB value.

## HTTP probing follows every redirect that names a target (2026-08-17)
`fingerprint::probe_http` followed only 301. A device answering with 302 — much the most common
form for a login redirect — had its bare redirect response recorded as the probe, so the real
page was never fetched and any identifying markup on it could not contribute to detection.

Now follows 301, 302, 307 and 308. 300 (Multiple Choices) and 304 (Not Modified) stay excluded:
neither names a single resource to follow. The existing `MAX_REDIRECTS` bound and the
already-seen cycle guard are unchanged, so a redirect loop still terminates.

## Single answer for device type (2026-08-17)
Discovery classified each fingerprinted device twice per cycle, in two shapes that disagreed.
Persistence tried each detector as an independent `if`, so a fingerprint matching two detectors
was saved as two device types and started two poll loops; the naming code a few lines later
used an `else if` chain, so the UI showed only the first match. The stored type and the
displayed type could therefore differ.

Both now call `devices::detect_type`, which returns at most one `DeviceType`, first match wins.
This is not a theoretical overlap: three of the four detectors key off port 80 plus a substring
of the HTTP response body.

`DeviceType` deliberately does *not* carry the `Devices.type` column string. Each device module
already owns its own, and a second copy would be one more thing to keep in step.

The local machine stays outside this enum. It is identified by comparing against this host's own
interface addresses, which needs no fingerprint and cannot be mistaken — `dom_local::detect`,
which existed only to match the other modules' shape and always returned false, was removed.

## Local machine persists like every other device (2026-08-17)
`dom_local::save_device` issued its own SQL: an `INSERT OR IGNORE` followed by an unconditional
`UPDATE Devices SET name = ?, type = ? WHERE ip = ?`. That second statement rewrote whatever row
occupied the address, so if one of this host's interface addresses ever coincided with a row
discovered as another device type, that row's type and name were silently overwritten.

It now goes through `db::upsert_device` like every other type. Consequence worth noting: an
existing row at that address keeps its `type`, where the old code forced it to `dom_local`.

It passes no fingerprint. The local machine is re-derived from this host's own interfaces every
discovery cycle, so it has no identity to preserve across an address change — and our own
address never appears in our own ARP cache, which is where fingerprints come from.

## Licence: Anti-Capitalist Software License (2026-08-18)

Moved from the Prosperity Public License to the [Anti-Capitalist Software License
v1.4](https://anticapitalist.software/), removing paid commercial licensing entirely. There is now no
exemption to buy: an organization either satisfies the conditions or has no licence.

The two licences restrict along different axes, and the change is not simply "stricter":

*  Prosperity restricted by **purpose** — any commercial use, by anyone, needed a paid licence after a
   thirty-day trial.
*  The ACSL restricts by **who you are** — an individual working for themselves, a non-profit, an
   educational institution, or a co-operative whose owners are all workers with equal equity and vote.
   Those users may use Dom commercially, and may sell it, for free. A conventionally-owned company
   cannot use it at any price. Law enforcement and the military are excluded outright, which Prosperity
   said nothing about.

So this is *looser* for co-operatives, which previously would have had to pay, and absolute for
everyone else.

Costs of the move, recorded because they are real:

*  **No patent grant.** Prosperity granted patent rights explicitly; the ACSL is silent on patents.
*  **Conditions on identity are harder to self-assess** than conditions on purpose. "Are all owners
   workers with equal equity?" is a question about a company's constitution, not its intentions.
*  Still not OSI or FSF compliant, for the same reason as before and now also because it discriminates
   between users — which is a deliberate choice, not an oversight.

The previous grants remain irrevocable for the versions they were published under. The AGPL-3.0
versions were on Codeberg; the Prosperity versions were pushed to GitHub, so that grant is live for
anyone who took a copy, thirty-day commercial trial included. A licence change cannot reach backwards.

## Running on modest hardware (2026-08-26)

Prompted by moving Dom to a Raspberry Pi 2: 32-bit ARMv7, four 900 MHz cores,
1 GB of RAM, an SD card, and no battery-backed clock. Four things stopped it, and
none of them were about the Pi being small — they were costs that had simply never
been paid attention to on a desktop.

### What did not need changing

Worth recording, because it bounded the work. All 18 `as usize` sites are clamped
chart arithmetic and nothing narrows from a 64-bit integer, so there are no
32-bit hazards; timestamps are `i64` milliseconds throughout. Memory is about
98 MB. Four cores match a runtime that defaults to one worker each, and the work
is I/O-bound anyway.

### Decision: one transaction per poll, and `synchronous=NORMAL`

Every insert was its own transaction and `synchronous` was `FULL`, so every one
flushed the disk. A single battery poll wrote eleven of them, every two seconds —
five and a half disk flushes a second, forever, before counting the switches and
the wallbox. On an SD card that is both the dominant latency and what destroys
the card.

Each poll is now one transaction, which also buys atomicity worth having on its
own: the coarser tiers are derived from these rows, and a raw reading with no
energy beside it is worse than neither.

`NORMAL` risks only the last committed transactions on a power cut, and nothing at
all when the process dies, which WAL already covers. Against a series sampled
every two seconds that is invisible. Set through the connect options rather than
by issuing a `PRAGMA`, because it is a property of a connection: running the
pragma against the pool sets it on whichever one connection served that query.

Together: about 770,000 flushed commits a day become about 147,000, and those no
longer flush the disk individually.

### Decision: the today-charts read the tier that already holds their answer

`query_today_energy` re-aggregated 136,000 raw rows into per-minute averages every
sixty seconds, and `query_device_energy_today` did it again. Both filtered on
`date(timestamp, 'localtime')`, which no index can satisfy, so both scanned the
whole table: 0.5 s here, several seconds on a Pi, four times a minute.

`EnergyMinute` already holds exactly per-minute aggregates. Reading it instead,
bounded by explicit instants, is an index range scan — **0.498 s against 0.003 s**
on the same data.

The minute tier only advances when the rollup runs, so the last few minutes come
from the raw rows, split at `minute_tier_boundary`. The import/export and
charge/discharge halves are taken from the tier's own `energy_ws_pos`/`_neg`
rather than re-derived, because a minute that did both nets out.

An index on `Energy(timestamp)` makes that tail a seek rather than a scan. It
costs about 30 MB against a table retention holds to four days, and rows arrive in
timestamp order, so maintaining it is an append to the right edge of the tree —
close to the cheapest an index can be, which matters when the concern is SD wear.

Verified against the live database with a real boundary: identical to six decimal
places, and a test asserts the figures do not move as the boundary advances.

### Decision: no terminal is not an error

`ratatui::try_init()` failing returned an error from `main`, so a Dom with no TTY
exited immediately and collected nothing. That is the normal way to run this on a
Pi — as a service, headless.

`tui::run` now reports `Interface::Unavailable` instead, and `main` waits on
Ctrl-C or `SIGTERM` while the background tasks carry on. Nothing else changes:
polling, integration and the rollup tiers never needed a terminal, and the log is
where the output goes.

### Decision: wait for a plausible clock before recording

A Pi has no RTC. It boots at whatever was last written to disk and jumps when NTP
catches up, and a sample stamped days in the past is rolled into the wrong day —
or into one already marked done, which is never rolled up and is then pruned.

The database is the only evidence available that time has passed: a row stamped
last Tuesday proves the clock once read last Tuesday. Startup therefore waits
until the clock is at least as late as the newest thing recorded, for up to two
minutes. Bounded, because waiting forever is worse — a machine with no network
would never start. It does nothing at all where the clock is fine.

### Decision: hand freed pages back

`auto_vacuum=INCREMENTAL` only puts freed pages on a list; nothing shrinks the
file until `incremental_vacuum` is called, and nothing called it. That is how 265
MB of live data came to occupy 1.29 GB, reclaimed by a manual `VACUUM` on
2026-08-26.

The hourly prune now hands back a bounded number of pages, which runs alongside
everything else — unlike `VACUUM`, which rewrites the whole file under an
exclusive lock and needs Dom stopped.

`auto_vacuum` is a property of the file fixed at creation, so it is set in the
connect options and takes effect for new databases. An existing one created
without it stays as it is until a full `VACUUM` rewrites the format.

### Related fix: the switch had the battery's gap bug

`mystrom_switch::save_energy` integrated across unbounded intervals exactly as the
battery once did, so a switch unreachable for hours had its last known power
multiplied across the whole absence. The clamp is now shared as
`devices::max_integration_gap_ms`, derived from each device's own configured
interval.

### Building for the Pi

Cross-compile. `ring` and `libsqlite3-sys` need a C cross-compiler
(`gcc-arm-linux-gnueabihf`), and the release profile's fat LTO with one codegen
unit is likely to exhaust 1 GB of RAM on the device itself. **The ARM build has
not been compile-verified** — the toolchain was not installable here, so the
32-bit review above is by inspection.

## Self-sufficiency when the battery charges from the grid (2026-08-24)

### Problem

`consumption`, as the Sonnen reports it, is house load alone. Charging the
battery is not consumption — it is a separate sink, and the four measured powers
satisfy `production + battery = consumption + grid`, verified against 42,092
recorded samples to a mean residual of 29 W.

So `consumption − grid_import` is exactly self-sufficiency for as long as the
battery only ever charges from the sun, which is what all 55 recorded days do:
import never exceeds consumption on any of them. It stops being true the moment
the battery charges overnight from the grid, which it will do in winter.

A night importing 10 kWh into the battery while the house uses 2 reports −500%,
clamped to 0. The following day runs entirely off that battery and reports 100%.
Both are wrong, and it is the daily bars that the statistics view draws. Only the
two-day total — 12 consumed, 12 imported, 0% — is right, which is why the old
definition was never visibly broken.

### Decision: attribute grid energy to when it reaches the house

Two new metrics, recorded like any other and carried up by the existing tiers:

- `grid_to_house` — imported energy that served the house, directly or later via
  the battery. Self-sufficiency is measured against this.
- `grid_to_battery` — imported energy that went into the battery. Not part of
  consumption; shown in the totals panel when it is non-zero, because it is what
  explains an import figure far larger than the house used.

`self_consumed_kwh` and therefore the green segment of every bar now subtract
`grid_to_house` rather than `grid_import`.

### The battery is a tank, so provenance is tracked

Knowing whether energy leaving the battery was originally solar or grid cannot be
derived from the instantaneous flows — a battery's contents carry no labels. So
`energy::GridOrigin` carries how much of what is stored arrived from the grid, and
discharge is attributed in proportion to the mixture.

Being path-dependent, it accumulates every error there is: round-trip losses, the
29 W residual, intervals skipped because Dom was not running. Two observations
stop that becoming permanent drift, and both are facts rather than estimates —
the total can never exceed the battery's reported remaining capacity, and an
empty battery holds no grid energy at all, so reaching the 5% reserve floor
resets it outright. Without the second, one bad week would colour every day after
it.

### Each end of an interval is split separately

The instantaneous attribution assumes the battery is either charging or
discharging and the grid either importing or exporting. That holds at an instant
and not across one: measured on this installation, the grid reverses direction
within **9.6%** of consecutive samples (the battery only 0.92%). Splitting the
*average* of an interval that both imported and exported describes an interval
that did neither.

So each end is split on its own and the results are integrated — the trapezoid
rule applied to the derived quantities rather than to the raw ones.

### A gap keeps the mixture rather than guessing at it

`save_energy` refuses to integrate across a long gap, but the battery kept
working through it, so the provenance still has to cross it. Holding the *share*
constant is the assumption that makes no claim: the unobserved energy is taken to
have looked like what was already there. Calling it solar would flatter
self-sufficiency and calling it grid would punish it, and neither is known.

`resync` deliberately takes both the before and after totals. Taking only the new
one would compute the share from the very number it then scales by, which cannot
change the answer — the gap would silently do nothing.

### History keeps its old answer

Days already in the record have no `grid_to_house` series, and neither does the
minute tier they would be re-rolled from. The rollup sums that metric *without* an
`ELSE 0`, so its absence is NULL rather than zero, and NULL falls back to
`grid_import` — what the old definition always assumed. Treating absence as zero
would have reported that none of history's imported energy ever reached the
house, flipping every past day to 100% self-sufficient.

Verified against the live database: of 55 recorded days, the 4 still holding 2s
data re-roll to figures identical to before, and the other 51 are never revisited
at all.

## Stop polling the network for its own sake (2026-08-23)

### Problem

Two background tasks generated continuous traffic that nothing consumed.

`ping_infrastructure_task` sent three ICMP packets every ten seconds to each
router and modem, and three every thirty seconds to each access point — roughly a
packet a second, indefinitely — purely to populate the OK/SLOW/DEGRADED/LOST
column in the Network view.

The MikroTik poll ran every ten seconds and made three REST requests each time.
Across six devices that is nearly two round-trips a second, every one carrying the
router administrator password, to fetch DHCP leases whose only consumer —
discovery — runs every five minutes.

### Decision: derive health from the poll instead of measuring it separately

Infrastructure status now comes from `ConnStatus` and the duration of the poll
that produced it. That costs nothing: the request happens regardless, and timing
it is an `Instant`.

It is also the better signal. A device answers ICMP from its network stack; what
Dom needs is its API, and those can differ — a router under load, one refusing
credentials, or one whose `www` service is off will all ping perfectly while being
useless to Dom. The old check would have called that OK.

`Slow` is now 750 ms rather than 50 ms, because a REST round-trip is a heavier
thing than an echo reply: it measures "the device is labouring", not link latency.

`PingHistory`, `PingResult`, `PingConfig`, `App::get_ping_config` and
`devices::ping_device_multi` all went with the task. The four-hourly ICMP sweep
stays — it is how devices are *discovered*, which is a different job, and its
round-trip times still fill the latency column in the Devices view.

### Decision: thirty minutes for the MikroTik REST poll, and only for that

`mikrotik::DEFAULT_POLL_SECS` is 1800, down from a request every ten seconds.
Everything it fetches justifies it: DHCP leases, which discovery reads out of
`App` whenever it happens to run rather than requesting itself; firewall rules,
which change when a person changes them; and cumulative byte counters, where a
longer interval costs resolution rather than accuracy.

**Nothing else moved.** `DISCOVERY_INTERVAL_SECS` was briefly changed alongside it
and put back: discovery is how *every* device is found, energy hardware included,
and a battery or wallbox that appears or moves address only starts being polled
once discovery notices. It is not chatter on behalf of a view.

The measurement intervals are untouched and should stay that way. The battery and
switches are polled every two seconds and the wallbox every five because they
report a *rate* that has to be integrated — a missed sample is energy that cannot
be recovered afterwards. Infrastructure reports *state*, which can be sampled
whenever. That distinction, not the traffic volume, is what sets these numbers.

The one consequence is the Internet-traffic chart. Its bars were two minutes wide
and each poll's byte delta was divided by 120 seconds, so a longer poll interval
would have left most bars empty and overstated the rate in the rest. The bar width
is now a constant tied to the poll interval, with a test pinning them together —
half-hour bars, 48 across a day. The counters are cumulative, so what is lost is
the shape of short bursts, not the totals.

### What this leaves

Startup, five minutes later, and every four hours: one ICMP sweep. Every five
minutes: discovery. Every thirty minutes: one REST poll per MikroTik device. The
device poll loops keep their own intervals, unchanged.

## Talking to devices over TLS, pinned (2026-08-23)

### Problem

MikroTik devices were administered over plain HTTP with `Authorization: Basic`, so the
router administrator password crossed the LAN readable by anything that could see it, at
least 50,000 times a day across six devices — on a LAN that carries WiFi and IoT hardware.

### Decision: trust on first use, not a certificate authority

Verifying against the webpki root store, as `src/online/` does, cannot work here. RouterOS
generates its own self-signed certificate, so there is no chain to a public root, and the
device is reached by IP rather than by a name a certificate could attest to.

Dom therefore records the SHA-256 of the certificate presented on the first connection and
requires the same one thereafter. This is deliberately weaker than a public CA — a device
already being impersonated at first contact is trusted, and there is no way around that
without a secret established out of band. It is much stronger than what it replaces: after
first contact nothing on the network can read or alter the traffic without the device's
private key.

The ordering is the property that matters. A pin mismatch fails the *handshake*, so it
happens before any credential is written to the socket.

### A mismatch never falls back to cleartext

Two failures look similar and are not. "Not serving TLS" is a device that has not been set
up for it; "serving a different certificate" is either a device that changed or something
pretending to be it. Only the first falls back to HTTP. Retrying the second in cleartext
would hand the password to precisely the party the pin had just refused, which would make
the pin worse than useless.

`rest_get_tls` distinguishes them explicitly rather than treating every TLS error alike.

### A changed certificate is a question, not a decision

A certificate legitimately changes: a router reset, a regenerated key, a firmware upgrade.
Dom cannot tell that from an attack and does not try. It stops polling the device, leaves
its password unsent, and shows both fingerprints in the Network view with 'k' to accept the
new one. The poll loop re-reads the pin every tick, so accepting takes effect on the next
poll rather than at the next restart.

Only the code path a person drives may write a pin. A pin that the code which failed to
match it can rewrite is not a pin.

### HTTPS is preferred per request, not configured

Every request tries TLS on 443 first and falls back to HTTP only when nothing is listening
there. That means enabling `www-ssl` on a router is picked up with no configuration in Dom —
which matters, because RouterOS ships with it disabled, and all six devices here had it
disabled and port 443 closed when this was written. A hard switch would have stopped all
MikroTik polling.

Measured cost of the failed attempt on this LAN: 0.4–0.6 ms, against a ten-second poll
interval. Cleartext is not silent — the devices it applies to are named in the Network view,
with the RouterOS commands that fix it.

### Why `ring` is now a direct dependency

Pinning needs SHA-256. `ring` was already in the tree as `tokio-rustls`'s crypto provider,
and rustls keeps its own hash implementation private, so naming `ring` directly adds a line
to `Cargo.toml` and nothing to the build. Hand-writing the hash, as this codebase does for
base64 and HTTP, was rejected: those are encodings, and this is the thing the security
property rests on.

## Eco mode's electrical supply comes from the database (2026-08-23)

`MAINS_VOLTAGE` and `PHASES` were constants describing one installation. They are facts
about a building, not about the protocol, and a wrong value does not fail — it scales every
Eco target by the ratio it is wrong by, so a single-phase site left on the three-phase
default would be asked for three times the current it can carry.

They are now `supply_volts` and `supply_phases` in `Config`, read each Eco tick so a
correction takes effect without a restart, and defaulting to the previous values so an
existing installation is unaffected. `Config` rather than the wallbox's own row because the
supply is a property of the site, which every device on it shares.

A stored value that cannot describe a real supply — zero, negative, four phases, twelve
volts — is refused on write and ignored on read in favour of the default. Eco divides by
this on every tick, so it must never be able to yield a zero.

## Code health pass (2026-08-23)

Twelve findings from the pass recorded in REVIEW.md were fixed. Most were small; the
decisions worth keeping are below. The rest of the pass's findings stay in REVIEW.md
because they need a judgement that has not been made yet.

### Decision: diagnostics go to a file, and consequences go into `App`

There was no logger at all. `env_logger` was a declared dependency, nothing ever called
`init()`, and the `log` crate discards every record when no logger is registered — so all
seventeen call sites were no-ops.

Installing one is not quite enough, because the obvious destination is not available: the
TUI owns the terminal for the process's whole life, so stderr would scroll behind the
alternate screen or draw through the interface. Everything therefore goes to `dom.log`
beside the database, rotated once at startup past 5 MB so a restart preserves the previous
run's tail — which is exactly what is wanted when the restart is what is being
investigated.

The second half matters more. A message in a file is one nobody will see, so anything a
person has to *act* on belongs in `App` too, where a view can show it. The rollup failure
is the case that forced this: `statistics_task` skips pruning whenever a rollup pass fails,
and the database then resumes growing by about a gigabyte a month. That is now
`App::rollup_error`, shown across the Statistics view's header, alongside the existing
`last_scan_error`, `last_weather_error` and `solar.last_error`.

### The device merge, and why the fix is a list plus semantics

`migrate_device_ip` moved the losing row's history across four tables before deleting it.
Eight tables reference `Devices(id)`, and `sqlx` enables `PRAGMA foreign_keys` on every
connection, so the four rollup tiers added later were not merely orphaned — the delete
failed outright with `(code: 787)`, the transaction rolled back, and the duplicate row
survived to be retried every five minutes. Reproduced against the real schema before the
fix, and the loser accumulates rollup rows within one rollup interval of its first poll,
so this was reachable within fifteen minutes of any IP change.

Extending the list was not sufficient, because all four tiers carry `device_id` inside a
composite primary key and both sides can hold the same day — the transition day especially.
Each tier stores enough to combine exactly, and the choice is recorded in
`merge_device_history`: energy sums, `peak_w` takes the larger of the two since a peak
cannot be recovered from a sum, and the two state summaries take min and max with a
sample-weighted mean. Nothing is lost and nothing is invented; the result is what a single
device reporting both streams would have recorded.

A test asserts that every table the *schema* says references `Devices(id)` has been handled,
so this list cannot go stale a third time without a failure.

### Decision: restrict the database, do not encrypt it

`Devices` holds `api_key`, `username` and `password` in plain text, and the file was created
under the ambient umask — mode 644, so every local account could read the router
administrator password. It is now `0600`, applied on every start rather than only on
creation, so databases that already exist are corrected rather than left as found.

Encryption at rest was considered and rejected. The key would have to live somewhere Dom can
reach unattended on the same machine, which on a single-user home server moves the secret
rather than protecting it — the threat it would actually address is an attacker with the
disk but not the running system, which is not the situation Dom is in. Recorded as a
decision rather than left as an omission. The credentials still cross the LAN in the clear
on every MikroTik poll, which is a separate and larger problem; see REVIEW.md.

### Timers fire on a window, not on a minute

`timer_job` matched a timer when its `HH:MM` equalled the current minute, which required a
tick to land inside that minute. A suspended machine or a slow round of unreachable switches
meant the minute passed unvisited and the firing was lost for the day. It now fires on the
minutes that elapsed since the previous sweep.

Two bounds fall out of that. The sweep starts at "now" on startup, so Dom coming up in the
evening does not run the morning's schedule; and a gap longer than
`MAX_TIMER_CATCHUP_MINUTES` is not replayed, because putting every switch through hours of
missed schedule at once, out of order, is worse than having missed it — the point of a timer
is *when* it happens.

Separately, a firing was recorded as done whether or not the switch accepted it. It is now
recorded only on success and retried until the day rolls over, and `set_relay` actually
checks the reply: it previously returned `Ok(())` as long as the TCP connect and write
succeeded, so a device answering `500`, or not answering at all, reported success.

### Smaller decisions

- The check for "has this device moved to another address" fired on exactly one tick, and
  folded a database error into "has not moved" — so one hiccup lost the handoff until
  restart. It now runs on the tick a device goes Lost and every thirty failures after, and
  the counter was widened from `u8` so it cannot saturate and freeze the schedule.
- `ip_sort_key` mapped every IPv6 address to `u32::MAX`, so they compared equal and their
  order came from `HashSet` iteration, which is not stable between runs.
- The address input was bounded and the rename input was not. Both now share one bound,
  counted in characters rather than bytes so a non-ASCII name is not cut short early.
- The redirect loop guard compared port and path but not the address, so a redirect to the
  same resource on a different host read as a cycle. `HttpProbe` now records the address it
  went to.
- `Stats::back` had no floor when `oldest_day` was unknown, so Left kept counting while the
  view stopped moving and Right took as many presses to undo.
- `ctrlc`, `dirs` and `insta` were declared and never used; removing them took 167 lines out
  of `Cargo.lock`. `tempfile` was also unused, but is now used by the tests that exercise
  file creation and permissions — which is what it was declared for.

## Solar production forecast (2026-08-22)

### Problem

The Environment view could say what the temperature is, and the Statistics view what was produced,
but nothing could say what *will* be produced. "How much will we make tomorrow" is the question that
turns a record into a plan — whether to run the dishwasher tonight or at noon, whether the battery
should be full by morning.

### Decision: fit one parameter against the house's own production

The model is a single free parameter:

    Wh per 15 min = k · GTI · (1 + γ·(T_cell − 25)),  T_cell ≈ T_air + 0.03·GTI

`GTI` is plane-of-array irradiance, `γ` the power temperature coefficient of crystalline silicon
(−0.004/°C), and `k` everything else at once: panel area, module efficiency, inverter losses,
soiling, shading, wiring.

The alternative was to ask the user for panel area, efficiency, tilt and azimuth and compute forward
from a datasheet. Rejected: nobody knows their roof to ten degrees, a datasheet says nothing about
the tree at the end of the garden, and every one of those numbers drifts. Fitting `k` against
recorded production absorbs all of it into one figure that is re-measured continuously.

Measured on 44 days of this installation's own data: the fit recovered a 9,185 W array against an
observed peak of 9,547 W, a figure it was never shown. Held-out days — fitted on alternate days,
scored on the rest — came out at 8.6% mean absolute error on the daily total, 16 of 22 days within
10%.

### The model is deliberately incomplete

**Cloud cover is not a regressor.** Plane-of-array irradiance already encodes cloud attenuation;
regressing on both fits the same physics twice and yields a confident, wrong coefficient. Cloud
cover is fetched and displayed, because it is what a person reads off a forecast, and it is not in
the fit.

**No clipping term.** Checked against the real series: output per unit of irradiance is flat across
the whole GTI range with no ceiling, so `min(…, P_max)` would add a parameter that fits noise. This
is a property of this installation and should be revisited if the array outgrows the inverter.

**No recent-days correction.** Scaling tomorrow's forecast by how the last few days turned out was
tested and is actively harmful: it removes the bias (−3.06 → −0.85 kWh) while raising the error, and
the shorter the window the worse it gets — 3 days took 8.5% to 9.5%. Day-to-day forecast error is
driven by cloud *timing*, which does not persist, so chasing it injects noise. The long-window
calibration is the local adjustment; it just must not react to last Tuesday.

### Planes are chosen by daily error, not by per-step error

The orientation search fits `k` per 15-minute step — the right estimator for a scale — but chooses
between candidate planes on the RMS error of *daily totals*. The two disagree, measurably. Over the
same 44 days, picking the plane with the lowest per-step error gave 10.2% held-out daily error;
picking the lowest daily error gave 8.6%, against 8.0% for the best plane in the grid. Per-step
error is dominated by exactly when a cloud arrives, which is noise at the scale anyone asks the
question.

The search is coarse (5 tilts × 5 azimuths) followed by a refinement pass on the eight neighbours of
the winner, because the coarse grid brackets the optimum rather than landing on it. The optimum is
broad — several nearby planes score within a few tenths of a percent — so what comes out is an
*effective* plane that absorbs shading and any array split across roof faces, not a survey of the
roof. The refinement is only accepted if it actually explains the days better.

### Decision: Open-Meteo, reversing the earlier rejection

The outdoor-temperature work above chose MeteoSwiss open data over Open-Meteo and recorded the
reason. That reasoning is not repeated here, because the data is different.

MeteoSwiss publishes point forecasts as Open Government Data, and it was the obvious first choice —
same host as the temperature feed, same parameter codes (`tre200h0` beside the `tre200s0` already
consumed). It was measured and rejected: the CSVs are one file per parameter covering all ~6000
Swiss locations for nine days, and the hourly 2 m temperature file alone is **32,523,436 bytes**.
That is 32.5 MB an hour to extract ~216 numbers for one house, about 23 GB a month, and rows are
ordered by time rather than by location so there is no early exit. The equivalent Open-Meteo request
is about 7 KB. The nationwide-file trade that was cheap for temperature (170 KB) is not cheap here.

Open-Meteo runs MeteoSwiss's own ICON-CH1 and ICON-CH2 models, so this is the same federal forecast
delivered in a shape a house can afford. No model is pinned: ICON-CH1 reaches only 33 hours, and
pinning it would truncate exactly the day being asked about.

**The licence conflict is real and satisfiable.** Open-Meteo's free tier is non-commercial, and it
names "personal home automation" as a permitted use — which is what Dom running in a house is. The
ACSL permits a worker-owned co-operative to sell Dom, and such a user would need their own API key or
to self-host the open-source server. That is a constraint on a licensee, not a blocker. Dom's own
usage is roughly 48 forecast calls a day plus one calibration, against a 10,000/day limit.

The README's "Online services" section still describes only the two LAN-external calls that existed
before this and states that Dom "talks only to your own network, with two optional exceptions". That
is now wrong on both the count and the licence terms, and it is the document a licensee would read
before hitting the non-commercial limit. Left alone deliberately — AGENTS.md forbids editing README
files unasked — and flagged for the next person to touch it.

Data is CC BY 4.0. The credit is displayed in the Environment view, not just documented.

### Fifteen minutes, and why ten is not available

Irradiance at 15 minutes is native model output, carrying sub-hourly structure no interpolation
would produce. Temperature at the same step is interpolated from hourly and is taken anyway, because
it arrives in the same document and the model consumes both together.

Ten-minute forecasts do not exist anywhere. Weather models emit hourly steps; anything finer is
nowcasting, which stops six hours out and so cannot speak about tomorrow. The 10-minute figure
elsewhere in Dom is the *observation* cadence, which is a different thing.

### Past forecasts are never rewritten

`SolarForecast` rows are only ever written for steps that have not happened yet. Once a step's time
has passed, its row stands as what was expected, and a later fetch leaves it alone. Without this the
forecast-versus-actual comparison would be meaningless — every past forecast would be retroactively
corrected to what the model now believes happened.

The rule is a `WHERE` clause in the upsert rather than a caller convention, so it holds even if a
caller forgets.

Calibration deliberately does *not* read these rows. It fetches the archive, which is the best
estimate of the irradiance that actually occurred — the aim is to measure the array, not the
forecast. Keeping the two apart is why the view can report that the model is worth ~6.7% and the
whole chain ~8.5%, and that the difference is the atmosphere.

### Screening: whole days, then individual steps

Two screens, and the finer one does most of the work.

A day qualifies only if its per-minute rows cover at least 98% of it. A 15-minute step is used only
if it carries at least 850 of its 900 seconds. A half-recorded step reports half the production the
weather predicts, and fitting on it drags `k` down by exactly that much.

The step screen also cleans up a historical data fault at no extra cost. Before the integration gap
was clamped (below), a stalled poll loop wrote hours of energy into one row; every such row sits in a
minute with `span_secs = 2`, so the step containing it falls short of 850 and is dropped — smeared
energy and all. This needs no guess at a plausible power for an array that has not been measured
yet, which a magnitude threshold would have. Verified against the live database: of 4,224 candidate
steps, 120 are dropped and no surviving step carries physically impossible energy.

Note that dropping the *step* is right and dropping the *day* is wrong: the rest of the day is
sound, and there is not enough history to discard days casually.

### The error band is measured, not asserted

The view shows a ± figure only once at least seven finished days have both a forecast made before
the day happened and a recorded total. Before that there is no band, because there is no evidence
for one. A calibration fitted on fewer than fourteen days is labelled as rough rather than presented
as a result.

### Related fix: unbounded integration in the Sonnen poll loop

`save_energy` computed `dt` as the wall-clock gap between consecutive readings with no upper bound,
then integrated across it. When the loop stalled — restart, network drop, battery unreachable — the
trapezoid multiplied current power by the whole gap into a single row labelled `resolution = '2s'`.
The worst observed case attributed 10.8 kWh to one 2-second row, which flowed into `EnergyDaily` and
inflated that day from 47.25 to 58.19 kWh: 23% high, and entirely plausible-looking beside its
neighbours at 56.17 and 57.65. Across 53 days, 38 such rows carried ~19.9 kWh of energy that was
never produced.

Intervals longer than ten poll periods are now refused. A gap is missing data: recording nothing is
correct and leaves a hole `EnergyMinute.span_secs` makes visible, whereas interpolating across it
invents energy. The existing rows cannot be repaired — the trapezoid smeared real production into
them alongside the phantom — so they are screened out rather than corrected.

## Online services: geocoding and outdoor temperature (2026-08-18)

### First outbound dependency

Everything Dom talked to before this was on the LAN. Outdoor temperature cannot be, so this adds the
first two calls that leave the house. Both are optional and neither is made until a location is set:
with no address configured the weather task returns immediately.

### Decision: MeteoSwiss open data over Open-Meteo

Both were tested against the live services. Open-Meteo serves MeteoSwiss's own ICON-CH1 model
interpolated to exact coordinates and elevation, in a 450-byte response — arguably closer to the
temperature at a specific house than any station reading. It was rejected because its free tier is
explicitly non-commercial only ("You may only use the free API services for non-commercial
purposes"), while Dom's own licence permits commercial use: shipping it as the default would embed a
term some of Dom's own licensees could not satisfy.

(That reasoning was first written while Dom was under the Prosperity licence, which sold commercial
exemptions. It survives the move to the Anti-Capitalist Software License unchanged in substance:
that licence lets a worker-owned co-operative use and sell Dom commercially, so the conflict with a
non-commercial-only API applies to those users instead.)

**This decision was later revisited, and went the other way for forecasts** — see "Solar production
forecast" above. It stands for outdoor temperature: the nationwide MeteoSwiss file is 170 KB, which
is a cheap price for avoiding the licence constraint entirely. The equivalent forecast file is 32.5
MB an hour, which is not.

MeteoSwiss open data is Open Government Data — free of charge, reusable including commercially, with
attribution. It is also a real measurement rather than a model output. The costs are that the nearest
station may be far away and at a very different altitude, and that the published file covers the whole
country (~170 KB) to obtain one number.

That distance and altitude are surfaced in the UI rather than hidden. A reading from 20 km away and
900 m higher is not the temperature outside the house, and presenting a bare number would imply
otherwise.

An incidental privacy benefit: because the file is nationwide and the nearest station is chosen
locally, the location is never sent to MeteoSwiss. Only the geocoder ever sees the address.

### Decision: a TLS transport, not an HTTP client

Both services are HTTPS-only and the project had no TLS at all. Measured as crates actually added to
this dependency graph: `reqwest` + rustls was +59, `ureq` +17, and `tokio-rustls` + `webpki-roots` +8
as it turned out (more overlap with the existing tokio and ring than an isolated measurement showed).

Chose the transport and hand-wrote the HTTP, which is what the codebase already does for LAN devices
(`fingerprint::http_get`, `devices::*::fetch_report`). `ring` rather than the default `aws-lc-rs`
provider so the build needs no cmake or nasm.

### HTTP/1.0 on purpose

The requests ask for HTTP/1.0. Over 1.1, swisstopo answers with `Transfer-Encoding: chunked`, which
would mean implementing de-chunking; over 1.0 it sends the body and closes, so read-to-EOF is correct.
Verified against both endpoints. This is noted in the module so it is not "modernised" to 1.1 without
chunked decoding being added first.

### LV95 throughout, and the transposition trap

MeteoSwiss publishes station positions in LV95 (EPSG:2056), so the geocoder is asked for LV95 too
(`sr=2056`). LV95 is metric, so nearest-station is Pythagoras in metres — no great-circle arithmetic,
no reprojection.

The trap: swisstopo reports LV95 as `(x = northing, y = easting)`, the reverse of the
`[easting, northing]` ordering in the MeteoSwiss document. Transposing them still parses, still yields
coordinates inside Switzerland, and still returns a confident answer — just the wrong station. Two
tests pin it: the nearest station to Bern's coordinates must be Bern, and to Lugano's must be Lugano.
A range assertion also guards it, since a Swiss easting (~2.5-2.8M) and northing (~1.07-1.3M) cannot
be confused numerically.

### Storage

Readings go to `OutdoorTemperature`, keyed by the station's own measurement instant so re-fetching an
unrefreshed reading is ignored — the publication cadence and the poll interval are both ten minutes
and will not stay in step. At 144 rows a day the series is kept indefinitely at full resolution: a
rollup would save a few megabytes a decade and cost the accuracy of daily extremes.

## Environment view, and incremental temperature rollup (2026-08-18)

### Which of the three unbuilt views could be built

README listed Security, Environment and Household as intended categories. Only Environment had a
data source: myStrom switches report a temperature every two seconds, and it was already being
recorded. Security and Household have no supported hardware at all, so building them would have
produced two empty screens behind two keybindings, which is worse than not offering them. They stay
out of the `View` enum until a device that feeds them exists.

What the switches report is their own case temperature, not an ambient reading — it tracks the
appliance plugged in more than the room. The view labels it as device temperature rather than
presenting it as something it isn't.

### Decision: accumulate the daily summary, don't recompute it

Every other rollup recomputes a day from a raw series retained longer than the re-roll window.
Temperature cannot work that way: its source is `RawDeviceMeasurements`, pruned after 24 hours, which
is *shorter* than the two-day re-roll window. Recomputing yesterday at 23:00 would see only the last
hour of it still inside that window and would overwrite a complete day with a sliver.

So `TemperatureDaily` is folded into incrementally. It stores `temp_sum` and `samples` rather than an
average, so merging is exact, and a `last_ts` watermark so only samples newer than the previous pass
are added. Min and max merge as `MIN(existing, new)` / `MAX(existing, new)`.

The whole fold is a single `INSERT ... SELECT ... ON CONFLICT DO UPDATE`, so reading the watermark
and advancing it cannot interleave with another pass. Re-running with nothing new is a no-op, which is
what makes it safe to call every few minutes.

The consequence worth stating: temperature history begins the day this shipped. Unlike the energy
tiers, which were backfilled from 47 days of retained 2s samples, there is nothing to backfill from —
those days' raw temperature readings were already gone.

### Presentation

One row per day drawing the min-to-max span as a bar on a shared scale, with the mean marked inside
it, rather than a line chart of daily averages. A day's spread is as informative as its middle for a
temperature, and the horizontal drift of the bars shows a warming or cooling run without reading any
numbers.

## Tiered measurement retention (2026-08-17)

### Problem

The 2-second series was kept forever. `Energy` alone was 12.7M rows over 47 days, and with
`EnergyStorage` and their indexes it accounted for essentially the entire 1.7 GB database, growing
about a gigabyte a month. The single largest object was `idx_energy_metric_res_time` at 914 MB
against a 555 MB table, because it carries TEXT `metric` and `resolution` for every row.

The decisive observation is that **nothing renders 2-second data**. Every read of `Energy` is scoped
to the current local day, and the finest thing any of them produces is a per-minute bucket. Live
gauges come from in-memory state, not the database. So the series was retained at roughly thirty
times the resolution anything consumes.

### Decision: three tiers, each retained longer than the last

`2s` for 4 days, `EnergyMinute` for 90 days, `EnergyDaily` forever. `EnergyStorage` follows the same
shape, summarised into `StorageDaily`.

Downsampling is sound here in a way it would not be for a sampled series: `energy_ws` is an
*integral* over each interval, so summing thirty 2-second rows into a minute preserves the total
exactly. Confirmed on real data — aggregating a day in two stages matched one stage to seven decimal
places, and a full rollup-then-prune of 3.4M real rows left twelve closed days bit-identical.

What downsampling does lose is intra-minute shape, and with it peak draw. `peak_w` is therefore
captured per minute at rollup time, since it is the one figure a coarser tier cannot reconstruct. A
2-second energy divided by its interval is already a 2-second average power, which is the right
quantity for the questions peaks answer — fuse and grid-connection limits are thermal.

### Retention is bounded by its consumers, not by taste

4 days of raw data is not a preference. It must cover the "today" queries, which look no further back
than the current local day, and it must cover `DAYS_ALWAYS_REROLLED`, because re-rolling a day whose
source rows were partly deleted would replace a complete total with a partial one. Four days leaves
two days of margin over the two-day re-roll window.

That constraint is also why the today-queries were left reading the 2s tier. Pointing them at the
finest *available* resolution was considered and rejected: retention already guarantees the raw rows
for today exist, mixing tiers in one query would double-count minutes present at both, and the
sign-split queries (grid import/export, battery charge/discharge) are not tier-invariant unless they
read the split columns. The refactor would have added a correctness-critical invariant for a
background query that already completes in a couple of seconds. Recorded in REVIEW.md instead.

### Pruning cannot outrun rolling up

Each prune refuses, in SQL, to delete a day the next tier up has not recorded in `RollupProgress` —
raw energy needs the minute tier, raw storage needs the storage tier, minutes need the daily tier.
This is an interlock rather than a matter of call ordering: a rollup failure followed by a prune
would otherwise destroy data permanently, and the caller cannot be relied on to remember.

Deletion is batched at 20,000 rows per statement, capped per pass. The first prune has millions of
rows to clear; one statement would mean a multi-gigabyte WAL and a write lock held for minutes
against a database the poll loops write to every two seconds.

### Reclaiming the space is deliberately not automatic

SQLite's `DELETE` returns pages to the free list rather than shrinking the file, so the file does not
get smaller — briefly it gets larger. Reclaiming needs `VACUUM`, which rewrites the whole database
under an exclusive lock; on a measured 13-day sample, 338 MB became 108 MB in about 7 seconds. That
is an operator action with Dom stopped, not something the app should do to itself while running.

## Long-term statistics: daily energy rollup (2026-08-17)

### Problem

The monthly/yearly statistics view cannot be served from the `Energy` table. That table holds
one row per metric per 2 seconds and nothing prunes it: 47 days of recording is 12.7 million rows and
1.7 GB, and a bare `COUNT(*)` over it measured at 16 seconds. A month is millions of rows and a year
would be a hundred million — far past what a view can aggregate while someone waits.

### Decision: pre-aggregate by local day

A `EnergyDaily` table holds one row per (device, local day, metric). Every window the view offers is
then cheap: a week is 7 rows per metric, a month about 30, and a year is 12 monthly buckets folded
from ~365 daily rows.

Daily is the right granularity because it is the coarsest bucket all three windows can be built from.
Storing 10-minute or hourly rollups instead — the `resolution` column in `Energy` anticipates them,
though nothing has ever written one — would mean 50,000+ rows per metric per year for no benefit at
these window sizes.

The rollup keeps `device_id` even though the statistics view sums over devices. That keeps the table a
faithful aggregation of its source, and leaves a per-device breakdown available without a migration.

### Import and export are split at rollup time

`Energy`'s `grid` series is signed: negative is drawn from the grid, positive is fed back. A daily sum
of the signed values gives only the net, which cannot be separated afterwards. So the rollup writes
`grid_import` and `grid_export` as distinct derived metrics, using the same sign split that
`query_today_energy` already used for its daily figures. `EnergyDaily.metric` is therefore *not* a
copy of `Energy.metric`.

The per-device `power` metric is excluded from the house totals, as it already is in
`query_today_energy`: a switch's or wallbox's own draw is part of the battery's whole-house
`consumption` figure, so adding it would double-count.

### Local days, expressed as UTC ranges

`Energy.timestamp` is naive UTC, so a local calendar day has to become a UTC half-open range before it
can be compared against an indexed column. `date(timestamp, 'localtime')` is correct but not
indexable, and without the index a single day's aggregation reads every row that device ever wrote.

The upper bound is the *next local midnight* rather than the start plus 24 hours, which is what makes
a 23- or 25-hour DST day come out right. Verified against the live database: the explicit-bounds query
and the `date(..., 'localtime')` query return byte-identical totals over the same day (41,628 rows,
matching to five decimal places).

### One device at a time

The aggregation is issued per device, not once across all of them, because that is the shape
`idx_energy_metric_res_time` can serve — its leading column is `device_id`, so with a bounded
timestamp range the query is an index range scan. Measured at ~0.16s per device-day against 12.7M
rows, against ~1.6s for the same range without the device prefix.

### Re-rolling, and not trusting a row's existence

A day that already has rows is skipped, except for the two most recent days with data, which are
always re-rolled. Re-rolling only the current day would leave a permanent gap: with the app not
running at the moment a day ends — closed at 23:50, opened at 00:10 — that day's final minutes would
never be aggregated, and because rows already existed it would look complete forever. Two days closes
that without having to persist a watermark.

Rollup is idempotent (`ON CONFLICT ... DO UPDATE`), so re-rolling replaces rather than accumulates.

A day with no samples writes nothing at all rather than writing zeros, so a device that was offline
stays distinguishable from one that genuinely used nothing. The view carries that distinction through
to the screen: a bucket with no data is labelled as such instead of drawn as a zero bar, and the
self-sufficiency ratios read as a dash rather than 0% when there is no denominator — 0% would claim
everything was bought from the grid.

### Backfill is throttled

The first run after this shipped has the entire recorded history to aggregate, against a database the
device poll loops are reading at the same time. The rollup sleeps 200ms between days so that work
spreads out instead of competing for disk. Steady state is two device-days per pass, every 15 minutes.

### Calendar periods, not rolling windows

Months are the 1st to the last and years January to December — so browsing back with Left lands on
periods a person recognizes rather than arbitrary 30-day slices. The current period is clamped to end
at today, so a partial month reports what has actually happened rather than implying a full one.

A weekly window existed initially and was removed (2026-08-18): with a month already shown as
individual days, a week was the same bars over a shorter span rather than a different view of the
data, and it cost a third entry key.

Browsing back stops once a period would end before the oldest day that has any data, rather than
walking indefinitely through empty periods.

## Colour theming (2026-08-17)

### Problem
Every colour the UI drew was a literal in `render.rs` (30 of them) or in `app.rs` (5 more, on
`NetworkDeviceStatus::color`). Two of those literals were a white foreground over a coloured
background, which AGENTS.md forbids precisely because it is unreadable on a light terminal —
but simply dropping the foreground, as that rule suggests, would have given dark-on-DarkGray
instead. The colours could not be fixed one at a time; they needed somewhere to live.

### Decision: a semantic palette, selected per terminal
`src/tui/theme.rs` holds a `Theme` of about two dozen colours named for their *role* —
`consumption`, `focus_border`, `gauge_track`, `status_lost` — not for a hue, so a light
palette can pick a different colour for the same role. `Theme` is `Copy`, and `App` stores
only the `ThemeMode`; `App::theme()` builds the palette on demand, keeping one source of
truth rather than a mode and a palette that could disagree.

The dark palette is value-for-value what the app drew before, so enabling theming changed
nothing for anyone already on a dark terminal.

The light palette uses 256-colour indices rather than the base ANSI names. The colours that
actually break on white have no dark variant in the 16-colour set: plain yellow and cyan are
near-invisible, and a white foreground disappears entirely. 256-colour support is effectively
universal on terminals that run this app, and a 16-colour terminal degrades each index to the
nearest base colour, which is still readable.

`NetworkDeviceStatus::color` moved to `Theme::status_color` so that `app` does not depend on
`tui`. The status→colour mapping was only ever used by the renderer.

AGENTS.md prefers the Stylize helpers (`.cyan()`, `.dim()`) over a hand-built `Style`, but
each of those hardcodes one ANSI colour and cannot be themed. A palette lookup is a
runtime-computed style, which AGENTS.md explicitly permits via `Span::styled` — that is the
form `render.rs` now uses throughout, and it should not be "simplified" back to the fixed
helpers.

### Detection: COLORFGBG, and an honest fallback
Auto-detection reads the terminal's `COLORFGBG` (`"<fg>;<bg>"` as ANSI indices; the background
is the last field, since rxvt writes three). Anything unparseable — including the literal
`"default"` — yields no answer rather than a guess.

An OSC 11 background-colour query would be more reliable and was rejected: it needs a raw-mode
write-then-read against the tty during startup and hangs on terminals that never answer.

`COLORFGBG` is unset under tmux and most modern terminals, so the no-answer path is the common
one, not an edge case; it falls back to the dark palette. `detect_mode` takes the variable's
value as an argument rather than reading the environment itself, which is what makes it
testable.

### Selection persists, and overrides detection
't' toggles the palette and records the choice in the new `Config` table, a plain key/value
table so that adding a setting needs no migration. A stored choice wins over detection on every
subsequent start — detection only decides for a user who has never chosen. Absence of the key
is what "never chosen" means, so `get_config` returning `None` is an ordinary result and not an
error.
