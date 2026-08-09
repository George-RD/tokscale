import { describe, expect, it } from "vitest";
import {
  mergeClientBreakdownsWithRegressionGuard,
  planParserVersionRedistribution,
  recalculateDayTotals,
  type ClientBreakdownData,
} from "../../src/lib/db/helpers";

function client(tokens: number): ClientBreakdownData {
  return {
    tokens,
    cost: tokens / 100,
    input: Math.floor(tokens * 0.6),
    output: tokens - Math.floor(tokens * 0.6),
    cacheRead: 0,
    cacheWrite: 0,
    reasoning: 0,
    messages: tokens > 0 ? 1 : 0,
    models: {
      "gpt-test": {
        tokens,
        cost: tokens / 100,
        input: Math.floor(tokens * 0.6),
        output: tokens - Math.floor(tokens * 0.6),
        cacheRead: 0,
        cacheWrite: 0,
        reasoning: 0,
        messages: tokens > 0 ? 1 : 0,
      },
    },
  };
}

function incoming(date: string, tokens: number, clientName = "copilot") {
  return {
    date,
    clients:
      tokens > 0
        ? [
            {
              client: clientName,
              tokens: {
                input: Math.floor(tokens * 0.6),
                output: tokens - Math.floor(tokens * 0.6),
                cacheRead: 0,
                cacheWrite: 0,
                reasoning: 0,
              },
            },
          ]
        : [],
  };
}

function existing(date: string, tokens: number, clientName = "copilot") {
  return {
    date,
    breakdown: { [clientName]: client(tokens) },
  };
}

describe("parser-version redistribution", () => {
  it("removes an exact creation-day total and keeps scalar/JSON/model/cost totals coherent", () => {
    const plan = planParserVersionRedistribution(
      [existing("2026-07-01", 100)],
      [incoming("2026-07-01", 0), incoming("2026-07-02", 100)],
      "copilot",
      2
    );
    expect(plan.authorizedDecreaseDates).toEqual(new Set(["2026-07-01"]));

    const creation = mergeClientBreakdownsWithRegressionGuard(
      { copilot: client(100) },
      {},
      new Set(["copilot"]),
      undefined,
      true,
      { allowParserVersionDecreases: new Set(["copilot"]) }
    ).merged;
    const shutdown = { copilot: client(100) };
    const creationTotals = recalculateDayTotals(creation);
    const shutdownTotals = recalculateDayTotals(shutdown);

    expect(creation.copilot).toBeUndefined();
    expect(creationTotals.tokens + shutdownTotals.tokens).toBe(100);
    expect(creationTotals.cost + shutdownTotals.cost).toBe(1);
    expect(shutdownTotals.inputTokens).toBe(shutdown.copilot.models["gpt-test"].input);
    expect(shutdownTotals.outputTokens).toBe(shutdown.copilot.models["gpt-test"].output);
  });

  it("authorizes an omitted creation day only when a new day funds it", () => {
    const plan = planParserVersionRedistribution(
      [existing("2026-07-01", 100)],
      [incoming("2026-07-02", 100)],
      "copilot",
      2
    );

    expect(plan.transition).toBe(true);
    expect(plan.authorizedDecreaseDates).toEqual(new Set(["2026-07-01"]));
  });

  it("preserves deleted history beyond the positive re-attribution budget", () => {
    const plan = planParserVersionRedistribution(
      [
        existing("2026-06-01", 50),
        existing("2026-07-01", 100),
      ],
      [incoming("2026-07-02", 100)],
      "copilot",
      2
    );

    expect(plan.authorizedDecreaseDates).toEqual(new Set(["2026-07-01"]));
    expect(plan.authorizedDecreaseDates).not.toContain("2026-06-01");
  });

  it("does not reopen the allowance for the same persisted parser version", () => {
    const plan = planParserVersionRedistribution(
      [existing("2026-07-01", 100)],
      [incoming("2026-07-02", 100)],
      "copilot",
      2,
      2
    );

    expect(plan.transition).toBe(false);
    expect(plan.authorizedDecreaseDates.size).toBe(0);
  });

  it("keeps the old-CLI monotonic behavior before any forward transition", () => {
    const { merged } = mergeClientBreakdownsWithRegressionGuard(
      { copilot: client(100) },
      { copilot: client(75) },
      new Set(["copilot"])
    );

    expect(merged.copilot.tokens).toBe(100);
  });

  it("blocks a lower parser generation from re-inflating a healed row", () => {
    const stale = planParserVersionRedistribution(
      [],
      [incoming("2026-07-01", 100)],
      "copilot",
      1,
      2
    );
    const { merged } = mergeClientBreakdownsWithRegressionGuard(
      {},
      { copilot: client(100) },
      new Set(["copilot"]),
      undefined,
      true,
      { staleParserClients: new Set(["copilot"]) }
    );

    expect(stale.stale).toBe(true);
    expect(merged.copilot).toBeUndefined();
  });

  it("does not transition a client omitted by a partial client filter", () => {
    const declaredVersions = new Map([["codex", 1]]);
    const plans = [...declaredVersions].map(([clientName, version]) =>
      planParserVersionRedistribution(
        [existing("2026-07-01", 100)],
        [incoming("2026-07-02", 100)],
        clientName,
        version
      )
    );

    expect(plans).toHaveLength(1);
    expect(plans[0].client).toBe("codex");
    expect(plans[0].transition).toBe(false);
    expect(plans.some((plan) => plan.client === "copilot")).toBe(false);
  });
});
