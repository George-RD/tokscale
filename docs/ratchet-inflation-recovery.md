# Ratchet inflation recovery

Design for [#960](https://github.com/junhoyeo/tokscale/issues/960). Rescanning
the same local history under a different timezone permanently inflates stored
totals, and the monotonic guard that causes it exists to prevent a real
production data loss. This document proposes a correction that heals the
inflation without weakening that protection, and states what happens to every
class of user under the change.

> **Status: proposal. Nothing here is implemented.** No `scanScope`,
> `daily_breakdown_prereplace_backup`, or gate branch exists in the tree. Accept,
> reject, or amend.

Verified against `origin/main` @ `7b2885e1`. Every file:line below was read at
that commit; the "how to re-derive" section says how to recheck each claim.

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

Both failures present to the server as the same signal — *a per-day client total
went down* — and the payload carries nothing that separates them. Any fix that
simply relaxes the guard re-breaks the user `d9df8c9c` was written for.

## The discriminator already exists

`compute_time_metrics` (`sessionize.rs:180`) is:

```rust
let total_active_time_ms: i64 = intervals.iter().map(|s| s.active_duration_ms).sum();
```

A plain sum of interval durations. There is no date bucketing and no `chrono::Local`
anywhere in the function — the only `Local` references in the file are at `:288`
and `:377-379`, both in the daily-bucketing path. It is timezone-invariant by
construction.

That gives the property the guard lacks:

| | `SUM(daily_breakdown)` | `submitted_devices.total_active_time_ms` |
|---|---|---|
| sessions deleted | falls | **falls** |
| timezone re-split | inflates | **unchanged** |
| parser attributes better (#961) | falls | unchanged |

Deletion moves the invariant. Re-splitting does not. So the invariant answers
the question the numbers alone cannot: *is this payload deficient, or are the
stored rows inflated?*

There is a second, equally important property. `time_metrics` and the daily rows
are computed from the **same filtered set** (`crates/tokscale-core/src/lib.rs:2655-2662`):

```rust
let filtered = filter_messages_for_report(all_messages, &options);

let intervals   = sessionize::sessionize(&filtered, DEFAULT_IDLE_GAP_MS);
let time_metrics = compute_time_metrics(&intervals, DEFAULT_IDLE_GAP_MS);
let daily_active_time = compute_daily_active_time(&intervals);
let contributions = aggregator::aggregate_by_date(filtered);
```

So a `--client` or `--since` scan produces a proportionally smaller
`time_metrics` too. The invariant is not merely timezone-invariant, it is
**scope-proportional**: it shrinks whenever coverage shrinks, for any reason.

## The gate

On submit, per device, a payload may **replace** that device's daily rows across
its date range when **both** hold:

1. `payload.timeMetrics.totalActiveTimeMs >= submitted_devices.total_active_time_ms`
2. the payload's client set ⊇ the client set of the stored rows in that date range

Otherwise the current guard applies, unchanged.

Condition 1 establishes that no coverage was lost. Condition 2 closes a hole
condition 1 misses on its own: a user who alternates `--client codex` and
`--client claude` and never submits unfiltered can have a codex-only payload
exceed a codex-derived baseline, and a range-scoped replace would then delete
their claude rows. Requiring the payload to cover at least the clients already
stored makes a partial payload fail before it can erase anything.

Condition 2 is checkable server-side today. It needs no CLI change, which
matters because `summary.clients` is built by folding over the contributions'
own content (`crates/tokscale-core/src/aggregator.rs:121-129`) and so cannot
distinguish `--client codex` from a codex-only user.

Replaced rows are copied to a backup table first, following the precedent
migration `0016` set with `daily_breakdown_premigration_0016_backup`.

## Behavior for every user state

The column that matters is what happens to a user who does nothing differently.

| # | User state | Gate | Outcome |
|---|---|---|---|
| 1 | Never submitted | n/a | Plain insert. No change. |
| 2 | Healthy, single device, stable TZ | passes | Values identical to stored; with the changed-rows-only narrowing this is a no-op beyond new days. |
| 3 | Healthy, multi-device | per-device | Each device gated independently under `UNIQUE(submission_id, submitted_device_id, date)`. No cross-device effect. |
| 4 | **TZ-inflated, sessions intact** | passes | Rows replaced with the correct un-re-split values. **Healed on next submit.** |
| 5 | TZ-inflated, multi-device, some devices clean | per-device | Each device heals as it submits. Partial healing moves monotonically toward truth. |
| 6 | **Deleted local sessions (`d9df8c9c`)** | **fails** (invariant dropped) | Current guard defends. **History preserved exactly as today.** |
| 7 | Deleted sessions *and* changed TZ | fails | Guard defends everything. No loss, but no healing either — this is Phase 3's constituency. |
| 8 | Retired device, never submits again | never runs | Rows frozen as-is. Pre-existing condition, not worsened. |
| 9 | Pre-`0019` device, stored metrics `NULL` | cannot evaluate | Keep the guard and record the baseline. One-submit warm-up, then behaves as its real state. Treating `NULL` as `0` would let any payload pass and is rejected. |
| 10 | Old CLI, never sends `timeMetrics` | cannot evaluate | Guard, indefinitely. `timeMetrics` is `.optional()` (`validation/submission.ts:229`), so nothing breaks. |
| 11 | Habitual `--client` submitter | fails while partial | Scope-proportional metric falls below a full-scope baseline. Never replaced, never healed. Safe. |
| 12 | `--since` submitter | may pass | Replace is already scoped to the payload's date range, so a narrow scan heals a narrow window. Correct, just slower. |
| 13 | Backfill / `tokscale import` user | **excluded** | `provenance.origin === "backfill"` must skip the gate outright. Aggregate imports have no session intervals and must never overwrite live CLI rows. |
| 14 | **#961 parser upgrade (Hermes)** | passes | Sessions unchanged, tokens legitimately lower. Correct values land. **Fixed as a side effect.** |
| 15 | **Parser regression** | passes | Same signature as 14, wrong values land. **This is the known hole** — see below. |
| 16 | Hidden / moderated user | orthogonal | `leaderboardHidden` affects ranking only; the submit path is untouched. |
| 17 | Alternating TZ every day (VPS / shell-rc case) | passes | Rows stop ratcheting and instead reflect whichever split the latest submit used. Each value is internally consistent. Oscillation replaces unbounded inflation. |

State 17 deserves a note. #960's third comment established this is the plausible
high-frequency population: neither the launchd plist nor the systemd unit sets
`TZ`, so autosubmit inherits `/etc/localtime` while a shell-exported `TZ` applies
to manual runs — the same device id alternating zones indefinitely. Under the
gate this becomes day-to-day churn rather than permanent inflation, which is
strictly better but still not stable. See "Complementary CLI fix" below.

## Phases

### Phase 1 — Gate

Ships alone. No CLI release. Heals states 4, 5, 14, and 17.

1. Migration for `daily_breakdown_prereplace_backup`, generated with
   `drizzle-kit generate` and never hand-written. Latest applied is `0021`, so
   this lands as `0022`.
2. `packages/frontend/src/app/api/submit/route.ts`: evaluate the gate before
   merging; on pass, copy affected rows to the backup table and write the
   payload's values directly instead of routing through
   `mergeClientBreakdownsWithRegressionGuard`.
3. Restrict the write to rows whose values actually changed, so a healthy user
   does not rewrite their whole range on every submit.
4. The gate and the replace must share the merge's transaction. A gate evaluated
   outside it races a concurrent submit from another device.

Tests: gate passes → replace; invariant dropped → guard; client-filtered payload
→ guard; alternating-filter payload (condition 2) → guard; `backfill` origin →
guard; `NULL` baseline → guard plus baseline recorded; multi-device isolation.

**Verify before building:** that stored `total_active_time_ms` is the
`GREATEST`-floored high-water mark of a quantity that cannot inflate. The whole
design rests on it, and it is now load-bearing for a write path rather than a
diagnostic.

### Phase 2 — Declare

Closes state 15. Requires a CLI release, so it lands on an adoption curve.

1. CLI: add `scanScope { parserVersions: Record<client, u32> }` to
   `TsTokenContributionData` (`crates/tokscale-cli/src/main.rs:4327`).
2. Extend `SubmissionProvenanceSchema` (`validation/submission.ts:205`), which is
   already optional and already excluded from `generateSubmissionHash`, so older
   clients are unaffected.
3. If the gate passes but client `C`'s tokens fell **and** `C`'s parser version
   is unchanged, defend `C` specifically rather than replacing it.

`meta.version` is the CLI version, not a per-client `parser_version`, and cannot
substitute.

### Phase 3 — Compensate (conditional)

Only for state 7 — devices that fail the gate permanently because their
high-water mark is unreachable after a genuine deletion. Adds
`tzOffsetMinutes` to `scanScope`, then accepts a decrease on day `d` when the
declared TZ differs from the stored one, `d±1` rose by approximately the amount
`d` fell, and the direction matches the declared delta.

**Do not build this on speculation.** Once Phase 1 ships, gate-failure counts
give a direct census of state 7, which is a better measurement than the
active-time ratio proxy in #960's first comment because it counts the affected
population instead of estimating it.

## Complementary CLI fix

Worth considering independently of all three phases: have the CLI **pin the
bucketing timezone** — record it in the config directory on first scan and reuse
it, rather than reading `chrono::Local` every time, with an explicit
`tokscale config set timezone` to change it deliberately.

That removes the re-split at the source, which no server-side change can do; the
gate can only clean up after one. It would make Phase 3 unnecessary for anyone
who upgrades, and would stabilize state 17 outright. Existing damage still needs
Phase 1.

The trade-off is that a user who genuinely relocates keeps bucketing into their
old zone until they change the setting. For historical data that is arguably the
correct behavior — day boundaries stay stable — but it is a product decision.

## Known holes

- **Parser regressions pass the gate until Phase 2 ships.** The backup table is
  the only protection in the interim, which makes it a same-PR requirement for
  Phase 1, not a later refinement.
- **A user who stopped using a client entirely** has a full scan that no longer
  covers a stored client, fails condition 2, and is never healed. Conservative
  and safe; needs Phase 2's explicit scope declaration to resolve.
- **State 7 has no automatic remedy** before Phase 3.
- **Conservation is approximate.** Pricing changes and rounding mean comparisons
  need a tolerance band rather than equality.

## Decision needed

Phase 1 makes production usage rows writable downward on a path that today only
ever grows them. Even with backups that is the riskiest change here, and the
measurement that would size its blast radius has not run.

Two options:

- ship Phase 1 behind a per-user allowlist and validate against one known
  inflated account first, then widen; or
- run #960's diagnostic SQL first and ship broadly once the distribution is known.

The first gets a real correction in front of a real user sooner; the second
knows what it is touching before it touches it.

## How to re-derive

| Claim | Command |
|---|---|
| `compute_time_metrics` is a plain sum | `rg -n -A15 'pub fn compute_time_metrics' crates/tokscale-core/src/sessionize.rs` |
| No `Local` in that function | `rg -n 'Local' crates/tokscale-core/src/sessionize.rs` — expect only `:288`, `:377-379` |
| Metrics and daily rows share `filtered` | `sed -n '2650,2665p' crates/tokscale-core/src/lib.rs` |
| `timeMetrics` and provenance are optional | `sed -n '200,232p' packages/frontend/src/lib/validation/submission.ts` |
| Guard location | `rg -n 'mergeClientBreakdownsWithRegressionGuard' packages/frontend/src/lib/db/helpers.ts` |
| Latest migration | `ls packages/frontend/src/lib/db/migrations/*.sql \| tail -3` |
| Why the guard exists | `git log -1 d9df8c9c` |
