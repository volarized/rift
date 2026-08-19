/**
 * Loads one version's generated Protobuf packages. MCP uses JSON at the agent
 * boundary. The server and SCIP export use the packages loaded here.
 * `protoFor(version)` reads the live `protocol/` tree for the draft and the
 * frozen `protocol/versions/<date>/` snapshot for a dated version.
 */

import { isAbsolute, join } from "node:path";
import protobuf from "protobufjs";
import { protocolDirFor } from "@/lib/protocol-dir";
import type { DocVersion } from "@/lib/versions";

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

/** One version's loaded packages and everything derived from them. */
export interface ProtoData {
  scipServices: ProtoService[];
  protoMessages: ProtoMessage[];
  protoEnums: ProtoEnum[];
  protoEnumOwner: Record<string, string>;
  protoTypeNames: string[];
  scipMessages: ProtoMessage[];
  scipEnums: ProtoEnum[];
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

  const scipMessages = protoMessages.filter(
    (message) => message.package === "scip" || message.package === "rift.scip",
  );
  const scipEnums = protoEnums.filter(
    (declared) => declared.package === "scip" || declared.package === "rift.scip",
  );

  return {
    scipServices,
    protoMessages,
    protoEnums,
    protoEnumOwner,
    protoTypeNames: [...types.keys()],
    scipMessages,
    scipEnums,
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
