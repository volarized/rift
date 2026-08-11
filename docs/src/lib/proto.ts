/**
 * Loads one version's generated Protobuf packages. MCP uses JSON at the agent
 * boundary. The server, adapters, and SCIP export use the packages loaded here.
 * `protoFor(version)` reads the live `protocol/` tree for the draft and the
 * frozen `protocol/versions/<date>/` snapshot for a dated version.
 */

import { isAbsolute, join } from "node:path";
import protobuf from "protobufjs";
import { protocolDirFor } from "@/lib/protocol-dir";
import type { DocVersion } from "@/lib/versions";

// protobufjs bundles the well-known types it can inline, but not
// descriptor.proto, which `adapter.proto` needs to declare its own options. It
// ships in the package, and is reached from the build's own directory the same
// way the schemas are: `require.resolve` would hand webpack a module request
// for a `.proto`, and webpack has no loader that can parse one.
const DESCRIPTOR_PROTO = join(
  process.cwd(),
  "node_modules/protobufjs/google/protobuf/descriptor.proto",
);

export interface ProtoField {
  name: string;
  number: number;
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
  fullName: string;
  package: string;
  comment?: string;
  fields: ProtoField[];
  /** Declared `oneof` names, in declaration order. */
  oneofs: string[];
  /** Enums declared inside this message, which is where every enum here lives. */
  enums: ProtoEnum[];
}

export interface ProtoEnum {
  name: string;
  fullName: string;
  package: string;
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
  package: string;
  comment?: string;
  rpcs: ProtoRpc[];
}

export interface ProtoSection {
  name: string;
  /** Message and enum names, in declaration order. */
  types: string[];
}

/** One version's loaded packages and everything derived from them. */
export interface ProtoData {
  adapterServices: ProtoService[];
  scipServices: ProtoService[];
  protoMessages: ProtoMessage[];
  protoEnums: ProtoEnum[];
  protoEnumOwner: Record<string, string>;
  adapterOwned: Set<string>;
  protoWrappers: Set<string>;
  protoTypeNames: string[];
  scipMessages: ProtoMessage[];
  scipEnums: ProtoEnum[];
  protoSections: ProtoSection[];
}

/** `rift.core.Symbol` reads as `Symbol`; the package adds nothing on a page. */
function short(name: string): string {
  return name.split(".").pop() ?? name;
}

