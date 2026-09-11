import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { join, resolve } from "node:path";
import {
  bbSourceCommit,
  materializeBbRuntime,
  CONTRACT_PACKAGES,
} from "./bb-runtime.js";
import {
  collectHttpRoutes,
  collectNamedSchemas,
  collectProtocols,
  collectThreadEvents,
  loadBbModules,
  type BbModules,
  type HttpRouteModel,
  type ProtocolModel,
  type ThreadEventModel,
} from "./collect.js";
import { collectErrorCodes } from "./error-codes.js";
import { internSubtrees } from "./intern.js";
import {
  isZodSchema,
  schemaFrom,
  stableJson,
  zodToJsonSchema,
  type JsonSchema,
  type JsonValue,
} from "./zod-schema.js";

const FORMAT = "loom.bb-contract/v1";
const JSON_SCHEMA_DIALECT = "https://json-schema.org/draft/2020-12/schema";
/** `publicApiRoutes` paths are relative to this mount point (apps/server). */
const PUBLIC_API_PREFIX = "/api/v1";
const TOOL_DIR = resolve(import.meta.dir, "..");

interface Args {
  bbSrc: string;
  outDir: string;
  workDir: string;
}

function parseArgs(argv: string[]): Args {
  const get = (flag: string): string | undefined => {
    const index = argv.indexOf(flag);
    return index === -1 ? undefined : argv[index + 1];
  };
  const bbSrc = get("--bb") ?? process.env.BB_SRC;
  if (!bbSrc) {
    throw new Error(
      "usage: bun run src/index.ts --bb <path-to-bb-checkout> [--out <dir>]",
    );
  }
  return {
    bbSrc: resolve(bbSrc),
    outDir: resolve(
      get("--out") ?? join(TOOL_DIR, "..", "..", "contracts", "bb"),
    ),
    workDir: resolve(get("--work") ?? join(TOOL_DIR, ".work")),
  };
}

function asJson(value: unknown): JsonValue {
  return JSON.parse(JSON.stringify(value)) as JsonValue;
}

/** Convert a plain object of zod schemas (a "schema by type" map). */
function convertSchemaMap(
  value: unknown,
  io: "input" | "output",
): Record<string, JsonSchema> {
  const out: Record<string, JsonSchema> = {};
  if (typeof value !== "object" || value === null) return out;
  for (const key of Object.keys(value as Record<string, unknown>).sort()) {
    const member = (value as Record<string, unknown>)[key];
    if (isZodSchema(member)) {
      out[key] = zodToJsonSchema(member, io);
    }
  }
  return out;
}

/** Literal `type` values of a zod discriminated union, for documentation. */
function discriminatorValues(schema: unknown): string[] {
  const options = (schema as { options?: unknown[] } | undefined)?.options;
  if (!Array.isArray(options)) return [];
  const values: string[] = [];
  for (const option of options) {
    const shape = (option as { shape?: Record<string, unknown> }).shape;
    const typeSchema = shape?.type as { value?: unknown } | undefined;
    if (typeof typeSchema?.value === "string") values.push(typeSchema.value);
  }
  return values.sort();
}

function requiredSchema(
  schemas: Record<string, JsonSchema>,
  name: string,
): JsonSchema {
  const schema = schemas[name];
  if (!schema) throw new Error(`missing contract schema \`${name}\``);
  return schema;
}

function buildServerApi(
  routes: HttpRouteModel[],
  allSchemas: Record<string, JsonSchema>,
): JsonValue {
  return asJson({
    $schema: JSON_SCHEMA_DIALECT,
    "x-loom-contract-format": FORMAT,
    kind: "bb-http",
    mountPath: PUBLIC_API_PREFIX,
    errorResponse: requiredSchema(allSchemas, "apiErrorSchema"),
    lifecycleErrors: requiredSchema(allSchemas, "lifecycleApiErrorSchema"),
    routes: routes.map((route) => ({
      id: route.id,
      method: route.method,
      path: route.path,
      fullPath: `${PUBLIC_API_PREFIX}${route.path}`,
      request: route.request,
      responses: route.responses,
    })),
  });
}

