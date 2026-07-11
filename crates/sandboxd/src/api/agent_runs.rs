//! Agent-run endpoints.

use crate::agent;
use crate::auth::AuthContext;
use crate::error::{ApiError, ApiResult};
use crate::model::{AgentRun, AgentRunReport, CreateAgentRunRequest};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Default, Deserialize)]
pub struct ListQuery {
    parent_run_id: Option<String>,
    label: Option<String>,
    state: Option<String>,
}

fn report_value(run: &AgentRun) -> AgentRunReport {
    run.report
        .clone()
        .unwrap_or_else(|| agent::report_for_run(run))
}

fn run_view(run: &AgentRun) -> Value {
    let report = report_value(run);
    json!({
        "id": run.id,
        "state": run.state.as_str(),
        "sandbox_id": run.sandbox_id,
        "template": run.template,
        "repo": run.repo,
        "prompt": run.prompt,
        "model": run.model,
        "agent": run.agent.as_str(),
        "api_key_secret": run.api_key_secret,
        "hardness": run.hardness.as_str(),
        "loop": run.r#loop,
        "github": run.github,
        "task": run.task,
        "mode": run.mode.as_str(),
        "constraints": run.constraints,
        "context": run.context,
        "verify": run.verify,
        "verification_result": run.verification_result,
        "verification_results": run.verification_results,
        "artifacts": run.artifacts,
        "constraint_result": run.constraint_result,
        "report": report,
        "branch": run.branch,
        "commit": run.commit,
        "pr_url": run.pr_url,
        "error": run.error,
        "logs_truncated": run.logs_truncated,
        "created_at": run.created_at,
        "updated_at": run.updated_at,
        "finished_at": run.finished_at,
        "status_url": format!("/v1/agent-runs/{}", run.id),
        "logs_url": format!("/v1/agent-runs/{}/logs", run.id),
        "report_url": format!("/v1/agent-runs/{}/report", run.id),
        "children_url": format!("/v1/agent-runs/{}/children", run.id),
    })
}

fn run_list_view(run: &AgentRun) -> Value {
    let report = report_value(run);
    json!({
        "id": run.id,
        "state": run.state.as_str(),
        "task": run.task,
        "mode": run.mode.as_str(),
        "agent": run.agent.as_str(),
        "model": run.model,
        "template": run.template,
        "hardness": run.hardness.as_str(),
        "sandbox_id": run.sandbox_id,
        "branch": run.branch,
        "commit": run.commit,
        "pr_url": run.pr_url,
        "error": run.error,
        "created_at": run.created_at,
        "updated_at": run.updated_at,
        "finished_at": run.finished_at,
        "report": {
            "outcome": report.outcome,
            "summary": report.summary,
            "changed_files": report.diff_stats.files_changed,
            "artifacts": report.artifacts.len(),
            "verification": report.verification,
        },
        "status_url": format!("/v1/agent-runs/{}", run.id),
        "logs_url": format!("/v1/agent-runs/{}/logs", run.id),
        "report_url": format!("/v1/agent-runs/{}/report", run.id),
        "children_url": format!("/v1/agent-runs/{}/children", run.id),
    })
}

fn load_owned(state: &AppState, ctx: &AuthContext, id: &str) -> ApiResult<AgentRun> {
    let run = state
        .store
        .get_agent_run(id)
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::NotFound(format!("agent run {id}")))?;
    if run.org_id != ctx.org_id && !ctx.admin {
        return Err(ApiError::NotFound(format!("agent run {id}")));
    }
    Ok(run)
}

pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<CreateAgentRunRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let run = agent::start_agent_run(state, ctx, req)?;
    Ok((StatusCode::ACCEPTED, Json(run_view(&run))))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let mut runs = state
        .store
        .list_agent_runs_for_org(&ctx.org_id)
        .map_err(ApiError::Internal)?;
    runs.retain(|run| {
        query
            .parent_run_id
            .as_ref()
            .map(|v| run.task.parent_run_id.as_ref() == Some(v))
            .unwrap_or(true)
            && query
                .label
                .as_ref()
                .map(|v| run.task.labels.iter().any(|label| label == v))
                .unwrap_or(true)
            && query
                .state
                .as_ref()
                .map(|v| run.state.as_str() == v)
                .unwrap_or(true)
    });
    Ok(Json(json!({
        "agent_runs": runs.iter().map(run_list_view).collect::<Vec<_>>()
    })))
}

pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let run = load_owned(&state, &ctx, &id)?;
    Ok(Json(run_view(&run)))
}

pub async fn report(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let run = load_owned(&state, &ctx, &id)?;
    Ok(Json(json!(report_value(&run))))
}

pub async fn children(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let parent = load_owned(&state, &ctx, &id)?;
    let runs = state
        .store
        .list_agent_runs_for_org(&parent.org_id)
        .map_err(ApiError::Internal)?;
    let children = runs
        .iter()
        .filter(|run| run.task.parent_run_id.as_deref() == Some(&id))
        .map(run_list_view)
        .collect::<Vec<_>>();
    Ok(Json(json!({ "agent_runs": children })))
}

pub async fn cancel(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let run = load_owned(&state, &ctx, &id)?;
    let run = agent::cancel_agent_run(&state, run).map_err(ApiError::Internal)?;
    Ok(Json(run_view(&run)))
}

pub async fn logs(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let run = load_owned(&state, &ctx, &id)?;
    Ok(Json(json!({
        "id": run.id,
        "state": run.state.as_str(),
        "stdout": run.stdout,
        "stderr": run.stderr,
        "diff": run.diff,
        "truncated": run.logs_truncated,
    })))
}
