# Pending

Findings from the repository-wide code health passes. The other findings from those passes
have since been fixed; see SPECS.md for the decisions taken.

# From the pass of 2026-09-11

This pass took robustness as its subject. Every file in `src/` was opened; the four largest
(`db.rs`, `tui/render.rs`, and the KEBA and MikroTik device modules) were read in the parts
bearing on that subject rather than end to end, so a later pass over those four for
*anything else* would still find new ground. Fourteen findings were fixed in the pass itself,
and the pending list was then worked through on 2026-09-12 — see SPECS.md, "Code health pass
(2026-09-11)". What remains is what needed a judgement call or a larger change.

`tests/` was not read, only run. Three of the fixes came from noticing that a behaviour
nothing asserted had quietly stopped happening (`App::forget_device` and the orientation
search's refinement pass), or that an assertion was quietly wrong (`seed_days`, which made two
rollup tests fail for nine hours every night). What the suite does *not* cover, and what it
covers incorrectly, is worth a pass of its own.

It was preceded by a lint sweep: `clippy` with `pedantic` and `nursery` reports 1,206 hits,
almost all of them style (`missing_errors_doc`, `must_use_candidate`, `doc_markdown`). They
were read as pointers to inspect and nothing was changed to satisfy one. Worth noting that
the 2026-08-23 pass recorded that sweep as finding "nothing" for `significant_drop_tightening`;
it now reports twenty, plus two `significant_drop_in_scrutinee` at `keba.rs`. None of them is
a lock held across an `await` — that remains true — but the claim as written is no longer
accurate about the lint.

## One write failure looks like all of them

`App::write_error` now says that recording a measurement failed, and the status bar shows it.
What it cannot say is *how much* has failed: it holds the most recent message, so one
transient `SQLITE_BUSY` on one device and a disk that has been refusing every write for an
hour read identically, and the field clears as soon as any loop's next write succeeds.

That was the right first move — the failure was invisible, and now it is not — but "one
device hiccuped" and "nothing has been recorded since Tuesday" are different situations and
the user cannot currently tell them apart. A count of consecutive failures, or the instant of
the last successful write, would separate them. Left open because which of those to show is a
judgement about the interface, not about the mechanism.



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
