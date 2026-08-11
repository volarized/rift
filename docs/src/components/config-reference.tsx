import { Fragment, type ReactNode } from "react";
import { Prose } from "@/components/protocol/prose";
import { Constraints } from "@/components/protocol/type-expr";
import { type ConfigSchema, configDefs, configDocument, workspaceConfig } from "@/lib/config";
import { protoFor } from "@/lib/proto";
import { branches, props, refName, sub } from "@/lib/protocol";
import { hrefFor } from "@/lib/protocol-surface";
import { DRAFT } from "@/lib/versions";

const BASE_PATH = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

function protocolHref(name: string): string {
  return protoFor(DRAFT).adapterOwned.has(name)
    ? `${BASE_PATH}/docs/${DRAFT}/protocol/adapter#msg-${name}`
    : hrefFor(DRAFT, name);
}

function anchor(name: string): string {
  return `config-type-${name.replace(/([a-z0-9])([A-Z])/g, "$1-$2").toLowerCase()}`;
}

function Literal({ value }: { value: unknown }): ReactNode {
  return (
    <code className="text-[0.875em]">
      {typeof value === "string" ? value : JSON.stringify(value)}
    </code>
  );
}

function Joined({ values }: { values: readonly ReactNode[] }): ReactNode {
  return values.map((value, index) => (
    // biome-ignore lint/suspicious/noArrayIndexKey: positional schema expression
    <Fragment key={index}>
      {index > 0 ? <span className="text-fd-muted-foreground"> | </span> : null}
      {value}
    </Fragment>
  ));
}

function ConfigType({ schema }: { schema: ConfigSchema | undefined }): ReactNode {
  if (!schema) return <Literal value="any" />;
  const name = refName(schema);
  if (name) {
    const target = configDefs[name];
    const href = target?.["rift:package"] ? hrefFor(DRAFT, name) : `#${anchor(name)}`;
    return (
      <a href={href} className="font-mono text-[0.875em] no-underline hover:underline">
        {name}
      </a>
    );
  }
  if ("const" in schema) return <Literal value={schema.const} />;
  if (schema.enum) {
    return (
      <Joined values={schema.enum.map((value) => <Literal key={String(value)} value={value} />)} />
    );
  }
  const union = branches(schema);
  if (union) {
    return (
      <Joined
        values={union.list.map((branch, index) => (
          // biome-ignore lint/suspicious/noArrayIndexKey: positional schema expression
          <ConfigType key={index} schema={branch as ConfigSchema} />
        ))}
      />
    );
  }
  if (schema.type === "array") {
    return (
      <>
        <span className="text-fd-muted-foreground">array of </span>
        <ConfigType schema={sub(schema.items) as ConfigSchema | undefined} />
      </>
    );
  }
  if (schema.type === "object" || schema.properties) {
    const value = sub(schema.additionalProperties) as ConfigSchema | undefined;
    return value ? (
      <>
        <span className="text-fd-muted-foreground">map string to </span>
        <ConfigType schema={value} />
      </>
    ) : (
      <Literal value="table" />
    );
  }
  if (typeof schema.type === "string") return <Literal value={schema.type} />;
  return <Literal value="any" />;
}

function ProtocolBound({ schema }: { schema: ConfigSchema }): ReactNode {
  const field = schema["rift:bounds"] ?? schema["rift:prefixOf"];
  const type = schema["rift:selectsType"] ?? schema["rift:describesAs"];
  const name = field?.model ?? type;
  if (!name) return null;
  return (
    <span className="mt-1 block text-fd-muted-foreground text-xs">
      Protocol seam: <a href={protocolHref(name)}>{field ? `${name}.${field.field}` : name}</a>
    </span>
  );
}

function PropertyTable({ schema }: { schema: ConfigSchema }): ReactNode {
  const properties = props(schema) as [string, ConfigSchema][];
  if (properties.length === 0) return null;
  const required = new Set(schema.required ?? []);
  const hasDefaults = properties.some(([, property]) => property.default !== undefined);
  return (
    <div className="my-4 overflow-x-auto">
      <table className="my-0 w-full text-sm">
        <thead>
          <tr>
            <th className="text-left">Property</th>
            <th className="text-left">Type</th>
            {hasDefaults ? <th className="text-left">Default</th> : null}
            <th className="text-left">Description</th>
          </tr>
        </thead>
        <tbody>
          {properties.map(([name, property]) => (
            <tr key={name} className="align-top">
              <td className="whitespace-nowrap">
                <code className="text-[0.875em]">{name}</code>
                {required.has(name) ? (
                  <sup className="ml-1 text-fd-muted-foreground text-[0.7em] uppercase">req</sup>
                ) : null}
              </td>
              <td>
                <span className="grid gap-0.5">
                  <span>
                    <ConfigType schema={property} />
                  </span>
                  <Constraints schema={property} />
                </span>
              </td>
              {hasDefaults ? (
                <td>
                  {property.default !== undefined ? (
                    <Literal value={property.default} />
                  ) : (
                    <span className="text-fd-muted-foreground">—</span>
                  )}
                </td>
              ) : null}
              <td>
                {property.description ? <Prose text={property.description} /> : "—"}
                <ProtocolBound schema={property} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function dereference(schema: ConfigSchema): ConfigSchema {
  const name = refName(schema);
  return name ? configDefs[name] : schema;
}

export function ConfigFile(): ReactNode {
  return (
    <pre className="overflow-x-auto">
      <code className="language-toml">{workspaceConfig}</code>
    </pre>
  );
}

export function ConfigReference(): ReactNode {
  const rootProperties = props(configDocument) as [string, ConfigSchema][];
  const localValueTypes = Object.entries(configDefs).filter(
    ([, schema]) =>
      !schema["rift:package"] &&
      !rootProperties.some(([, root]) => refName(root) === schema.title) &&
      !schema.properties,
  );
  return (
    <>
      {rootProperties.map(([name, property]) => {
        const tableSchema = dereference(
          (sub(property.additionalProperties) as ConfigSchema | undefined) ?? property,
        );
        return (
          <section key={name}>
            <h3 id={`schema-${name}`}>{name}</h3>
            {tableSchema.description ? (
              <p>
                <Prose text={tableSchema.description} />
              </p>
            ) : null}
            <PropertyTable schema={tableSchema} />
            <ProtocolBound schema={property} />
          </section>
        );
      })}
      <h3>Value types</h3>
      <dl>
        {localValueTypes.map(([name, schema]) => (
          <Fragment key={name}>
            <dt id={anchor(name)} className="mt-4 font-semibold">
              <code>{name}</code>
            </dt>
            <dd className="ml-0">
              <ConfigType schema={schema} /> <Constraints schema={schema} />
              {schema.description ? (
                <span className="block">
                  <Prose text={schema.description} />
                </span>
              ) : null}
            </dd>
          </Fragment>
        ))}
      </dl>
    </>
  );
}
