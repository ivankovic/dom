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
