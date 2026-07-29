import type { MetadataRoute } from "next";
import { groupUrl, homeUrl, leaderboardUrl, profileUrl } from "./urls";

/**
 * sitemaps.org caps one sitemap file at 50,000 URLs / 50 MB uncompressed, and
 * an over-limit file is rejected wholesale rather than truncated. We budget
 * each section separately so a runaway section can't invalidate the sitemap.
 *
 * If a section ever actually saturates its budget, split into per-segment
 * sitemaps (app/u/sitemap.ts, app/groups/sitemap.ts) instead of raising these.
 * Next.js serves `generateSitemaps()` shards at /u/sitemap/0.xml etc. without
 * emitting an index file, so sharding also means listing every shard in
 * robots.ts by hand — nested per-segment sitemaps stay far simpler.
 */
export const SITEMAP_USER_LIMIT = 40_000;
export const SITEMAP_GROUP_LIMIT = 5_000;

export interface SitemapUserRow {
  /** Canonical casing from the DB — /u/[username] permanent-redirects other
   *  casings, and a sitemap should never point at a redirect. */
  username: string;
  /** Last submission time; null for rows that predate the column default. */
  updatedAt: Date | null;
}

export interface SitemapGroupRow {
  slug: string;
  updatedAt: Date | null;
}

/**
 * Pages that exist independently of the database.
 *
 * Deliberately excluded, and mirrored by the disallow list in app/robots.ts:
 * - /settings, /profile, /device  — auth-gated or redirect-only
 * - /groups/new                   — auth-gated
 * - /groups/join/[token]          — invite tokens, must never be indexed
 * - /groups                       — redirects to /leaderboard?view=groups
 * - /local                        — client-only viewer; renders empty to a
 *                                   crawler, so listing it would just add a
 *                                   thin-content URL to the index
 *
 * The leaderboard is listed once, bare. Its filter params (period, sortBy,
 * page, from/to, search) all canonicalize back to this URL, so listing any of
 * them would point crawlers at pages that disclaim themselves.
 */
export function buildCoreEntries(now: Date): MetadataRoute.Sitemap {
  return [
    {
      url: homeUrl(),
      lastModified: now,
      changeFrequency: "daily",
      priority: 1,
    },
    {
      url: leaderboardUrl(),
      lastModified: now,
      changeFrequency: "hourly",
      priority: 0.9,
    },
    {
      url: leaderboardUrl("groups"),
      lastModified: now,
      changeFrequency: "daily",
      priority: 0.7,
    },
  ];
}

export function buildUserEntries(
  rows: readonly SitemapUserRow[],
  fallbackLastModified: Date
): MetadataRoute.Sitemap {
  return rows.map((row) => ({
    url: profileUrl(row.username),
    lastModified: row.updatedAt ?? fallbackLastModified,
    changeFrequency: "daily",
    priority: 0.7,
  }));
}

export function buildGroupEntries(
  rows: readonly SitemapGroupRow[],
  fallbackLastModified: Date
): MetadataRoute.Sitemap {
  return rows.map((row) => ({
    url: groupUrl(row.slug),
    lastModified: row.updatedAt ?? fallbackLastModified,
    changeFrequency: "weekly",
    priority: 0.6,
  }));
}
