/// <reference types="bun-types" />
import postgres from "postgres";

// Vercel has no buildCommand-level distinction between "preview build for a
// WIP branch" and "production build" other than VERCEL_ENV — and DATABASE_URL
// is the SAME value across Production/Preview/Development in this project.
// Without this gate, pushing an unreviewed migration to any branch would
// apply it to prod the moment its preview build runs.
if (process.env.VERCEL_ENV !== "production") {
  console.log(
    `skip - migrate-prod: VERCEL_ENV=${process.env.VERCEL_ENV ?? "(unset)"}, not production`
  );
  process.exit(0);
}

const databaseUrl = process.env.DATABASE_URL;
if (!databaseUrl) {
  throw new Error("DATABASE_URL is required");
}

// drizzle-kit/drizzle-orm take no advisory lock of their own, so concurrent
// builds (rapid pushes, a manual redeploy overlapping an in-flight one) can
// race two `drizzle-kit migrate` runs against each other. Hold a session
// lock for the lifetime of this process to serialize them.
const LOCK_KEY = "tokscale_drizzle_migrate";
const MAX_ATTEMPTS = 5;
const RETRY_DELAY_MS = 3000;

const sql = postgres(databaseUrl, { max: 1 });

function sleep(ms: number) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function runMigrate(): Promise<{ ok: boolean; deadlock: boolean; stderr: string }> {
  const proc = Bun.spawn(["bunx", "drizzle-kit", "migrate"], {
    stdout: "inherit",
    stderr: "pipe",
  });
  const stderr = await new Response(proc.stderr).text();
  process.stderr.write(stderr);
  const exitCode = await proc.exited;
  if (exitCode === 0) {
    return { ok: true, deadlock: false, stderr };
  }
  // Postgres deadlock_detected is SQLSTATE 40P01.
  const deadlock = /40P01|deadlock detected/i.test(stderr);
  return { ok: false, deadlock, stderr };
}

try {
  await sql`SELECT pg_advisory_lock(hashtext(${LOCK_KEY}))`;
  console.log(`ok - acquired advisory lock (${LOCK_KEY})`);

  let lastResult: Awaited<ReturnType<typeof runMigrate>> | undefined;
  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt++) {
    lastResult = await runMigrate();
    if (lastResult.ok) {
      console.log(`ok - drizzle-kit migrate succeeded (attempt ${attempt}/${MAX_ATTEMPTS})`);
      break;
    }
    if (!lastResult.deadlock) {
      throw new Error(
        `drizzle-kit migrate failed (attempt ${attempt}/${MAX_ATTEMPTS}, not a deadlock — not retrying)`
      );
    }
    console.warn(
      `warn - drizzle-kit migrate hit a deadlock (attempt ${attempt}/${MAX_ATTEMPTS})`
    );
    if (attempt === MAX_ATTEMPTS) {
      throw new Error(`drizzle-kit migrate deadlocked ${MAX_ATTEMPTS} times in a row`);
    }
    await sleep(RETRY_DELAY_MS);
  }
} finally {
  await sql`SELECT pg_advisory_unlock(hashtext(${LOCK_KEY}))`;
  await sql.end();
}
