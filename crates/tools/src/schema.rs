//! P3/TASK-201：工具参数 JSON Schema 核心关键字的递归校验。

use crate::ToolSpec;
use protocol::{ErrorCode, ErrorEnvelope};

/// 校验工具参数。
///
/// 工具参数必须是 JSON object；参数违反 schema 时返回 `ToolArgsInvalid`，
/// 工具自身携带了非法 schema 时返回 `Internal`。
pub fn validate_args(spec: &ToolSpec, args: &serde_json::Value) -> Result<(), ErrorEnvelope> {
    let root = spec
        .parameters_schema
        .as_object()
        .ok_or_else(|| schema_error("$", "tool parameters schema must be an object"))?;
    validate_schema(&spec.parameters_schema, "$")?;

    if !args.is_object() {
        return Err(args_error("$", "tool arguments must be an object"));
    }

    // OpenAI/DSH 工具参数的根节点固定为 object。即使 schema 省略 type，
    // properties/required 等约束仍应在根对象上生效。
    if let Some(schema_type) = root.get("type") {
        let types = schema_types(schema_type, "$")?;
        if !types.contains(&"object") {
            return Err(schema_error(
                "$.type",
                "tool parameters root type must include object",
            ));
        }
    }
    validate_value(&spec.parameters_schema, args, "$")?;
    Ok(())
}

fn validate_schema(schema: &serde_json::Value, path: &str) -> Result<(), ErrorEnvelope> {
    if schema.is_boolean() {
        return Ok(());
    }

    let object = schema
        .as_object()
        .ok_or_else(|| schema_error(path, "schema must be an object or boolean"))?;

    if let Some(schema_type) = object.get("type") {
        schema_types(schema_type, path)?;
    }

    if let Some(values) = object.get("enum") {
        let values = values
            .as_array()
            .ok_or_else(|| schema_error(&format!("{path}.enum"), "enum must be an array"))?;
        if values.is_empty() {
            return Err(schema_error(
                &format!("{path}.enum"),
                "enum must contain at least one value",
            ));
        }
    }

    if let Some(required) = object.get("required") {
        let required = required.as_array().ok_or_else(|| {
            schema_error(&format!("{path}.required"), "required must be an array")
        })?;
        let mut names = std::collections::BTreeSet::new();
        for (index, name) in required.iter().enumerate() {
            let name = name.as_str().ok_or_else(|| {
                schema_error(
                    &format!("{path}.required[{index}]"),
                    "required entries must be strings",
                )
            })?;
            if !names.insert(name) {
                return Err(schema_error(
                    &format!("{path}.required[{index}]"),
                    "required entries must be unique",
                ));
            }
        }
    }

    if let Some(properties) = object.get("properties") {
        let properties = properties.as_object().ok_or_else(|| {
            schema_error(
                &format!("{path}.properties"),
                "properties must be an object",
            )
        })?;
        for (name, property_schema) in properties {
            validate_schema(property_schema, &property_path(path, name))?;
        }
    }

    if let Some(items) = object.get("items") {
        validate_schema(items, &format!("{path}.items"))?;
    }

    if let Some(additional) = object.get("additionalProperties") {
        if !additional.is_boolean() && !additional.is_object() {
            return Err(schema_error(
                &format!("{path}.additionalProperties"),
                "additionalProperties must be a boolean or schema",
            ));
        }
        if additional.is_object() {
            validate_schema(additional, &format!("{path}.additionalProperties"))?;
        }
    }

    Ok(())
}

