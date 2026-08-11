/**
 * The documented protocol versions, MCP-style: dated snapshots plus the
 * in-progress draft. The cut-release workflow promotes the draft into a dated
 * tree and adds its entry to `DATED_VERSIONS`, newest first.
 *
 * Pure constants only — the version banner imports this from a client
 * component, so anything Node-only (the artifact directory lookup) lives in
 * `protocol-dir.ts` instead.
 */

export const DRAFT = "draft";

/** Released versions, newest first. Empty until the first cut. */
export const DATED_VERSIONS: string[] = [];

/** The newest released version, or null before the first cut. */
export const LATEST_VERSION: string | null = DATED_VERSIONS[0] ?? null;

/** Every documented tree, in sidebar order: dated versions, then the draft. */
export const DOC_VERSIONS: string[] = [...DATED_VERSIONS, DRAFT];

export type DocVersion = string;
