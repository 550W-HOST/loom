import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import ts from "typescript";
import type { JsonSchema } from "./zod-schema.js";

export interface ResponseAlias {
  /** Unique alias emitted into the generated TS file. */
  readonly alias: string;
  /** Route path as declared in `publicApiRoutes`. */
  readonly path: string;
  /** HTTP method, lower-case. */
  readonly method: string;
}

export interface ResponseSchemaResult {
  readonly schemas: Map<string, JsonSchema>;
  readonly unresolved: string[];
}

const HTTP_METHOD_KEY: Record<string, string> = {
  get: "$get",
  post: "$post",
  patch: "$patch",
  delete: "$delete",
  put: "$put",
};

function writeAliasFile(workDir: string, aliases: ResponseAlias[]): string {
  const lines = ['import type { PublicApiSchema } from "@bb/server-contract";', ""];
  for (const { alias, path, method } of aliases) {
    const methodKey = HTTP_METHOD_KEY[method];
    if (!methodKey) throw new Error(`unsupported method ${method}`);
    lines.push(
      `export type ${alias} = PublicApiSchema[${JSON.stringify(path)}][${JSON.stringify(methodKey)}]["output"];`,
    );
  }
  const file = join(workDir, "gen", "responses.ts");
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, lines.join("\n") + "\n");
  return file;
}

/**
 * Resolve every HTTP route's response type through the contract's own
 * `PublicApiSchema` indexed type and lower it to JSON Schema.
 *
 * Indexing by path+method means the response shape comes from the same type
 * the route descriptor advertises to TypeScript clients, so the artifact and
 * the type surface cannot drift.
 */
export function buildResponseSchemas(
  workDir: string,
  aliases: ResponseAlias[],
): ResponseSchemaResult {
  const entry = writeAliasFile(workDir, aliases);
  const options: ts.CompilerOptions = {
    strict: true,
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.NodeNext,
    moduleResolution: ts.ModuleResolutionKind.NodeNext,
    customConditions: ["source"],
    skipLibCheck: true,
    esModuleInterop: true,
    noEmit: true,
    baseUrl: resolve(workDir),
  };
  const program = ts.createProgram([entry], options);
  const checker = program.getTypeChecker();
  const source = program.getSourceFile(entry);
  if (!source) throw new Error(`failed to load ${entry}`);

  const errors = ts
    .getPreEmitDiagnostics(program)
    .filter((d) => d.file?.fileName === entry);
  if (errors.length > 0) {
    const text = errors
      .map((d) => ts.flattenDiagnosticMessageText(d.messageText, " "))
      .slice(0, 5)
      .join("; ");
    throw new Error(`contract response types do not resolve: ${text}`);
  }

  const ctx: ConvertContext = { checker, visiting: new Set(), node: source };
  const schemas = new Map<string, JsonSchema>();
  const unresolved: string[] = [];
  for (const statement of source.statements) {
    if (!ts.isTypeAliasDeclaration(statement)) continue;
    const alias = statement.name.text;
    const type = checker.getTypeFromTypeNode(statement.type);
    const converted = typeToSchema(type, ctx);
    schemas.set(alias, converted);
    if (isOpaque(converted)) unresolved.push(alias);
  }
  return { schemas, unresolved };
}

interface ConvertContext {
  checker: ts.TypeChecker;
  visiting: Set<ts.Type>;
  node: ts.SourceFile;
}

/** A schema that carries no constraints: we could not describe the type. */
function isOpaque(schema: JsonSchema): boolean {
  return Object.keys(schema).length === 0;
}

function constSchema(type: ts.Type): JsonSchema | null {
  if (type.isStringLiteral()) return { type: "string", const: type.value };
  if (type.isNumberLiteral()) return { type: "number", const: type.value };
  const flags = type.flags;
  if (flags & ts.TypeFlags.BooleanLiteral) {
    return {
      type: "boolean",
      const: (type as unknown as { intrinsicName: string }).intrinsicName === "true",
    };
  }
  return null;
}

