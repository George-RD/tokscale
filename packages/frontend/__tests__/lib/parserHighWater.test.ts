import { describe, expect, it } from "vitest";
import {
  addClientBreakdownIncrement,
  foldParserClientSnapshot,
  planParserHighWaterSubmission,
  type ParserClientHighWaterState,
} from "../../src/lib/db/parserHighWater";
import { recalculateDayTotals, type ClientBreakdownData } from "../../src/lib/db/helpers";

function contribution(
  date: string,
  tokens: number,
  modelId = "model-a",
  cost = tokens / 10
) {
  return {
    date,
    clients: [
      {
        client: "copilot",
        modelId,
        tokens: {
          input: tokens,
          output: 0,
          cacheRead: 0,
          cacheWrite: 0,
          reasoning: 0,
        },
        cost,
        messages: tokens > 0 ? 1 : 0,
      },
    ],
  };
}

function snapshot(...rows: ReturnType<typeof contribution>[]) {
  return foldParserClientSnapshot(rows, "copilot");
}

function legacy(...rows: ReturnType<typeof contribution>[]) {
  return snapshot(...rows);
}

function baseline(
  existingLegacyDays: Record<string, ClientBreakdownData>,
  incomingDays: Record<string, ClientBreakdownData>
) {
  return planParserHighWaterSubmission({
    client: "copilot",
    incomingVersion: 2,
    fullHistory: true,
    existingLegacyDays,
    incomingDays,
  });
}

function next(
  state: ParserClientHighWaterState,
  incomingDays: Record<string, ClientBreakdownData>
) {
  return planParserHighWaterSubmission({
    client: "copilot",
    incomingVersion: 2,
    fullHistory: true,
    existingLegacyDays: {},
    incomingDays,
    state,
  });
}