function buildClientWs(protocols: ProtocolModel[], modules: BbModules): JsonValue {
  const clientFacing = protocols.filter((p) => p.id !== "host-daemon");
  const changeKinds = {
    thread: (modules.domain.THREAD_CHANGE_KINDS as string[]) ?? [],
    project: (modules.domain.PROJECT_CHANGE_KINDS as string[]) ?? [],
    environment: (modules.domain.ENVIRONMENT_CHANGE_KINDS as string[]) ?? [],
    host: (modules.domain.HOST_CHANGE_KINDS as string[]) ?? [],
    system: (modules.domain.SYSTEM_CHANGE_KINDS as string[]) ?? [],
  };
  return asJson({
    $schema: JSON_SCHEMA_DIALECT,
    "x-loom-contract-format": FORMAT,
    kind: "bb-client-ws",
    protocols: clientFacing.map((protocol) => ({
      id: protocol.id,
      endpoint: protocol.endpoint,
      ...(protocol.subprotocol ? { subprotocol: protocol.subprotocol } : {}),
      clientToServer: protocol.clientToServer,
      serverToClient: protocol.serverToClient,
    })),
    subscriptionTarget: schemaFrom(
      modules.labels,
      "realtimeSubscriptionTargetSchema",
      "input",
    ),
    changeKinds,
  });
}

function buildHostDaemon(
  daemonProtocol: ProtocolModel,
  modules: BbModules,
  allSchemas: Record<string, JsonSchema>,
): JsonValue {
  const hdc = modules.hostDaemonContract;
  const pick = (name: string): JsonSchema | undefined => allSchemas[name];
  const orNull = (name: string): JsonSchema | null => pick(name) ?? null;
  const settledTypes = discriminatorValues(hdc.hostDaemonCommandSchema);
  const onlineRpcTypes = discriminatorValues(hdc.hostDaemonOnlineRpcCommandSchema);
  const resultsByType = convertSchemaMap(
    hdc.hostDaemonCommandResultSchemaByType,
    "output",
  );
  const onlineRpcResultsByType = convertSchemaMap(
    hdc.hostDaemonOnlineRpcResultSchemaByType,
    "output",
  );

  return asJson({
    $schema: JSON_SCHEMA_DIALECT,
    "x-loom-contract-format": FORMAT,
    kind: "bb-host-daemon",
    protocolVersion: hdc.HOST_DAEMON_PROTOCOL_VERSION ?? null,
    websocket: {
      id: daemonProtocol.id,
      endpoint: daemonProtocol.endpoint,
      subprotocol: daemonProtocol.subprotocol,
      clientToServer: daemonProtocol.clientToServer,
      serverToClient: daemonProtocol.serverToClient,
    },
    commands: {
      settled: requiredSchema(allSchemas, "hostDaemonCommandSchema"),
      onlineRpc: requiredSchema(allSchemas, "hostDaemonOnlineRpcCommandSchema"),
      rpc: requiredSchema(allSchemas, "hostDaemonRpcCommandSchema"),
      settledTypes,
      onlineRpcTypes,
      rpcTypes: [...new Set([...settledTypes, ...onlineRpcTypes])].sort(),
      resultsByType,
      onlineRpcResultsByType,
    },
    http: {
      enrollRequest: orNull("hostDaemonEnrollRequestSchema"),
      enrollResponse: orNull("hostDaemonEnrollResponseSchema"),
      enrollKeyRequest: orNull("hostDaemonEnrollKeyRequestSchema"),
      enrollKeyResponse: orNull("hostDaemonEnrollKeyResponseSchema"),
      sessionOpenRequest: orNull("hostDaemonSessionOpenRequestSchema"),
      sessionOpenResponse: orNull("hostDaemonSessionOpenResponseSchema"),
    },
    events: {
      batchRequest: orNull("hostDaemonEventBatchRequestSchema"),
      batchResponse: orNull("hostDaemonEventBatchResponseSchema"),
    },
    toolCalls: {
      request: orNull("hostDaemonToolCallRequestSchema"),
      response: orNull("hostDaemonToolCallResponseSchema"),
    },
    interactions: {
      request: orNull("hostDaemonInteractiveRequestSchema"),
      response: orNull("hostDaemonInteractiveRequestResponseSchema"),
      interruptRequest: orNull("hostDaemonInteractiveInterruptRequestSchema"),
      interruptResponse: orNull("hostDaemonInteractiveInterruptResponseSchema"),
    },
    terminalOutputChunk: orNull("hostDaemonTerminalOutputChunkSchema"),
  });
}

