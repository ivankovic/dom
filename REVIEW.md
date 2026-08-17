# Pending

Remaining findings from the repository-wide code health pass (2026-08-17). The other
findings from that pass have since been fixed; see SPECS.md for the decisions taken.

## `src/tui/mod.rs` has no test module

README.md asks every file in `src/` to end with its own test module. `src/tui/mod.rs`
(the terminal event loop) has none, and it is the one remaining file where that is a
real gap rather than a formality — `src/lib.rs` is only `pub mod` declarations.

The loop mutates `App` in response to `crossterm` events, so testing it needs a way to
feed synthetic events without a terminal. That is a test-harness change rather than a
unit test, which is why it was not done as part of a health pass. Everything the loop
*decides* is currently untested: which key maps to which view, that Escape cancels
rather than commits a rename, that the theme toggle persists.

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
