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
purposes"), and Dom is sold under paid commercial licences: shipping it as the default would embed a
term our own commercial licensees could not satisfy.

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
