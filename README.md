# Dom

Dom - Croatian word meaning "home".

A smarthome automation app.

This is Personal Software. I built it for me. If anyone else benefits from it, great. As such
this is open source software available under the anti-capitalist Software License v1.4.

**There is no paid licence, and no way to buy an exemption.**

If you do not meet the conditions of the licence, you have no licence to this software at all.

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

There are six views:

- **Current** ('c'): the overview — energy gauges plus the state of your network infrastructure.
- **Energy** ('e'): consumption, production, grid and battery in detail.
- **Network** ('n'): routers, modems and access points, their health, and Internet traffic.
- **Devices** ('d'): every device discovered on the network, configured or not, with a detail panel.
- **Statistics** ('m'/'y'): long-term energy totals and self-sufficiency, by month or year.
- **Environment** ('v'): outdoor temperature for your location, plus device temperatures.

Security and Household are intended categories that are not implemented yet — no alarm, camera,
smoke, vacuum or mower devices are supported, so there is nothing for those views to show and they
do not exist.

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
| m | Statistics, monthly |
| y | Statistics, yearly |
| v | Environment view |
| a | In Environment: set your location by address |
| Left, Right | In Statistics: previous/next period |
| k | In Network: accept a device's changed TLS certificate |
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

If Dom is talking to any of them insecurely, a **Security** panel appears saying so. It is absent
when everything is fine, rather than showing a permanent "all clear" that the eye learns to skip.
See "Talking to your routers" below.

Each device shows a status derived from its own poll — whether Dom can actually talk to it:

| Status | Meaning |
|--------|---------|
| OK | The last poll succeeded, promptly |
| SLOW | It succeeded, but the device took 750 ms or more to answer |
| DEGRADED | Polls are failing, but not yet given up on |
| LOST | The device has stopped answering |
| UNKNOWN | Nothing has polled it, so its health is genuinely unknown rather than assumed fine |

This is measured on requests Dom was making anyway. There is no separate health check: an earlier
version pinged routers and modems three times every ten seconds and access points three times every
thirty, purely to fill in this column — about a packet a second, all day, to answer a question the
poll loop was already answering. It was also the worse answer, since a device can return a ping
while its API is unreachable or refusing credentials, and it is the API that Dom needs.

Alongside is a log of recent status changes, kept across restarts, and a chart of the modem's
Internet-facing traffic for today, in half-hour bars.

### How often Dom touches the network

| What | How often |
|---|---|
| ICMP sweep of the local subnets and ARP cache | at startup, again after 5 minutes, then every 4 hours |
| Device discovery (fingerprinting) | every 5 minutes |
| MikroTik REST poll (DHCP leases, firewall rules, traffic counters) | every 30 minutes |

Everything the MikroTik poll fetches is slow-moving or coarse: leases that discovery reads out of
memory whenever it happens to run, firewall rules that change when a person changes them, and
cumulative byte counters where a longer interval costs resolution rather than accuracy. The traffic
chart's bar width follows the poll interval, so each bar is exactly one poll.

Discovery is deliberately *not* slowed with it. It is how every device is found, energy hardware
included — a battery or wallbox that appears, or moves to a new address, only starts being polled
once discovery notices.

None of this affects measurement. The battery and switches are polled every two seconds and the
wallbox every five, because what they report is a rate that has to be integrated: a missed sample is
energy that cannot be recovered afterwards. A router reports state, which can be sampled whenever.

Both the sweep and discovery run once at startup, and 's' forces both at any time, so these intervals
only govern how long an *unattended* change takes to show up.

### Talking to your routers

Dom administers MikroTik devices over their REST API, which means presenting the router
administrator password on every poll — six devices every ten seconds.

It uses HTTPS where the device offers it, and **pins the certificate**: the first time Dom connects
it records the certificate the device presented, and every connection after that must present the
same one. A router has no certificate signed by a public authority — RouterOS generates its own, and
it is reached by IP rather than by name — so there is nothing to check it against except what it
showed last time.

That is trust on first use. It is weaker than a public certificate authority, because a device
already being impersonated the first time Dom connects would be trusted. It is much stronger than
what it replaces. After that first connection nothing on the network can read or alter the traffic
without the device's private key, and an attempt to substitute a certificate fails **before** the
password is written to the socket.

If a certificate changes, Dom cannot tell a reinstalled router from an impersonated one, so it does
not guess: it stops polling that device, does not send its password, and says so in the Network view
with both fingerprints. If you did reset or reinstall it, press 'k' to accept the new one — polling
resumes on the next cycle.

**RouterOS does not serve HTTPS out of the box.** Until you enable it, Dom falls back to plain HTTP
and tells you it is doing so, naming the devices. To fix that, on each device:

```
/certificate add name=dom common-name=dom
/certificate sign dom
/ip service set www-ssl certificate=dom disabled=no
```