function load(version: DocVersion): ProtoData {
  const protocolDir = protocolDirFor(version);

  const root = (() => {
    const loaded = new protobuf.Root();
    loaded.resolvePath = (_origin, target) => {
      if (isAbsolute(target)) return target;
      if (target === "google/protobuf/descriptor.proto") return DESCRIPTOR_PROTO;
      return target.startsWith("google/")
        ? (protobuf.util.path.resolve("", target) as string)
        : join(protocolDir, target);
    };
    // `alternateCommentMode` reads `//` comments as documentation. Without it
    // protobufjs only picks up `/** */`, and every comment in these files is `//`.
    loaded.loadSync(
      [
        join(protocolDir, "rift/core.proto"),
        join(protocolDir, "rift/mcp.proto"),
        join(protocolDir, "rift/adapter.proto"),
        join(protocolDir, "rift/scip.proto"),
        join(protocolDir, "scip/scip.proto"),
      ],
      { alternateCommentMode: true },
    );
    loaded.resolveAll();
    return loaded;
  })();

  // Everything declared in the generated Rift and SCIP packages.
  const types = new Map<string, protobuf.Type | protobuf.Enum>();
  const services: ProtoService[] = [];

  (function collect(node: protobuf.NamespaceBase) {
    for (const child of node.nestedArray) {
      if (child instanceof protobuf.Service) {
        services.push({
          name: child.name,
          package: child.parent?.fullName.slice(1) ?? "",
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
        types.set(child.fullName.slice(1), child);
      }
      if (child instanceof protobuf.Namespace) collect(child);
    }
  })(root);

  function describe(field: protobuf.Field): ProtoField {
    const type = short(field.type);
    const target = field.resolvedType?.fullName.slice(1);
    return {
      name: field.name,
      number: field.id,
      type,
      repeated: field.repeated,
      map: field instanceof protobuf.MapField,
      // proto3 explicit presence, which the generator emits for a schema's
      // optional properties and for anything the schema allows to be null.
      optional: field.optional === true && !field.repeated && !field.partOf,
      oneof: field.partOf?.name,
      comment: field.comment ?? undefined,
      link: target && types.has(target) ? target : undefined,
    };
  }

  function describeEnum(declared: protobuf.Enum): ProtoEnum {
    const fullName = declared.fullName.slice(1);
    return {
      name: declared.name,
      fullName,
      package: fullName.split(".").slice(0, -1).join("."),
      comment: declared.comment ?? undefined,
      values: Object.entries(declared.values).map(([name, number]) => ({
        name,
        number,
        comment: declared.comments[name] ?? undefined,
      })),
    };
  }

  const adapterServices = services.filter((service) => service.package === "rift.adapter");
  const scipServices = services.filter((service) => service.package === "rift.scip");

  const protoMessages: ProtoMessage[] = [...types.values()]
    .filter((t): t is protobuf.Type => t instanceof protobuf.Type)
    .map((type) => ({
      name: type.name,
      fullName: type.fullName.slice(1),
      package: type.fullName.slice(1).split(".").slice(0, -1).join("."),
      comment: type.comment ?? undefined,
      fields: type.fieldsArray.map(describe),
      oneofs: type.oneofsArray.map((oneof) => oneof.name),
      enums: type.nestedArray.flatMap((child) =>
        child instanceof protobuf.Enum ? [describeEnum(child)] : [],
      ),
    }));

  const protoEnums: ProtoEnum[] = [...types.values()]
    .filter((t): t is protobuf.Enum => t instanceof protobuf.Enum)
    .map(describeEnum);

  // Which message an enum is declared inside. Protobuf scopes an enum's values
  // to the enclosing namespace, so nesting is what lets `Refusal.Reason` values
  // coexist across messages — and it means a nested enum has no heading of its
  // own to link to.
  const protoEnumOwner: Record<string, string> = Object.fromEntries(
    protoMessages.flatMap((message) =>
      message.enums.map((declared) => [declared.name, message.name] as const),
    ),
  );

  // Which proto types the adapter file declares, as opposed to the generated
  // model. Messages and enums only: the file also declares the option extensions
  // that carry its grouping, and those are fields rather than types.
  const adapterOwned: Set<string> = (() => {
    const owned = new Set<string>();
    const namespace = root.lookup("rift.adapter");
    if (namespace instanceof protobuf.Namespace) {
      for (const child of namespace.nestedArray) {
        if (child instanceof protobuf.Type || child instanceof protobuf.Enum) owned.add(child.name);
      }
    }
    return owned;
  })();

  // The messages that exist only because protobuf forbids a repeated field
  // directly inside a `oneof`. Each is one repeated field and nothing else, so
  // there is nothing to read on a page of its own.
  //
  // Recognised by shape rather than by name, so a new one costs nothing, and
  // scoped to the adapter file for the same reason: `FilterAll` and
  // `OriginMappingExact` are shaped alike but are model types with meaning of
  // their own. A leading comment is disqualifying — a wrapper someone found worth
  // explaining is no longer plumbing.
  const protoWrappers: Set<string> = new Set(
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

  const scipMessages = protoMessages.filter(
    (message) => message.package === "scip" || message.package === "rift.scip",
  );
  const scipEnums = protoEnums.filter(
    (declared) => declared.package === "scip" || declared.package === "rift.scip",
  );

  // The groups `adapter.proto` divides itself into, read from the option each
  // declaration carries:
  //
  //   message AdapterState {
  //     option (section) = "Adapter state";
  //
  // Declaring it in the file is the same reason `mcp.json` carries `rift:axes`:
  // a grouping kept in the documentation code is a second place to edit, and it
  // drifts silently when a message moves. Sections appear in the order they are
  // first declared, so the page reads in file order.
  const protoSections: ProtoSection[] = (() => {
    const namespace = root.lookup("rift.adapter");
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

  return {
    adapterServices,
    scipServices,
    protoMessages,
    protoEnums,
    protoEnumOwner,
    adapterOwned,
    protoWrappers,
    protoTypeNames: [...types.keys()],
    scipMessages,
    scipEnums,
    protoSections,
  };
}

const loaded = new Map<DocVersion, ProtoData>();

/** One version's packages, loaded once per build. */
export function protoFor(version: DocVersion): ProtoData {
  let data = loaded.get(version);
  if (!data) {
    data = load(version);
    loaded.set(version, data);
  }
  return data;
}
