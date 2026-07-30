# Ratchet inflation recovery

Design for [#960](https://github.com/junhoyeo/tokscale/issues/960). Rescanning
the same local history under a different timezone permanently inflates stored
totals, and the monotonic guard that causes it exists to prevent a real
production data loss. This document proposes a correction that heals the
inflation without weakening that protection.

> **Status: proposal. Nothing here is implemented.** Accept, reject, or amend.

Verified against `origin/main` @ `96e58fd9`. Every file:line below was read at
that commit; "How to re-derive" says how to recheck each claim. If you are
reading this at a later commit, re-run that section before trusting a line
number — this header went stale once already.

## The two failures are one mechanism

`compute_daily_active_time` (`crates/tokscale-core/src/sessionize.rs:285`)
delegates to `compute_daily_active_time_with_timezone(intervals, &chrono::Local)`
at `:288`, and `timestamp_to_date`
(`crates/tokscale-core/src/sessions/mod.rs:443-444`) does the same for token
attribution. So which calendar day a unit of usage lands in is a function of the
machine's timezone at scan time.

`mergeClientBreakdownsWithRegressionGuard`
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
> merge that permanently inflates `SUM(daily)`.

So `totalActiveTimeMs` is derived from `submittedDevices` (`:787-790`) and is
immune. `totalTokens` is still `SUM(dailyBreakdown.tokens)` (`:751`, written at
`:834`) and is not — only because no per-device token total exists to derive
from. Tokens have the same invariant: a re-split moves them between days without
creating or destroying them, so a total over a window wider than the shift is
invariant to it.

**Which window is not a detail.** It is the central design decision, and the
next section is the honest accounting of what each choice costs.

### Why not active time as the sensor

An earlier draft gated on `total_active_time_ms`. It is timezone-invariant, but
it is a **proxy** for token coverage rather than a measure of it: only 11 of 45
session parsers populate `duration_ms`, the rest falling back to the wall-clock
span between messages (`sessionize.rs:155-160`), and a single-message session
contributes zero active time regardless of its tokens.

## Bucket width: a bounded-error trade-off, not an exact fix

Any bucket keyed on a **local** date inherits the instability that causes the
bug. Widening the bucket reduces the error; it never removes it. Two errors
trade against each other, and they are not symmetric in cost.

**Boundary leak.** Offsets span UTC−12 to UTC+14, so shifted instants differ by
up to 26 hours and a date can move by up to **two** calendar days in the extreme
(`2026-01-01T10:00Z` is Jan 2 in UTC+14 and Dec 31 in UTC−12); one day is typical
for zone pairs one user alternates between. At each bucket boundary that sliver
is counted in both buckets, permanently.

**Swallowing.** Inside a bucket, a deletion freezes it. 100 tokens submitted,
sessions deleted, 30 tokens earned afterwards in the same bucket: the payload
reports 30, `GREATEST` holds 100, and the new work is invisible until it exceeds
the old peak on its own. Today's per-day rows get this right — January preserved,
February inserted alongside, total 130 — so a bucket that is too wide is a
**regression against current behavior**, landing on the `d9df8c9c` user.

| width | boundaries/year | swallow window |
|---|---|---|
| daily | 365 | none |
| weekly | 52 | ≤ 7 days |
| monthly | 12 | ≤ 31 days |
| yearly | 1 | ≤ 366 days |

A boundary leaks only the midnight-crossing sliver of one or two days. Swallowing
hides all of a user's recent work and is immediately visible as "my number
stopped moving". **The costs are lopsided toward preferring narrower buckets**,
which is the opposite of what the previous draft assumed when it picked monthly.

Rather than settle this by intuition, Phase 1 measures it.

## Storage

```
submitted_device_client_totals(
  submitted_device_id, client, origin, bucket_width, bucket_key,
  tokens_highwater, cost_highwater)
PRIMARY KEY (submitted_device_id, client, origin, bucket_width, bucket_key)
```

maintained with `GREATEST` on conflict, exactly as `route.ts:412` does. Buckets
are folded server-side from the payload's `contributions` on their `date`, so no
CLI change is required.

`bucket_width` lets Phase 1 record `week` and `month` side by side and retire the
loser. `bucket_key` is a stable string (`YYYY-MM`, or ISO `YYYY-Www`).

**`origin` is part of the key, and this is load-bearing.** `getSubmitDevice`
(`:154-168`) falls back to `LEGACY_SUBMIT_DEVICE_KEY` when a payload omits
`device`, so a `tokscale import` backfill and a legacy CLI submit land on the
*same* device row. Keyed without origin, `GREATEST` would take the **max** of
imported and locally-scanned history instead of their sum, silently dropping
whichever is smaller from the ranked total.

`submissions.totalTokens` cannot serve as its own reference: it is recomputed
from the daily rows every submit (`:751`) and is itself inflated.

## Phase 1 — Populate only. No behavior change.

Write the table at both widths. Change nothing that reads.

Inert by design, and it converts two guesses into measurements:

```sql
-- per client, per device, per bucket
SUM(daily tokens in that bucket) / tokens_highwater
```

- **How much inflation exists, and when** — a per-bucket ratio shows which
  periods drifted, which a lifetime ratio cannot. This is the census every later
  phase is gated on, and it is on tokens rather than the active-time proxy in
  #960's first comment.
- **What the boundary leak actually costs** — comparing the weekly and monthly
  reconstructions of the same account measures the leak directly, settling the
  width empirically instead of by argument.

Nothing can regress, because nothing reads it yet.

**One thing the census cannot see.** `ClientId::OpenCodeReview` declares
`submit_default: true` (`crates/tokscale-core/src/clients.rs:509-517`) but is
parsed only by `parse_local_clients` (`lib.rs:3239`), which the submit path never
calls — `message_cache.rs:853-855` states this outright. Its usage appears in
`tokscale report` and has never reached the server, so it is absent from the
baseline entirely. When that gap is fixed, affected accounts will show a step
increase that is real usage arriving late, not drift. Audited against the whole
registry: of 37 `submit_default` clients it is the only one missing (Goose,
Hermes and Kilo appear absent from a naive `ClientId::` grep but are handled at
`:1171`, `:1161` and via `ClientId::KiloCode` at `:1087`).

## Phase 2 — Switch the read path

Change `:751` to derive `totalTokens` and `totalCost` from
`SUM(tokens_highwater)` over the winning `bucket_width`, mirroring `:787-790`.

**The leaderboard becomes correct immediately** — `getLeaderboard.ts:369,371,396`
reads `submissions.totalTokens` — with no row rewrite, no delete path, no backup
table and no gate. The precedent sits twelve lines below the line being changed.

**This phase can zero out real accounts if shipped naively, and that is its one
sharp edge.** A user's high-water rows only exist once they have submitted since
Phase 1 landed. Switch the derivation before that and `SUM` over zero rows
returns 0 — the account's ranked total collapses to nothing. The damage is
display-only, since `daily_breakdown` is untouched and the next submit repairs
it, but it is a visible, alarming, self-inflicted outage for a user who did
nothing.

Two acceptable guards, in preference order:

1. **Coverage gate.** Do not switch until Phase 1 shows high-water rows covering
   every device that has submitted recently. Phase 1 already measures exactly
   this, so the gate costs nothing extra.
2. **Per-user fallback.** Derive from the high-water table only when the user has
   rows spanning their stored date range; otherwise fall back to
   `SUM(daily_breakdown)` for that user. Correct but leaves two code paths live,
   and the fallback path stays inflated — so it must be temporary, not permanent.

`COALESCE(SUM(highwater), SUM(daily))` looks like a third option and is not one:
a user with *partial* coverage returns a non-null but too-small sum, which
coalesce will happily accept.

| Surface | Source | Fixed by Phase 2? |
|---|---|---|
| Leaderboard, profile total, all-time | `submissions.totalTokens` | **yes, to within the boundary leak** |
| Heatmap, per-day views, weekly/monthly | `daily_breakdown` rows | no — Phase 4 |
| `inputTokens` / `outputTokens` (`:753-754`) | `daily_breakdown` | no — no payload-level invariant exists |

Deriving per-device also inherits the additivity the comment at `:766-769`
cites: two devices reporting 100 and 40 total 140, where a max would drop the
second machine.

**Known limit.** All device-less submissions share `LEGACY_SUBMIT_DEVICE_KEY`, so
a legacy user with two machines has both collapsed into one row and their
high-water is a max, not a sum. Pre-#517 behavior, not introduced here, but
Phase 2 makes it visible in the ranked total rather than hidden in the daily
merge.

## Phase 3 — Pin the bucket key (the only exact fix)

**The numbering here is misleading and worth saying plainly: this is not third in
sequence.** It is a CLI change with no server dependency, so it can and probably
should ship in parallel with Phases 1 and 2 rather than after them. It is also
the only item that stops the damage from growing while everything else is being
decided. Read the phases as two tracks — server (1, 2, 4) and client (3, 5) —
that meet only at Phase 4.

Every error above exists because the bucket key is derived from a mutable input.
Have the CLI **record its bucketing timezone in the config directory on first
scan and reuse it**, instead of reading `chrono::Local` each time, with
`tokscale config set timezone` to change it deliberately.

That makes local dates stable, which is qualitatively different from making them
coarser:

- **the boundary leak goes to zero** — no re-split ever happens, at any width;
- **swallowing goes to zero** — buckets can safely narrow to daily once a device
  reports a pinned zone, because per-day keys are now stable;
- **it removes the cause**, which no server-side change can do. Phases 1, 2 and 4
  only clean up after a re-split has already happened.

Requires a CLI release, so it lands on an adoption curve, and it does not repair
existing damage. Devices that report a pinned zone can be moved to daily buckets
individually, so the benefit arrives per-device as users upgrade rather than
waiting on full adoption.

Trade-off: a user who relocates keeps bucketing into their old zone until they
change the setting. For historical data that is arguably correct — day boundaries
stay stable — but it is a product decision.

## Phase 4 — Heal the daily rows

For the day-level surfaces, and only if Phase 1's census says the tail justifies
it. This is the risky part, quarantined behind a measurement.

Per device, per client `C`, `C` may be rewritten from the payload when both:

1. **Range coverage** — the payload's contribution dates span at least the
   device's stored date range for `C`. Without it a `--since` scan's smaller
   total is not comparable to an all-time high-water.
2. **Invariant clears** — for every bucket the rewrite would touch, the payload's
   token total for `C` in that bucket is at least the stored high-water.

Otherwise `C` falls through to the current guard for the buckets that failed.
Checking per bucket, like checking per client, keeps one shrunken period from
blocking every other period's repair.

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

### Zero, do not delete

The route has **no delete path for `daily_breakdown`**, and adding one is the
largest source of risk here. It is also unnecessary: remove `C`'s entry from
`source_breakdown` and let `recalculateDayTotals` (`helpers.ts:68`) recompute. A
day that lands at zero keeps a zero row. `activeDays` already guards with
`COUNT(DISTINCT CASE WHEN tokens > 0 …)` (`:757`), so a zero row does not inflate
it; other consumers must be checked before shipping.

### The zero-out itself is mandatory

The per-day loop visits only days present in the payload (`:604`), and
contributions come from `aggregate_by_date`, which emits only days with activity.
A re-split that empties day `d` leaves `d` absent, unvisited and stale forever.
**Rewriting the day that gained while the day that lost keeps its old value
reproduces the double count exactly** — a heal without the zero-out repairs
nothing.

### Assert, then commit

After rewriting `C`, verify `SUM(daily for C over the range) == payload total for
C` inside the transaction and roll back on mismatch. This turns a silent
corruption on the one path where silence is most expensive into a caught error
and a preserved account.

### Bound the writes

Touch only days where stored differs from payload. A re-split changes a small
fraction of a long history.

### Interactions a naive rewrite breaks

**Alias folds.** A preserved fold must keep its raw alias keys (`:638-645`) or
"the heal floor is burned on the first partial resubmit and the double count
re-cements permanently". Safe rule: **a client with a fold floor is never
eligible for this heal.**

That rule is now doing more work than originally intended, because the fold path
has a live aliasing defect underneath it. `:94` copies a client with
`{ ...data, models: { ...data.models } }` — a new map, but the model *value
objects* remain shared with `rawExistingBreakdown`. `:107` then calls
`mergeModelBreakdowns`, which at `:48-54` does `existingModel.tokens += …`
**in place**, mutating the stored raw object; `:640-643` writes that mutated
object back as the supposedly-raw alias key. Each submit folds the already-folded
values again, so the nested `models` map compounds without bound. Client-level
`tokens` escapes because `{ ...data }` copied the scalar, which is why day totals
and the leaderboard look correct while per-model views drift.

This is a separate bug and should be fixed on its own (deep-copy the model
values). It is recorded here because it means fold-affected rows are actively
corrupting while unhealed, so "exclude them" is a deferral, not a resolution.

**Backfill coexistence.** `origin: "backfill"` is stamped per client inside
`source_breakdown` (`:595-601`) and carried through merges by
`deriveClientBreakdownProvenance` (`helpers.ts:113-126`). A CLI scan cannot see
imported history, so the rewrite must **preserve `backfill`-tagged entries**. A
payload whose own `provenance.origin` is `backfill` never heals.

**Day-level active time is ratcheted at TWO enforcement points, and the second
one silently cancels the heal.** `mergeActiveTimeMs` (`:178-185`) is a
`Math.max` in the JS merge, and `:707` mirrors it in SQL:

```sql
active_time_ms = GREATEST(daily_breakdown.active_time_ms, EXCLUDED.active_time_ms),
```

whose own comment says the arm "must not be a hole in the monotonic guard the
in-memory merge path applies." A heal writes a *lower* value. Routed through
that `ON CONFLICT` arm, `GREATEST(stale_high, healed_low)` returns the stale
value — **the heal reports success and changes nothing.** No error, no warning;
the post-rewrite assertion above is the only thing that would catch it. Both
enforcement points must be bypassed, not just the JS one.

**Transaction.** Gate, backup, rewrite, zero-out and assertion share the existing
transaction. Note this is already stronger than it looks: `:303-314` takes
`.for('update')` on the `submissions` row, which is unique per user
(`submissions_user_id_unique`), so every submit for one user is serialized —
including across devices. A second device cannot commit between the merge's read
and its write, so the gate is never computed against stale data. An audit pass
initially concluded otherwise; the lock is what makes it safe.

## Phase 5 — Declare

Requires a CLI release. Adds `scanScope { parserVersions }` to
`TsTokenContributionData` (`crates/tokscale-cli/src/main.rs:4327`) and extends
`SubmissionProvenanceSchema` (`validation/submission.ts:205`, already optional and
excluded from `generateSubmissionHash`). A client's token decrease is accepted
when its parser version changed and defended when it did not — separating #961's
legitimate re-attribution from a parser regression.

`meta.version` is the CLI version, not a per-client `parser_version`.

## Phase 6 — Compensate (conditional)

Only for devices permanently blocked after a genuine deletion. Adds
`tzOffsetMinutes`; accepts a decrease on day `d` when the declared TZ differs,
`d±1` rose by approximately what `d` fell, and the direction matches. Largely
obviated by Phase 3 for anyone who upgrades. Build only if the census shows the
population is real.

## Behavior for every user state

P2 fixes ranked totals; P3 removes the cause; P4 fixes day-level rows.

| # | User state | P2 | P4 | Outcome |
|---|---|---|---|---|
| 1 | Never submitted | n/a | n/a | Plain insert. |
| 2 | Healthy, stable TZ | correct | no-op | Payload equals stored. |
| 3 | Healthy, multi-device | correct, additive | per-device | `UNIQUE(submission_id, submitted_device_id, date)` scopes every write. |
| 4 | **TZ-inflated, sessions intact** | **fixed to within the boundary leak** | **healed** | P3 stops it recurring. |
| 5 | TZ-inflated, multi-device | fixed | per-device | Each device heals independently. |
| 6 | **Deleted sessions (`d9df8c9c`)** | high-water held | blocked | Protected exactly as today. |
| 6b | Sessions moved where the collector does not scan | held | blocked, temporarily | Self-resolves once support lands. |
| 6c | **Deleted sessions, then kept working** | earlier buckets hold, later buckets grow | later buckets heal | Bucket width bounds how long new work stays invisible. Narrower is better here. |
| 7 | Deleted sessions *and* changed TZ | held | blocked | No loss, no healing. Phase 6. |
| 8 | Retired device | contributes its peak | never runs | Pre-existing, not worsened. |
| 9 | No high-water yet | falls back to stored | blocked | One-submit warm-up. A missing baseline must not be read as `0`. |
| 10 | Legacy device-less CLI | max, not sum, across machines | n/a | Pre-#517 behavior, now visible rather than hidden. |
| 11 | `--client codex` submitter | correct for codex | codex heals | Other clients absent from `submittedClients`, untouched. |
| 12 | `--since` submitter | correct | blocked | Fails range coverage; heals on the next full scan. |
| 13 | Backfill user | **additive** via `origin` | excluded | The `origin` key stops import and CLI overwriting each other. |
| 14 | #961 partial `session_model_usage` | correct | that client blocked | Others still heal. Phase 5. |
| 15 | Parser regression | held | blocked | Correctly defended. |
| 16 | Client with an active alias fold | correct | excluded | Fold heal runs first. |
| 17 | Hidden / moderated user | orthogonal | orthogonal | `leaderboardHidden` affects ranking only. |
| 18 | Alternating TZ daily | fixed to within the leak | heals each scan | P3 eliminates it at the source. |

## Known holes

- **Phase 2 bounds inflation, it does not eliminate it.** The boundary leak
  survives until Phase 3 pins the key or Phase 4 repairs the rows.
- **Filtered period leaderboards over-count independently of all of this.**
  `getLeaderboard.ts:207-233` filters whole daily *rows* and the aggregation then
  adds `row.tokens` (`:158`), the row's total across every client — so
  `client:codex` counts other clients that shared a day and device. The all-time
  path filters per user via `sources_used` (`:374-379`), a different semantic
  again. Phase 2 does not touch this, and fixing the totals it reads will not fix
  it. Listed so nobody validates Phase 2 against a filtered board and concludes
  the derivation is wrong.
- **Swallowing survives inside one bucket.** Bounded by the chosen width; zero
  only after Phase 3 permits daily buckets.
- **`inputTokens` / `outputTokens` stay inflated** until Phase 4. No
  payload-level invariant exists for them.
- **#961 is not healed until Phase 5.**
- **A Phase 4 block means "the token total dropped", not "the user deleted
  something."** Genuine deletion is permanent (6, 7); a collector lagging a
  client's new session location is temporary (6b) —
  [#779](https://github.com/junhoyeo/tokscale/issues/779) is the worked example,
  Codex `archived_sessions` scanned today (`scanner.rs:1389-1395`) after a
  ten-day report-to-fix window. The census must report these separately.
- **A user who stopped using a client** keeps its stale rows: absent from
  `submittedClients`, never rewritten, never zeroed.
- **Cost is recomputed at current pricing** on rewrite, so historical costs shift
  if pricing changed. Tokens are exact; cost is not.
- **Stale comment.** `schema.ts` describes `dailyBreakdown.timestampMs` as "the
  earliest message in this **UTC** day bucket". The bucket is local
  (`sessions/mod.rs:443-444`). Worth correcting — a wrong comment about which
  timezone a bucket uses is precisely the trap this whole issue came from.

## Decision needed

Phases 1 and 2 are low-risk and independently valuable: one writes a table
nothing reads, the other changes a single derivation to match a pattern already
proven on adjacent columns. Phase 3 is the only change that removes the cause and
should be weighed earlier than its number suggests. Phase 4 is where production
rows become writable downward, and it is gated on Phase 1's census.

Open: whether Phase 2 ships broadly or behind a per-user allowlist validated
against one known inflated account first.

## How to re-derive

| Claim | Command |
|---|---|
| Session metrics already avoid `SUM(daily)`, and why | `sed -n '763,791p' packages/frontend/src/app/api/submit/route.ts` |
| `totalTokens` still uses `SUM(daily)` | `rg -n 'totalTokens' packages/frontend/src/app/api/submit/route.ts` — `:751`, written `:834` |
| Leaderboard reads that column | `rg -n 'submissions.totalTokens' packages/frontend/src/lib/leaderboard/getLeaderboard.ts` |
| Contribution dates are local, not UTC | `sed -n '443,445p' crates/tokscale-core/src/sessions/mod.rs` |
| Device-less submits share a legacy key | `sed -n '154,168p' packages/frontend/src/app/api/submit/route.ts` |
| Guard is per-client; fold heal-floor precedent | `sed -n '154,238p' packages/frontend/src/lib/db/helpers.ts` |
| Only 11 of 45 parsers set `duration_ms` | `rg -l 'duration_ms:\s*Some\|duration_ms =' crates/tokscale-core/src/sessions/ \| wc -l`; `ls crates/tokscale-core/src/sessions/*.rs \| wc -l` |
| Only payload days are visited | `sed -n '604,672p' packages/frontend/src/app/api/submit/route.ts` |
| No delete path exists | `rg -n '\.delete\(' packages/frontend/src/app/api/submit/route.ts` — expect no `dailyBreakdown` hit |
| `activeDays` ignores zero rows | `sed -n '757p' packages/frontend/src/app/api/submit/route.ts` |
| Fold writeback restores raw alias keys | `sed -n '632,646p' packages/frontend/src/app/api/submit/route.ts` |
| Backfill origin is per-client | `sed -n '592,602p' packages/frontend/src/app/api/submit/route.ts` |
| `submittedClients` is the scope set | `sed -n '274,282p' packages/frontend/src/app/api/submit/route.ts` |
| Why the guard exists | `git log -1 d9df8c9c` |
