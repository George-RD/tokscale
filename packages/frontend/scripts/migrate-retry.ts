// Classify a failed `drizzle-kit migrate` run as transient (worth retrying
// with a fresh process/connection) or not. Extracted from migrate-prod.ts so
// the classification is unit-testable without running the migration side
// effects that module performs on import.
//
// Two transient classes:
//   - Postgres deadlock_detected (SQLSTATE 40P01).
//   - A dropped/reset DB connection. drizzle-kit runs each migration inside a
//     transaction, so a mid-flight connection loss (e.g. a managed-Postgres
//     proxy severing the socket during a slow index build / ADD CONSTRAINT)
//     rolls the migration back with no partial state -- the postgres driver
//     surfaces it as CONNECTION_CLOSED / ECONNRESET / "Connection terminated"
//     rather than a SQLSTATE. Re-running from a fresh process reconnects and
//     re-attempts the still-pending migration idempotently.
// Anything else (a real SQL error) is non-retryable and fails the build.
export function classifyFailure(stderr: string): { retryable: boolean; reason: string } {
  if (/40P01|deadlock detected/i.test(stderr)) {
    return { retryable: true, reason: "deadlock" };
  }
  if (
    // postgres.js surfaces dropped/failed connections via these error codes
    // (CONNECTION_* / CONNECT_TIMEOUT) and Node's socket errnos (ECONN* etc.);
    // the text variants cover server-initiated terminations.
    /CONNECTION_CLOSED|CONNECTION_ENDED|CONNECTION_DESTROYED|CONNECT_TIMEOUT|ECONNRESET|ECONNREFUSED|ETIMEDOUT|EPIPE|connection closed|connection terminated|terminating connection|server closed the connection|Connection ended/i.test(
      stderr
    )
  ) {
    return { retryable: true, reason: "connection error" };
  }
  return { retryable: false, reason: "non-retryable error" };
}
