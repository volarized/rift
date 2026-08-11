/** Generated configuration contract and the workspace config checked against it. */

import { readFileSync } from "node:fs";
import { join } from "node:path";
import type { Schema } from "@/lib/protocol";

export interface ConfigSchema extends Schema {
  "rift:bounds"?: { model: string; field: string };
  "rift:prefixOf"?: { model: string; field: string };
  "rift:selectsType"?: string;
  "rift:describesAs"?: string;
  "rift:conversion"?: string;
  "rift:operation"?: string;
  "rift:package"?: string;
}

interface ConfigDocument extends ConfigSchema {
  $id: string;
  $defs: Record<string, ConfigSchema>;
}

const WORKSPACE = join(process.cwd(), "..");

export const configDocument = JSON.parse(
  readFileSync(join(WORKSPACE, "protocol", "rift.schema.json"), "utf8"),
) as ConfigDocument;

export const configDefs = configDocument.$defs;
export const workspaceConfig = readFileSync(join(WORKSPACE, "docs", "rift.toml"), "utf8").trim();