function buildThreadEvent(model: ThreadEventModel): JsonValue {
  return asJson({
    $schema: JSON_SCHEMA_DIALECT,
    "x-loom-contract-format": FORMAT,
    kind: "bb-thread-event",
    discriminator: "type",
    schema: model.schema,
    eventTypes: model.eventTypes,
    schemasByType: model.schemasByType,
  });
}

function writeJson(
  path: string,
  value: JsonValue,
  intern = false,
): { bytes: number; sha256: string } {
  const document = intern ? internSubtrees(value) : value;
  const text = JSON.stringify(stableJson(document), null, 2) + "\n";
  writeFileSync(path, text, "utf8");
  return {
    bytes: Buffer.byteLength(text),
    sha256: createHash("sha256").update(text).digest("hex"),
  };
}

async function main(): Promise<void> {
  const args = parseArgs(process.argv.slice(2));
  const runtime = materializeBbRuntime(args.bbSrc, args.workDir);
  const modules = await loadBbModules(runtime.root);

  const { routes, responseSchemas } = collectHttpRoutes(modules, runtime.root);
  const protocols = collectProtocols(modules);
  const named = collectNamedSchemas(modules, [
    "serverContract",
    "hostDaemonContract",
    "domain",
  ]);
  const threadEvent = collectThreadEvents(
    requiredSchema(named.schemas, "threadEventSchema"),
    modules.domain.threadEventTypeValues,
  );
  const daemonProtocol = protocols.find((p) => p.id === "host-daemon")!;

  const serverApi = buildServerApi(routes, named.schemas);
  const clientWs = buildClientWs(protocols, modules);
  const hostDaemon = buildHostDaemon(daemonProtocol, modules, named.schemas);
  const errorCodes = asJson({
    $schema: JSON_SCHEMA_DIALECT,
    "x-loom-contract-format": FORMAT,
    kind: "bb-error-codes",
    note: "Best-effort inventory scanned from apps/server/src; the contract package types the error body but leaves `code` a free string.",
    codes: collectErrorCodes(args.bbSrc),
  });

  mkdirSync(args.outDir, { recursive: true });
  const files: Record<string, { bytes: number; sha256: string }> = {};
  files["server-api.json"] = writeJson(
    join(args.outDir, "server-api.json"),
    serverApi,
    true,
  );
  files["client-ws.json"] = writeJson(
    join(args.outDir, "client-ws.json"),
    clientWs,
    true,
  );
  files["host-daemon.json"] = writeJson(
    join(args.outDir, "host-daemon.json"),
    hostDaemon,
    true,
  );
  files["error-codes.json"] = writeJson(
    join(args.outDir, "error-codes.json"),
    errorCodes,
  );
  files["thread-event.json"] = writeJson(
    join(args.outDir, "thread-event.json"),
    buildThreadEvent(threadEvent),
  );

  const manifest = asJson({
    format: FORMAT,
    generator: "tools/contract-export",
    jsonSchemaDialect: JSON_SCHEMA_DIALECT,
    source: {
      repository: "https://github.com/get-bb/bb",
      commit: bbSourceCommit(args.bbSrc),
      packages: [...CONTRACT_PACKAGES],
    },
    counts: {
      httpRoutes: routes.length,
      httpRoutesWithRequestSchema: routes.filter((r) => r.request.schema).length,
      httpRoutesWithOpaqueResponse: responseSchemas.unresolved.length,
      clientProtocols: protocols.length,
      namedSchemas: Object.keys(named.schemas).length,
      threadEventTypes: threadEvent.eventTypes.length,
      errorCodes: (errorCodes as { codes: unknown[] }).codes.length,
    },
    failures: {
      schemaConversions: named.failures,
      opaqueResponseTypes: responseSchemas.unresolved,
    },
    files,
  });
  writeJson(join(args.outDir, "manifest.json"), manifest);

  rmSync(args.workDir, { recursive: true, force: true });

  console.log(`bb contract exported to ${args.outDir}`);
  console.log(JSON.stringify((manifest as { counts: unknown }).counts, null, 2));
  if (named.failures.length > 0) {
    console.warn(`\n${named.failures.length} schema conversion failure(s):`);
    for (const failure of named.failures.slice(0, 10)) console.warn(`  ${failure}`);
  }
}

await main();
