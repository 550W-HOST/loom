//! A validator for the JSON Schema subset the bb contract export emits.
//!
//! The artifacts are generated with every definition inlined or interned into
//! a root `$defs` table, so the only reference form the validator has to
//! resolve is a local JSON pointer. The supported keywords are exactly those
//! the exporter can produce; anything else is ignored rather than guessed at.

use serde_json::Value;

/// One failure of an instance against a schema, with a JSON-ish path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// Validate `instance` against `schema`. `root` is the document `$ref`s are
/// resolved against (`#/$defs/...`).
pub fn validate(root: &Value, schema: &Value, instance: &Value) -> Vec<Violation> {
    let mut violations = Vec::new();
    check(root, schema, instance, "$", &mut violations);
    violations
}

/// Convenience predicate used for `anyOf`/`not` branches.
pub fn is_valid(root: &Value, schema: &Value, instance: &Value) -> bool {
    validate(root, schema, instance).is_empty()
}

/// True when the schema constrains nothing (emitted for recursive cuts).
fn is_unconstrained(schema: &Value) -> bool {
    match schema {
        Value::Bool(true) => true,
        Value::Object(map) => {
            map.is_empty()
                || (map.len() == 1 && map.contains_key("$schema"))
                || (map.len() == 2 && map.contains_key("$schema") && map.contains_key("$defs"))
        }
        _ => false,
    }
}

fn resolve_pointer<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    root.pointer(pointer)
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_matches(expected: &str, instance: &Value) -> bool {
    match expected {
        "integer" => matches!(instance, Value::Number(n) if n.is_i64() || n.is_u64()),
        "number" => instance.is_number(),
        other => json_type_name(instance) == other,
    }
}

fn check(root: &Value, schema: &Value, instance: &Value, path: &str, out: &mut Vec<Violation>) {
    let map = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            out.push(Violation {
                path: path.to_string(),
                message: "no value is allowed here".to_string(),
            });
            return;
        }
        Value::Object(map) => map,
        _ => return,
    };

    if is_unconstrained(schema) {
        return;
    }

    if let Some(Value::String(reference)) = map.get("$ref") {
        match resolve_pointer(root, reference) {
            Some(target) => check(root, target, instance, path, out),
            None => out.push(Violation {
                path: path.to_string(),
                message: format!("unresolved $ref {reference}"),
            }),
        }
        return;
    }

    if let Some(expected) = map.get("type") {
        let ok = match expected {
            Value::String(name) => type_matches(name, instance),
            Value::Array(names) => names
                .iter()
                .filter_map(Value::as_str)
                .any(|name| type_matches(name, instance)),
            _ => true,
        };
        if !ok {
            out.push(Violation {
                path: path.to_string(),
                message: format!(
                    "expected {}, found {}",
                    describe(expected),
                    json_type_name(instance)
                ),
            });
            return;
        }
    }

    if let Some(constant) = map.get("const") {
        if instance != constant {
            out.push(Violation {
                path: path.to_string(),
                message: format!("expected const {constant}, found {instance}"),
            });
        }
    }

    if let Some(Value::Array(values)) = map.get("enum") {
        if !values.iter().any(|value| value == instance) {
            out.push(Violation {
                path: path.to_string(),
                message: format!("value {instance} is not one of the allowed values"),
            });
        }
    }

    if let Some(Value::Array(branches)) = map.get("anyOf") {
        if !branches
            .iter()
            .any(|branch| is_valid(root, branch, instance))
        {
            out.push(Violation {
                path: path.to_string(),
                message: "value matches none of the anyOf branches".to_string(),
            });
        }
    }

    if let Some(Value::Array(branches)) = map.get("allOf") {
        for branch in branches {
            check(root, branch, instance, path, out);
        }
    }

    if let Some(Value::Array(branches)) = map.get("oneOf") {
        let matches = branches
            .iter()
            .filter(|branch| is_valid(root, branch, instance))
            .count();
        if matches != 1 {
            out.push(Violation {
                path: path.to_string(),
                message: format!("value matches {matches} oneOf branches, expected exactly 1"),
            });
        }
    }

    if let Some(Value::Object(not)) = map.get("not") {
        if is_valid(root, &Value::Object(not.clone()), instance) {
            out.push(Violation {
                path: path.to_string(),
                message: "value matches a disallowed shape".to_string(),
            });
        }
    }

    match instance {
        Value::Object(object) => check_object(root, map, object, path, out),
        Value::Array(items) => check_array(root, map, items, path, out),
        Value::String(text) => check_string(map, text, path, out),
        Value::Number(number) => check_number(map, number, path, out),
        _ => {}
    }
}