Dom picks this up by itself — it tries HTTPS on every poll and only falls back when nothing answers,
so there is nothing to configure on this side. The failed attempt costs well under a millisecond on a
local network.

## Devices view

Every device the scanner has found, whether or not Dom knows how to poll it, with a detail panel for
the selected one. Press 'r' to give a device your own label, which takes precedence over the name
learned from DHCP.

For a myStrom switch the detail panel also allows toggling the relay, switching auto-mode between
disabled and time-based, and adding or deleting scheduled timers.

For a KEBA wallbox, Enter cycles the charging mode through four settings — the new mode is only shown
as fact once the wallbox confirms it:

| Mode | What the car draws |
|---|---|
| **Disabled** | Nothing. |
| **Full power** | Whatever the wallbox's hardware limit allows, regardless of where it comes from. |
| **Eco** | Only solar surplus, after the house battery has taken its share. The battery has priority. |
| **Eco (car first)** | Only solar surplus, but the car claims it before the battery does. |

The two Eco modes are a control loop, not a switch: every ten seconds Dom estimates how much solar
power is genuinely spare and sets the car's charging current to match, so the car tracks the sun
instead of pulling from the grid.

Grid export cannot be used to measure "spare" directly — the battery absorbs surplus before any of it
reaches the grid, so the export figure reads near zero whether there is surplus or not. Dom
reconstructs it from production, house consumption and the battery's own power instead.

Two things are held back deliberately. A 10% margin is kept in reserve, so an ordinary fluctuation — a
cloud, a kettle — costs a little unclaimed export rather than tipping the car into buying from the
grid. And once charging, Dom rides out a dip below the 6 A minimum rather than stopping the instant it
is crossed, because a target hovering at that boundary would otherwise switch the charger on and off
every ten seconds.

**Eco mode needs to know your electrical supply.** It defaults to 230 V across three phases. If yours
differs, set it before using Eco — the current Dom commands is scaled by this, so a single-phase site
left on the default would be asked for three times what it should be:

```bash
sqlite3 db.sqlite "INSERT OR REPLACE INTO Config VALUES ('supply_volts','230'),('supply_phases','1');"
```

The change takes effect on the next ten-second cycle; there is no need to restart. A value that
cannot describe a real supply is ignored in favour of the default rather than used.

## Statistics view

Long-term energy, aggregated by calendar period. 'm' and 'y' each open the view directly on the
monthly or yearly window; pressing the same key again returns to the current period after you have
browsed. Left and Right step back and forward one period, stopping at the oldest recorded day and at
today.

The top shows period totals — consumption, production, grid import, grid export — and two ratios:

- **Self-sufficiency**: the share of what you consumed that did not come from the grid.
- **Self-consumption**: the share of what you produced that you used rather than exported.

Both read as a dash rather than 0% when there is nothing to divide by, so a period with no data is
never mistaken for a period where you bought everything from the grid.

**Consumption is the house alone.** Energy going into the battery is not consumption — it shows up
when it comes back out. And self-sufficiency counts imported energy against the period it reached the
house in, not the period it was bought in: a winter night spent charging the battery from the grid is
not self-sufficient, and neither is the next morning that runs on it. If the battery is charged from
the grid at all, the totals panel says how much of the import went there rather than to the house.

Below is one bar per day (per month, in the yearly window). Each bar's length is that period's
consumption relative to the largest in view, split into the part covered by your own generation and
the part imported, so the self-sufficiency of each day is visible without reading the numbers:

```
      ┃━━━━┃ own generation   ┃━━━━┃ imported
                                        consumed     produced     self
  1   ┃━━━━━━━━━━━━━━━━━━━━━━━━━━━┃     30.0 kWh     40.0 kWh  100.0 %
  2   ┃━━━━━━━━━━━━━┃┃━━━━━━━┃          24.0 kWh     18.0 kWh   62.5 %
  3   ┃━━┃┃━━━━━━━━━━━━━━━━━━━━━━┃      28.0 kWh      4.0 kWh   10.7 %
```

The three figures after each bar are that period's **consumed** total, what it **produced**, and its
**self**-sufficiency — the share of what it used that did not come from the grid, which is the same
thing the green part of the bar shows.

Each part is drawn with a mark at both ends rather than as a solid block, so where one stops and the
next begins is unambiguous — the two ends meeting in the middle (`┃┃`) say plainly that the imported
part is stacked on top of your own generation rather than measured from zero.

Note that the bar is **consumption**, not production. A day that generated far more than it used
looks the same as one that generated exactly what it used — both are entirely own-generation. The
`prod` figure beside the bar is what tells them apart, and bar lengths are scaled against the largest
consumption *or* production in view, so an export-heavy period leaves every bar short of the full
width.

