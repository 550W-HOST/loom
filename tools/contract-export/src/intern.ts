import { createHash } from "node:crypto";
import type { JsonValue } from "./zod-schema.js";

/**
 * Structural interning: replace repeated subtrees with `$ref`s into a single
 * top-level `$defs` map.
 *
 * bb's contract repeats the same domain shapes across hundreds of routes and
 * messages, so inlining every one of them produces a multi-megabyte artifact
 * whose diffs are dominated by duplicated text. Interning is purely structural
 * — it does not need to know which subtrees are schemas — and the references
 * it emits are resolved by the Rust validator (`#/$defs/<name>`).
 */

const MIN_WEIGHT = 8;
const HASH_LENGTH = 16;

function isContainer(
  value: JsonValue,
): value is JsonValue[] | { [key: string]: JsonValue } {
  return typeof value === "object" && value !== null;
}

/**
 * Keywords that only ever appear on a JSON Schema node.
 *
 * Interning is restricted to these nodes: a structural descriptor such as a
 * route's `{ source, schema }` wrapper must stay a real object, because typed
 * consumers (the Rust contract) read its fields directly.
 */
const SCHEMA_KEYWORDS = new Set([
  "type",
  "anyOf",
  "allOf",
  "oneOf",
  "not",
  "const",
  "enum",
  "items",
  "prefixItems",
  "properties",
  "required",
  "additionalProperties",
  "minItems",
  "maxItems",
  "minLength",
  "maxLength",
  "minimum",
  "maximum",
  "exclusiveMinimum",
  "exclusiveMaximum",
  "pattern",
  "uniqueItems",
]);

function looksLikeSchema(value: JsonValue): boolean {
  if (!isContainer(value) || Array.isArray(value)) return false;
  return Object.keys(value).some((key) => SCHEMA_KEYWORDS.has(key));
}

function digest(raw: string): string {
  return createHash("sha1").update(raw).digest("hex").slice(0, HASH_LENGTH);
}

interface NodeIndex {
  counts: Map<string, number>;
  representative: Map<string, JsonValue>;
  weight: Map<string, number>;
  hashes: Map<object, string>;
}

function indexNode(value: JsonValue, idx: NodeIndex): string {
  if (Array.isArray(value)) {
    let raw = "[";
    let weight = 1;
    for (const item of value) {
      const childHash = indexNode(item, idx);
      raw += childHash + ",";
      weight += idx.weight.get(childHash) ?? 1;
    }
    raw += "]";
    return record(value, digest(raw), weight, idx);
  }
  if (isContainer(value)) {
    const keys = Object.keys(value).sort();
    let raw = "{";
    let weight = 1;
    for (const key of keys) {
      const childHash = indexNode(value[key] as JsonValue, idx);
      raw += `${key}:${childHash},`;
      weight += idx.weight.get(childHash) ?? 1;
    }
    raw += "}";
    return record(value, digest(raw), weight, idx);
  }
  const hash = digest(`${typeof value}:${JSON.stringify(value)}`);
  idx.counts.set(hash, (idx.counts.get(hash) ?? 0) + 1);
  idx.weight.set(hash, 1);
  return hash;
}

function record(
  value: JsonValue,
  hash: string,
  weight: number,
  idx: NodeIndex,
): string {
  idx.hashes.set(value as object, hash);
  idx.counts.set(hash, (idx.counts.get(hash) ?? 0) + 1);
  idx.weight.set(hash, weight);
  if (!idx.representative.has(hash)) idx.representative.set(hash, value);
  return hash;
}

export function internSubtrees(document: JsonValue): JsonValue {
  const idx: NodeIndex = {
    counts: new Map(),
    representative: new Map(),
    weight: new Map(),
    hashes: new Map(),
  };
  indexNode(document, idx);

  const solid = [...idx.counts.keys()]
    .filter((hash) => (idx.counts.get(hash) ?? 0) >= 2)
    .filter((hash) => (idx.weight.get(hash) ?? 0) >= MIN_WEIGHT)
    .filter((hash) => looksLikeSchema(idx.representative.get(hash)!))
    .sort();

  if (solid.length === 0) return document;

  const nameOf = new Map<string, string>();
  solid.forEach((hash, position) => nameOf.set(hash, `d${position}`));

  /**
   * Render a subtree, substituting interned `$ref`s only where the JSON
   * Schema grammar allows a schema.
   *
   * The `properties` keyword is the reason this is not a plain tree walk: its
   * value is a map of property names to schemas, not a schema itself. Treating
   * it as one lets the intern pass replace the whole map with a `$ref` to a
   * sibling schema it happens to be structurally equal to, producing
   * `"properties": { "$ref": "..." }` — a shape no validator can read, and
   * one that silently rejects every valid instance. So a `properties` map is
   * emitted literally, while each of its values recurses as a schema.
   */
  const render = (
    value: JsonValue,
    selfHash: string | null,
    asSchema: boolean,
  ): JsonValue => {
    if (Array.isArray(value)) {
      return value.map((item) => render(item, selfHash, true));
    }
    if (isContainer(value)) {
      const hash = idx.hashes.get(value as object)!;
      if (asSchema && hash !== selfHash && nameOf.has(hash)) {
        return { $ref: `#/$defs/${nameOf.get(hash)}` } as JsonValue;
      }
      const out: { [key: string]: JsonValue } = {};
      for (const key of Object.keys(value).sort()) {
        const child = value[key] as JsonValue;
        if (key === "properties" && isContainer(child) && !Array.isArray(child)) {
          const map: { [key: string]: JsonValue } = {};
          for (const name of Object.keys(child).sort()) {
            map[name] = render(child[name] as JsonValue, selfHash, true);
          }
          out[key] = map;
          continue;
        }
        out[key] = render(child, selfHash, true);
      }
      return out;
    }
    return value;
  };

  const renderSchema = (value: JsonValue, selfHash: string | null): JsonValue =>
    render(value, selfHash, true);

  const defs: { [key: string]: JsonValue } = {};
  for (const hash of solid) {
    defs[nameOf.get(hash)!] = renderSchema(idx.representative.get(hash)!, hash);
  }

  const root = renderSchema(document, null) as { [key: string]: JsonValue };
  root.$defs = defs;
  return root;
}
