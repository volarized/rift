/**
 * Where one version's protocol artifacts live on disk. Server-side only:
 * the loaders read files at build time, and `node:path` must stay out of
 * the client bundle the version banner belongs to.
 */

import { join } from "node:path";
import { type DocVersion, DRAFT } from "@/lib/versions";

export function protocolDirFor(version: DocVersion): string {
  return version === DRAFT
    ? join(process.cwd(), "..", "protocol")
    : join(process.cwd(), "..", "protocol", "versions", version);
}
