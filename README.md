# Dom

Smarthome app.

Robust. Efficient. Old school.

# Usage

A Terminal User Interface is available. Simply run the app.

You can exit the app at any time by hitting 'q', or Ctrl-C.

Escape closes whichever dialog is open — the add-timer popup, or a rename in progress — and cancels
it. It does not exit the app.

## Theme

You can switch between the dark and light theme with 't'. Your choice is remembered, and from then
on it takes precedence over auto-detection on every start.

Until you pick one, the app tries to auto-detect the theme from the terminal's `COLORFGBG`
variable. For many terminal and multiplexer combinations — tmux among them — there isn't enough
information available to make the correct choice, and the dark theme is used.

## Views

There are four views:

- **Current** ('c'): the overview — energy gauges plus the state of your network infrastructure.
- **Energy** ('e'): consumption, production, grid and battery in detail.
- **Network** ('n'): routers, modems and access points, their health, and Internet traffic.
- **Devices** ('d'): every device discovered on the network, configured or not, with a detail panel.

Security, Environment and Household are intended categories that are not implemented yet — no
sensors of those kinds are supported, and there are no views for them.

### Keyboard shortcuts

| Key | Action |
|-----|--------|
| q, Ctrl-C | Quit the app |
| Esc | Close the open dialog |
| t | Toggle theme (dark/light) |
| c | Current view (the overview) |
| e | Energy view |
| n | Network view |
| d | Devices view |
| s | Rescan now (wakes the ping scan and discovery immediately) |
| r | Rename the selected device |
| Tab | Move between the device list and the detail panel |
| Up, Down | Move within the focused list |

While a dialog is open — the add-timer popup, or a rename in progress — keys go to the dialog, so
the shortcuts above are inactive until you close it with Escape.

## Energy view

Four sections, top to bottom:

1. Summary gauges: consumption, production, grid (marked importing or exporting) and battery
   (charging or discharging), plus the battery's stored energy. The battery gauge changes colour by
   fill level.
2. Per-device power: the current draw of each device reporting one, selectable with Up/Down.
3. Per-device energy for today, in kWh.
4. Line charts of today's consumption, production, grid and battery, in kW.

There is no manual sensor-configuration popup. Devices are found by scanning — press 's' to scan
immediately instead of waiting for the next cycle.

## Network view

The health of your network infrastructure: routers first, then 5G modems, then access points. Roles
are assigned from device labels where they say "router", "modem"/"5g" or "ap", and any remaining
MikroTik devices fill whichever roles are still empty.

Each device shows a status derived from its last five pings:

| Status | Meaning |
|--------|---------|
| OK | All five pings answered, all under 50 ms |
| SLOW | All answered, but at least one took 50 ms or more |
| DEGRADED | At least one ping was lost |
| LOST | No successful pings |
| UNKNOWN | Ping scanning itself is broken (e.g. missing `CAP_NET_RAW`), so health is genuinely unknown rather than assumed fine |

Alongside is a log of recent status changes, kept across restarts, and a chart of the modem's
Internet-facing traffic for today.

## Devices view

Every device the scanner has found, whether or not Dom knows how to poll it, with a detail panel for
the selected one. Press 'r' to give a device your own label, which takes precedence over the name
learned from DHCP.

For a myStrom switch the detail panel also allows toggling the relay, switching auto-mode between
disabled and time-based, and adding or deleting scheduled timers. For a KEBA wallbox it allows
switching the charging mode between disabled and full power — the new mode is only shown as fact once
the wallbox confirms it.

## Supported devices

- Sonnen Eco 8 battery
- KEBA wallbox
- MikroTik routers and access points
- myStrom WiFi switches
- The machine Dom itself runs on

Anything else on the network still appears in the Devices view with its open ports and ping health;
it just isn't polled.

# Installation

Use cargo install.

There are no packages currently available.

Dom takes no command-line arguments. Running it starts the TUI.

Device discovery, polling and storage all run as background tasks alongside the TUI, so simply
leaving the app running is what collects data. There is no separate headless mode.

State lives in `db.sqlite` in the working directory: discovered devices and their credentials,
measurement history, and application settings such as the chosen theme (in the `Config` table).

## Platform Support

Only Linux is supported. Support for macOS and Windows is a non-goal.

# Contact

You can contact me at [marko@ivankovic.me](marko@ivankovic.me).

# License

Copyright (C) 2026 Marko Ivankovic

