# Pending

Findings from the repository-wide code health passes. The other findings from those passes
have since been fixed; see SPECS.md for the decisions taken.

# From the pass of 2026-09-11

This pass took robustness as its subject. Every file in `src/` was opened; the four largest
(`db.rs`, `tui/render.rs`, and the KEBA and MikroTik device modules) were read in the parts
bearing on that subject rather than end to end, so a later pass over those four for
*anything else* would still find new ground. Fourteen findings were fixed — see SPECS.md,
"Code health pass (2026-09-11)". What remains is what needed a judgement call or a larger
change.

`tests/` was not read, only run. Two of the fixes came from noticing that a behaviour
nothing asserted had quietly stopped happening (`App::forget_device`, and the orientation
search's refinement pass), so what the suite does *not* cover is worth a pass of its own.

It was preceded by a lint sweep: `clippy` with `pedantic` and `nursery` reports 1,206 hits,
almost all of them style (`missing_errors_doc`, `must_use_candidate`, `doc_markdown`). They
were read as pointers to inspect and nothing was changed to satisfy one. Worth noting that
the 2026-08-23 pass recorded that sweep as finding "nothing" for `significant_drop_tightening`;
it now reports twenty, plus two `significant_drop_in_scrutinee` at `keba.rs`. None of them is
a lock held across an `await` — that remains true — but the claim as written is no longer
accurate about the lint.

## Nothing sets `busy_timeout`

`db::connect` reasons explicitly about `synchronous`, `auto_vacuum` and WAL, and says why
each is set on the *options* rather than as a pragma. It does not mention `busy_timeout`,
so the database runs on sqlx's five-second default with sqlx's ten-connection default pool,
against nine background tasks and a rollup that deletes in 20,000-row batches, all on one
SD card.

Not a confirmed bug, and the failure mode is contained: `handle_poll_failure` is reached
only from the *fetch* in all four poll loops — checked in each — so a write that fails on a
busy database does not get reported as an unreachable device. What it does instead is the
entry above this one. But `busy_timeout` is the one connection-level setting the file
deliberately reasons about and then leaves at a default it does not name, and ten writer
connections to a single-writer database is not obviously the right shape.

Related, and the reason this is written down at all: `tests/db.rs`'s
`freed_pages_are_handed_back_rather_than_hoarded` failed with `PoolTimedOut` during this
pass's baseline run, on a machine at load average 16, and then passed alone in 13.5 s.
`.config/nextest.toml` records that flake as having "not reproduced since". It has now.

## The poll-write failure path is a log line and nothing else

`logging`'s own module doc sets the rule: "a message written here is one the user will not see
unless they go looking. Anything a person needs to *act* on belongs in `App` as well." That is
what `rollup_error` exists for.

A poll that fetches fine but fails to *record* is the same class of thing and does not follow
it. All four device loops and `weather::refresh` now `log::warn!` and carry on — which is an
improvement on discarding it, and still only a file. A database that keeps refusing writes
leaves the views showing live readings, `ConnStatus::Online` against every device, and a series
quietly full of holes.

Not carried further here because it needs a judgement about shape rather than a line of code:
it is not per-device (the database is one resource, and every loop would report the same failure
at once), so it is not `last_error`, and the honest thing is probably one field like
`rollup_error` naming the most recent write failure across all of them. Worth deciding before
adding a sixth device type.

## A station with no altitude renders as `NaN m`

`weather::parse_stations` falls back to `f64::NAN` when a station's `altitude` field is
absent or will not parse, and the Environment view formats it with `{:.0}` — so the panel
reads `· NaN m · 12.3 km away`. The column is nullable, so nothing is lost in storage; this
is only what a person sees.

It matters more than a cosmetic glitch because of what the figure is *for*: the module doc
says the altitude is "worth showing: a reading from 1880 m means something different from one
at 550 m", and `OutdoorReading`'s doc says the distance and altitude "qualify the reading".
A `NaN` does not qualify anything.

`Option<f64>` is the honest type and there is already a convention for rendering one — `pct`
prints a padded em dash rather than a misleading zero. Left as a finding rather than fixed
because it is a type change across `weather`, `db`, `app` and `render` for a display defect,
which is a different trade than the rest of this pass was making.

## `load_all` lets one bad row disable every device of a type

Each device module's `load_all` parses the `ip` column with `?`, so a single unparseable
value fails the whole query. Every caller reads that as "no devices of this type":
`maybe_spawn_*_poll_loop` returns without spawning, `eco_job` skips its tick, and
`bootstrap_known_devices` adds nothing. One corrupt row therefore stops polling for every
battery, or every switch, silently.

Two places in the same codebase already solve it the other way: `main::lease_addresses`
with `filter_map(|lease| lease.address.parse().ok())`, and — in a `load_all` of its own —
`dom_local::load_all`, which skips a bad row with `if let Ok(ip)`. The rows here are written
by Dom itself, so this is latent rather than broken, which is also why it is a judgement
call: skipping the row needs somewhere to say that it was skipped, or a device disappears
with no more explanation than it has now.

# From the pass of 2026-08-23

Eighteen of the findings from this pass were fixed on 2026-08-23 and removed from below;
see SPECS.md, "Code health pass" for the decisions. Four more have since been fixed and are
noted as such where they stood. What remains is what needed a judgement call or a larger
change.

Every file in `src/` was read for this pass. It was preceded by a lint sweep — `clippy`
with `pedantic` and `nursery`, plus `await_holding_lock` and `significant_drop_tightening`
specifically — which found nothing: no lock is held across an `await`, and the numeric
casts in the render and chart code are all clamped or saturating. Everything below came
from reading.

## The older half of `src/` has no module documentation

`solar.rs`, `stats.rs`, `theme.rs` and all of `online/` open with a `//!` block explaining
what the module is for and which decisions are load-bearing — they are the most readable
files in the project, and that is why.

`db.rs`, `app.rs`, `main.rs`, `fingerprint.rs`, `tui/mod.rs`, `tui/render.rs` and all five
device modules have none: not a reduced one, zero `//!` lines. `db.rs` is 4,522 lines and
`render.rs` 3,508, and both open directly on `use` statements. The per-item documentation
inside them is genuinely good, which makes the missing orientation more noticeable rather
than less — there is nothing that says what the file as a whole is responsible for.

Still true as of 2026-09-11, and now the oldest thing on this list. `alarm.rs` and
`cluster.rs`, added since, both open with one — so the convention is not in doubt, only
unapplied to the files that predate it.

## `main` spawns two tasks inline and the rest as named functions

**Half fixed on 2026-09-11.** The two anonymous `async move` blocks are now `prune_task` and
`chart_refresh_task`, named and documented like the other seven.

What remains is the part that cut across that split rather than along it: seven of the nine
drive their loop with `interval` and `MissedTickBehavior::Skip`, while two — `chart_refresh_task`
and `statistics_task` — end with a bare `sleep`, so their period drifts by however long the
work above them took. That is defensible for `statistics_task`, whose pass is deliberately
throttled and of variable length, but it is not stated anywhere, and nothing at the call site
distinguishes any of the nine from the others.

## `Energy.resolution` is now vestigial

Every row in `Energy` has `resolution = '2s'`, and the coarser tiers live in their own tables — for
the reasons recorded in SPECS.md. The column and its `CHECK (resolution IN ('2s','1min','10min'))`
constraint remain, and every query still filters on it. Harmless, but it reads as though the table
holds multiple resolutions when it does not, and it costs a repeated TEXT value per row in both the
table and the 914 MB index.

## The `Energy` index is larger than the table

`idx_energy_metric_res_time` was 914 MB against a 555 MB table, or 75 bytes per row against 46,
because it carries `metric` and `resolution` as TEXT for every row. Interning both as small integers
would shrink it substantially.

Independent of retention: pruning fixed the *growth*, this would fix the *density*. It needs a rewrite
of the table, so it is worth doing only alongside some other migration that already has to.

## Poll intervals are read once at spawn time

A poll loop reads `poll_interval_secs` from its `DeviceRecord` when it starts and builds
its ticker from it. Changing a device's interval in the DB therefore has no effect until
the process restarts. Nothing in the UI currently edits intervals, so this is latent
rather than broken — worth knowing before adding that.

## `resolve_redirect` cannot follow a redirect to an IPv6 literal

The host/port split uses `rfind(':')`. For `http://[::1]:8080/x` that does pick the right
port, but the host comes out as `[::1]`, which `IpAddr::from_str` rejects (it does not
accept brackets) — so the redirect is followed against the address already being probed
instead of the one named. A bare IPv6 authority is misread outright.

Harmless today: local devices redirect to IPv4 addresses or to hostnames, and the
fall-back-to-the-probed-address behaviour is deliberate for hostnames. It would matter for
a device that redirects across hosts on an IPv6-only segment.

# From the pass of 2026-08-17

## `src/tui/mod.rs`'s event dispatch is still untested

**Largely fixed since.** The key map is now `apply_key`, a pure function of `App`, the event
and the dialog target — documented as such — and it is exercised directly by the tests at the
foot of the file, no terminal required. What the original finding asked for has happened.

What is left is narrower than a harness: `event_loop` still owns the part that cannot be
pure, which is the `await` after the key press — the database write, the device command, the
spawned rename. The 2026-09-11 pass changed exactly that code (see SPECS.md, "a key press
that did not do anything says so") and could not test the change, because deciding *what* to
do is separated from *doing* it and only the first half is reachable.

## The today-queries still read the 2-second tier

**Fixed since.** `db::minute_tier_boundary` is the boundary the finding said the fix needed:
it returns the start of the first minute `EnergyMinute` does not cover, and both
`query_today_energy` and its grid totals now read the minute tier below it and the 2s rows at
or above it. The sign-split caveat the finding warned about was honoured — the totals read
`energy_ws_pos`/`energy_ws_neg` from the minute tier rather than re-splitting an aggregate.
