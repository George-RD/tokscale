import type { MetadataRoute } from "next";
import { SITE_URL } from "@/lib/seo/sitemap";

export default function robots(): MetadataRoute.Robots {
  return {
    rules: {
      userAgent: "*",
      allow: "/",
      // Mirrors the exclusions documented on buildCoreEntries(). /groups/join/
      // is the one that actually matters: those URLs carry single-use invite
      // tokens and must never reach an index.
      disallow: [
        "/api/",
        "/settings",
        "/profile",
        "/device",
        "/groups/new",
        "/groups/join/",
      ],
    },
    sitemap: `${SITE_URL}/sitemap.xml`,
  };
}
