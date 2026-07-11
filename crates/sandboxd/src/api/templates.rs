//! Org-scoped named sandbox templates.

use crate::auth::AuthContext;
use crate::error::{ApiError, ApiResult};
use crate::ids;
use crate::model::{
    CreateTemplateRequest, SandboxTemplate, SpawnTemplateRequest, UpdateTemplateRequest,
};
use crate::service;
use crate::state::AppState;
use crate::templates::{
    create_request_from_value, merge_create, normalize_create_value, valid_name,
    MAX_TEMPLATE_SPAWN_COUNT,
};
use crate::views::sandbox_view;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::Utc;
use serde_json::{json, Value};

fn template_view(t: &SandboxTemplate) -> Value {
    json!({
        "id": t.id,
        "name": t.name,
        "description": t.description,
        "create": t.create,
        "created_at": t.created_at,
        "updated_at": t.updated_at,
    })
}

fn load_owned_by_name(
    state: &AppState,
    ctx: &AuthContext,
    name: &str,
) -> ApiResult<SandboxTemplate> {
    state
        .store
        .get_template_by_name(&ctx.org_id, name)
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::NotFound(format!("template {name}")))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let templates = state
        .store
        .list_templates_for_org(&ctx.org_id)
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({
        "templates": templates.iter().map(template_view).collect::<Vec<_>>()
    })))
}

pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let t = load_owned_by_name(&state, &ctx, &name)?;
    Ok(Json(template_view(&t)))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<CreateTemplateRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if !valid_name(&req.name) {
        return Err(ApiError::BadRequest(
            "template name must be 1-64 chars of letters, digits, '.', '-' or '_'".into(),
        ));
    }
    if state
        .store
        .get_template_by_name(&ctx.org_id, &req.name)
        .map_err(ApiError::Internal)?
        .is_some()
    {
        return Err(ApiError::Conflict(format!(
            "a template named '{}' already exists",
            req.name
        )));
    }
    let create = normalize_create_value(req.create)?;
    let now = Utc::now();
    let t = SandboxTemplate {
        id: ids::template_id(),
        org_id: ctx.org_id.clone(),
        name: req.name,
        description: req.description,
        create,
        created_at: now,
        updated_at: now,
    };
    state.store.put_template(&t).map_err(ApiError::Internal)?;
    Ok((StatusCode::CREATED, Json(template_view(&t))))
}

pub async fn update(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(name): Path<String>,
    Json(req): Json<UpdateTemplateRequest>,
) -> ApiResult<Json<Value>> {
    let mut t = load_owned_by_name(&state, &ctx, &name)?;
    t.description = req.description;
    t.create = normalize_create_value(req.create)?;
    t.updated_at = Utc::now();
    state.store.put_template(&t).map_err(ApiError::Internal)?;
    Ok(Json(template_view(&t)))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let t = load_owned_by_name(&state, &ctx, &name)?;
    let deleted = state
        .store
        .delete_template(&t.id)
        .map_err(ApiError::Internal)?;
    if !deleted {
        return Err(ApiError::NotFound(format!("template {name}")));
    }
    Ok(Json(json!({ "name": name, "deleted": true })))
}

pub async fn spawn_sandboxes(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(name): Path<String>,
    Json(req): Json<SpawnTemplateRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let t = load_owned_by_name(&state, &ctx, &name)?;
    let count = req.count.unwrap_or(1);
    if count == 0 || count > MAX_TEMPLATE_SPAWN_COUNT {
        return Err(ApiError::BadRequest(format!(
            "count must be 1..={MAX_TEMPLATE_SPAWN_COUNT}"
        )));
    }
    let create = merge_create(t.create.clone(), req.overrides)?;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let create_req = create_request_from_value(create.clone())?;
        let sb = service::create_sandbox(&state, &ctx, create_req).await?;
        out.push(sandbox_view(&state, &sb));
    }
    Ok((StatusCode::CREATED, Json(json!({ "sandboxes": out }))))
}
