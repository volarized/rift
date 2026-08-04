/**
 * Loads the adapter seam, which is protobuf rather than JSON Schema.
 *
 * The two seams are deliberately different artifacts. MCP is JSON-RPC, so its
 * surface is JSON Schema; the adapter is gRPC over a Unix socket, so its
 * surface is a `.proto` and a service definition. `protocol.ts` reads the
 * first, this reads the second, and the pages render both.
 *
 * `core.proto` is generated from `core.json`, so the model appears here as the
 * same messages the MCP surface describes as definitions. That is the mapping
 * the adapter page shows: one model, two spellings, one source.
 */

import { join } from "node:path";
import protobuf from "protobufjs";

const PROTOCOL_DIR = join(process.cwd(), "..", "protocol");

const root = (() => {
  const loaded = new protobuf.Root();
  // Imports are written bare (`import "core.proto"`), because a .proto is
  // compiled with an include path rather than resolved relative to a URL.
  loaded.resolvePath = (_origin, target) =>
    target.startsWith("google/")
      ? (protobuf.util.path.resolve("", target) as string)
      : join(PROTOCOL_DIR, target.split("/").pop() ?? target);
  // `alternateCommentMode` reads `//` comments as documentation. Without it
  // protobufjs only picks up `/** */`, and every comment in these files is `//`.
  loaded.loadSync([join(PROTOCOL_DIR, "core.proto"), join(PROTOCOL_DIR, "adapter.proto")], {
    alternateCommentMode: true,
  });
  loaded.resolveAll();
  return loaded;
})();

export interface ProtoField {
  name: string;
  type: string;
  repeated: boolean;
  map: boolean;
  optional: boolean;
  /** The `oneof` this field belongs to, if any. */
  oneof?: string;
  comment?: string;
  /** Set when the field's type is a message or enum in these files. */
  link?: string;
}

export interface ProtoMessage {
  name: string;
  comment?: string;
  fields: ProtoField[];
  /** Declared `oneof` names, in declaration order. */
  oneofs: string[];
}

export interface ProtoEnum {
  name: string;
  comment?: string;
  values: { name: string; number: number; comment?: string }[];
}

export interface ProtoRpc {
  name: string;
  comment?: string;
  request: string;
  response: string;
  requestStream: boolean;
  responseStream: boolean;
}

export interface ProtoService {
  name: string;
  comment?: string;
  rpcs: ProtoRpc[];
}

/** Everything declared in the two files, by fully qualified name. */
const types = new Map<string, protobuf.Type | protobuf.Enum>();
const services: ProtoService[] = [];

(function collect(node: protobuf.NamespaceBase) {
  for (const child of node.nestedArray) {
    if (child instanceof protobuf.Service) {
      services.push({
        name: child.name,
        comment: child.comment ?? undefined,
        rpcs: child.methodsArray.map((method) => ({
          name: method.name,
          comment: method.comment ?? undefined,
          request: short(method.requestType),
          response: short(method.responseType),
          requestStream: method.requestStream === true,
          responseStream: method.responseStream === true,
        })),
      });
    }
    if (child instanceof protobuf.Type || child instanceof protobuf.Enum) {
      types.set(child.name, child);
    }
    if (child instanceof protobuf.Namespace) collect(child);
  }
})(root);

/** `rift.core.v1.Symbol` reads as `Symbol`; the package adds nothing on a page. */
function short(name: string): string {
  return name.split(".").pop() ?? name;
}

function describe(field: protobuf.Field): ProtoField {
  const type = short(field.type);
  return {
    name: field.name,
    type,
    repeated: field.repeated,
    map: field instanceof protobuf.MapField,
    // proto3 explicit presence, which the generator emits for a schema's
    // optional properties and for anything the schema allows to be null.
    optional: field.optional === true && !field.repeated && !field.partOf,
    oneof: field.partOf?.name,
    comment: field.comment ?? undefined,
    link: types.has(type) ? type : undefined,
  };
}

export const protoServices: ProtoService[] = services;

export const protoMessages: ProtoMessage[] = [...types.values()]
  .filter((t): t is protobuf.Type => t instanceof protobuf.Type)
  .map((type) => ({
    name: type.name,
    comment: type.comment ?? undefined,
    fields: type.fieldsArray.map(describe),
    oneofs: type.oneofsArray.map((oneof) => oneof.name),
  }));

export const protoEnums: ProtoEnum[] = [...types.values()]
  .filter((t): t is protobuf.Enum => t instanceof protobuf.Enum)
  .map((declared) => ({
    name: declared.name,
    comment: declared.comment ?? undefined,
    values: Object.entries(declared.values).map(([name, number]) => ({
      name,
      number,
      comment: declared.comments[name] ?? undefined,
    })),
  }));

/** Which proto types the adapter file declares, as opposed to the generated model. */
export const adapterOwned: Set<string> = (() => {
  const owned = new Set<string>();
  const namespace = root.lookup("rift.adapter.v1");
  if (namespace instanceof protobuf.Namespace) {
    for (const child of namespace.nestedArray) owned.add(child.name);
  }
  return owned;
})();

export const protoTypeNames: string[] = [...types.keys()];
