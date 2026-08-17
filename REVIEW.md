# Pending

Findings from the repository-wide code health pass (2026-08-17). Items here were
deliberately *not* changed during that pass, either because fixing them alters
behaviour (rather than just structure) or because the fix is a judgement call
that belongs to a human.

## README.md describes an architecture the code does not have

`README.md`'s "Code structure" section specifies a layout that largely does not
exist. AGENTS.md forbids editing README.md without an explicit request, so this
is recorded rather than fixed. Either the README is a roadmap and should say so,
or it has drifted and should be corrected.

Specified but absent: `src/model/` (the entire World Model layer, including the
`home.energy.usage.total.estimated` access pattern the README documents at
length), `src/headless.rs`, `src/path.rs`, `src/actions.rs`, `src/sensors.rs`,
`src/devices/{discovery,protocol,manager}.rs`, `src/devices/device/`,
`src/tui/{event,theme}.rs`, `src/tui/components/`, `src/tui/screens/`, `src/db/`
(the code has a flat `src/db.rs`), `src/test/harness.rs`, `benches/`, and the
per-directory `SPECS.md` files under `model/`, `devices/`, `tui/` and `db/`.

Three feature claims are also untrue of the current code:

*  **Time series storage.** The README says "tsink is used to store time series
   data". `tsink` is not a dependency and appears nowhere in `Cargo.toml` or
   `Cargo.lock`; time series live in the SQLite `RawDeviceMeasurements`,
   `Energy` and `EnergyStorage` tables.
*  **Headless mode.** The README documents `dom --headless` / `dom -h` with a
   description of its behaviour. There is no argument parsing anywhere in the
   binary; the TUI always runs.
*  **Theme support.** The README says a dark/light theme can be toggled and
   auto-detected. There is no theme module, no toggle, and no light/dark
   handling — only individual hardcoded `Color` values.

## README.md's keyboard shortcuts do not match the code

The table in `README.md` disagrees with the event loop in `src/tui/mod.rs`, and
also with the README's own prose (line 18 says `c` toggles the theme, line 46
says `c` opens a configuration panel, and the table says `c` opens the
Communication view).

What the code actually binds: `q` / `Ctrl-C` quit, `c` Current view, `e` Energy
view, `n` Network view, `d` Devices view, `s` trigger a rescan, `r` rename the
selected device, `Esc` close the open dialog, `Up`/`Down` navigate.

Documented but not bound at all: `t`, `g`, `a`, `v`, `h`, `m`. Bound but not
documented: `n`, `d`, `r`. Documented with a different meaning than the code
gives them: `c` and `s`.

The README also describes Security, Environment and Household views; the `View`
enum has only `Current`, `Energy`, `Network` and `Devices`.

## Device type detection is expressed twice, with different semantics

`main.rs::discovery_task` decides a device's type twice per discovery cycle, in
two shapes that disagree:

*  When persisting devices, each detector is a separate `if`. A fingerprint
   matching two detectors is therefore saved as two device types, and starts two
   poll loops.
*  When naming the same device for the UI a few lines later, the detectors are
   chained with `else if`, so only the first match wins.

The two also mean `detect` runs twice per device type per fingerprint. A single
`detect_type(&Fingerprint) -> Option<DeviceType>` used by both sites would make
the precedence explicit and unify the semantics. Not done here because
collapsing the save path to first-match-wins is a behaviour change: it would
stop saving a device that currently matches two detectors as both, and whether
any real device does so cannot be established from the code alone.

## `dom_local::save_device` bypasses the fingerprint/IP-migration mechanism

Every other device type persists through `db::upsert_device`, which implements
the MAC-fingerprint identity and in-place IP migration described in SPECS.md.
`devices/dom_local.rs::save_device` instead issues raw SQL: an
`INSERT OR IGNORE`, then `UPDATE Devices SET name = ?, type = ? WHERE ip = ?`.

Two consequences. It stores no fingerprint, so the local machine never
participates in IP migration. More seriously, the unconditional `UPDATE ...
WHERE ip = ?` rewrites *whatever* row currently sits at that IP — if a local
interface address ever coincides with a row previously discovered as another
device type, that row's `type` and `name` are silently overwritten to
`dom_local`.

## `App::forget_device` does not clear every per-IP map

`forget_device` (added by this pass) clears exactly the maps the four poll loops
cleared before it existed: `conn_status`, `last_error`, `polled_ips` and the
per-type readings maps. Other `HashMap<IpAddr, _>` fields on `App` keep their
entries for an IP that has been migrated away: `ping_history`, `ping_configs`,
`device_energy_today`, `switch_auto_modes`, `switch_timers` and
`last_network_status`.

Clearing those too is probably correct, but it is a behaviour change rather than
a refactor, so the pass preserved the existing set.

## HTTP probing follows only 301 redirects

`fingerprint::probe_http` treats a response as a redirect only when
`status_code(&raw) == Some(301)`. Devices that answer with 302 (very common for
a login redirect), 307 or 308 are recorded as-is and never followed, so any
identifying markup behind such a redirect is missed and the device may fail
detection.

## Hardcoded white foreground, against the AGENTS.md TUI convention

AGENTS.md says "Avoid hardcoded white: do not use `.white()`; prefer the default
foreground (no color)." Two sites in `src/tui/render.rs` set
`.fg(Color::White)`: the status bar (line ~389, on a `DarkGray` background) and
`highlighted_line` (line ~553, on a `Blue` background).

Left alone deliberately: both pair white with a coloured background, so dropping
the foreground would fall back to the terminal default, which on a light-themed
terminal means dark-on-DarkGray and dark-on-Blue. Fixing this properly means
choosing a readable pair for both terminal themes, which needs a human looking
at the result — it is not a mechanical substitution.

## `src/tui/mod.rs` has no test module

README.md asks every file in `src/` to end with its own test module.
`src/tui/mod.rs` (the 535-line terminal event loop) has none, and it is the one
remaining file where that is a real gap rather than a formality — `src/lib.rs`
is only `pub mod` declarations. Testing it needs a way to feed synthetic
`crossterm` events to the loop, which is closer to a harness (README specifies a
`src/test/harness.rs` that does not exist) than to a unit test.

## `dom_local::detect` is a permanent `false`

`devices/dom_local.rs::detect` ignores its argument and always returns `false`;
the real check is `is_local_ip`, called separately. It exists only so the module
matches the shape of the other device modules. Harmless, but it means the
uniform "every device module has a `detect`" assumption does not hold, which is
worth knowing if the detectors are ever unified as suggested above.