fn validate_value(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    path: &str,
) -> Result<(), ErrorEnvelope> {
    if let Some(allowed) = schema.as_bool() {
        return if allowed {
            Ok(())
        } else {
            Err(args_error(path, "value is rejected by schema"))
        };
    }

    let object = schema
        .as_object()
        .ok_or_else(|| schema_error(path, "schema must be an object or boolean"))?;

    if let Some(values) = object.get("enum").and_then(serde_json::Value::as_array) {
        if !values.iter().any(|candidate| candidate == value) {
            return Err(args_error(
                path,
                "value is not one of the allowed enum values",
            ));
        }
    }

    if let Some(schema_type) = object.get("type") {
        let types = schema_types(schema_type, path)?;
        if !types
            .iter()
            .any(|schema_type| type_matches(schema_type, value))
        {
            return Err(args_error(
                path,
                &format!("value does not match schema type {}", types.join(" or ")),
            ));
        }
    }

    if let Some(map) = value.as_object() {
        validate_object(object, map, path)?;
    }

    if let Some(values) = value.as_array() {
        if let Some(items) = object.get("items") {
            for (index, item) in values.iter().enumerate() {
                validate_value(items, item, &format!("{path}[{index}]"))?;
            }
        }
    }

    Ok(())
}

fn validate_object(
    schema: &serde_json::Map<String, serde_json::Value>,
    value: &serde_json::Map<String, serde_json::Value>,
    path: &str,
) -> Result<(), ErrorEnvelope> {
    if let Some(required) = schema.get("required").and_then(serde_json::Value::as_array) {
        for name in required.iter().filter_map(serde_json::Value::as_str) {
            if !value.contains_key(name) {
                return Err(args_error(
                    &property_path(path, name),
                    "required property is missing",
                ));
            }
        }
    }

    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object);
    let additional = schema.get("additionalProperties");

    for (name, property_value) in value {
        let property_path = property_path(path, name);
        if let Some(property_schema) = properties.and_then(|items| items.get(name)) {
            validate_value(property_schema, property_value, &property_path)?;
            continue;
        }

        match additional {
            Some(serde_json::Value::Bool(false)) => {
                return Err(args_error(
                    &property_path,
                    "additional property is not allowed",
                ))
            }
            Some(additional_schema) if additional_schema.is_object() => {
                validate_value(additional_schema, property_value, &property_path)?;
            }
            _ => {}
        }
    }

    Ok(())
}

fn schema_types<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Result<Vec<&'a str>, ErrorEnvelope> {
    const SUPPORTED: [&str; 7] = [
        "object", "array", "string", "number", "integer", "boolean", "null",
    ];

    let types = if let Some(schema_type) = value.as_str() {
        vec![schema_type]
    } else if let Some(values) = value.as_array() {
        if values.is_empty() {
            return Err(schema_error(
                &format!("{path}.type"),
                "type array must not be empty",
            ));
        }
        values
            .iter()
            .enumerate()
            .map(|(index, item)| {
                item.as_str().ok_or_else(|| {
                    schema_error(
                        &format!("{path}.type[{index}]"),
                        "type entries must be strings",
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        return Err(schema_error(
            &format!("{path}.type"),
            "type must be a string or an array of strings",
        ));
    };

    let mut unique = std::collections::BTreeSet::new();
    for (index, schema_type) in types.iter().enumerate() {
        if !SUPPORTED.contains(schema_type) {
            return Err(schema_error(
                &format!("{path}.type[{index}]"),
                &format!("unsupported schema type: {schema_type}"),
            ));
        }
        if !unique.insert(*schema_type) {
            return Err(schema_error(
                &format!("{path}.type[{index}]"),
                "type entries must be unique",
            ));
        }
    }
    Ok(types)
}

fn type_matches(schema_type: &str, value: &serde_json::Value) -> bool {
    match schema_type {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value
            .as_number()
            .is_some_and(|number| number.is_i64() || number.is_u64()),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn property_path(parent: &str, name: &str) -> String {
    if name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        format!("{parent}.{name}")
    } else {
        format!("{parent}[{name:?}]")
    }
}

fn schema_error(path: &str, message: &str) -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::Internal,
        format!("invalid tool schema at {path}: {message}"),
    )
}

fn args_error(path: &str, message: &str) -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::ToolArgsInvalid,
        format!("invalid tool arguments at {path}: {message}"),
    )
}
