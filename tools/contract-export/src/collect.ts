import {
  schemaFrom,
  zodToJsonSchema,
  type JsonSchema,
  type JsonValue,
} from "./zod-schema.js";
import type { ResponseSchemaResult } from "./ts-schema.js";
import { buildResponseSchemas, type ResponseAlias } from "./ts-schema.js";

export interface BbModules {
  serverContract: Record<string, unknown>;
  domain: Record<string, unknown>;
  hostDaemonContract: Record<string, unknown>;
  labels: { label: string; module: Record<string, unknown> }[];
}

/** Load the contract packages that were copied into the scratch tree. */
export async function loadBbModules(root: string): Promise<BbModules> {
  const serverContract = (await import(
    Bun.resolveSync("@bb/server-contract", root)
  )) as Record<string, unknown>;
  const domain = (await import(
    Bun.resolveSync("@bb/domain", root)
  )) as Record<string, unknown>;
  const hostDaemonContract = (await import(
    Bun.resolveSync("@bb/host-daemon-contract", root)
  )) as Record<string, unknown>;
  return {
    serverContract,
    domain,
    hostDaemonContract,
    labels: [
      { label: "@bb/server-contract", module: serverContract },
      { label: "@bb/domain", module: domain },
      { label: "@bb/host-daemon-contract", module: hostDaemonContract },
    ],
  };
}

export interface HttpRouteModel {
  id: string;
  method: string;
  path: string;
  request: { source: string; schema: JsonSchema | null };
  responses: { status: number; format: string; schema: JsonSchema | null }[];
}

interface RuntimeRoute {
  path: string;
  method: string;
  request: { source: string; schema?: unknown };
  response:
    | { status: number; format: string }
    | { status: number; format: string }[];
}

function routeId(namespace: string, key: string): string {
  return `${namespace}.${key}`;
}

/**
 * Walk `publicApiRoutes` and pair every request (zod, converted directly) with
 * the response type resolved through the contract's indexed type.
 */
export function collectHttpRoutes(
  modules: BbModules,
  workDir: string,
): { routes: HttpRouteModel[]; responseSchemas: ResponseSchemaResult } {
  const raw: { id: string; route: RuntimeRoute }[] = [];
  const groups = modules.serverContract.publicApiRoutes as Record<
    string,
    Record<string, RuntimeRoute>
  >;
  for (const namespace of Object.keys(groups).sort()) {
    const group = groups[namespace]!;
    for (const key of Object.keys(group).sort()) {
      raw.push({ id: routeId(namespace, key), route: group[key]! });
    }
  }
  raw.sort((a, b) => a.id.localeCompare(b.id));

  const aliases: ResponseAlias[] = raw.map(({ id, route }, index) => ({
    alias: `R${index}`,
    path: route.path,
    method: route.method,
  }));
  const responseSchemas = buildResponseSchemas(workDir, aliases);

  const routes: HttpRouteModel[] = raw.map(({ id, route }, index) => {
    const requestSchema =
      route.request.schema !== undefined
        ? zodToJsonSchema(route.request.schema as never, "input")
        : null;
    const responses = (Array.isArray(route.response)
      ? route.response
      : [route.response]
    ).map((descriptor) => ({
      status: descriptor.status,
      format: descriptor.format,
      schema:
        descriptor.format === "json"
          ? (responseSchemas.schemas.get(`R${index}`) ?? null)
          : null,
    }));
    return {
      id,
      method: route.method.toUpperCase(),
      path: route.path,
      request: { source: route.request.source, schema: requestSchema },
      responses,
    };
  });

  return { routes, responseSchemas };
}

export interface ProtocolModel {
  id: string;
  endpoint: string;
  subprotocol?: string;
  clientToServer: { name: string; schema: JsonSchema }[];
  serverToClient: { name: string; schema: JsonSchema }[];
}

export interface ThreadEventModel {
  schema: JsonSchema;
  eventTypes: string[];
  schemasByType: Record<string, JsonSchema>;
}

