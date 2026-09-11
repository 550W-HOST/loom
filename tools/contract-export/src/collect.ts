import { schemaFrom, zodToJsonSchema, type JsonSchema } from "./zod-schema.js";
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