Licensed under the [Prosperity Public License 3.0.0](https://prosperitylicense.com/versions/3.0.0).

- **Noncommercial use is free.** Personal use, hobby projects, study, research and
  experiment are all unrestricted. So is use by charities, educational institutions,
  public research bodies, public safety and health organizations, environmental
  protection organizations and government institutions — regardless of how they are
  funded.
- **Commercial use gets a thirty-day trial.** One trial per company, covering all
  personnel, not one trial per person. Past that, you need a license.
- **Contributions back don't count as commercial use.** Feedback, changes and additions
  contributed back under a standard permissive license (Blue Oak, Apache-2.0, MIT,
  BSD-2-Clause) are free to develop.

See the [LICENSE](LICENSE) file for the full terms.

Note that this is deliberately **not** an open source license: it does not meet the OSI
definition, and it is not free software in the FSF sense. That is the intent.

## Need a commercial license?

Commercial licensing is available, for individually negotiated compensation.

[Contact me](mailto:marko@ivankovic.me) for options.

## Previously AGPL-3.0

Up until August 2026, this project was published under AGPL-3.0 from a repository hosted
on Codeberg. That grant is irrevocable for the versions it was made under — anyone who
obtained the code under AGPL-3.0 keeps their AGPL-3.0 rights to those versions. The
Prosperity license applies to this repository and everything going forward.

# For Developers, human or otherwise

This part of the README is mostly used to tell the AI how to work in this code. Still, useful for humans too.

## Time and the real world

Across Dom, all timestamps, instants, durations and other time types **must** use milliseconds. The
only exception is in the very last moment before a value is displayed to the user. For usability,
a humanized text can be displayed. But such humanized values should never be stored or computed
with. Computations and storage **must** be done using 64bit types that represent milliseconds.

This is because milliseconds are a good middle ground between seconds, which would be too coarse for
things like light-switch operations and nanoseconds, which are far too granular for control that
goes over a network layer that has millisecond latency.

## Digital model of the real world

Dom has a digital model of your house.

The fundamental design principle for Dom is to accept that the digital model will always be a little
off of the real world. Sensors have latency, and we don't have sensors in every atom of every room.

The most important decision is to timestamp every event and use the timestamps to estimate the drift
between realiy and the model.

## Structure: shared state, background tasks, one renderer

Dom is a set of background tokio tasks and a TUI, all reading and writing one shared `App` struct
behind an `RwLock` (`Arc<RwLock<App>>`, aliased `SharedState`).

There is no separate world-model layer. `App` *is* the model: it holds the latest reading from each
device, connection status, ping history, the device list and the UI's own state. Anything that wants
to know something about the house reads it from `App`; anything that learns something writes it
there. This is deliberately flat — the app is small enough that a layer of indirection between
"what the battery just reported" and "what the screen shows" would cost more than it saves.

The tasks, all spawned from `main`:

- **Ping scan** — ICMP-sweeps the local subnets and the ARP cache. Runs at startup, again after five
  minutes to catch devices that were down at t=0, then every four hours. A failure here (typically a
  missing `CAP_NET_RAW`) is recorded for display and must never block discovery, which needs only TCP.
- **Discovery** — every five minutes, fingerprints the union of the last ping scan's results and the
  router's DHCP leases, identifies device types, persists them, and starts a poll loop for each new
  one.
- **One poll loop per device** — talks to its own device on its own interval and writes readings into
  `App` and the DB.
- **Infrastructure ping** — per-device health checks at role-dependent intervals.
- **Pruning** and **chart refresh** — hourly and every 60 seconds respectively.
- **Timer job** — fires scheduled switch timers every 30 seconds.

Both the ping scan and discovery can be woken early; that is what 's' does.

### Device modules

One module per device type under `src/devices/`, each with the same shape: a `detect` that recognizes
the device from a fingerprint, functions to talk its protocol, a `save_device`/`load_all` pair, and a
`poll_loop`. Shared behaviour that would otherwise be copied per module lives in `src/devices/mod.rs`
— network scanning, the DB timestamp format, and the poll-failure/IP-migration handling every loop
needs.

Device identity is the MAC address from the kernel ARP cache, not the IP, so a device that gets a new
DHCP lease is recognized as the same device rather than appearing twice. See SPECS.md.

### Who talks to the DB?

Everything that needs to, through `src/db.rs`. Device modules write their own measurements. This is a
departure from stricter layering, chosen because every alternative meant passing rows through `App`
for no benefit.

## Technology

The project is completely written in Rust.

SQLite is the only datastore, via `sqlx`. It holds configuration, device records and the measurement
time series alike — `RawDeviceMeasurements` for point readings, `Energy` and `EnergyStorage` for
integrated energy, `NetworkStatusEvents` for infrastructure history, and `Config` for settings. A
dedicated time-series database was considered unnecessary at this data rate.

The UI is a Terminal UI written using the excellent Ratatui and Crossterm libraries.

### UI structure

`src/tui/mod.rs` owns the event loop: it reads crossterm events, mutates `App` under the write lock,
and asks `render` to draw. `src/tui/render.rs` is a flat sequence of `render_*` functions that read
`App` and draw — they hold no state of their own.

This is not the Ratatui component architecture. Components own their state and handle their own
events; here all state lives in `App` and all event handling in one loop, which keeps every state
transition in a single readable place.

Colours come from `src/tui/theme.rs` rather than being written inline, so both the dark and light
palettes work. Fields are named for meaning (`consumption`, `focus_border`), not for a colour.

## Code quality

Code must always be formatted using the automated standard Rust formatter.

No Rust check errors are allowed. Rust check should be run frequently.

## Testing

Automated tests should be run frequently during coding.

Benchmarks should be used to measure quality. These should be run on demand.

### Automated tests

Each file in src/ should end with the test module for that file, as is typicall in Rust. These tests
should test both happy-path and corner cases.

**Tests in src/ must run in under 1 second**.

Each general user flow (e.g. adding a new directory to be watched for PDFs, removing a directory,
updating a PDF and checking that the txt file updates) should have a test in test/. These should all
be happy-path tests, they should not test errors unless the error is a general user flow.

**Tests in tests/ must run in under 5 seconds.**

### How should tests handle dependencies?

*No mocks*. Mocks prevent testing through the interface and are brittle.

Ideally, the real implementation is used.

When necessary, e.g. for filesystem or database access, fake in-memory implementations should be used.
For the database that means `db::init("sqlite://:memory:")`, which applies the real schema — the
queries under test are `sqlx::query` rather than the compile-time-checked `sqlx::query!`, so only a
real database exercises them.

`tests/common/mod.rs` holds the shared integration-test helpers: an in-memory pool, and `MockHttp`, a
raw-TCP server that answers with a canned response so device protocol code can be driven end to end
over a real socket.

## Code structure

Rust's project structure is followed.

```
<root of the repository>
    |- /src               <- The implementation
        |- main.rs        <- Entry point; spawns every background task, then the TUI
        |- lib.rs         <- Library exports (so tests/ can reach the internals)
        |- app.rs         <- The shared state every task reads and writes, and the
        |                    types it holds. This is the model.
        |- db.rs          <- Schema, migrations and every query
        |- fingerprint.rs <- Port scan + HTTP probing used to identify a device
        |- devices/       <- Talking to devices
            |- mod.rs             <- Network scanning, and the behaviour every
            |                        device module shares
            |- sonnen_batterie.rs
            |- keba.rs
            |- mikrotik.rs
            |- mystrom_switch.rs
            |- dom_local.rs       <- The machine Dom runs on
        |- tui/           <- Everything terminal
            |- mod.rs     <- The event loop
            |- render.rs  <- Drawing; flat render_* functions, no state
            |- theme.rs   <- Dark/light palettes and terminal detection
    |- /tests             <- Integration tests, one binary per file
        |- common/mod.rs  <- In-memory DB and MockHttp helpers
        |- db.rs
        |- keba.rs
        |- mikrotik.rs
        |- mystrom_switch.rs
        |- sonnen_batterie.rs
        |- dom_local_test.rs
    |- README.md          <- This file. Only very high level information goes here
    |- AGENTS.md          <- AI-only instructions
    |- SPECS.md           <- Detailed specifications and all decisions that were taken
    |- TODO.md            <- Small to mid size TODO items to be fixed in the future
    |- REVIEW.md          <- Comments about the codebase that need improving
```

Directories are added only once they hold more than a file or two; `db.rs` and `app.rs` are single
files today and stay that way until there is a reason to split them.

The SPECS.md and README.md files can exist in any subdirectory, and they always serve the same
purpose:

*  README.md - High level summary. Must be readable to humans.
*  SPECS.md - Semi-structured collection of specifications and a decision log of every decision that
   was taken during implementation. SPECS.md files must not contain any code snippets.

The TODO.md and REVIEW.md files are always only in the root of the repository.
