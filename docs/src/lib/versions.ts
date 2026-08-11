/**
 * The documented protocol versions, MCP-style: dated snapshots plus the
 * in-progress draft. Dated trees read their artifacts from
 * `protocol/versions/<date>/`; the draft reads the live `protocol/` tree.
 * The cut-release workflow promotes the draft into a dated tree and adds its
 * entry to `DATED_VERSIONS`, newest first.
 */

import { join } from "node:path";

export const DRAFT = "draft";

/** Released versions, newest first. Empty until the first cut. */
export const DATED_VERSIONS: string[] = [];

/** The newest released version, or null before the first cut. */
export const LATEST_VERSION: string | null = DATED_VERSIONS[0] ?? null;

/** Every documented tree, in sidebar order: dated versions, then the draft. */
export const DOC_VERSIONS: string[] = [...DATED_VERSIONS, DRAFT];

export type DocVersion = string;

export function protocolDirFor(version: DocVersion): string {
  return version === DRAFT
    ? join(process.cwd(), "..", "protocol")
    : join(process.cwd(), "..", "protocol", "versions", version);
}
