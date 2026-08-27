# Pending

Findings from the repository-wide code health passes. The other findings from those passes
have since been fixed; see SPECS.md for the decisions taken.

# From the pass of 2026-08-23

Eighteen of the findings from this pass were fixed on 2026-08-23 and removed from below;
see SPECS.md, "Code health pass" for the decisions. What remains is what needed a
judgement call or a larger change.

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
device modules have none: not a reduced one, zero `//!` lines. `db.rs` is 3,482 lines and
`render.rs` 2,849, and both open directly on `use` statements. The per-item documentation
inside them is genuinely good, which makes the missing orientation more noticeable rather
than less — there is nothing that says what the file as a whole is responsible for.

## `main` spawns two tasks inline and the rest as named functions

Nine background tasks are spawned from `main`. Seven are named `async fn`s with doc
comments explaining their schedule; two — the hourly prune of `RawDeviceMeasurements` and
network status events, and the 60-second chart refresh — are anonymous `async move` blocks
inline in `main`, inside bare `{ }` scopes that exist only to shadow `pool` and `state`.

Separately, and cutting across that split rather than along it: seven of the nine drive
their loop with `interval` and `MissedTickBehavior::Skip`, while two — the inline chart
refresh and the named `statistics_task` — end with a bare `sleep`, so their period drifts
by however long the work above them took. That is defensible for `statistics_task`, whose
pass is deliberately throttled and of variable length, but it is not stated anywhere, and
nothing at the call site distinguishes any of the nine from the others.


# From the pass of 2026-08-17

## `src/tui/mod.rs`'s event dispatch is still untested

Partly addressed on 2026-08-23: the file now has a test module, and the six pure helpers
it had all along — the detail-panel slot model, KEBA mode cycling, `is_valid_hhmm`, the
energy device list, the view predicate, and the input bound — are covered by eleven tests.
That was the cheap half, and it was cheap only because nothing had ever opened the file to
look.

What remains is the part the original finding was really about: `event_loop` itself decides
which key maps to which view, that Escape cancels rather than commits a rename, and that
the theme toggle persists — and none of it is exercised. It mutates `App` in response to
`crossterm` events, so testing it needs a way to feed synthetic events without a terminal.
That is a harness change rather than a unit test.

## The today-queries still read the 2-second tier

`query_today_energy` reads roughly 43,000 raw rows to produce 1,440 per-minute buckets. Against
`EnergyMinute` the same output would come from 1,440 rows, roughly thirty times cheaper, and that
query runs every 60 seconds in the background.

Not done, because it needs a boundary: the minute tier deliberately excludes the minute in progress,
so a query spanning both tiers must know exactly where one ends and the other begins, or it
double-counts. The mechanism is straightforward — take `MAX(minute)` for today, add 60 seconds, read
minute rows below it and 2s rows above — but it adds a correctness-critical invariant to a query that
already completes in a couple of seconds.

Note also that the sign-split queries (grid import/export in `query_today_energy`, charge/discharge in
`query_device_energy_today`) cannot naively move to the minute tier: they must read `energy_ws_pos`
and `energy_ws_neg` rather than re-splitting an aggregated value, since a minute that both imported
and exported nets out.

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
