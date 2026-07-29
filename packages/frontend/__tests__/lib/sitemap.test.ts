import { describe, expect, it } from "vitest";

import {
  SITE_URL,
  SITEMAP_GROUP_LIMIT,
  SITEMAP_USER_LIMIT,
  buildCoreEntries,
  buildGroupEntries,
  buildUserEntries,
} from "@/lib/seo/sitemap";

const NOW = new Date("2026-07-29T00:00:00.000Z");
const SUBMITTED_AT = new Date("2026-07-01T12:34:56.000Z");

describe("buildCoreEntries", () => {
  it("lists the home and leaderboard pages as absolute canonical URLs", () => {
    const urls = buildCoreEntries(NOW).map((entry) => entry.url);

    expect(urls).toEqual([SITE_URL, `${SITE_URL}/leaderboard`]);
  });

  it("omits auth-gated, redirect-only, and invite routes", () => {
    const urls = buildCoreEntries(NOW).map((entry) => entry.url);

    for (const excluded of [
      "/settings",
      "/profile",
      "/device",
      "/local",
      "/groups",
      "/groups/new",
      "/groups/join",
    ]) {
      expect(urls).not.toContain(`${SITE_URL}${excluded}`);
    }
  });
});

describe("buildUserEntries", () => {
  it("points at /u/<username> using the submission time as lastModified", () => {
    const [entry] = buildUserEntries(
      [{ username: "junhoyeo", updatedAt: SUBMITTED_AT }],
      NOW
    );

    expect(entry.url).toBe(`${SITE_URL}/u/junhoyeo`);
    expect(entry.lastModified).toBe(SUBMITTED_AT);
  });

  it("falls back to the generation time when a row has no submission time", () => {
    const [entry] = buildUserEntries([{ username: "junhoyeo", updatedAt: null }], NOW);

    expect(entry.lastModified).toBe(NOW);
  });

  it("preserves the DB's username casing so entries never point at a redirect", () => {
    // /u/[username] issues a permanentRedirect to the canonical casing, and a
    // sitemap URL that redirects is dropped rather than followed.
    const [entry] = buildUserEntries(
      [{ username: "JunhoYeo", updatedAt: SUBMITTED_AT }],
      NOW
    );

    expect(entry.url).toBe(`${SITE_URL}/u/JunhoYeo`);
  });

  it("percent-encodes usernames so an odd row cannot emit a malformed URL", () => {
    const [entry] = buildUserEntries(
      [{ username: "a b/c?d", updatedAt: SUBMITTED_AT }],
      NOW
    );

    expect(entry.url).toBe(`${SITE_URL}/u/a%20b%2Fc%3Fd`);
  });

  it("returns nothing when no user has submitted", () => {
    expect(buildUserEntries([], NOW)).toEqual([]);
  });
});

describe("buildGroupEntries", () => {
  it("points at /groups/<slug> using the group's update time", () => {
    const [entry] = buildGroupEntries(
      [{ slug: "anthropic", updatedAt: SUBMITTED_AT }],
      NOW
    );

    expect(entry.url).toBe(`${SITE_URL}/groups/anthropic`);
    expect(entry.lastModified).toBe(SUBMITTED_AT);
  });
});

describe("sitemap size budget", () => {
  it("cannot exceed the 50,000-URL limit for a single sitemap file", () => {
    // An over-limit sitemap is rejected wholesale, not truncated, so the
    // per-section budgets plus the core pages have to fit with room to spare.
    const worstCase =
      buildCoreEntries(NOW).length + SITEMAP_USER_LIMIT + SITEMAP_GROUP_LIMIT;

    expect(worstCase).toBeLessThan(50_000);
  });
});