fn check_object(
    root: &Value,
    schema: &serde_json::Map<String, Value>,
    object: &serde_json::Map<String, Value>,
    path: &str,
    out: &mut Vec<Violation>,
) {
    if let Some(Value::Array(required)) = schema.get("required") {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                out.push(Violation {
                    path: path.to_string(),
                    message: format!("missing required property `{name}`"),
                });
            }
        }
    }

    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(properties) = properties {
        for (name, subschema) in properties {
            if let Some(value) = object.get(name) {
                check(root, subschema, value, &child_path(path, name), out);
            }
        }
    }

    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => {
            if let Some(properties) = properties {
                for name in object.keys() {
                    if !properties.contains_key(name) {
                        out.push(Violation {
                            path: child_path(path, name),
                            message: "property is not allowed here".to_string(),
                        });
                    }
                }
            }
        }
        Some(additional @ Value::Object(_)) => {
            for (name, value) in object {
                let known = properties.is_some_and(|p| p.contains_key(name));
                if !known {
                    check(root, additional, value, &child_path(path, name), out);
                }
            }
        }
        _ => {}
    }
}

fn check_array(
    root: &Value,
    schema: &serde_json::Map<String, Value>,
    items: &[Value],
    path: &str,
    out: &mut Vec<Violation>,
) {
    if let Some(min) = schema.get("minItems").and_then(Value::as_u64) {
        if (items.len() as u64) < min {
            out.push(Violation {
                path: path.to_string(),
                message: format!("expected at least {min} items, found {}", items.len()),
            });
        }
    }
    if let Some(max) = schema.get("maxItems").and_then(Value::as_u64) {
        if (items.len() as u64) > max {
            out.push(Violation {
                path: path.to_string(),
                message: format!("expected at most {max} items, found {}", items.len()),
            });
        }
    }
    if let Some(Value::Array(prefix)) = schema.get("prefixItems") {
        for (index, subschema) in prefix.iter().enumerate() {
            if let Some(value) = items.get(index) {
                check(root, subschema, value, &format!("{path}[{index}]"), out);
            }
        }
    }
    if let Some(item_schema) = schema.get("items") {
        let start = schema
            .get("prefixItems")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        for (index, value) in items.iter().enumerate().skip(start) {
            check(root, item_schema, value, &format!("{path}[{index}]"), out);
        }
    }
    if schema.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
        let mut seen: Vec<&Value> = Vec::new();
        for value in items {
            if seen.contains(&value) {
                out.push(Violation {
                    path: path.to_string(),
                    message: "array items must be unique".to_string(),
                });
                break;
            }
            seen.push(value);
        }
    }
}

fn check_string(
    schema: &serde_json::Map<String, Value>,
    text: &str,
    path: &str,
    out: &mut Vec<Violation>,
) {
    let length = text.chars().count() as u64;
    if let Some(min) = schema.get("minLength").and_then(Value::as_u64) {
        if length < min {
            out.push(Violation {
                path: path.to_string(),
                message: format!("expected at least {min} characters, found {length}"),
            });
        }
    }
    if let Some(max) = schema.get("maxLength").and_then(Value::as_u64) {
        if length > max {
            out.push(Violation {
                path: path.to_string(),
                message: format!("expected at most {max} characters, found {length}"),
            });
        }
    }
    // `pattern` is intentionally not enforced: the validator keeps zero
    // dependencies, and a wrong regex dialect is worse than no check. The
    // contract still carries the pattern for other consumers.
}

fn check_number(
    schema: &serde_json::Map<String, Value>,
    number: &serde_json::Number,
    path: &str,
    out: &mut Vec<Violation>,
) {
    let value = number.as_f64().unwrap_or_default();
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
        if value < minimum {
            out.push(Violation {
                path: path.to_string(),
                message: format!("value {value} is below minimum {minimum}"),
            });
        }
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
        if value > maximum {
            out.push(Violation {
                path: path.to_string(),
                message: format!("value {value} is above maximum {maximum}"),
            });
        }
    }
    if let Some(exclusive) = schema.get("exclusiveMinimum").and_then(Value::as_f64) {
        if value <= exclusive {
            out.push(Violation {
                path: path.to_string(),
                message: format!("value {value} must be greater than {exclusive}"),
            });
        }
    }
    if let Some(exclusive) = schema.get("exclusiveMaximum").and_then(Value::as_f64) {
        if value >= exclusive {
            out.push(Violation {
                path: path.to_string(),
                message: format!("value {value} must be less than {exclusive}"),
            });
        }
    }
}

fn child_path(path: &str, name: &str) -> String {
    format!("{path}.{name}")
}

fn describe(expected: &Value) -> String {
    match expected {
        Value::String(name) => name.clone(),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" | "),
        other => other.to_string(),
    }
}
