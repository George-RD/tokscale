# Ratchet inflation recovery

Design for [#960](https://github.com/junhoyeo/tokscale/issues/960). Rescanning
the same local history under a different timezone permanently inflates stored
totals, and the monotonic guard that causes it exists to prevent a real
production data loss. This document proposes a correction that heals the
inflation without weakening that protection, and states what happens to every
class of user under the change.

> **Status: proposal. Nothing here is implemented.** No gate branch,
> `total_tokens_reported` column, or backup table exists in the tree. Accept,
> reject, or amend.

Verified against `origin/main` @ `10c88c9d`. Every file:line below was read at
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

## The discriminator: a day-agnostic token total

The re-split moves tokens **between** days. It does not create or destroy them.
So any total taken across all days is invariant to it, while the per-day rows
are not.

The payload already carries exactly that. `calculate_summary`
(`crates/tokscale-core/src/aggregator.rs:107-110`):

```rust
let total_tokens: i64 = contributions
    .iter()
    .map(|c| c.totals.tokens)
    .fold(0i64, i64::saturating_add);
```

A fold over every contribution, with no reference to which day each belongs to.
It has three properties the guard needs, and it has them simultaneously:

1. **Timezone-invariant** — summed across all days, so a re-split cannot change it.
2. **Scope-proportional** — computed from the same `filtered` set as the daily
   rows (`crates/tokscale-core/src/lib.rs:2655-2662`), so a `--client` or
   `--since` scan yields a proportionally smaller total and cannot pass a
   full-scope baseline.
3. **It measures the quantity being protected.** Tokens are what the leaderboard
   ranks and what the profile shows.

`summary` is a required field of `SubmissionDataSchema`
(`packages/frontend/src/lib/validation/submission.ts:91-100`), so every payload
from every CLI version carries it.

| | `SUM(daily_breakdown)` | `summary.totalTokens` |
|---|---|---|
| sessions deleted | falls | **falls** |
| timezone re-split | inflates | **unchanged** |
| parser attributes better (#961) | falls | falls |

### Why not active time

An earlier draft of this document used `submitted_devices.total_active_time_ms`
as the sensor, on the grounds that `compute_time_metrics` (`sessionize.rs:180`)
is a plain sum of interval durations with no date bucketing. That part is true —
it is timezone-invariant, and `schema.ts` documents this in the column comment.

It was still the wrong choice, because it is a **proxy** for token coverage
rather than a measure of it, and the two can move independently:

- Only 11 of 45 session parsers populate `duration_ms`. For the rest,
  `active_duration_ms` falls back to the wall-clock span between messages in a
  block (`sessionize.rs:155-160`), which is a different shape of quantity.
- A session containing a single message contributes zero active time regardless
  of how many tokens it carries.

So a token-coverage loss concentrated in short or single-message sessions is
invisible to an active-time sensor. The gate would pass and overwrite the stored
rows with token-deficient data — the exact failure the gate exists to prevent,
on the exact quantity that matters. Active time is retained below only as an
optional corroborating signal, never as the gate.

## The gate

On submit, per device, a payload may **replace** that device's daily rows across
its date range when **both** hold:

1. `payload.summary.totalTokens >= submitted_devices.total_tokens_reported`
2. the payload's client set ⊇ the client set of the stored rows in that date range

Otherwise the current guard applies, unchanged.

`total_tokens_reported` is a new per-device high-water column, maintained with
`GREATEST` on the conflict arm exactly as the existing metric columns are
(`packages/frontend/src/app/api/submit/route.ts:412`). It is **not** the same as
`submissions.totalTokens`, which is recomputed from the daily rows on every
submit (`route.ts:787`) and is therefore itself inflated — it cannot serve as its
own reference.

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
| 6 | **Deleted local sessions (`d9df8c9c`)** | **fails** (token total dropped) | Current guard defends. **History preserved exactly as today.** |
| 6b | Sessions moved somewhere the collector does not yet scan | fails, temporarily | Same protection as 6, but self-resolving: healing resumes once collector support lands. Not a member of state 7. |
| 7 | Deleted sessions *and* changed TZ | fails | Guard defends everything. No loss, but no healing either — this is Phase 3's constituency. |
| 8 | Retired device, never submits again | never runs | Rows frozen as-is. Pre-existing condition, not worsened. |
| 9 | Device with no stored high-water yet | cannot evaluate | Keep the guard and record the baseline. One-submit warm-up, then behaves as its real state. Treating a missing baseline as `0` would let any payload pass and is rejected. |
| 10 | Any CLI version | evaluable | `summary.totalTokens` is a required field, so unlike a `timeMetrics`-based gate there is no permanently-unevaluable population. |
| 11 | Habitual `--client` submitter | fails while partial | Scope-proportional total falls below a full-scope baseline. Never replaced, never healed. Safe. |
| 12 | `--since` submitter | may pass | Replace is already scoped to the payload's date range, so a narrow scan heals a narrow window. Correct, just slower. |
| 13 | Backfill / `tokscale import` user | **excluded** | `provenance.origin === "backfill"` must skip the gate outright. Aggregate imports must never overwrite live CLI rows. |
| 14 | **#961 parser upgrade (Hermes)** | **fails** | Better attribution genuinely lowers the token total, so the token sensor cannot distinguish it from loss. Needs Phase 2. See below. |
| 15 | Parser regression | fails | Correctly defended, for the same reason 14 is incorrectly defended. |
| 16 | Hidden / moderated user | orthogonal | `leaderboardHidden` affects ranking only; the submit path is untouched. |
| 17 | Alternating TZ every day (VPS / shell-rc case) | passes | Rows stop ratcheting and instead reflect whichever split the latest submit used. Each value is internally consistent. Oscillation replaces unbounded inflation. |

Two rows changed meaning when the sensor moved from active time to tokens, and
the trade is worth stating plainly:

- **State 10 improved.** `timeMetrics` is `.optional()` (`validation/submission.ts:229`),
  so an active-time gate could never evaluate an old CLI. `summary` is required,
  so the token gate always can.
- **State 14 regressed.** An active-time gate would have healed #961's
  constituency for free, because better per-model attribution changes tokens
  without touching session shape. The token gate sees that same drop as coverage
  loss and defends. #961 therefore moves from "fixed as a side effect of Phase 1"
  to "requires Phase 2", which is the honest cost of measuring the right
  quantity instead of a convenient one.

State 17 deserves a note. #960's third comment established this is the plausible
high-frequency population: neither the launchd plist nor the systemd unit sets
`TZ`, so autosubmit inherits `/etc/localtime` while a shell-exported `TZ` applies
to manual runs — the same device id alternating zones indefinitely. Under the
gate this becomes day-to-day churn rather than permanent inflation, which is
strictly better but still not stable. See "Complementary CLI fix" below.

## Phases

### Phase 1 — Gate

Ships alone. No CLI release. Heals states 4, 5, and 17.

1. Migration adding `submitted_devices.total_tokens_reported` and the
   `daily_breakdown_prereplace_backup` table, generated with `drizzle-kit
   generate` and never hand-written. Latest applied is `0021`, so this lands as
   `0022`.
2. Maintain the new column with `GREATEST` on the conflict arm, alongside the
   existing metric columns at `route.ts:412`.
3. `packages/frontend/src/app/api/submit/route.ts`: evaluate the gate before
   merging; on pass, copy affected rows to the backup table and write the
   payload's values directly instead of routing through
   `mergeClientBreakdownsWithRegressionGuard`.
4. Restrict the write to rows whose values actually changed, so a healthy user
   does not rewrite their whole range on every submit.
5. The gate and the replace must share the merge's transaction. A gate evaluated
   outside it races a concurrent submit from another device.

Tests: gate passes → replace; token total dropped → guard; client-filtered
payload → guard; alternating-filter payload (condition 2) → guard; `backfill`
origin → guard; missing baseline → guard plus baseline recorded; multi-device
isolation.

### Phase 2 — Declare

Restores state 14 and separates it from state 15. Requires a CLI release, so it
lands on an adoption curve.

1. CLI: add `scanScope { parserVersions: Record<client, u32> }` to
   `TsTokenContributionData` (`crates/tokscale-cli/src/main.rs:4327`).
2. Extend `SubmissionProvenanceSchema` (`validation/submission.ts:205`), which is
   already optional and already excluded from `generateSubmissionHash`, so older
   clients are unaffected.
3. A token decrease for client `C` is accepted when `C`'s parser version changed,
   and defended when it did not.

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

- **#961's constituency is not healed until Phase 2.** This is a real
  regression against the active-time design and is accepted deliberately; see
  the state-14 note above.
- **A gate failure means "the token total dropped", not "the user deleted
  something."** Because the high-water mark never comes down, a device stays
  blocked until it accumulates enough new usage to exceed its own peak. Causes
  differ in duration: genuine deletion is permanent (states 6, 7), while a
  collector that does not yet scan a client's new session location is temporary
  (state 6b) — [#779](https://github.com/junhoyeo/tokscale/issues/779) is the
  worked example, with Codex's `archived_sessions` now scanned
  (`crates/tokscale-core/src/scanner.rs:1389-1395`) after a ten-day
  report-to-fix window. The Phase 1 census must report these separately; only
  the permanent kind sizes Phase 3.
- **A user who stopped using a client entirely** has a full scan that no longer
  covers a stored client, fails condition 2, and is never healed. Conservative
  and safe; needs Phase 2's explicit scope declaration to resolve.
- **Comparisons need a tolerance band.** Pricing changes and rounding mean
  equality will not hold exactly.

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
| `summary.totalTokens` is day-agnostic | `sed -n '103,112p' crates/tokscale-core/src/aggregator.rs` |
| `summary` is a required payload field | `sed -n '91,100p' packages/frontend/src/lib/validation/submission.ts` |
| Sensor and daily rows share `filtered` | `sed -n '2650,2665p' crates/tokscale-core/src/lib.rs` |
| Only 11 of 45 parsers set `duration_ms` | `rg -l 'duration_ms:\s*Some\|duration_ms =' crates/tokscale-core/src/sessions/ \| wc -l` then `ls crates/tokscale-core/src/sessions/*.rs \| wc -l` |
| Active time falls back to wall-clock span | `sed -n '126,162p' crates/tokscale-core/src/sessionize.rs` |
| Existing high-water columns use `GREATEST` | `rg -n 'totalActiveTimeMs' packages/frontend/src/app/api/submit/route.ts` — expect `GREATEST` at `:412` |
| `submissions.totalTokens` is derived, not stored | `rg -n 'totalTokens' packages/frontend/src/app/api/submit/route.ts` — see `:787` |
| Guard location | `rg -n 'mergeClientBreakdownsWithRegressionGuard' packages/frontend/src/lib/db/helpers.ts` |
| Latest migration | `ls packages/frontend/src/lib/db/migrations/*.sql \| tail -3` |
| Why the guard exists | `git log -1 d9df8c9c` |