A day with no recorded data is marked as such rather than drawn as a zero.

The current period is partial: this month means the 1st to today, not the 1st to the 31st.

### Where the numbers come from

The statistics view reads a daily rollup table, not the 2s measurement series. The series is far too
large to aggregate on demand — a single month of it is millions of rows — so a background task
totals each local day into `EnergyDaily` once, and the view reads that. The first run after this
feature was added has the whole recorded history to work through, and does so gradually so as not to
compete with the device poll loops for disk.

Grid import and export are split when the day is rolled up, not afterwards: the underlying series is
signed, and a daily sum of it would collapse the two into a net figure that could not be separated
again.

## Environment view

Outdoor temperature for your location on top, then the temperatures your own devices report, then a
history chart for whichever sensor is selected (Up/Down to change).

### Outdoor temperature

Press 'a' and type an address — "Bundesplatz 3 Bern", or just a town. Dom resolves it with
[swisstopo](https://api3.geo.admin.ch)'s federal search service and shows you what it matched, so you
can retype if it picked the wrong thing. The location is remembered.

The reading itself is the current 10-minute mean from the nearest station in SwissMetNet, MeteoSwiss's
automatic monitoring network, refreshed every ten minutes.

It is shown with the station's **name, altitude and distance** — because that qualifies it. A
measurement taken 20 km away and 900 m higher up is not the temperature outside your door, and the
view should not imply that it is. For Bern the nearest station is 5 km away at a near-identical
altitude; in a mountain valley it may be much less representative.

Readings are kept indefinitely at their ten-minute resolution: 144 rows a day is a few megabytes a
decade, so unlike the energy series this one needs no rollup or pruning.

If a fetch fails, the view says so rather than continuing to show the last number as though it were
current. With no location set, nothing is ever fetched.

### Device temperatures

**The sensor readings are device temperatures, not room temperatures.** The only hardware Dom supports that
reports one is the myStrom switch, and what it reports is its own case temperature — which tracks
the appliance plugged into it more than the room it sits in. The view says so rather than presenting
the figure as ambient.

Each sensor row shows the live reading and today's range so far. Below, one row per day draws that
day's minimum-to-maximum span as a bar across a shared temperature scale, with the daily mean marked
inside it — so a run of warming or cooling days is visible as the bars drift across the scale,
without reading any numbers.

Daily figures are accumulated as each day happens rather than computed afterwards, because their
source (`RawDeviceMeasurements`) is kept for only 24 hours. That also means history starts from the
day this feature was first run; earlier days cannot be reconstructed.

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
leaving the app running is what collects data.

**Headless works.** With no terminal to draw on — under systemd, over ssh without a TTY — Dom says so
and keeps collecting, with the log as the only output. It stops on Ctrl-C or `SIGTERM`. Nothing about
polling, integrating or rolling up needs a screen.

State lives in `db.sqlite` in the working directory: discovered devices and their credentials,
measurement history at several resolutions, and application settings such as the chosen theme (in
the `Config` table).

**Those credentials are stored in plain text**, because Dom has to present them to your devices
unattended. The database is therefore created readable only by the user running Dom (mode `0600`,
including its `-wal` and `-shm` files), and an existing one is tightened to match on every start.
There is no encryption at rest: the key would have to live on the same machine, within reach of the
same process, which moves the secret rather than protecting it.

Diagnostics go to `dom.log` beside the database — not to the terminal, which the TUI owns for as long
as it runs. It is rotated to `dom.log.1` at startup once it passes 5 MB, so a restart never destroys
the log of what happened before it. `RUST_LOG=debug` raises the level.

### How long measurements are kept

Energy is stored at three resolutions, each retained for progressively longer:

| Resolution | Retained | Why |
|---|---|---|
| 2 seconds | 4 days | What devices actually report. Serves the "today" views, and is the source the coarser tiers are built from. |
| 1 minute | 90 days | The finest resolution anything displays. Summing 2-second energies into a minute is exactly lossless for totals. |
| 1 day | forever | What the Statistics view reads. Tiny — a few thousand rows a year. |

Battery state of charge, and device temperature, are each summarised to a daily min/max/average on
the same schedule. Temperature is folded in incrementally as the day passes, since its source is kept
for only 24 hours and so cannot be re-read later.

This matters because the 2-second series is large: a month of it is millions of rows, and keeping it
indefinitely grew the database by roughly a gigabyte a month while storing about thirty times the
resolution anything renders. Rolled up and pruned, the database settles at a few hundred megabytes
and stays roughly flat.

Rolling up and pruning happen in the background, and pruning refuses to drop any day the next tier
up has not yet summarised — so a rollup problem can never turn into lost history. Peak power per
minute is recorded before the raw samples go, since that is the one figure a coarser tier cannot
reconstruct.

**Space is reclaimed gradually, in the background.** SQLite's `DELETE` returns pages to an internal
free list rather than shrinking the file, so a prune on its own makes the file no smaller. The hourly
prune hands a bounded number of those pages back as it goes, which needs no lock and runs while Dom
does.

That only applies to databases Dom created. One made before this existed has the wrong file format
recorded in it, and needs a one-off full `VACUUM` to convert — after which the background reclaim
keeps it that way. `VACUUM` rewrites the whole file under an exclusive lock, so stop Dom and run:

```bash
sqlite3 db.sqlite "PRAGMA auto_vacuum=INCREMENTAL; VACUUM;"
```

On a measured 13-day sample this took the file from 338 MB to 108 MB in about 7 seconds. `VACUUM`
rewrites the whole database under an exclusive lock, which is why Dom has to be stopped for it.

## Online services

Dom talks only to your own network, with three optional exceptions — all used solely by the
Environment view, and none contacted at all until you set a location:

| Service | Used for | Data sent |
|---|---|---|
| [swisstopo SearchServer](https://api3.geo.admin.ch) | Turning an address into coordinates | The address you type, once, when you set it |
| [MeteoSwiss Open Data](https://www.meteoswiss.admin.ch/services-and-publications/service/open-data.html) | Outdoor temperature | Nothing — it is a fixed nationwide file, fetched whole |
| [Open-Meteo](https://open-meteo.com) | Solar irradiance forecast, for predicted production | Your coordinates, and the array's tilt and azimuth |

Note the second column of the second row: the temperature file covers all of Switzerland, so Dom
downloads the same document everyone else does and picks the nearest station locally. Your location is
never sent to MeteoSwiss.

The first two are free federal services needing no account or API key. Weather data is MeteoSwiss
Open Government Data — free of charge and reusable, including commercially, with the source
attributed.

Open-Meteo needs no key either, and its data is CC BY 4.0 — the credit is shown in the Environment
view. It is the one service whose terms bind **you** rather than just this program: the free tier is
non-commercial, and explicitly names personal home automation as a permitted use. Running Dom in your
own house is squarely within it. Selling Dom or running it as part of a business — which the licence
below permits a worker-owned co-operative to do — would need an Open-Meteo API plan, or self-hosting
their open-source server.

It is used rather than MeteoSwiss's own forecast because of size, not preference: MeteoSwiss
publishes point forecasts as open data, but as one file per parameter covering every location in the
country — 32.5 MB an hour to read one house's numbers. The same request to Open-Meteo is about 7 KB,
and it serves MeteoSwiss's own ICON-CH1 and ICON-CH2 models, so it is the same forecast.

## Platform Support

Only Linux is supported. Support for macOS and Windows is a non-goal.

### Small machines

Dom is meant to sit on something cheap and always-on, and is written for that: a Raspberry Pi 2 —
32-bit ARMv7, 900 MHz, 1 GB of RAM — is the machine the following were chosen against.

- **Writes are batched.** Each poll commits once rather than once per measurement, and the database
  runs `synchronous=NORMAL`, so commits do not flush the disk individually. On an SD card that is the
  difference between a card that lasts and one that does not.
- **The today-charts read the per-minute tier**, not the two-second series, which is an index range
  scan rather than a full table scan — measured at 0.003 s against 0.498 s.
- **Startup waits for a plausible clock.** A Pi has no battery-backed clock and boots at whatever was
  last written to disk; measurements stamped in the past are rolled into the wrong day, or into one
  already summarised and therefore never summarised again. Dom waits, briefly and boundedly, for the
  clock to catch up with the newest thing it has already recorded.

Cross-compile rather than building on the device: `ring` and `libsqlite3-sys` need a C toolchain, and
the release profile's fat LTO will likely exhaust 1 GB of RAM on the Pi itself.

# Contact

You can contact me at [marko@ivankovic.me](marko@ivankovic.me).

# License

Copyright © 2026 Marko Ivankovic

Licensed under the [Anti-Capitalist Software License v1.4](https://anticapitalist.software/).

This is anti-capitalist software, released for free use by individuals and organizations that do not
operate by capitalist principles. **There is no paid licence, and no way to buy an exemption.** If you
do not meet the conditions below, you have no licence to this software at all.

You may use, modify, distribute and even sell copies of it, provided you are:

- an individual person, labouring for yourself; or
- a non-profit organization; or
- an educational institution; or
- an organization that seeks shared profit for all of its members and lets non-members set the cost
  of their labour.

And, if you are an organization:

- all owners are workers and all workers are owners, with equal equity and/or equal vote; and
- you are not law enforcement or military, nor working for or under either.

See the [LICENSE](LICENSE) file for the exact terms, which are what actually govern.

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
        |- stats.rs       <- Long-term statistics: period arithmetic and bucketing
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
