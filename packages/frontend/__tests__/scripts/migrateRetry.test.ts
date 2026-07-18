import { describe, it, expect } from "vitest";
import { classifyFailure } from "../../scripts/migrate-retry";

describe("classifyFailure", () => {
  it("retries a Postgres deadlock (40P01)", () => {
    expect(classifyFailure("error: deadlock detected")).toEqual({
      retryable: true,
      reason: "deadlock",
    });
    expect(classifyFailure("... (SQLSTATE 40P01) ...")).toEqual({
      retryable: true,
      reason: "deadlock",
    });
  });

  it("retries a dropped DB connection", () => {
    // The exact error that failed migration 0017's first production deploy:
    // the managed-Postgres proxy severed the socket mid-migration.
    expect(
      classifyFailure("Error: write CONNECTION_CLOSED nozomi.proxy.rlwy.net:27021")
    ).toEqual({ retryable: true, reason: "connection error" });
    expect(classifyFailure("read ECONNRESET")).toEqual({
      retryable: true,
      reason: "connection error",
    });
    expect(classifyFailure("Connection terminated unexpectedly")).toEqual({
      retryable: true,
      reason: "connection error",
    });
    expect(classifyFailure("terminating connection due to administrator command")).toEqual({
      retryable: true,
      reason: "connection error",
    });
    // postgres.js connect/startup timeout is reported as CONNECT_TIMEOUT.
    expect(classifyFailure("Error: CONNECT_TIMEOUT")).toEqual({
      retryable: true,
      reason: "connection error",
    });
  });

  it("does NOT retry a genuine SQL error", () => {
    expect(classifyFailure('error: column "foo" does not exist')).toEqual({
      retryable: false,
      reason: "non-retryable error",
    });
    expect(
      classifyFailure('relation "daily_breakdown" already exists')
    ).toEqual({ retryable: false, reason: "non-retryable error" });
    expect(classifyFailure("")).toEqual({
      retryable: false,
      reason: "non-retryable error",
    });
  });
});