function isJsonObject(value: JsonValue | undefined): value is JsonSchema {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function unionBranches(schema: JsonSchema): JsonSchema[] {
  for (const key of ["anyOf", "oneOf"] as const) {
    const value = schema[key];
    if (!Array.isArray(value)) continue;
    return value.filter(isJsonObject);
  }
  return [];
}

function discriminantValue(schema: JsonSchema): string | undefined {
  const properties = isJsonObject(schema.properties)
    ? schema.properties
    : undefined;
  const type = properties && isJsonObject(properties.type)
    ? properties.type
    : undefined;
  return typeof type?.const === "string" ? type.const : undefined;
}

/**
 * Extract the top-level discriminated-union branches without walking into
 * nested item unions. The generated ThreadEvent schema has one provider and
 * one system union, each containing the event branches.
 */
function discriminatedBranches(schema: JsonSchema): JsonSchema[] {
  const type = discriminantValue(schema);
  if (type) return [schema];
  return unionBranches(schema).flatMap(discriminatedBranches);
}

/**
 * Build the event index from the converted runtime schema and bb's exported
 * type inventory. Both are checked so a newly added event cannot disappear
 * silently from the artifact.
 */
export function collectThreadEvents(
  schema: JsonSchema,
  rawEventTypes: unknown,
): ThreadEventModel {
  if (
    !Array.isArray(rawEventTypes) ||
    !rawEventTypes.every((value): value is string => typeof value === "string")
  ) {
    throw new Error("bb domain does not export a valid thread event type list");
  }

  const eventTypes = [...rawEventTypes];
  if (new Set(eventTypes).size !== eventTypes.length) {
    throw new Error("bb thread event type list contains duplicates");
  }

  const schemasByType: Record<string, JsonSchema> = {};
  for (const branch of discriminatedBranches(schema)) {
    const type = discriminantValue(branch);
    if (!type) {
      throw new Error("ThreadEvent contains a union branch without a type literal");
    }
    if (!eventTypes.includes(type)) {
      throw new Error(`ThreadEvent schema contains unlisted event type \`${type}\``);
    }
    if (schemasByType[type]) {
      throw new Error(`ThreadEvent schema contains duplicate event type \`${type}\``);
    }
    schemasByType[type] = branch;
  }

  const missing = eventTypes.filter((type) => !schemasByType[type]);
  if (missing.length > 0) {
    throw new Error(
      `ThreadEvent type list contains schema conversion gaps: ${missing.join(", ")}`,
    );
  }
  if (Object.keys(schemasByType).length !== eventTypes.length) {
    throw new Error("ThreadEvent schema branch count does not match its type list");
  }

  return { schema, eventTypes, schemasByType };
}

/** The three WebSocket surfaces bb exposes, each with its message schemas. */
export function collectProtocols(modules: BbModules): ProtocolModel[] {
  const { labels, domain, serverContract, hostDaemonContract } = modules;

  return [
    {
      id: "client",
      endpoint: "/ws",
      clientToServer: [
        {
          name: "clientMessage",
          schema: schemaFrom(labels, "clientMessageSchema", "input"),
        },
      ],
      serverToClient: [
        { name: "pong", schema: schemaFrom(labels, "pongMessageSchema", "output") },
        {
          name: "changed",
          schema: schemaFrom(labels, "changedMessageSchema", "output"),
        },
        {
          name: "changedLenient",
          schema: schemaFrom(labels, "changedMessageLenientSchema", "output"),
        },
      ],
    },
    {
      id: "terminal",
      endpoint: "/ws/terminals/:terminalId",
      clientToServer: [
        {
          name: "terminalClientMessage",
          schema: schemaFrom(labels, "terminalClientMessageSchema", "input"),
        },
      ],
      serverToClient: [
        {
          name: "terminalServerMessage",
          schema: schemaFrom(labels, "terminalServerMessageSchema", "output"),
        },
      ],
    },
    {
      id: "host-daemon",
      endpoint: "/internal/ws",
      subprotocol: "bb-host-daemon.v1",
      // The daemon connects outbound and the server pushes commands down the
      // same socket; names follow the bb schema, not the socket's role.
      clientToServer: [
        {
          name: "hostDaemonDaemonWsMessage",
          schema: schemaFrom(labels, "hostDaemonDaemonWsMessageSchema", "input"),
        },
      ],
      serverToClient: [
        {
          name: "hostDaemonServerWsMessage",
          schema: schemaFrom(labels, "hostDaemonServerWsMessageSchema", "output"),
        },
      ],
    },
  ];
  void domain;
  void serverContract;
  void hostDaemonContract;
}

/** Every zod schema the daemon contract package exports, converted. */
export function collectNamedSchemas(
  modules: BbModules,
  packages: ("hostDaemonContract" | "serverContract" | "domain")[],
  io: "input" | "output" = "output",
): { schemas: Record<string, JsonSchema>; failures: string[] } {
  const schemas: Record<string, JsonSchema> = {};
  const failures: string[] = [];
  for (const pkg of packages) {
    const module = modules[pkg];
    for (const name of Object.keys(module).sort()) {
      const value = module[name];
      if (
        typeof value !== "object" ||
        value === null ||
        !("parse" in value) ||
        !("_zod" in value)
      ) {
        continue;
      }
      try {
        schemas[name] = zodToJsonSchema(value as never, io);
      } catch (error) {
        failures.push(`${pkg}.${name}: ${(error as Error).message}`);
      }
    }
  }
  return { schemas, failures };
}
