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

// protobufjs bundles the well-known types it can inline, but not
// descriptor.proto, which `adapter.proto` needs to declare its own options. It
// ships in the package, and is reached from the build's own directory the same
// way the schemas are: `require.resolve` would hand webpack a module request
// for a `.proto`, and webpack has no loader that can parse one.
const DESCRIPTOR_PROTO = join(
  process.cwd(),
  "node_modules/protobufjs/google/protobuf/descriptor.proto",
);

const root = (() => {
  const loaded = new protobuf.Root();
  // Imports are written bare (`import "core.proto"`), because a .proto is
  // compiled with an include path rather than resolved relative to a URL.
  loaded.resolvePath = (_origin, target) => {
    if (target === "google/protobuf/descriptor.proto") return DESCRIPTOR_PROTO;
    return target.startsWith("google/")
      ? (protobuf.util.path.resolve("", target) as string)
      : join(PROTOCOL_DIR, target.split("/").pop() ?? target);
  };
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
  /** Enums declared inside this message, which is where every enum here lives. */
  enums: ProtoEnum[];
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

function describeEnum(declared: protobuf.Enum): ProtoEnum {
  return {
    name: declared.name,
    comment: declared.comment ?? undefined,
    values: Object.entries(declared.values).map(([name, number]) => ({
      name,
      number,
      comment: declared.comments[name] ?? undefined,
    })),
  };
}

export const protoMessages: ProtoMessage[] = [...types.values()]
  .filter((t): t is protobuf.Type => t instanceof protobuf.Type)
  .map((type) => ({
    name: type.name,
    comment: type.comment ?? undefined,
    fields: type.fieldsArray.map(describe),
    oneofs: type.oneofsArray.map((oneof) => oneof.name),
    enums: type.nestedArray.flatMap((child) =>
      child instanceof protobuf.Enum ? [describeEnum(child)] : [],
    ),
  }));

export const protoEnums: ProtoEnum[] = [...types.values()]
  .filter((t): t is protobuf.Enum => t instanceof protobuf.Enum)
  .map(describeEnum);

/**
 * Which message an enum is declared inside. Protobuf scopes an enum's values to
 * the enclosing namespace, so nesting is what lets `Refusal.Reason.UNSPECIFIED`
 * and `AuxiliaryClaim.Purpose.UNSPECIFIED` both exist — and it means a nested
 * enum has no heading of its own to link to.
 */
export const protoEnumOwner: Record<string, string> = Object.fromEntries(
  protoMessages.flatMap((message) =>
    message.enums.map((declared) => [declared.name, message.name] as const),
  ),
);

/**
 * Which proto types the adapter file declares, as opposed to the generated
 * model. Messages and enums only: the file also declares the option extensions
 * that carry its grouping, and those are fields rather than types.
 */
export const adapterOwned: Set<string> = (() => {
  const owned = new Set<string>();
  const namespace = root.lookup("rift.adapter.v1");
  if (namespace instanceof protobuf.Namespace) {
    for (const child of namespace.nestedArray) {
      if (child instanceof protobuf.Type || child instanceof protobuf.Enum) owned.add(child.name);
    }
  }
  return owned;
})();

/**
 * The messages that exist only because protobuf forbids a repeated field
 * directly inside a `oneof`. Each is one repeated field and nothing else, so
 * there is nothing to read on a page of its own.
 *
 * Recognised by shape rather than by name, so a new one costs nothing, and
 * scoped to the adapter file for the same reason: `FilterAll` and
 * `SourceMapEntryExact` are shaped alike but are model types with meaning of
 * their own. A leading comment is disqualifying — a wrapper someone found worth
 * explaining is no longer plumbing.
 */
export const protoWrappers: Set<string> = new Set(
  protoMessages
    .filter(
      (message) =>
        adapterOwned.has(message.name) &&
        message.fields.length === 1 &&
        message.fields[0].repeated &&
        message.oneofs.length === 0 &&
        !message.comment,
    )
    .map((message) => message.name),
);

export const protoTypeNames: string[] = [...types.keys()];

export interface ProtoSection {
  name: string;
  /** Message and enum names, in declaration order. */
  types: string[];
}

/**
 * The groups `adapter.proto` divides itself into, read from the option each
 * declaration carries:
 *
 * ```proto
 * message WorkspaceState {
 *   option (section) = "Workspaces";
 * ```
 *
 * Declaring it in the file is the same reason `core.json` carries `rift:axes`:
 * a grouping kept in the documentation code is a second place to edit, and it
 * drifts silently when a message moves. Sections appear in the order they are
 * first declared, so the page reads in file order.
 */
export const protoSections: ProtoSection[] = (() => {
  const namespace = root.lookup("rift.adapter.v1");
  if (!(namespace instanceof protobuf.Namespace)) return [];
  const byName = new Map<string, ProtoSection>();
  for (const child of namespace.nestedArray) {
    if (!(child instanceof protobuf.Type)) continue;
    if (protoWrappers.has(child.name)) continue;
    const options = child.options as Record<string, string> | undefined;
    const name = options?.["(section)"];
    if (!name) continue;
    const section = byName.get(name) ?? { name, types: [] };
    section.types.push(child.name);
    byName.set(name, section);
  }
  return [...byName.values()];
})();
