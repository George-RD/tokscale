import { describe, expect, it } from "vitest";

import {
  SITEMAP_GROUP_LIMIT,
  SITEMAP_USER_LIMIT,
  buildCoreEntries,
  buildGroupEntries,
  buildUserEntries,
  latestSubmissionTime,
} from "@/lib/seo/sitemap";
import { SITE_URL, homeUrl, leaderboardUrl } from "@/lib/seo/urls";

const NOW = new Date("2026-07-29T00:00:00.000Z");
const SUBMITTED_AT = new Date("2026-07-01T12:34:56.000Z");

describe("buildCoreEntries", () => {
  it("lists the home page and both leaderboard views as absolute canonical URLs", () => {
    const urls = buildCoreEntries(NOW).map((entry) => entry.url);

    expect(urls).toEqual([
      SITE_URL,
      `${SITE_URL}/leaderboard`,
      `${SITE_URL}/leaderboard?view=groups`,
    ]);
  });

  it("uses the same builders the pages canonicalize with", () => {
    // If a sitemap URL and the page's own <link rel="canonical"> disagree,
    // the page is dropped rather than arbitrated — so they share one source.
    const urls = buildCoreEntries(NOW).map((entry) => entry.url);

    expect(urls).toContain(homeUrl());
    expect(urls).toContain(leaderboardUrl());
    expect(urls).toContain(leaderboardUrl("groups"));
  });

  it("lists no filtered leaderboard URL, since those disclaim themselves", () => {
    const urls = buildCoreEntries(NOW).map((entry) => entry.url);

    for (const param of ["period=", "sortBy=", "page=", "search=", "from=", "to="]) {
      expect(urls.some((url) => url.includes(param))).toBe(false);
    }
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

describe("latestSubmissionTime", () => {
  const OLDER = new Date("2026-07-01T00:00:00.000Z");
  const NEWER = new Date("2026-07-20T00:00:00.000Z");

  it("returns the newest submission time regardless of row order", () => {
    // Rows arrive ordered by totalTokens, not by time, so this cannot just
    // read the first or last element.
    const rows = [
      { username: "a", updatedAt: OLDER },
      { username: "b", updatedAt: NEWER },
      { username: "c", updatedAt: OLDER },
    ];

    expect(latestSubmissionTime(rows)).toBe(NEWER);
  });

  it("ignores rows with no submission time", () => {
    const rows = [
      { username: "a", updatedAt: null },
      { username: "b", updatedAt: OLDER },
      { username: "c", updatedAt: null },
    ];

    expect(latestSubmissionTime(rows)).toBe(OLDER);
  });

  it("returns null when there is nothing truthful to report", () => {
    expect(latestSubmissionTime([])).toBeNull();
    expect(latestSubmissionTime([{ username: "a", updatedAt: null }])).toBeNull();
  });

  it("does not advance when the same rows are passed again", () => {
    // The whole point: an hourly revalidation must not move lastmod unless a
    // submission actually landed. Google discounts lastmod site-wide when it
    // moves without the page changing.
    const rows = [{ username: "a", updatedAt: OLDER }];

    expect(latestSubmissionTime(rows)).toBe(latestSubmissionTime(rows));
  });
});

describe("buildCoreEntries lastmod", () => {
  it("stamps every core page with the supplied time, not the current time", () => {
    const activity = new Date("2026-07-20T00:00:00.000Z");

    for (const entry of buildCoreEntries(activity)) {
      expect(entry.lastModified).toBe(activity);
    }
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
