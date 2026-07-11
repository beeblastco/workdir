//! Shared helpers for named sandbox templates.

use crate::error::{ApiError, ApiResult};
use crate::model::CreateSandboxRequest;
use serde_json::{Map, Value};

pub const MAX_TEMPLATE_SPAWN_COUNT: u32 = 20;

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

pub fn normalize_create_value(value: Value) -> ApiResult<Value> {
    let value = match value {
        Value::Null => Value::Object(Map::new()),
        other => other,
    };
    if !value.is_object() {
        return Err(ApiError::BadRequest(
            "template create must be a sandbox create object".into(),
        ));
    }
    create_request_from_value(value.clone())?;
    Ok(value)
}

pub fn create_request_from_value(value: Value) -> ApiResult<CreateSandboxRequest> {
    serde_json::from_value(value)
        .map_err(|e| ApiError::BadRequest(format!("invalid sandbox create body: {e}")))
}

pub fn merge_create(mut base: Value, overrides: Option<Value>) -> ApiResult<Value> {
    if let Some(overrides) = overrides {
        if !overrides.is_object() {
            return Err(ApiError::BadRequest(
                "template spawn overrides must be an object".into(),
            ));
        }
        merge_json(&mut base, overrides);
    }
    normalize_create_value(base)
}

fn merge_json(base: &mut Value, overrides: Value) {
    match (base, overrides) {
        (Value::Object(base), Value::Object(overrides)) => {
            for (k, v) in overrides {
                match base.get_mut(&k) {
                    Some(existing) => merge_json(existing, v),
                    None => {
                        base.insert(k, v);
                    }
                }
            }
        }
        (base, overrides) => *base = overrides,
    }
}