describe("non-destructive parser generation high-water", () => {
  it("treats prototype-named model IDs as ordinary untrusted keys", () => {
    const first = baseline(
      {},
      snapshot(contribution("2026-07-01", 100, "__proto__", 10))
    );
    const plan = next(
      first.nextState!,
      snapshot(contribution("2026-07-01", 150, "__proto__", 15))
    );

    const model = plan.increments["2026-07-01"].models["__proto__"];
    expect(Object.getPrototypeOf(plan.increments["2026-07-01"].models)).toBeNull();
    expect(model).toMatchObject({ input: 50, tokens: 50, cost: 5 });
    expect((Object.prototype as { input?: number }).input).toBeUndefined();
  });

  it("preserves legacy rows and records the first v2 full snapshot as a no-add baseline", () => {
    const plan = baseline(
      legacy(contribution("2026-07-01", 100)),
      snapshot(contribution("2026-07-02", 100))
    );

    expect(plan.mode).toBe("baseline-legacy");
    expect(plan.increments).toEqual({});
    expect(plan.nextState?.version).toBe(2);
    expect(plan.nextState?.aggregate.tokens).toBe(100);
  });

  it("is idempotent when the same v2 full snapshot is replayed", () => {
    const first = baseline({}, snapshot(contribution("2026-07-01", 100)));
    const replay = next(
      first.nextState!,
      snapshot(contribution("2026-07-01", 100))
    );

    expect(replay.increments).toEqual({});
    expect(replay.nextState).toEqual(first.nextState);
  });

  it("does not add a later creation-to-shutdown date move with no aggregate growth", () => {
    const first = baseline(
      legacy(contribution("2026-07-01", 100)),
      snapshot(contribution("2026-07-01", 100))
    );
    const plan = next(
      first.nextState!,
      snapshot(contribution("2026-07-02", 100))
    );

    expect(plan.mode).toBe("incremental");
    expect(plan.increments).toEqual({});
  });

  it("does not spend unrelated new usage to delete or replace locally deleted history", () => {
    const first = baseline(
      legacy(contribution("2026-06-01", 100, "legacy-model", 5)),
      snapshot(contribution("2026-06-01", 100, "legacy-model", 5))
    );
    const plan = next(
      first.nextState!,
      snapshot(contribution("2026-07-02", 100, "different-model", 20))
    );

    // The full local snapshot lost 100 old tokens and gained 100 unrelated
    // tokens. Aggregate growth is zero, so neither model nor the higher cost
    // can be added and the stored legacy row is never touched.
    expect(plan.increments).toEqual({});
    expect(plan.nextState?.aggregate.tokens).toBe(100);
  });

  it("caps mixed deletion plus new work to net cumulative growth", () => {
    const first = baseline(
      legacy(contribution("2026-06-01", 100, "legacy-model", 5)),
      snapshot(contribution("2026-06-01", 100, "legacy-model", 5))
    );
    const plan = next(
      first.nextState!,
      snapshot(contribution("2026-07-02", 200, "different-model", 25))
    );
    const increment = plan.increments["2026-07-02"];

    expect(increment.tokens).toBe(100);
    expect(increment.input).toBe(100);
    expect(increment.cost).toBe(12.5);
    expect(increment.models["different-model"]).toMatchObject({
      tokens: 100,
      input: 100,
      cost: 12.5,
    });
  });

  it("does not treat repricing without token growth as new spend", () => {
    const first = baseline(
      legacy(contribution("2026-07-01", 100, "model-a", 5)),
      snapshot(contribution("2026-07-01", 100, "model-a", 5))
    );
    const repriced = next(
      first.nextState!,
      snapshot(contribution("2026-07-01", 100, "model-a", 50))
    );

    expect(repriced.increments).toEqual({});
  });

  it("allocates a bounded mixed move plus growth to the newest positive cell", () => {
    const first = baseline(
      legacy(contribution("2026-07-01", 100, "model-a", 10)),
      snapshot(contribution("2026-07-01", 100, "model-a", 10))
    );
    const plan = next(
      first.nextState!,
      snapshot(
        contribution("2026-07-02", 100, "model-a", 10),
        contribution("2026-07-03", 50, "model-b", 8)
      )
    );

    expect(plan.increments["2026-07-02"]).toBeUndefined();
    expect(plan.increments["2026-07-03"].tokens).toBe(50);
    expect(plan.increments["2026-07-03"].cost).toBe(8);
  });

  it("does not count a pure model rename as new work", () => {
    const first = baseline(
      legacy(contribution("2026-07-01", 100, "old-name", 10)),
      snapshot(contribution("2026-07-01", 100, "old-name", 10))
    );
    const plan = next(
      first.nextState!,
      snapshot(contribution("2026-07-01", 100, "new-name", 10))
    );

    expect(plan.increments).toEqual({});
  });

  it("quantizes pro-rated model, client JSON, and scalar cost coherently", () => {
    const initialDays = snapshot(contribution("2026-07-01", 2, "model-a", 1));
    const first = baseline({}, initialDays);
    const plan = next(
      first.nextState!,
      snapshot(contribution("2026-07-01", 3, "model-a", 1))
    );
    const increment = plan.increments["2026-07-01"];
    const stored = addClientBreakdownIncrement(
      initialDays["2026-07-01"],
      increment
    );
    const totals = recalculateDayTotals({ copilot: stored });

    expect(increment.models["model-a"].cost).toBe(0.3333);
    expect(increment.cost).toBe(0.3333);
    expect(stored.models["model-a"].cost).toBe(1.3333);
    expect(stored.cost).toBe(1.3333);
    expect(totals.cost.toFixed(4)).toBe("1.3333");
  });

  it("adds only post-baseline cumulative growth and keeps representations coherent", () => {
    const first = baseline(
      legacy(contribution("2026-07-01", 100, "model-a", 10)),
      snapshot(contribution("2026-07-01", 100, "model-a", 10))
    );
    const plan = next(
      first.nextState!,
      snapshot(contribution("2026-07-01", 100, "model-a", 10), contribution("2026-07-02", 50, "model-b", 7))
    );
    const increment = plan.increments["2026-07-02"];
    const stored = addClientBreakdownIncrement(undefined, increment);
    const totals = recalculateDayTotals({ copilot: stored });

    expect(stored.tokens).toBe(50);
    expect(stored.cost).toBe(7);
    expect(stored.models["model-b"].tokens).toBe(50);
    expect(totals).toMatchObject({ tokens: 50, cost: 7, inputTokens: 50 });
  });

  it("anchors a missing first v2 history snapshot to the larger stored legacy total", () => {
    const first = baseline(
      legacy(contribution("2026-06-01", 100, "model-a", 10)),
      {}
    );
    const restored = next(
      first.nextState!,
      snapshot(contribution("2026-06-01", 100, "model-a", 10))
    );

    expect(first.nextState?.aggregate.tokens).toBe(100);
    expect(restored.increments).toEqual({});
  });

  it("keeps parser identity on partial scans but freezes their unsafe changes after transition", () => {
    const first = baseline({}, snapshot(contribution("2026-07-01", 100)));
    const partial = planParserHighWaterSubmission({
      client: "copilot",
      incomingVersion: 2,
      fullHistory: false,
      existingLegacyDays: {},
      incomingDays: snapshot(contribution("2026-07-02", 100)),
      state: first.nextState,
    });

    expect(partial.mode).toBe("freeze");
    expect(partial.increments).toEqual({});
  });

  it("freezes a partial v2 scan when legacy Copilot history already exists", () => {
    const plan = planParserHighWaterSubmission({
      client: "copilot",
      incomingVersion: 2,
      fullHistory: false,
      existingLegacyDays: legacy(contribution("2026-07-01", 100)),
      incomingDays: snapshot(contribution("2026-07-02", 100)),
    });

    expect(plan.mode).toBe("freeze");
  });

  it("keeps old CLI status quo before transition and freezes it afterward", () => {
    const before = planParserHighWaterSubmission({
      client: "copilot",
      fullHistory: false,
      existingLegacyDays: {},
      incomingDays: snapshot(contribution("2026-07-01", 100)),
    });
    const first = baseline({}, snapshot(contribution("2026-07-01", 100)));
    const after = planParserHighWaterSubmission({
      client: "copilot",
      fullHistory: false,
      existingLegacyDays: {},
      incomingDays: snapshot(contribution("2026-07-02", 100)),
      state: first.nextState,
    });

    expect(before.mode).toBe("status-quo");
    expect(after.mode).toBe("freeze");
  });

  it.each([1, 3, 999])("freezes lower or unsupported Copilot generation %s", (version) => {
    const plan = planParserHighWaterSubmission({
      client: "copilot",
      incomingVersion: version,
      fullHistory: true,
      existingLegacyDays: legacy(contribution("2026-07-01", 100)),
      incomingDays: snapshot(contribution("2026-07-02", 200)),
    });

    expect(plan.mode).toBe("freeze");
    expect(plan.nextState).toBeUndefined();
  });

  it("never credits more than lifetime high-water growth across moves, deletions, and replays", () => {
    const first = baseline(
      legacy(contribution("2026-07-01", 100, "model-a", 10)),
      snapshot(contribution("2026-07-01", 100, "model-a", 10))
    );
    let state = first.nextState!;
    let credited = 0;
    let lifetimePeak = 100;
    let seed = 0x1032;

    for (let index = 0; index < 100; index += 1) {
      seed = (seed * 1664525 + 1013904223) >>> 0;
      const reportedTotal = seed % 260;
      const date = `2026-07-${String((seed % 4) + 1).padStart(2, "0")}`;
      const modelId = seed % 3 === 0 ? "renamed-model" : "model-a";
      const plan = next(
        state,
        reportedTotal === 0
          ? {}
          : snapshot(contribution(date, reportedTotal, modelId, reportedTotal / 7))
      );
      const added = Object.values(plan.increments).reduce(
        (sum, day) => sum + day.tokens,
        0
      );
      credited += added;
      lifetimePeak = Math.max(lifetimePeak, reportedTotal);

      expect(added).toBeGreaterThanOrEqual(0);
      expect(credited).toBeLessThanOrEqual(lifetimePeak - 100);
      state = plan.nextState!;
    }
  });

  it("fails closed when an accepted generation marker has lost its high-water", () => {
    const plan = planParserHighWaterSubmission({
      client: "copilot",
      incomingVersion: 2,
      fullHistory: true,
      existingLegacyDays: {},
      incomingDays: snapshot(contribution("2026-07-01", 100)),
      persistedVersion: 2,
    });

    expect(plan.mode).toBe("freeze");
    expect(plan.nextState).toBeUndefined();
  });

  it("allowlists the exact supported client as well as its generation", () => {
    const plan = planParserHighWaterSubmission({
      client: "codex",
      incomingVersion: 2,
      fullHistory: true,
      existingLegacyDays: {},
      incomingDays: {},
    });

    expect(plan.mode).toBe("freeze");
    expect(plan.nextState).toBeUndefined();
  });

  it("accepts a supported v2 snapshot normally for a brand-new device/client", () => {
    const plan = baseline({}, snapshot(contribution("2026-07-01", 100)));

    expect(plan.mode).toBe("baseline-new");
    expect(plan.nextState?.aggregate.tokens).toBe(100);
  });
});
