/** Generated configuration contract and the repository config checked against it. */

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

const REPOSITORY = join(process.cwd(), "..");

export const configDocument = JSON.parse(
  readFileSync(join(REPOSITORY, "protocol", "rift.schema.json"), "utf8"),
) as ConfigDocument;

export const configDefs = configDocument.$defs;
export const repositoryConfig = readFileSync(join(REPOSITORY, "rift.toml"), "utf8").trim();
