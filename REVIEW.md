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

**Fixed on 2026-09-12.** Every file in `src/` now opens with a `//!` block; see SPECS.md,
"every module says what it is for".

One caveat on how, because it bears on how much to trust them. The four largest files
(`db.rs`, `tui/render.rs`, and the KEBA and MikroTik device modules) were still not read end
to end — the 2026-09-11 caveat below stands. Their module docs were written from the
item-level documentation already in them, their section structure, SPECS.md, and targeted
reads of what each claim names, and then every symbol, constant and behaviour each doc asserts
was grepped and checked against the code. That caught one outright error: the MikroTik doc
first described `decide_poll_outcome` as handling counter resets, which it does not — it
decides which of the two fetches is allowed to fail the poll. What the docs state has been
verified; what they *omit* about those four files is not evidence of absence.

## `Energy.resolution` is still vestigial, and the column is now the whole of it

Every row in `Energy` has `resolution = '2s'`, and the coarser tiers live in their own tables —
for the reasons recorded in SPECS.md. The column and its
`CHECK (resolution IN ('2s','1min','10min'))` remain, and every query still filters on it. The
same is true of `EnergyStorage`.

**The index half of this was fixed on 2026-09-12 and the space claim below it was wrong;** see
SPECS.md, "the `Energy` index was fragmentation, not the columns in it". Both
`resolution`-leading indexes are now rebuilt without the column by
`db::repack_series_indexes`, behind `DOM_MIGRATE_ENERGY`.

What is left is only the column, and only as a readability problem: the table reads as though
it holds multiple resolutions when it does not. Measured, removing it is worth ~3 MB of a
124 MB database and *costs* space the way SQLite implements it — `ALTER TABLE ... DROP COLUMN`
rewrites rows in place and fragments the table, 48 MB to 53, so the drop has to be followed by
a `VACUUM` to come out ahead of where it started. Left open because the honest justification is
now clarity rather than space, which makes it a judgement about how much a misleading schema is
worth, and because it is the one change here that requires editing every query that names the
column (~15 sites) and so cannot be made optional.

## The `Energy` index is larger than the table

**Fixed on 2026-09-12, and the finding's reasoning was wrong.** It was recorded as 914 MB
against a 555 MB table "because it carries `metric` and `resolution` as TEXT for every row",
with interning both as integers as the remedy. Re-measured on a retention-bounded four-day
table, with the index built incrementally as production builds it, the figures are 84 MB against
48 — and the TEXT columns are ~3 MB of that. The other ~45 MB is page density: an index
maintained by inserts that do not arrive in its key order settles at around half full, and
`incremental_vacuum` returns whole free pages without ever repacking partially-full ones.

So "larger than the table" was an artifact of fragmentation, not of column width — repacked, the
index is 39 MB against a 48 MB table. Rebuilding it recovers 47.9 MB, more than dropping the
column would (42.6) and about the same as a full `VACUUM` (47.5). The interning the finding
asked for was declined on that evidence; the table of measurements is in SPECS.md.

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
