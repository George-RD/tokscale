# Ratchet inflation recovery

Design for [#960](https://github.com/junhoyeo/tokscale/issues/960). Rescanning
the same local history under a different timezone permanently inflates stored
totals, and the monotonic guard that causes it exists to prevent a real
production data loss. This document proposes a correction that heals the
inflation without weakening that protection, and states what happens to every
class of user under the change.

> **Status: proposal. Nothing here is implemented.** No gate, heal table, or
> zero-out pass exists in the tree. Accept, reject, or amend.

Verified against `origin/main` @ `1d844c3e`. Every file:line below was read at
that commit; "How to re-derive" says how to recheck each claim.

## The two failures are one mechanism

`compute_daily_active_time` (`crates/tokscale-core/src/sessionize.rs:285`)
delegates to `compute_daily_active_time_with_timezone(intervals, &chrono::Local)`
at `:288`. Token attribution follows message timestamps into the same local-day
buckets. So which calendar day a unit of usage lands in is a function of the
machine's timezone at scan time.

`mergeClientBreakdownsWithRegressionGuard`
(`packages/frontend/src/lib/db/helpers.ts:154`) then defends every per-day,
per-client decrease. A timezone change moves usage from day `d` to `d-1`; the
guard defends the stale value on `d` and accepts the new value on `d-1`, so the
account is credited twice.

The guard is not a mistake. `d9df8c9c` ("fix(submit): preserve usage history
after local session cleanup") added it after a user deleted local session files,
resubmitted, and had real history erased. Its commit message rejects the
alternative explicitly:

> `Rejected: Replace stored metrics with the newest snapshot | repeats the production data loss caused by local session cleanup`

Both failures present as the same signal — *a per-day client total went down* —
and the payload carries nothing that separates them. Any fix that simply relaxes
the guard re-breaks the user `d9df8c9c` was written for.

## This pattern already exists in the codebase

The guard has a documented exception, and it is the same shape as what this
document proposes. `foldedClientFloors` (`helpers.ts:158-172`, applied at
`:201-217`) handles the case where the stored value is an alias-folded double
count — a stale `kilocode` key summed with `kilo` for the same usage. There, a
*lower* incoming value is allowed to replace the stored one, gated on a floor:

> nothing proves an incoming submission covers the full day (partial re-parses
> are the exact case the guard exists for), so healing only happens when the
> incoming value is at least the largest single contribution: any truthful
> complete-day total must be >= each of the components that were summed.

That is precisely the structure needed here: *a known-inflated stored value may
be replaced by a smaller one, provided the incoming value clears an invariant
lower bound that proves the scan was complete.* The alias fold and the timezone
re-split are two instances of one problem, and the second should be built as a
second instance of the existing mechanism rather than as new machinery.

The differences are only in the bound and the axis:

| | alias fold | timezone re-split |
|---|---|---|
| inflated because | two keys summed for one client | one day's usage counted in two days |
| axis | client keys within a day | days within a client |
| floor that proves coverage | largest single folded component | client's token total across the range |

## The invariant: a day-agnostic per-client token total

The re-split moves tokens **between** days without creating or destroying them,
so any total taken across all days is invariant to it while the per-day rows are
not. `calculate_summary` (`crates/tokscale-core/src/aggregator.rs:107-110`)
already folds exactly this way, with no reference to which day each contribution
belongs to.

Tokens are the right quantity to measure because they are what the leaderboard
ranks and the profile shows.

### Why not active time

An earlier draft used `submitted_devices.total_active_time_ms`, on the grounds
that `compute_time_metrics` (`sessionize.rs:180`) is a plain sum of interval
durations with no date bucketing. That is true — it is timezone-invariant, and
`schema.ts` documents this. It was still wrong, because it is a **proxy** for
token coverage rather than a measure of it:

- Only 11 of 45 session parsers populate `duration_ms`. For the rest
  `active_duration_ms` falls back to the wall-clock span between messages in a
  block (`sessionize.rs:155-160`), a different shape of quantity.
- A session containing a single message contributes zero active time regardless
  of how many tokens it carries.

A token-coverage loss concentrated in short sessions is therefore invisible to
an active-time sensor: the gate would pass and overwrite stored rows with
token-deficient data — the exact failure the gate exists to prevent, on the
exact quantity that matters.

## The heal rule

Per device, per client `C`. `C` may be **rewritten** from the payload when both
hold:

1. **Range coverage** — the payload's contribution dates span at least the
   device's stored date range for `C`. Without this a `--since` scan's smaller
   total is not comparable to an all-time high-water.
2. **Invariant clears** — the payload's total tokens for `C` is at least the
   stored per-device high-water for `C`.

Otherwise `C` falls through to the current guard, unchanged.

Rewriting `C` means, across the covered range:

- days present in the payload → `C`'s entry is taken from the payload verbatim;
- **days absent from the payload → `C`'s entry is removed** (see below — this is
  the half of the repair that the current code structurally cannot do);
- clients other than `C` in those days are untouched.

### Per-client, not per-payload

The guard iterates per client (`helpers.ts:183`), and the heal must match that
granularity. A payload-level gate fails whole-device whenever any single client
legitimately shrinks — a narrow #961 partial-`session_model_usage` case would
block an unrelated client's timezone repair. Per-client scoping bounds the blast
radius of both a false accept and a false reject.

It also resolves client filtering for free: `--client codex` reports codex's
*full* total, so codex may legitimately heal while untouched clients are simply
absent from `submittedClients` and never rewritten.

### The zero-out pass is mandatory

The submit route's per-day loop only visits days present in the payload
(`route.ts:604`, `existingDaysMap.get(incomingDay.date)`), and the route has **no
delete path for `daily_breakdown` at all**. Contributions are produced by
`aggregate_by_date`, which emits only days with activity.

So when a re-split empties day `d` entirely, `d` is absent from the payload, is
never visited, and keeps its stale value forever. **Healing without an explicit
zero-out repairs only the day that gained and not the day that lost — which
leaves the double count exactly as it was.** This is the single most important
implementation detail in this document and it was missing from every earlier
draft.

Bounding it is what conditions 1 and 2 are for: absence may only be read as zero
inside a range the payload demonstrably covers, for a client whose invariant
cleared.

### Storage

A per-(device, client) token high-water is required. `submissions.totalTokens`
cannot serve — it is recomputed from the daily rows on every submit
(`route.ts:787`) and is itself inflated, so it would validate the inflation
against itself.

New table, maintained with `GREATEST` on conflict exactly as the existing metric
columns are (`route.ts:412`):

```
submitted_device_client_totals(submitted_device_id, client, tokens_highwater)
```

The device's stored date range needs no new storage — it is `MIN(date)`,
`MAX(date)` over that device's existing rows.

## Interactions that must be preserved

These are not edge cases; each is live code that a naive rewrite breaks.

**Alias fold writeback.** When a fold is preserved, the route deletes the
collapsed key and writes the *original raw alias keys* back
(`route.ts:638-645`), because the collapsed form is indistinguishable from real
usage and writing it back "would burn the heal floor on the first partial
resubmit and permanently re-cement the double count." The rewrite path must
reproduce this. Simplest safe rule: **a client with a fold floor is never
eligible for the timezone heal** — let the existing fold mechanism finish first.

**Backfill coexistence.** `origin: "backfill"` is stamped per client inside
`source_breakdown` (`route.ts:595-601`) and carried through merges by
`deriveClientBreakdownProvenance` (`helpers.ts:113-126`). CLI and imported
history therefore coexist in the same day's breakdown. A CLI scan legitimately
cannot see imported history, so the rewrite must **preserve entries tagged
`origin: "backfill"`** rather than removing them as "absent from the payload".
Separately, a payload whose own `provenance.origin` is `backfill` must be
excluded from healing entirely.

**Day-level active time.** `activeTimeMs` has its own monotonic merge
(`route.ts:622-630`) and is inflated by the same mechanism. It should be
rewritten alongside the client entries for a healed client, or it stays inflated
after the tokens are corrected.

**Transaction.** The gate, the backup write, the rewrite and the zero-out must
share the existing transaction. Evaluated outside it, the gate races a
concurrent submit from another device.

## Behavior for every user state

| # | User state | Heal | Outcome |
|---|---|---|---|
| 1 | Never submitted | n/a | Plain insert. |
| 2 | Healthy, single device, stable TZ | eligible, no-op | Payload equals stored; the changed-rows-only narrowing makes it a no-op beyond new days. |
| 3 | Healthy, multi-device | per-device | `UNIQUE(submission_id, submitted_device_id, date)` scopes every write. No cross-device effect. |
| 4 | **TZ-inflated, sessions intact** | **heals** | Gained days rewritten, emptied days zeroed. Correct on next full submit. |
| 5 | TZ-inflated, multi-device | per-device | Each device heals as it submits; partial healing moves monotonically toward truth. |
| 6 | **Deleted local sessions (`d9df8c9c`)** | **blocked** | Token total fell below the high-water. Current guard defends. History preserved exactly as today. |
| 6b | Sessions moved where the collector does not yet scan | blocked, temporarily | Same protection as 6; self-resolves once collector support lands. |
| 7 | Deleted sessions *and* changed TZ | blocked | No loss, no healing. Phase 3's constituency. |
| 8 | Retired device | never runs | Rows frozen. Pre-existing, not worsened. |
| 9 | No stored high-water yet | blocked | Guard applies, baseline recorded. One-submit warm-up. Treating a missing baseline as `0` would let any payload pass and is rejected. |
| 10 | Any CLI version | evaluable | Per-client totals are derivable from `contributions`, which every payload carries. |
| 11 | `--client codex` submitter | codex heals | Full total for the client it covers; other clients absent from `submittedClients`, never rewritten. |
| 12 | `--since` submitter | blocked | Fails range coverage. Safe; heals only once a full-range scan runs. |
| 13 | Backfill payload | **excluded** | Never heals. And CLI heals preserve `origin: "backfill"` entries. |
| 14 | #961 partial `session_model_usage` | that client blocked | Hermes defended, other clients still heal. Needs Phase 2. |
| 15 | Parser regression | blocked | Correctly defended, for the same reason 14 is incorrectly defended. |
| 16 | Client with an active alias fold | **excluded** | Fold heal runs first; timezone heal deferred to avoid burning the floor. |
| 17 | Hidden / moderated user | orthogonal | `leaderboardHidden` affects ranking only. |
| 18 | Alternating TZ daily (VPS / shell-rc) | heals each time | Rows reflect the latest scan instead of ratcheting. Oscillation replaces unbounded inflation. |

Two rows are worth stating plainly as costs rather than burying:

- **State 14 regressed** versus the active-time design. Better per-model
  attribution changes tokens without touching session shape, so an active-time
  gate would have healed #961 for free; the token gate reads that decrease as
  coverage loss. Per-client scoping limits it to the affected client, but #961
  still moves from "free" to "Phase 2". That is the price of measuring the right
  quantity instead of a convenient one.
- **State 12 is newly blocked.** An unscoped design would have healed `--since`
  scans; range coverage forbids it because their totals are not comparable to an
  all-time baseline.

## Phases

### Phase 1 — Heal

No CLI release. Heals states 4, 5, 11, 18.

1. Migration: `submitted_device_client_totals` and
   `daily_breakdown_prereplace_backup`, generated with `drizzle-kit generate`,
   never hand-written. Latest applied is `0021`; this lands as `0022`.
2. Maintain the high-water with `GREATEST` on the conflict arm.
3. `route.ts`: compute per-client payload totals and the device's stored range;
   evaluate the rule; on pass, back up affected rows, rewrite the client's
   entries, and run the zero-out for days absent from the payload.
4. Exclude clients with a fold floor, and payloads with `origin: "backfill"`.
   Preserve `backfill`-tagged entries during rewrite.
5. Restrict writes to rows that actually changed.
6. Everything inside the existing transaction.

Tests, each of which should fail before the change: heals a re-split; **zeroes an
emptied day**; blocks on a token drop; blocks on partial range; heals one client
while another is defended; leaves a fold-floored client alone; preserves a
backfill entry; blocks a backfill payload; device isolation; missing baseline
records without healing.

### Phase 2 — Declare

Restores state 14 and separates it from 15. Requires a CLI release.

1. `scanScope { parserVersions: Record<client, u32> }` on
   `TsTokenContributionData` (`crates/tokscale-cli/src/main.rs:4327`).
2. Extend `SubmissionProvenanceSchema` (`validation/submission.ts:205`) — already
   optional and excluded from `generateSubmissionHash`.
3. Accept a client's token decrease when its parser version changed; defend when
   it did not.

`meta.version` is the CLI version, not a per-client `parser_version`.

### Phase 3 — Compensate (conditional)

Only for state 7. Adds `tzOffsetMinutes` to `scanScope`; accepts a decrease on
day `d` when the declared TZ differs, `d±1` rose by approximately what `d` fell,
and the direction matches the delta.

**Do not build on speculation.** Phase 1's block counts give a direct census of
state 7 — better than #960's active-time ratio proxy, which estimates rather
than counts.

## Complementary CLI fix

Independent of all three phases: have the CLI **pin the bucketing timezone** —
record it in the config directory on first scan and reuse it instead of reading
`chrono::Local` each time, with `tokscale config set timezone` to change it
deliberately.

That removes the re-split at the source, which no server-side change can do; the
heal only cleans up after one. It makes Phase 3 unnecessary for anyone who
upgrades and stabilizes state 18 outright. Existing damage still needs Phase 1.

The trade-off is that a user who relocates keeps bucketing into their old zone
until they change the setting. For historical data that is arguably correct —
day boundaries stay stable — but it is a product decision.

## Known holes

- **#961 is not healed until Phase 2.** Accepted deliberately; see state 14.
- **A block means "the token total dropped", not "the user deleted something."**
  The high-water never falls, so a device stays blocked until it exceeds its own
  peak. Causes differ in duration: genuine deletion is permanent (states 6, 7);
  a collector that does not yet scan a client's new session location is
  temporary (6b) — [#779](https://github.com/junhoyeo/tokscale/issues/779) is the
  worked example, with Codex `archived_sessions` scanned today
  (`crates/tokscale-core/src/scanner.rs:1389-1395`) after a ten-day
  report-to-fix window. The census must report these separately; only the
  permanent kind sizes Phase 3.
- **A user who stopped using a client** keeps that client's stale rows: it is
  absent from `submittedClients`, so it is never rewritten and never zeroed.
- **Cost is rewritten with current pricing.** Rewriting a day recomputes cost
  from the payload, so historical costs shift if pricing changed since. Tokens
  are exact; cost is not.
- **No tolerance band is needed for the token comparison** — a re-split does not
  change a range total at all, so equality is exact. This corrects an earlier
  draft that asked for one.

## Decision needed

Phase 1 makes production usage rows writable downward, and adds a delete path
where none exists. Even with backups that is the riskiest change here, and the
measurement that would size it has not run.

- ship behind a per-user allowlist and validate against one known inflated
  account first, then widen; or
- run #960's diagnostic SQL first and ship broadly once the distribution is known.

The first puts a real correction in front of a real user sooner; the second knows
what it is touching before it touches it.

## How to re-derive

| Claim | Command |
|---|---|
| Guard is per-client; fold heal-floor precedent | `sed -n '154,238p' packages/frontend/src/lib/db/helpers.ts` |
| Range totals are day-agnostic | `sed -n '103,112p' crates/tokscale-core/src/aggregator.rs` |
| Only 11 of 45 parsers set `duration_ms` | `rg -l 'duration_ms:\s*Some\|duration_ms =' crates/tokscale-core/src/sessions/ \| wc -l`; `ls crates/tokscale-core/src/sessions/*.rs \| wc -l` |
| Active time falls back to wall-clock span | `sed -n '126,162p' crates/tokscale-core/src/sessionize.rs` |
| Only payload days are visited | `sed -n '604,672p' packages/frontend/src/app/api/submit/route.ts` |
| No delete path exists | `rg -n '\.delete\(' packages/frontend/src/app/api/submit/route.ts` — expect no `dailyBreakdown` hit |
| Fold writeback restores raw alias keys | `sed -n '632,646p' packages/frontend/src/app/api/submit/route.ts` |
| Backfill origin is per-client | `sed -n '592,602p' packages/frontend/src/app/api/submit/route.ts` |
| `submittedClients` is the scope set | `sed -n '274,282p' packages/frontend/src/app/api/submit/route.ts` |
| `submissions.totalTokens` is derived | `rg -n 'totalTokens' packages/frontend/src/app/api/submit/route.ts` — see `:787` |
| High-water columns use `GREATEST` | `rg -n 'totalActiveTimeMs' packages/frontend/src/app/api/submit/route.ts` — `:412` |
| Latest migration | `ls packages/frontend/src/lib/db/migrations/*.sql \| tail -3` |
| Why the guard exists | `git log -1 d9df8c9c` |