function typeToSchema(type: ts.Type, ctx: ConvertContext): JsonSchema {
  const { checker } = ctx;
  const flags = type.flags;

  const literal = constSchema(type);
  if (literal) return literal;

  if (flags & (ts.TypeFlags.Any | ts.TypeFlags.Unknown | ts.TypeFlags.Void)) {
    return {};
  }
  if (flags & ts.TypeFlags.Never) return { not: {} };
  if (flags & ts.TypeFlags.Null) return { type: "null" };
  if (flags & ts.TypeFlags.Undefined) return { type: "null" };
  if (flags & (ts.TypeFlags.StringLike | ts.TypeFlags.TemplateLiteral)) {
    return { type: "string" };
  }
  if (flags & ts.TypeFlags.NumberLike) return { type: "number" };
  if (flags & ts.TypeFlags.BooleanLike) return { type: "boolean" };
  if (flags & ts.TypeFlags.BigIntLike) return { type: "integer" };
  if (flags & ts.TypeFlags.ESSymbolLike) return {};

  if (flags & ts.TypeFlags.Union) {
    return unionToSchema(type as ts.UnionType, ctx);
  }
  if (flags & ts.TypeFlags.Intersection) {
    const parts = (type as ts.IntersectionType).types.map((t) =>
      typeToSchema(t, ctx),
    );
    return parts.length === 1 ? parts[0]! : { allOf: parts };
  }

  if (type.isClassOrInterface() && type.symbol?.getName() === "Date") {
    return { type: "string", format: "date-time" };
  }

  // Recursive type: stop the descent rather than emit an unresolvable ref.
  if (ctx.visiting.has(type)) return {};

  ctx.visiting.add(type);
  try {
    if (checker.isArrayType(type) || checker.isTupleType(type)) {
      return arrayToSchema(type, ctx);
    }
    return objectToSchema(type, ctx);
  } finally {
    ctx.visiting.delete(type);
  }
}

function unionToSchema(type: ts.UnionType, ctx: ConvertContext): JsonSchema {
  const flags = ts.TypeFlags;
  const parts: JsonSchema[] = [];
  let nullable = false;
  for (const member of type.types) {
    if (member.flags & (flags.Null | flags.Undefined | flags.Void)) {
      nullable = true;
      continue;
    }
    parts.push(typeToSchema(member, ctx));
  }
  if (parts.length === 0) return { type: "null" };
  if (nullable) parts.push({ type: "null" });
  const unique = dedupe(parts);
  return unique.length === 1 ? unique[0]! : { anyOf: unique };
}

function dedupe(parts: JsonSchema[]): JsonSchema[] {
  const seen = new Set<string>();
  const out: JsonSchema[] = [];
  for (const part of parts) {
    const key = JSON.stringify(part);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(part);
  }
  return out;
}

function arrayToSchema(type: ts.Type, ctx: ConvertContext): JsonSchema {
  const { checker } = ctx;
  if (checker.isTupleType(type)) {
    const args = checker.getTypeArguments(type as ts.TypeReference);
    return {
      type: "array",
      prefixItems: args.map((a) => typeToSchema(a, ctx)),
      minItems: args.length,
      maxItems: args.length,
    };
  }
  const args = checker.getTypeArguments(type as ts.TypeReference);
  const item = args.length > 0 ? typeToSchema(args[0]!, ctx) : {};
  return { type: "array", items: item };
}

function objectToSchema(type: ts.Type, ctx: ConvertContext): JsonSchema {
  const { checker } = ctx;
  // A pure function type (a handler signature) has no data shape.
  if (
    type.getCallSignatures().length > 0 &&
    checker.getPropertiesOfType(type).length === 0
  ) {
    return {};
  }

  {
    const properties: Record<string, JsonSchema> = {};
    const required: string[] = [];
    for (const symbol of checker.getPropertiesOfType(type)) {
      const name = symbol.getName();
      if (name.startsWith("__@")) continue; // unique symbols / brands
      const optional = (symbol.flags & ts.SymbolFlags.Optional) !== 0;
      let propType: ts.Type;
      try {
        propType = checker.getTypeOfSymbol(symbol);
      } catch {
        continue;
      }
      if (
        propType.getCallSignatures().length > 0 &&
        propType.getProperties().length === 0
      ) {
        continue; // method
      }
      properties[name] = typeToSchema(propType, ctx);
      if (!optional) required.push(name);
    }

    const schema: JsonSchema = { type: "object" };
    const indexType = checker.getIndexTypeOfType(type, ts.IndexKind.String);
    if (Object.keys(properties).length > 0) {
      schema.properties = properties;
      if (required.length > 0) schema.required = required;
    } else if (!indexType) {
      return {};
    }

    if (indexType) {
      schema.additionalProperties =
        indexType.flags & ts.TypeFlags.Any ? true : typeToSchema(indexType, ctx);
    } else {
      schema.additionalProperties = false;
    }
    return schema;
  }
}
