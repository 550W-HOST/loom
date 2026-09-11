import { z } from "zod";

export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

export type JsonSchema = { [key: string]: JsonValue };

export function isZodSchema(value: unknown): value is z.ZodType {
  return (
    typeof value === "object" &&
    value !== null &&
    "parse" in value &&
    "_zod" in value
  );
}

/**
 * Inline a fallback conversion's `$defs`, cutting self-referential cycles
 * with an unconstrained schema.
 *
 * zod cannot inline a recursive schema, so those conversions fall back to
 * `$ref`/`$defs`. Expanding the table keeps every artifact self-contained
 * without a cross-document `$ref` surface the Rust validator has to resolve;
 * only the recursion points lose precision.
 */
function inlineDefs(root: JsonSchema): JsonSchema {
  const defs = root.$defs as Record<string, JsonSchema> | undefined;
  const visiting = new Set<string>();
  const walk = (node: JsonValue): JsonValue => {
    if (Array.isArray(node)) return node.map(walk);
    if (node !== null && typeof node === "object") {
      const ref = node.$ref;
      if (typeof ref === "string") {
        if (ref === "#") return {};
        if (ref.startsWith("#/$defs/")) {
          const name = ref.slice("#/$defs/".length);
          if (visiting.has(name)) return {};
          const target = defs?.[name];
          if (target) {
            visiting.add(name);
            const expanded = walk(target);
            visiting.delete(name);
            return expanded;
          }
        }
      }
      const out: { [key: string]: JsonValue } = {};
      for (const key of Object.keys(node)) {
        if (key === "$defs") continue;
        out[key] = walk(node[key] as JsonValue);
      }
      return out;
    }
    return node;
  };
  return walk(root) as JsonSchema;
}

/**
 * Convert a zod schema to a self-contained JSON Schema 2020-12 document.
 *
 * `$refStrategy: "none"` inlines every definition so the artifact never needs
 * cross-file `$ref` resolution. A recursive schema cannot be inlined; it is
 * retried with `"root"` and then expanded with cycles cut.
 */
export function zodToJsonSchema(
  schema: z.ZodType,
  io: "input" | "output",
): JsonSchema {
  const options = {
    io,
    unrepresentable: "any" as const,
    target: "draft-2020-12" as const,
    reused: "inline" as const,
  };
  let converted: JsonSchema;
  try {
    converted = z.toJSONSchema(schema, {
      ...options,
      cycles: "throw",
    }) as JsonSchema;
  } catch {
    converted = z.toJSONSchema(schema, {
      ...options,
      cycles: "ref",
    }) as JsonSchema;
  }
  delete converted.$schema;
  if (converted.$defs) converted = inlineDefs(converted);
  return converted;
}

/**
 * Look up a schema by export name across a set of modules and convert it.
 *
 * Throws when the name is missing so a renamed bb export fails the export
 * loudly instead of silently dropping the message from the contract.
 */
export function schemaFrom(
  modules: ReadonlyArray<{ label: string; module: Record<string, unknown> }>,
  name: string,
  io: "input" | "output",
): JsonSchema {
  for (const { module } of modules) {
    const candidate = module[name];
    if (isZodSchema(candidate)) {
      return zodToJsonSchema(candidate, io);
    }
  }
  throw new Error(
    `bb contract export \`${name}\` not found in ${modules
      .map((m) => m.label)
      .join(", ")}`,
  );
}

/** Sort object keys recursively so regenerated artifacts are byte-stable. */
export function stableJson(value: JsonValue): JsonValue {
  if (Array.isArray(value)) return value.map(stableJson);
  if (value !== null && typeof value === "object") {
    const out: { [key: string]: JsonValue } = {};
    for (const key of Object.keys(value).sort()) {
      out[key] = stableJson(value[key] as JsonValue);
    }
    return out;
  }
  return value;
}
