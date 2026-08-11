"use client";

import { useRouter } from "next/navigation";
import { useEffect } from "react";

/**
 * Sends `/docs` to the tree a reader almost certainly wants: the newest
 * released version, or the draft before the first cut. A real page rather
 * than a server redirect because the site is a static export — there is no
 * server to answer 3xx. The page body below the component is the fallback
 * for a reader with scripts disabled.
 */
export function VersionRedirect({ to = "/docs/draft" }: { to?: string }) {
  const router = useRouter();
  useEffect(() => {
    router.replace(to);
  }, [router, to]);
  return null;
}
