# Ratchet inflation recovery

Design for [#960](https://github.com/junhoyeo/tokscale/issues/960). Rescanning
the same local history under a different timezone permanently inflates stored
totals, and the monotonic guard that causes it exists to prevent a real
production data loss. This document proposes a correction that heals the
inflation without weakening that protection.

> **Status: proposal. Nothing here is implemented.** Accept, reject, or amend.

Verified against `origin/main` @ `1a667579`. Every file:line below was read at
that commit; "How to re-derive" says how to recheck each claim.

## The two failures are one mechanism

`compute_daily_active_time` (`crates/tokscale-core/src/sessionize.rs:285`)
delegates to `compute_daily_active_time_with_timezone(intervals, &chrono::Local)`
at `:288`, so which calendar day a unit of usage lands in is a function of the
machine's timezone at scan time. `mergeClientBreakdownsWithRegressionGuard`
(`packages/frontend/src/lib/db/helpers.ts:154`) then defends every per-day,
per-client decrease. A timezone change moves usage from day `d` to `d-1`; the
guard defends the stale value on `d` and accepts the new value on `d-1`, so the
account is credited twice.

The guard is not a mistake. `d9df8c9c` ("fix(submit): preserve usage history
after local session cleanup") added it after a user deleted local session files,
resubmitted, and had real history erased:

> `Rejected: Replace stored metrics with the newest snapshot | repeats the production data loss caused by local session cleanup`

Both failures present as the same signal — *a per-day client total went down* —
and the payload carries nothing that separates them.

## The fix already exists here, applied to the wrong column

`route.ts:763-774` states the problem and its solution in the tree today:

> Session-shape totals come from the PER-DEVICE high-water marks, not from
> `SUM(daily_breakdown.active_time_ms)`. … (2) Timezone stability: the daily rows
> apportion each interval across LOCAL calendar days, so rescanning the same
> history under a different TZ re-splits it; combined with the monotonic per-day
> merge that permanently inflates `SUM(daily)`. The CLI's `timeMetrics` totals are
> plain sums of interval durations and carry no date bucketing, so they survive a
> TZ change unchanged.

So `totalActiveTimeMs` is derived from `submittedDevices` (`:787-790`) and is
already immune. `totalTokens` is still `SUM(dailyBreakdown.tokens)` (`:751`,
written at `:834`) and is not — only because no per-device token total exists to
derive from.

**Tokens have the same invariant.** A re-split moves tokens between days without
creating or destroying them, so a total taken over any window wider than the
shift is invariant to it. `calculate_summary`
(`crates/tokscale-core/src/aggregator.rs:107-110`) already folds that way for the
whole payload. Which window to store is a separate decision with real
consequences — see "Why month, and not a lifetime total" below.

The plan is therefore ordered by risk, not by ambition: give tokens the
treatment session metrics already have, and only then consider rewriting rows.

### Why not active time as the sensor

An earlier draft gated on `total_active_time_ms`. It is timezone-invariant, but
it is a **proxy** for token coverage rather than a measure of it: only 11 of 45
session parsers populate `duration_ms`, the rest falling back to the wall-clock
span between messages (`sessionize.rs:155-160`), and a single-message session
contributes zero active time regardless of its tokens. A token loss concentrated
in short sessions would be invisible to it.

## Storage

One table, needed by every phase below:

```
submitted_device_client_totals(submitted_device_id, client, origin, month, tokens_highwater, cost_highwater)
  PRIMARY KEY (submitted_device_id, client, origin, month)     -- month as 'YYYY-MM'
```

maintained with `GREATEST` on conflict, exactly as `route.ts:412` does. Buckets
are computed server-side by folding the payload's `contributions` on their
`date` prefix, so no CLI change is required.

### Why month, and not a lifetime total

Granularity is the whole design here, and both extremes are wrong.

A **per-day** high-water is just the daily rows again, and inflates under a
re-split — the original bug.

A **lifetime total** is timezone-proof but silently swallows new work after a
deletion. Concretely: 100 tokens submitted in January, January's session files
deleted, 30 tokens earned in February. The payload now reports 30, so
`GREATEST(100, 30)` holds at 100 and February's work never appears. The account
stays frozen until new usage alone exceeds the old peak.

That is strictly worse than today's behavior, which preserves January's rows and
inserts February's alongside them for a correct 130 — and it lands on the
`d9df8c9c` user, the exact person this protection exists for. "My usage isn't
going up" is a far more visible failure than a total that is too high.

**Monthly buckets keep both properties.** A timezone shift moves usage by at most
one day, so it stays inside its month except at a boundary, leaving monthly sums
invariant. And deleted months hold their high-water while later months grow
independently, so the January/February case sums to 130 correctly.

The residual is one day per month boundary (the 31st↔1st), and only the portion
of it crossing midnight — bounded and small, against today's unbounded
ratcheting. Coarser buckets shrink that residual further but re-introduce the
swallowing problem within a bucket: a yearly bucket breaks the moment someone
deletes January and works in February of the same year.

**`origin` is part of the key, and this is load-bearing.** `getSubmitDevice`
(`:154-168`) falls back to `LEGACY_SUBMIT_DEVICE_KEY` when a payload omits
`device`, so a `tokscale import` backfill and a legacy CLI submit land on the
*same* device row. Keyed only by `(device, client)`, a `GREATEST` high-water
would take the **max** of imported and locally-scanned history instead of their
sum, silently deleting whichever is smaller from the ranked total. Splitting by
origin makes them additive.

`submissions.totalTokens` cannot serve as its own reference: it is recomputed
from the daily rows every submit (`:751`) and is itself inflated.

## Phase 1 — Populate only. No behavior change.

Ship the table and write to it. Change nothing that reads.

This is deliberately inert, and it buys the measurement the rest of the plan is
gated on. After a week of normal submits,

```sql
SUM(daily tokens for that client in that month) / tokens_highwater
```

is an exact per-client, per-device, per-month inflation census — on tokens, the
quantity that matters, rather than the active-time ratio proxy in #960's first
comment. Per-month granularity also shows *when* each account drifted, which a
lifetime ratio cannot.
The measurement stops being a prerequisite and becomes a byproduct of storage
needed anyway. Nothing can regress, because nothing reads it yet.

## Phase 2 — Switch the read path

Change `:751` to derive `totalTokens` (and `totalCost`) from
`SUM(tokens_highwater)` across the user's `(device, client, origin, month)` rows,
mirroring `:787-790`. Summing over months is what makes a deleted month hold its
value while later months keep growing.

**The leaderboard becomes correct immediately** — `getLeaderboard.ts:369,371`
reads `submissions.totalTokens` — with no row rewrite, no delete path, no backup
table, and no gate. The precedent sits twelve lines below the line being changed.

Scope, stated honestly:

| Surface | Source | Fixed by Phase 2? |
|---|---|---|
| Leaderboard, profile total, all-time | `submissions.totalTokens` | **yes** |
| Heatmap, per-day views, weekly/monthly | `daily_breakdown` rows | no — Phase 3 |
| `inputTokens` / `outputTokens` (`:753-754`) | `daily_breakdown` | no — no payload-level invariant exists in `DataSummarySchema` |

Deriving from per-device rows also inherits the additivity property the comment
at `:766-769` cites: two devices reporting 100 and 40 total 140, where a max
would silently drop the second machine.

**Known limit.** All device-less submissions from one user share
`LEGACY_SUBMIT_DEVICE_KEY`, so a legacy user running two machines has both
collapsed into one row and their high-water is a max, not a sum. That is
pre-existing pre-#517 behavior, not a regression introduced here, but Phase 2
makes it visible in the ranked total rather than hidden in the daily merge.

## Phase 3 — Heal the daily rows

Only for the day-level surfaces, and only if the Phase 1 census says the tail
justifies it. This is the risky part, quarantined behind a measurement.

Per device, per client `C`, `C` may be rewritten from the payload when both:

1. **Range coverage** — the payload's contribution dates span at least the
   device's stored date range for `C`. Without it, a `--since` scan's smaller
   total is not comparable to an all-time high-water.
2. **Invariant clears** — for every month the rewrite would touch, the payload's
   token total for `C` in that month is at least the stored high-water for
   `(device, C, "cli", month)`.

Otherwise `C` falls through to the current guard for the months that failed.
Checking per month rather than per lifetime keeps one shrunken month from
blocking every other month's repair, the same reasoning that makes the check
per-client rather than per-payload.

### This is an existing pattern, not new machinery

`foldedClientFloors` (`helpers.ts:158-172`, applied at `:201-217`) already
implements *"a known-inflated stored value may be replaced by a smaller one when
the incoming value clears an invariant lower bound proving the scan was
complete"* — for alias folds:

> nothing proves an incoming submission covers the full day (partial re-parses
> are the exact case the guard exists for), so healing only happens when the
> incoming value is at least the largest single contribution.

Same structure, different axis: fold is client-keys-within-a-day, re-split is
days-within-a-client.

### Per-client, not per-payload

The guard iterates per client (`helpers.ts:183`) and the heal must match. A
payload-level gate fails whole-device whenever any single client legitimately
shrinks — a narrow #961 case would block an unrelated client's repair. Scoping
per client bounds both false accepts and false rejects, and resolves client
filtering for free: `--client codex` reports codex's *full* total.

### Zero, do not delete

The route has **no delete path for `daily_breakdown`**, and adding one is the
largest source of risk in this plan. It is also unnecessary: remove `C`'s entry
from `source_breakdown` and let `recalculateDayTotals` (`helpers.ts:68`)
recompute the row. A day that lands at zero keeps a zero row.

`activeDays` already guards with `COUNT(DISTINCT CASE WHEN tokens > 0 …)`
(`:757`), so a zero row does not inflate it. Other consumers of zero rows must
be checked before shipping.

### The zero-out itself is mandatory

The per-day loop visits only days present in the payload (`:604`), and
contributions come from `aggregate_by_date`, which emits only days with
activity. So a re-split that empties day `d` leaves `d` absent, unvisited, and
stale forever. **Rewriting the day that gained while the day that lost keeps its
old value reproduces the double count exactly** — a heal without the zero-out
repairs nothing. Conditions 1 and 2 are what make it safe to read absence as
zero.

### Assert, then commit

After rewriting `C`, verify `SUM(daily for C over the range) == payload total for
C` inside the transaction, and roll back on mismatch. This converts a silent
corruption on the one code path where silence is most expensive into a caught
error and a preserved account.

### Bound the writes

Touch only days where stored differs from payload. A re-split changes a small
fraction of a long history; rewriting the rest is pure write amplification.

### Interactions that a naive rewrite breaks

**Alias folds.** A preserved fold must keep its raw alias keys (`:638-645`) or
"the heal floor is burned on the first partial resubmit and the double count
re-cements permanently". Simplest safe rule: **a client with a fold floor is
never eligible for this heal** — let the existing mechanism finish first.

**Backfill coexistence.** `origin: "backfill"` is stamped per client inside
`source_breakdown` (`:595-601`) and carried through merges by
`deriveClientBreakdownProvenance` (`helpers.ts:113-126`). A CLI scan cannot see
imported history, so the rewrite must **preserve `backfill`-tagged entries**
rather than treating them as absent. A payload whose own `provenance.origin` is
`backfill` never heals.

**Day-level active time.** `activeTimeMs` has its own monotonic merge
(`:622-630`) and the same inflation. Rewrite it alongside a healed client or it
stays wrong after the tokens are fixed.

**Transaction.** Gate, backup, rewrite, zero-out and assertion all share the
existing transaction, or the gate races a concurrent submit from another device.

## Phase 4 — Declare

Requires a CLI release. Adds `scanScope { parserVersions }` to
`TsTokenContributionData` (`crates/tokscale-cli/src/main.rs:4327`) and extends
`SubmissionProvenanceSchema` (`validation/submission.ts:205`, already optional
and excluded from `generateSubmissionHash`). A client's token decrease is
accepted when its parser version changed and defended when it did not — which is
what separates #961's legitimate re-attribution from a parser regression.

`meta.version` is the CLI version, not a per-client `parser_version`.

## Phase 5 — Compensate (conditional)

Only for devices permanently blocked after a genuine deletion. Adds
`tzOffsetMinutes`; accepts a decrease on day `d` when the declared TZ differs,
`d±1` rose by approximately what `d` fell, and the direction matches. Build only
if the census shows this population is real.

## Complementary CLI fix

Independent of every phase: have the CLI **pin the bucketing timezone** — record
it in the config directory on first scan and reuse it instead of reading
`chrono::Local` each time, with `tokscale config set timezone` to change it
deliberately. That removes the re-split at the source, which no server-side
change can do. It makes Phase 5 unnecessary for anyone who upgrades.

Trade-off: a user who relocates keeps bucketing into their old zone until they
change the setting. For historical data that is arguably correct, but it is a
product decision.

## Behavior for every user state

Phase 2 (P2) fixes ranked totals; Phase 3 (P3) fixes day-level rows.

| # | User state | P2 | P3 | Outcome |
|---|---|---|---|---|
| 1 | Never submitted | n/a | n/a | Plain insert. |
| 2 | Healthy, stable TZ | correct | no-op | Payload equals stored. |
| 3 | Healthy, multi-device | correct, additive | per-device | `UNIQUE(submission_id, submitted_device_id, date)` scopes every write. |
| 4 | **TZ-inflated, sessions intact** | **fixed** | **healed** | Ranked total correct at P2; rows correct at P3. |
| 5 | TZ-inflated, multi-device | fixed | per-device | Each device heals independently. |
| 6 | **Deleted sessions (`d9df8c9c`)** | high-water held | blocked | Protected exactly as today. |
| 6b | Sessions moved where the collector does not scan | held | blocked, temporarily | Self-resolves once support lands. |
| 6c | **Deleted sessions, then kept working** | deleted months hold, later months grow | later months heal | The case monthly bucketing exists for. A lifetime high-water would swallow the new work until it exceeded the old peak. |
| 7 | Deleted sessions *and* changed TZ | held | blocked | No loss, no healing. Phase 5. |
| 8 | Retired device | contributes its peak | never runs | Pre-existing, not worsened. |
| 9 | No high-water yet | falls back to stored | blocked | One-submit warm-up. A missing baseline must not be read as `0`. |
| 10 | Legacy device-less CLI | max, not sum, across machines | n/a | Pre-#517 behavior, now visible rather than hidden. |
| 11 | `--client codex` submitter | correct for codex | codex heals | Other clients absent from `submittedClients`, untouched. |
| 12 | `--since` submitter | correct | blocked | Fails range coverage; heals on the next full scan. |
| 13 | Backfill user | **additive** via `origin` key | excluded | The `origin` key is what stops import and CLI from overwriting each other. |
| 14 | #961 partial `session_model_usage` | correct | that client blocked | Other clients still heal. Phase 4. |
| 15 | Parser regression | held | blocked | Correctly defended. |
| 16 | Client with an active alias fold | correct | excluded | Fold heal runs first. |
| 17 | Hidden / moderated user | orthogonal | orthogonal | `leaderboardHidden` affects ranking only. |
| 18 | Alternating TZ daily | **fixed** | heals each scan | Invariant total is unaffected by the alternation. |

## Known holes

- **`inputTokens` / `outputTokens` stay inflated** after Phase 2 and are only
  fixed by Phase 3. No payload-level invariant exists for them.
- **#961 is not healed until Phase 4.**
- **A Phase 3 block means "the token total dropped", not "the user deleted
  something."** Genuine deletion is permanent (6, 7); a collector lagging a
  client's new session location is temporary (6b) —
  [#779](https://github.com/junhoyeo/tokscale/issues/779) is the worked example,
  with Codex `archived_sessions` scanned today
  (`crates/tokscale-core/src/scanner.rs:1389-1395`) after a ten-day
  report-to-fix window. The census must report these separately.
- **A user who stopped using a client** keeps its stale rows: absent from
  `submittedClients`, never rewritten, never zeroed.
- **Cost is recomputed at current pricing** on rewrite, so historical costs shift
  if pricing changed. Tokens are exact; cost is not.
- **Month boundaries still inflate.** A session crossing midnight on the 1st can
  be counted in two months, so the bucket sums are invariant everywhere except
  there. Bounded at one day per boundary versus today's unbounded ratcheting,
  and Phase 3 repairs it at the row level, but Phase 2 alone does not eliminate
  inflation — it bounds it.
- **No tolerance band is needed within a month** — a re-split does not change a
  monthly total unless it crosses the boundary above, so equality is exact for
  the rest.

## Decision needed

Phases 1 and 2 are low-risk and independently valuable: one writes a table
nothing reads, the other changes a single derivation to match a pattern already
proven on the adjacent columns. Phase 3 is where production rows become writable
downward, and it is now explicitly gated on Phase 1's census rather than shipped
on the assumption that the tail is large.

The remaining question is whether Phase 2 ships broadly or behind a per-user
allowlist validated against one known inflated account first.

## How to re-derive

| Claim | Command |
|---|---|
| Session metrics already avoid `SUM(daily)`, and why | `sed -n '763,791p' packages/frontend/src/app/api/submit/route.ts` |
| `totalTokens` still uses `SUM(daily)` | `rg -n 'totalTokens' packages/frontend/src/app/api/submit/route.ts` — `:751`, written `:834` |
| Leaderboard reads that column | `rg -n 'submissions.totalTokens' packages/frontend/src/lib/leaderboard/getLeaderboard.ts` |
| Device-less submits share a legacy key | `sed -n '154,168p' packages/frontend/src/app/api/submit/route.ts` |
| Range totals are day-agnostic | `sed -n '103,112p' crates/tokscale-core/src/aggregator.rs` |
| Guard is per-client; fold heal-floor precedent | `sed -n '154,238p' packages/frontend/src/lib/db/helpers.ts` |
| Only 11 of 45 parsers set `duration_ms` | `rg -l 'duration_ms:\s*Some\|duration_ms =' crates/tokscale-core/src/sessions/ \| wc -l`; `ls crates/tokscale-core/src/sessions/*.rs \| wc -l` |
| Only payload days are visited | `sed -n '604,672p' packages/frontend/src/app/api/submit/route.ts` |
| No delete path exists | `rg -n '\.delete\(' packages/frontend/src/app/api/submit/route.ts` — expect no `dailyBreakdown` hit |
| `activeDays` ignores zero rows | `sed -n '757p' packages/frontend/src/app/api/submit/route.ts` |
| Fold writeback restores raw alias keys | `sed -n '632,646p' packages/frontend/src/app/api/submit/route.ts` |
| Backfill origin is per-client | `sed -n '592,602p' packages/frontend/src/app/api/submit/route.ts` |
| `submittedClients` is the scope set | `sed -n '274,282p' packages/frontend/src/app/api/submit/route.ts` |
| Why the guard exists | `git log -1 d9df8c9c` |
