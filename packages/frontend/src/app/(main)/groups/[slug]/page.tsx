import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { Navigation } from "@/components/layout/Navigation";
import { ServiceFooter } from "@/components/layout/ServiceFooter";
import { getSession } from "@/lib/auth/session";
import { getGroupLeaderboardData } from "@/lib/groups/getGroupLeaderboard";
import { getGroupMembership } from "@/lib/groups/permissions";
import { getGroupBySlug, getGroupMemberCount } from "@/lib/groups/queries";
import { groupUrl } from "@/lib/seo/urls";
import GroupDetailClient from "./GroupDetailClient";

interface GroupPageProps {
  params: Promise<{ slug: string }>;
}

/**
 * Built from the slug alone so it costs no extra query — the page's own
 * getGroupBySlug lookup is not React-cached, so touching the DB here would
 * double it on every request. Private groups notFound() below, and a 404 is
 * not indexed, so emitting a canonical for one is harmless.
 */
export async function generateMetadata({ params }: GroupPageProps): Promise<Metadata> {
  const { slug } = await params;

  return {
    alternates: {
      canonical: groupUrl(slug),
    },
  };
}

function PageShell({ children }: { children: React.ReactNode }) {
  return (
    <div className="service-page-shell">
      <Navigation />
      <main className="service-main" id="main-content">{children}</main>
      <ServiceFooter />
    </div>
  );
}

export default async function GroupPage({ params }: GroupPageProps) {
  const { slug } = await params;
  const group = await getGroupBySlug(slug);

  if (!group) {
    notFound();
  }

  const session = await getSession();
  const membership = session ? await getGroupMembership(group.id, session.id) : null;

  if (!group.isPublic && !membership) {
    notFound();
  }

  const [memberCount, initialData] = await Promise.all([
    getGroupMemberCount(group.id),
    getGroupLeaderboardData(group.id, "all", 1, 50, "tokens"),
  ]);

  return (
    <PageShell>
      <GroupDetailClient
        group={{
          id: group.id,
          name: group.name,
          slug: group.slug,
          description: group.description,
          avatarUrl: group.avatarUrl,
          isPublic: group.isPublic,
          memberCount,
          membership,
        }}
        currentUser={session}
        initialData={initialData}
      />
    </PageShell>
  );
}
