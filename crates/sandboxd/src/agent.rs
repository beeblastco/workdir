//! Agent-run orchestration: create a sandbox, run a coding-agent CLI, collect a
//! diff, and optionally turn that diff into a GitHub pull request.

use crate::auth::AuthContext;
use crate::error::{ApiError, ApiResult};
use crate::ids;
use crate::model::{
    AgentArtifact, AgentChangedFile, AgentConstraintResult, AgentConstraints, AgentDiffStats,
    AgentGithubConfig, AgentKind, AgentRun, AgentRunMode, AgentRunReport, AgentRunState,
    AgentVerificationResult, CreateAgentRunRequest, CreateSandboxRequest, Hardness,
};
use crate::node::NodeClient;
use crate::runtime::ExecRequest;
use crate::secrets;
use crate::service;
use crate::state::AppState;
use crate::templates::{create_request_from_value, merge_create};
use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;

const MAX_LOG_BYTES: usize = 1024 * 1024;
const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;
const AGENT_WORKDIR: &str = "/workspace";

struct HardnessProfile {
    create: Value,
    timeout_seconds: u64,
    loop_iterations: u32,
}

pub fn start_agent_run(
    state: AppState,
    ctx: AuthContext,
    req: CreateAgentRunRequest,
) -> ApiResult<AgentRun> {
    validate_secret_exists(&state, &ctx.org_id, &req.api_key_secret)?;
    if let Some(github) = &req.github {
        if let Some(token_secret) = &github.token_secret {
            validate_secret_exists(&state, &ctx.org_id, token_secret)?;
        }
    }
    if req.repo.url.trim().is_empty() {
        return Err(ApiError::BadRequest("repo.url cannot be empty".into()));
    }
    if req.prompt.trim().is_empty() {
        return Err(ApiError::BadRequest("prompt cannot be empty".into()));
    }
    if req.model.trim().is_empty() {
        return Err(ApiError::BadRequest("model cannot be empty".into()));
    }
    if !secrets::valid_name(&req.api_key_secret) {
        return Err(ApiError::BadRequest(
            "api_key_secret must be an env-style secret name".into(),
        ));
    }
    validate_agent_request(&state, &ctx, &req)?;

    let now = Utc::now();
    let run = AgentRun {
        id: ids::agent_run_id(),
        org_id: ctx.org_id.clone(),
        state: AgentRunState::Queued,
        sandbox_id: None,
        template: req.template,
        repo: req.repo,
        prompt: req.prompt,
        model: req.model,
        agent: req.agent,
        api_key_secret: req.api_key_secret,
        hardness: req.hardness.unwrap_or_default(),
        r#loop: req.r#loop.unwrap_or_default(),
        github: req.github,
        task: req.task.unwrap_or_default(),
        mode: req.mode.unwrap_or_default(),
        constraints: req.constraints.unwrap_or_default(),
        context: req.context.unwrap_or_default(),
        verify: req.verify,
        verification_results: Vec::new(),
        artifacts: Vec::new(),
        constraint_result: None,
        report: None,
        stdout: String::new(),
        stderr: String::new(),
        diff: String::new(),
        logs_truncated: false,
        verification_result: None,
        branch: None,
        commit: None,
        pr_url: None,
        error: None,
        created_at: now,
        updated_at: now,
        finished_at: None,
    };
    state
        .store
        .put_agent_run(&run)
        .map_err(ApiError::Internal)?;

    let run_id = run.id.clone();
    tokio::spawn(async move {
        if let Err(e) = run_agent_task(state.clone(), ctx, run_id.clone()).await {
            tracing::error!(agent_run = %run_id, error = ?e, "agent run failed");
            if let Ok(Some(mut run)) = state.store.get_agent_run(&run_id) {
                if run.state == AgentRunState::Cancelled {
                    return;
                }
                let now = Utc::now();
                run.state = AgentRunState::Failed;
                run.error = Some(e.to_string());
                run.updated_at = now;
                run.finished_at = Some(now);
                run.report = Some(report_for_run(&run));
                let _ = state.store.put_agent_run(&run);
            }
        }
    });

    Ok(run)
}

async fn run_agent_task(state: AppState, ctx: AuthContext, run_id: String) -> Result<()> {
    let mut run = state
        .store
        .get_agent_run(&run_id)?
        .ok_or_else(|| anyhow!("agent run {run_id} not found"))?;
    if run.state == AgentRunState::Cancelled {
        return Ok(());
    }
    run.state = AgentRunState::Running;
    run.updated_at = Utc::now();
    state.store.put_agent_run(&run)?;

    let create_value = build_create_value(&state, &run)?;
    let create_req: CreateSandboxRequest = create_request_from_value(create_value)?;
    let sb = service::create_sandbox(&state, &ctx, create_req).await?;
    run.sandbox_id = Some(sb.id.clone());
    run.updated_at = Utc::now();
    state.store.put_agent_run(&run)?;
    if check_cancelled(&state, &mut run)? {
        return Ok(());
    }

    let handle = sb
        .runtime_handle
        .clone()
        .ok_or_else(|| anyhow!("created sandbox has no runtime handle"))?;
    let node = state.node_for(sb.node_id.as_deref().unwrap_or(""));
    let api_secret_value = decrypt_secret(&state, &run.org_id, &run.api_key_secret)?;

    write_agent_context(&node, &handle, &run).await?;
    let prompt_write = node
        .exec(
            &handle,
            &ExecRequest {
                cmd: format!(
                    "printf '%s' {} > .workdir-agent-prompt.txt",
                    shell_quote(&agent_prompt(&run))
                ),
                cwd: Some(AGENT_WORKDIR.into()),
                env: BTreeMap::new(),
                background: false,
            },
        )
        .await
        .context("write agent prompt")?;
    if prompt_write.exit_code != 0 {
        bail!("write agent prompt failed: {}", prompt_write.stderr);
    }
    if check_cancelled(&state, &mut run)? {
        return Ok(());
    }

    let profile = hardness_profile(run.hardness);
    let iterations = effective_iterations(&run, profile.loop_iterations);
    let timeout_seconds = effective_timeout_seconds(&run, profile.timeout_seconds);
    let mut last_exit = 0;
    let mut stdout = String::new();
    let mut stderr = String::new();
    for _ in 0..iterations {
        let cmd = agent_command(&run);
        let env = agent_env(run.agent, &api_secret_value);
        let exec = tokio::time::timeout(
            Duration::from_secs(timeout_seconds),
            node.exec(
                &handle,
                &ExecRequest {
                    cmd,
                    cwd: Some(AGENT_WORKDIR.into()),
                    env,
                    background: false,
                },
            ),
        )
        .await
        .map_err(|_| anyhow!("agent timed out after {timeout_seconds}s"))?
        .context("run agent command")?;
        last_exit = exec.exit_code;
        stdout.push_str(&exec.stdout);
        stderr.push_str(&exec.stderr);
        if check_cancelled(&state, &mut run)? {
            return Ok(());
        }
        if exec.exit_code == 0 {
            break;
        }
    }

    let stdout = redact_secret(stdout, Some(&api_secret_value));
    let stderr = redact_secret(stderr, Some(&api_secret_value));
    let (stdout, stdout_truncated) = truncate(stdout, MAX_LOG_BYTES);
    let (stderr, stderr_truncated) = truncate(stderr, MAX_LOG_BYTES);
    run.stdout = stdout;
    run.stderr = stderr;
    run.logs_truncated = stdout_truncated || stderr_truncated;
    run.updated_at = Utc::now();
    state.store.put_agent_run(&run)?;

    if last_exit != 0 {
        bail!("agent exited with code {last_exit}");
    }

    run.artifacts = collect_artifacts(&node, &handle, artifact_capture_limit(&run)).await?;
    let cleanup = node
        .exec(
            &handle,
            &ExecRequest {
                cmd: "rm -f .workdir-agent-prompt.txt .workdir-agent-final.txt .workdir-agent-output-*.patch; rm -rf .workdir-codex-home .workdir/context .workdir/artifacts .workdir/bin; rmdir .workdir 2>/dev/null || true".to_string(),
                cwd: Some(AGENT_WORKDIR.into()),
                env: BTreeMap::new(),
                background: false,
            },
        )
        .await
        .context("clean agent internal files")?;
    if cleanup.exit_code != 0 {
        bail!("clean agent internal files failed: {}", cleanup.stderr);
    }

    let mut diff_stdout = collect_staged_diff(&node, &handle).await?;
    if run.mode == AgentRunMode::Change && diff_stdout.trim().is_empty() {
        let output = format!("{}\n{}", run.stdout, run.stderr);
        if apply_agent_output_patch(&node, &handle, &output).await? {
            diff_stdout = collect_staged_diff(&node, &handle).await?;
        }
    }
    if run.mode == AgentRunMode::Change && diff_stdout.trim().is_empty() {
        bail!("agent produced no git diff");
    }
    let raw_diff_bytes = diff_stdout.len();
    let diff_stdout = redact_secret(diff_stdout, Some(&api_secret_value));
    let (stored_diff, diff_truncated) = truncate(diff_stdout, MAX_DIFF_BYTES);
    run.diff = stored_diff.clone();
    run.logs_truncated = run.logs_truncated || diff_truncated;
    run.verification_results = run_verify_commands(&node, &handle, &run).await?;
    run.verification_result = Some(verification_summary(&run.verification_results));
    run.constraint_result = Some(evaluate_constraints(
        &run.constraints,
        &run.diff,
        raw_diff_bytes,
        &run.artifacts,
        diff_truncated,
    ));
    run.updated_at = Utc::now();
    state.store.put_agent_run(&run)?;

    if check_cancelled(&state, &mut run)? {
        return Ok(());
    }
    if let Some(result) = &run.constraint_result {
        if !result.passed {
            let msg = format!("constraints violated: {}", result.violations.join("; "));
            finish_failed_run(&state, &mut run, msg)?;
            return Ok(());
        }
    }
    if let Some((failed_name, failed_code)) = run
        .verification_results
        .iter()
        .find(|r| !r.passed && r.fail_run)
        .map(|r| (r.name.clone(), r.exit_code))
    {
        finish_failed_run(
            &state,
            &mut run,
            format!("verification '{failed_name}' failed with code {failed_code}"),
        )?;
        return Ok(());
    }

    if run.mode == AgentRunMode::Change {
        if let Some(github) = run.github.clone().filter(|g| g.token_secret.is_some()) {
            let token_secret = github.token_secret.as_deref().unwrap();
            let token = decrypt_secret(&state, &run.org_id, token_secret)?;
            let pr = create_github_pr(&state, &run, &github, &stored_diff, &token).await?;
            run.branch = Some(pr.branch);
            run.commit = Some(pr.commit);
            run.pr_url = Some(pr.url);
        }
    }

    let now = Utc::now();
    run.state = AgentRunState::Succeeded;
    run.updated_at = now;
    run.finished_at = Some(now);
    run.report = Some(report_for_run(&run));
    state.store.put_agent_run(&run)?;
    Ok(())
}

fn build_create_value(state: &AppState, run: &AgentRun) -> ApiResult<Value> {
    let base = if let Some(template) = &run.template {
        state
            .store
            .get_template_by_name(&run.org_id, template)
            .map_err(ApiError::Internal)?
            .ok_or_else(|| ApiError::NotFound(format!("template {template}")))?
            .create
    } else {
        hardness_profile(run.hardness).create
    };
    let mut create = merge_create(base, None)?;
    set_repo_git(&mut create, run);
    Ok(create)
}

fn set_repo_git(create: &mut Value, run: &AgentRun) {
    let startup = startup_object(create);
    let mut git = json!({
        "url": run.repo.url,
        "depth": 1,
    });
    if let Some(r) = &run.repo.r#ref {
        git["ref"] = Value::String(r.clone());
    }
    startup.insert("git".into(), git);
}

fn startup_object(create: &mut Value) -> &mut Map<String, Value> {
    if !create.is_object() {
        *create = json!({});
    }
    let root = create.as_object_mut().expect("create is object");
    let replace = !root.get("startup").map(|v| v.is_object()).unwrap_or(false);
    if replace {
        root.insert("startup".into(), json!({}));
    }
    root.get_mut("startup")
        .and_then(Value::as_object_mut)
        .expect("startup is object")
}

fn hardness_profile(h: Hardness) -> HardnessProfile {
    match h {
        Hardness::Easy => HardnessProfile {
            create: json!({
                "image": "node-python",
                "resources": { "cpu": 1, "memory_mb": 2048, "disk_gb": 16 },
                "auto_stop_seconds": 600
            }),
            timeout_seconds: 900,
            loop_iterations: 1,
        },
        Hardness::Medium => HardnessProfile {
            create: json!({
                "image": "node-python",
                "resources": { "cpu": 2, "memory_mb": 4096, "disk_gb": 16 },
                "auto_stop_seconds": 1200
            }),
            timeout_seconds: 1800,
            loop_iterations: 2,
        },
        Hardness::Hard => HardnessProfile {
            create: json!({
                "image": "heavy-build",
                "resources": { "cpu": 4, "memory_mb": 8192, "disk_gb": 32 },
                "auto_stop_seconds": 3600
            }),
            timeout_seconds: 3600,
            loop_iterations: 3,
        },
    }
}

fn effective_iterations(run: &AgentRun, profile_default: u32) -> u32 {
    match run.r#loop.max_iterations {
        Some(v) => v.clamp(1, 5),
        None if run.r#loop.goal.is_some() => profile_default.clamp(1, 5),
        None => 1,
    }
}

fn effective_timeout_seconds(run: &AgentRun, profile_default: u64) -> u64 {
    run.constraints
        .max_runtime_seconds
        .map(|v| v.max(1).min(profile_default))
        .unwrap_or(profile_default)
}

fn validate_agent_request(
    state: &AppState,
    ctx: &AuthContext,
    req: &CreateAgentRunRequest,
) -> ApiResult<()> {
    if let Some(task) = &req.task {
        if let Some(parent) = &task.parent_run_id {
            let parent = state
                .store
                .get_agent_run(parent)
                .map_err(ApiError::Internal)?
                .ok_or_else(|| ApiError::BadRequest("task.parent_run_id was not found".into()))?;
            if parent.org_id != ctx.org_id && !ctx.admin {
                return Err(ApiError::BadRequest(
                    "task.parent_run_id was not found".into(),
                ));
            }
        }
    }
    if let Some(context) = &req.context {
        for file in &context.files {
            validate_workspace_relative_path(&file.path, "context.files[].path")?;
        }
        for link in &context.links {
            if link.url.trim().is_empty() {
                return Err(ApiError::BadRequest(
                    "context.links[].url cannot be empty".into(),
                ));
            }
        }
    }
    for verify in &req.verify {
        if verify.name.trim().is_empty() {
            return Err(ApiError::BadRequest("verify[].name cannot be empty".into()));
        }
        if verify.run.trim().is_empty() {
            return Err(ApiError::BadRequest("verify[].run cannot be empty".into()));
        }
    }
    if let Some(constraints) = &req.constraints {
        for path in constraints
            .allowed_paths
            .iter()
            .chain(constraints.blocked_paths.iter())
        {
            validate_workspace_relative_path(path, "constraints paths")?;
        }
    }
    Ok(())
}

fn validate_workspace_relative_path(path: &str, field: &str) -> ApiResult<()> {
    if path.trim().is_empty() || path.starts_with('/') || path.split('/').any(|p| p == "..") {
        return Err(ApiError::BadRequest(format!(
            "{field} must be a non-empty relative path that does not escape the workspace"
        )));
    }
    Ok(())
}

fn check_cancelled(state: &AppState, run: &mut AgentRun) -> Result<bool> {
    if let Some(stored) = state.store.get_agent_run(&run.id)? {
        if stored.state == AgentRunState::Cancelled {
            *run = stored;
            return Ok(true);
        }
    }
    Ok(false)
}

fn finish_failed_run(state: &AppState, run: &mut AgentRun, error: String) -> Result<()> {
    let now = Utc::now();
    run.state = AgentRunState::Failed;
    run.error = Some(error);
    run.updated_at = now;
    run.finished_at = Some(now);
    run.report = Some(report_for_run(run));
    state.store.put_agent_run(run)?;
    Ok(())
}

pub fn cancel_agent_run(state: &AppState, mut run: AgentRun) -> Result<AgentRun> {
    if matches!(run.state, AgentRunState::Queued | AgentRunState::Running) {
        let now = Utc::now();
        run.state = AgentRunState::Cancelled;
        run.error = Some("cancelled".into());
        run.updated_at = now;
        run.finished_at = Some(now);
        run.report = Some(report_for_run(&run));
        state.store.put_agent_run(&run)?;
    }
    Ok(run)
}

async fn write_agent_context(
    node: &Arc<dyn NodeClient>,
    handle: &str,
    run: &AgentRun,
) -> Result<()> {
    if let Some(instructions) = &run.context.instructions {
        node.write_file(
            handle,
            ".workdir/context/instructions.md",
            instructions.as_bytes(),
        )
        .await
        .context("write agent context instructions")?;
    }
    if !run.context.links.is_empty() {
        let mut links = String::new();
        for link in &run.context.links {
            let title = link.title.as_deref().unwrap_or("link");
            links.push_str(&format!("- [{title}]({})", link.url));
            if let Some(desc) = &link.description {
                links.push_str(&format!(": {desc}"));
            }
            links.push('\n');
        }
        node.write_file(handle, ".workdir/context/links.md", links.as_bytes())
            .await
            .context("write agent context links")?;
    }
    for file in &run.context.files {
        let path = format!(".workdir/context/{}", file.path.trim_start_matches('/'));
        node.write_file(handle, &path, file.content.as_bytes())
            .await
            .with_context(|| format!("write agent context file {}", file.path))?;
    }
    Ok(())
}

async fn collect_staged_diff(node: &Arc<dyn NodeClient>, handle: &str) -> Result<String> {
    let diff = node
        .exec(
            handle,
            &ExecRequest {
                cmd: "git add -A && git diff --cached --binary".to_string(),
                cwd: Some(AGENT_WORKDIR.into()),
                env: BTreeMap::new(),
                background: false,
            },
        )
        .await
        .context("collect git diff")?;
    if diff.exit_code != 0 {
        bail!("collect git diff failed: {}", diff.stderr);
    }
    Ok(diff.stdout)
}

async fn apply_agent_output_patch(
    node: &Arc<dyn NodeClient>,
    handle: &str,
    output: &str,
) -> Result<bool> {
    for (idx, patch) in patch_candidates(output).into_iter().enumerate() {
        let path = format!(".workdir-agent-output-{idx}.patch");
        node.write_file(handle, &path, patch.as_bytes())
            .await
            .context("write agent output patch")?;
        let quoted_path = shell_quote(&path);
        let apply = node
            .exec(
                handle,
                &ExecRequest {
                    cmd: format!(
                        "set +e; git apply --check --binary {quoted_path}; check=$?; \
                         if [ $check -eq 0 ]; then git apply --index --binary {quoted_path}; status=$?; \
                         else status=$check; fi; rm -f {quoted_path}; exit $status"
                    ),
                    cwd: Some(AGENT_WORKDIR.into()),
                    env: BTreeMap::new(),
                    background: false,
                },
            )
            .await
            .context("apply agent output patch")?;
        if apply.exit_code == 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn artifact_capture_limit(run: &AgentRun) -> usize {
    run.constraints
        .max_artifact_bytes
        .unwrap_or(64 * 1024)
        .min(1024 * 1024)
}

async fn collect_artifacts(
    node: &Arc<dyn NodeClient>,
    handle: &str,
    max_total_bytes: usize,
) -> Result<Vec<AgentArtifact>> {
    let exists = node
        .exec(
            handle,
            &ExecRequest {
                cmd: "test -d .workdir/artifacts".to_string(),
                cwd: Some(AGENT_WORKDIR.into()),
                env: BTreeMap::new(),
                background: false,
            },
        )
        .await
        .context("check agent artifacts")?;
    if exists.exit_code != 0 {
        return Ok(Vec::new());
    }
    let mut artifacts = Vec::new();
    let mut consumed = 0usize;
    collect_artifacts_inner(
        node,
        handle,
        ".workdir/artifacts",
        "",
        max_total_bytes,
        &mut consumed,
        &mut artifacts,
    )
    .await?;
    Ok(artifacts)
}

async fn collect_artifacts_inner(
    node: &Arc<dyn NodeClient>,
    handle: &str,
    root: &str,
    rel: &str,
    max_total_bytes: usize,
    consumed: &mut usize,
    artifacts: &mut Vec<AgentArtifact>,
) -> Result<()> {
    let path = if rel.is_empty() {
        root.to_string()
    } else {
        format!("{root}/{rel}")
    };
    let entries = match node.list_dir(handle, &path).await {
        Ok(entries) => entries,
        Err(e) if crate::node::is_file_not_found(&e) => return Ok(()),
        Err(e) => return Err(e).context("list agent artifacts"),
    };
    for entry in entries {
        let child_rel = if rel.is_empty() {
            entry.name.clone()
        } else {
            format!("{rel}/{}", entry.name)
        };
        if entry.dir {
            Box::pin(collect_artifacts_inner(
                node,
                handle,
                root,
                &child_rel,
                max_total_bytes,
                consumed,
                artifacts,
            ))
            .await?;
            continue;
        }
        let file_path = format!("{root}/{child_rel}");
        let bytes = node
            .read_file(handle, &file_path)
            .await
            .with_context(|| format!("read agent artifact {child_rel}"))?;
        let remaining = max_total_bytes.saturating_sub(*consumed);
        let take = remaining.min(bytes.len());
        let content = std::str::from_utf8(&bytes[..take])
            .ok()
            .map(|s| s.to_string());
        artifacts.push(AgentArtifact {
            path: child_rel,
            bytes: bytes.len(),
            content,
            truncated: take < bytes.len(),
        });
        *consumed = (*consumed).saturating_add(bytes.len());
    }
    Ok(())
}

async fn run_verify_commands(
    node: &Arc<dyn NodeClient>,
    handle: &str,
    run: &AgentRun,
) -> Result<Vec<AgentVerificationResult>> {
    let mut out = Vec::new();
    for verify in &run.verify {
        let start = Instant::now();
        let timeout = verify.timeout_seconds.unwrap_or(600).clamp(1, 3600);
        let result = tokio::time::timeout(
            Duration::from_secs(timeout),
            node.exec(
                handle,
                &ExecRequest {
                    cmd: verify.run.clone(),
                    cwd: Some(AGENT_WORKDIR.into()),
                    env: BTreeMap::new(),
                    background: false,
                },
            ),
        )
        .await;
        let duration_ms = start.elapsed().as_millis() as u64;
        let fail_run = verify.fail_run.unwrap_or(false);
        match result {
            Ok(Ok(exec)) => out.push(AgentVerificationResult {
                name: verify.name.clone(),
                command: verify.run.clone(),
                exit_code: exec.exit_code,
                passed: exec.exit_code == 0,
                fail_run,
                duration_ms,
                stdout_tail: tail_text(&exec.stdout, 4000),
                stderr_tail: tail_text(&exec.stderr, 4000),
            }),
            Ok(Err(e)) => out.push(AgentVerificationResult {
                name: verify.name.clone(),
                command: verify.run.clone(),
                exit_code: -1,
                passed: false,
                fail_run,
                duration_ms,
                stdout_tail: String::new(),
                stderr_tail: tail_text(&e.to_string(), 4000),
            }),
            Err(_) => out.push(AgentVerificationResult {
                name: verify.name.clone(),
                command: verify.run.clone(),
                exit_code: -1,
                passed: false,
                fail_run,
                duration_ms,
                stdout_tail: String::new(),
                stderr_tail: format!("timed out after {timeout}s"),
            }),
        }
    }
    Ok(out)
}

fn verification_summary(results: &[AgentVerificationResult]) -> String {
    if results.is_empty() {
        return "not_configured".into();
    }
    if results.iter().all(|r| r.passed) {
        "passed".into()
    } else {
        "failed".into()
    }
}

fn patch_candidates(output: &str) -> Vec<String> {
    let normalized = output.replace("\r\n", "\n");
    let mut out = Vec::new();
    for block in fenced_blocks(&normalized) {
        if let Some(patch) = extract_patch_region(&block) {
            push_unique_patch(&mut out, patch);
        }
    }
    if let Some(patch) = extract_patch_region(&normalized) {
        push_unique_patch(&mut out, patch);
    }
    out
}

fn fenced_blocks(output: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    let mut in_fence = false;
    for line in output.lines() {
        if line.trim_start().starts_with("```") {
            if in_fence {
                let block = current.join("\n");
                if looks_like_patch(&block) {
                    blocks.push(block);
                }
                current.clear();
                in_fence = false;
            } else {
                in_fence = true;
                current.clear();
            }
            continue;
        }
        if in_fence {
            current.push(line);
        }
    }
    blocks
}

fn push_unique_patch(out: &mut Vec<String>, mut patch: String) {
    if !patch.ends_with('\n') {
        patch.push('\n');
    }
    if !out.iter().any(|existing| existing == &patch) {
        out.push(patch);
    }
}

fn looks_like_patch(value: &str) -> bool {
    value.contains("diff --git ") || value.starts_with("--- ") || value.contains("\n--- ")
}

fn extract_patch_region(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.iter().enumerate().find_map(|(idx, line)| {
        if is_git_diff_start(line) || is_bare_diff_start(&lines, idx) {
            Some(idx)
        } else {
            None
        }
    })?;

    let mut patch = Vec::new();
    let mut in_hunk = false;
    for line in &lines[start..] {
        if line.trim_start().starts_with("```") {
            break;
        }
        if is_git_diff_start(line) {
            in_hunk = false;
            patch.push(*line);
            continue;
        }
        if is_patch_metadata(line) {
            patch.push(*line);
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            patch.push(*line);
            continue;
        }
        if in_hunk && is_hunk_line(line) {
            patch.push(*line);
            continue;
        }
        if patch.is_empty() {
            continue;
        }
        break;
    }

    if patch.is_empty() {
        None
    } else {
        Some(patch.join("\n"))
    }
}

fn is_git_diff_start(line: &str) -> bool {
    line.starts_with("diff --git ")
}

fn is_bare_diff_start(lines: &[&str], idx: usize) -> bool {
    lines.get(idx).is_some_and(|line| line.starts_with("--- "))
        && lines
            .get(idx + 1)
            .is_some_and(|line| line.starts_with("+++ "))
}

fn is_patch_metadata(line: &str) -> bool {
    line.starts_with("index ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
}

fn is_hunk_line(line: &str) -> bool {
    line.starts_with('+')
        || line.starts_with('-')
        || line.starts_with(' ')
        || line.starts_with("\\ No newline at end of file")
}

fn evaluate_constraints(
    constraints: &AgentConstraints,
    diff: &str,
    raw_diff_bytes: usize,
    artifacts: &[AgentArtifact],
    diff_truncated: bool,
) -> AgentConstraintResult {
    let changed = changed_files_from_diff(diff);
    let mut violations = Vec::new();
    if diff_truncated {
        violations.push(format!(
            "diff exceeded internal capture limit of {MAX_DIFF_BYTES} bytes"
        ));
    }
    if let Some(max) = constraints.max_diff_bytes {
        if raw_diff_bytes > max {
            violations.push(format!(
                "diff has {raw_diff_bytes} bytes, above max_diff_bytes {max}"
            ));
        }
    }
    if let Some(max) = constraints.max_changed_files {
        if changed.len() > max {
            violations.push(format!(
                "diff changes {} files, above max_changed_files {max}",
                changed.len()
            ));
        }
    }
    if let Some(max) = constraints.max_artifact_bytes {
        let artifact_bytes: usize = artifacts.iter().map(|a| a.bytes).sum();
        if artifact_bytes > max {
            violations.push(format!(
                "artifacts use {artifact_bytes} bytes, above max_artifact_bytes {max}"
            ));
        }
    }
    for file in &changed {
        if !constraints.allowed_paths.is_empty()
            && !constraints
                .allowed_paths
                .iter()
                .any(|p| path_matches_constraint(&file.path, p))
        {
            violations.push(format!("{} is outside allowed_paths", file.path));
        }
        if constraints
            .blocked_paths
            .iter()
            .any(|p| path_matches_constraint(&file.path, p))
        {
            violations.push(format!("{} matches blocked_paths", file.path));
        }
    }
    AgentConstraintResult {
        passed: violations.is_empty(),
        violations,
    }
}

fn path_matches_constraint(path: &str, pattern: &str) -> bool {
    let path = normalize_report_path(path);
    let pattern = normalize_report_path(pattern);
    if pattern.is_empty() {
        return false;
    }
    if pattern.ends_with('/') {
        return path.starts_with(&pattern);
    }
    path == pattern || path.starts_with(&format!("{pattern}/"))
}

fn normalize_report_path(path: &str) -> String {
    path.trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

fn changed_files_from_diff(diff: &str) -> Vec<AgentChangedFile> {
    let mut files = Vec::<AgentChangedFile>::new();
    let mut current: Option<AgentChangedFile> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(file) = current.take() {
                files.push(file);
            }
            let path = rest
                .split_whitespace()
                .nth(1)
                .or_else(|| rest.split_whitespace().next())
                .unwrap_or("")
                .trim_start_matches("b/")
                .trim_start_matches("a/")
                .to_string();
            current = Some(AgentChangedFile {
                path,
                status: "modified".into(),
                additions: 0,
                deletions: 0,
            });
            continue;
        }
        if let Some(file) = current.as_mut() {
            if line.starts_with("new file mode ") {
                file.status = "added".into();
            } else if line.starts_with("deleted file mode ") {
                file.status = "deleted".into();
            } else if line.starts_with("rename from ") {
                file.status = "renamed".into();
            } else if line.starts_with("+++ b/") {
                file.path = line.trim_start_matches("+++ b/").to_string();
            } else if line.starts_with('+') && !line.starts_with("+++") {
                file.additions = file.additions.saturating_add(1);
            } else if line.starts_with('-') && !line.starts_with("---") {
                file.deletions = file.deletions.saturating_add(1);
            }
        }
    }
    if let Some(file) = current {
        files.push(file);
    }
    files
}

fn diff_stats(diff: &str, changed_files: &[AgentChangedFile]) -> AgentDiffStats {
    AgentDiffStats {
        files_changed: changed_files.len(),
        additions: changed_files.iter().map(|f| f.additions).sum(),
        deletions: changed_files.iter().map(|f| f.deletions).sum(),
        bytes: diff.len(),
    }
}

pub fn report_for_run(run: &AgentRun) -> AgentRunReport {
    let changed_files = changed_files_from_diff(&run.diff);
    let diff_stats = diff_stats(&run.diff, &changed_files);
    let constraints = run
        .constraint_result
        .clone()
        .unwrap_or_else(|| AgentConstraintResult {
            passed: true,
            violations: Vec::new(),
        });
    let outcome = report_outcome(run, &constraints);
    AgentRunReport {
        run_id: run.id.clone(),
        outcome: outcome.clone(),
        summary: report_summary(run, &outcome, &diff_stats),
        task: run.task.clone(),
        mode: run.mode.as_str().into(),
        agent: run.agent.as_str().into(),
        model: run.model.clone(),
        template: run.template.clone(),
        hardness: run.hardness.as_str().into(),
        sandbox_id: run.sandbox_id.clone(),
        branch: run.branch.clone(),
        commit: run.commit.clone(),
        pr_url: run.pr_url.clone(),
        diff_stats,
        changed_files,
        constraints,
        verification: run.verification_results.clone(),
        artifacts: run.artifacts.clone(),
        error: run.error.clone(),
        stdout_tail: tail_text(&run.stdout, 4000),
        stderr_tail: tail_text(&run.stderr, 4000),
        logs_truncated: run.logs_truncated,
        duration_ms: run.finished_at.map(|finished| {
            finished
                .signed_duration_since(run.created_at)
                .num_milliseconds()
                .max(0) as u64
        }),
        generated_at: Utc::now(),
    }
}

fn report_outcome(run: &AgentRun, constraints: &AgentConstraintResult) -> String {
    match run.state {
        AgentRunState::Cancelled => "cancelled".into(),
        AgentRunState::Failed if !constraints.passed => "constraint_failed".into(),
        AgentRunState::Failed
            if run
                .verification_results
                .iter()
                .any(|r| !r.passed && r.fail_run) =>
        {
            "verification_failed".into()
        }
        AgentRunState::Failed => "failed".into(),
        AgentRunState::Succeeded if run.verification_results.iter().any(|r| !r.passed) => {
            "completed_with_failed_checks".into()
        }
        AgentRunState::Succeeded if run.mode == AgentRunMode::Review => "reviewed".into(),
        AgentRunState::Succeeded => "succeeded".into(),
        AgentRunState::Running => "running".into(),
        AgentRunState::Queued => "queued".into(),
    }
}

fn report_summary(run: &AgentRun, outcome: &str, stats: &AgentDiffStats) -> String {
    if let Some(error) = &run.error {
        return format!("{outcome}: {error}");
    }
    let checks = verification_summary(&run.verification_results);
    if run.mode == AgentRunMode::Review {
        return format!(
            "Review completed with {checks} verification, {} artifact(s), and {} changed file(s).",
            run.artifacts.len(),
            stats.files_changed
        );
    }
    let pr = if run.pr_url.is_some() {
        "draft PR opened"
    } else {
        "no PR opened"
    };
    format!(
        "Changed {} file(s) with {} insertion(s) and {} deletion(s); verification {checks}; {pr}.",
        stats.files_changed, stats.additions, stats.deletions
    )
}

fn tail_text(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut start = value.len() - max;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    format!("...{}", &value[start..])
}

fn task_title(run: &AgentRun) -> String {
    let raw = run
        .task
        .name
        .as_deref()
        .or_else(|| run.prompt.lines().find(|line| !line.trim().is_empty()))
        .unwrap_or("Workdir agent task")
        .trim();
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, 90)
}

fn task_slug(run: &AgentRun) -> String {
    let title = task_title(run);
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= 48 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "agent-run".into()
    } else {
        slug.into()
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn short_run_id(run: &AgentRun) -> String {
    run.id
        .strip_prefix("arun_")
        .unwrap_or(&run.id)
        .chars()
        .take(12)
        .collect()
}

fn render_report_markdown(run: &AgentRun) -> String {
    let report = report_for_run(run);
    let mut body = String::new();
    body.push_str(&format!("{}\n\n", report.summary));
    body.push_str("## Workdir report\n\n");
    body.push_str(&format!("- Run: `{}`\n", report.run_id));
    body.push_str(&format!("- Outcome: `{}`\n", report.outcome));
    body.push_str(&format!(
        "- Agent: `{}` / `{}`\n",
        report.agent, report.model
    ));
    body.push_str(&format!("- Hardness: `{}`\n", report.hardness));
    if let Some(task_name) = &report.task.name {
        body.push_str(&format!("- Task: `{task_name}`\n"));
    }
    if let Some(parent) = &report.task.parent_run_id {
        body.push_str(&format!("- Parent run: `{parent}`\n"));
    }
    if !report.task.labels.is_empty() {
        body.push_str(&format!(
            "- Labels: `{}`\n",
            report.task.labels.join("`, `")
        ));
    }
    body.push_str(&format!(
        "- Diff: {} file(s), {} insertion(s), {} deletion(s)\n",
        report.diff_stats.files_changed, report.diff_stats.additions, report.diff_stats.deletions
    ));
    if !report.verification.is_empty() {
        body.push_str("\n## Verification\n\n");
        for check in &report.verification {
            let status = if check.passed { "passed" } else { "failed" };
            body.push_str(&format!(
                "- `{}` {status} with exit code `{}`\n",
                check.name, check.exit_code
            ));
        }
    }
    if !report.changed_files.is_empty() {
        body.push_str("\n## Changed files\n\n");
        for file in &report.changed_files {
            body.push_str(&format!(
                "- `{}` ({}, +{}, -{})\n",
                file.path, file.status, file.additions, file.deletions
            ));
        }
    }
    body.push_str("\n## Prompt\n\n");
    body.push_str(&run.prompt);
    body.push('\n');
    body
}

fn agent_prompt(run: &AgentRun) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "Workdir background agent instructions:\n\
         - Modify files in the current git checkout directly.\n\
         - Do not only describe a patch or print a diff; apply the changes to the working tree.\n\
         - Do not commit, push, or open a pull request. Workdir will create the branch, commit, push, and pull request from your diff.\n\
         - Do not ask follow-up questions. Make reasonable assumptions and finish the requested change.\n\
         - Put optional small handoff artifacts for the main agent under .workdir/artifacts/.\n\n",
    );
    if run.mode == AgentRunMode::Review {
        prompt.push_str(
            "Mode: review\n\
             Inspect the repository and report findings. Do not intentionally modify tracked files.\n\
             A git diff is not required for this run.\n\n",
        );
    } else {
        prompt.push_str("Mode: change\nLeave the requested change in the working tree.\n\n");
    }
    if run.context.instructions.is_some()
        || !run.context.files.is_empty()
        || !run.context.links.is_empty()
    {
        prompt.push_str("Additional context is available under .workdir/context/.\n\n");
    }
    if let Some(goal) = &run.r#loop.goal {
        prompt.push_str("Goal:\n");
        prompt.push_str(goal);
        prompt.push_str("\n\n");
    }
    prompt.push_str(&run.prompt);
    prompt
}

fn agent_env(agent: AgentKind, api_key: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    match agent {
        AgentKind::Codex => {
            env.insert("CODEX_API_KEY".into(), api_key.to_string());
            // Older docs and Workdir examples used OPENAI_API_KEY. Keep it
            // available for compatibility with existing templates and tests.
            env.insert("OPENAI_API_KEY".into(), api_key.to_string());
        }
        AgentKind::ClaudeCode => {
            env.insert("ANTHROPIC_API_KEY".into(), api_key.to_string());
        }
    }
    env
}

fn agent_command(run: &AgentRun) -> String {
    let model = shell_quote(&run.model);
    match run.agent {
        AgentKind::Codex => {
            let codex_home = shell_quote(&format!("/tmp/workdir-codex-home-{}", run.id));
            format!(
            "set -e; export PATH=\"$PWD/.workdir/bin:$PWD:$PATH\"; \
             export CODEX_HOME={codex_home}; mkdir -p \"$CODEX_HOME\"; \
             if ! command -v codex >/dev/null 2>&1; then npm install -g @openai/codex; fi; \
             codex exec --model {model} --ignore-user-config \
               --dangerously-bypass-approvals-and-sandbox \
               --cd \"$PWD\" --skip-git-repo-check --output-last-message /tmp/workdir-agent-final.txt - \
               < .workdir-agent-prompt.txt"
            )
        }
        AgentKind::ClaudeCode => format!(
            "set -e; export PATH=\"$PWD/.workdir/bin:$PWD:$PATH\"; \
             if ! command -v claude >/dev/null 2>&1; then npm install -g @anthropic-ai/claude-code; fi; \
             claude --print --model {model} --permission-mode bypassPermissions \
               --dangerously-skip-permissions < .workdir-agent-prompt.txt \
               | tee /tmp/workdir-agent-final.txt"
        ),
    }
}

fn validate_secret_exists(state: &AppState, org_id: &str, name: &str) -> ApiResult<()> {
    if !secrets::valid_name(name) {
        return Err(ApiError::BadRequest(format!(
            "secret '{name}' must be an env-style identifier"
        )));
    }
    state
        .store
        .get_secret(org_id, name)
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::BadRequest(format!("secret '{name}' is not defined")))?;
    Ok(())
}

fn decrypt_secret(state: &AppState, org_id: &str, name: &str) -> Result<String> {
    let rec = state
        .store
        .get_secret(org_id, name)?
        .ok_or_else(|| anyhow!("secret '{name}' is not defined"))?;
    crate::secrets::decrypt(&state.secret_key, &rec)
}

fn truncate(mut value: String, max: usize) -> (String, bool) {
    if value.len() <= max {
        return (value, false);
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    (value, true)
}

fn redact_secret(value: String, secret: Option<&str>) -> String {
    match secret.filter(|s| !s.is_empty()) {
        Some(secret) => value.replace(secret, "[redacted]"),
        None => value,
    }
}

struct PullRequestResult {
    branch: String,
    commit: String,
    url: String,
}

async fn create_github_pr(
    state: &AppState,
    run: &AgentRun,
    github: &AgentGithubConfig,
    diff: &str,
    token: &str,
) -> Result<PullRequestResult> {
    let repo = GithubRepo::parse(&run.repo.url)?;
    let base = github
        .base_branch
        .clone()
        .or_else(|| run.repo.r#ref.clone())
        .unwrap_or_else(|| "main".to_string());
    let title = task_title(run);
    let branch = format!("workdir/{}-{}", task_slug(run), short_run_id(run));
    let root = std::env::temp_dir().join(format!("workdir-agent-{}", run.id));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).context("create agent temp dir")?;
    let repo_dir = root.join("repo");
    let patch_path = root.join("agent.patch");
    std::fs::write(&patch_path, diff).context("write agent patch")?;
    let askpass = root.join("askpass.sh");
    std::fs::write(
        &askpass,
        "#!/bin/sh\ncase \"$1\" in *Username*) echo x-access-token ;; *) echo \"$GITHUB_TOKEN\" ;; esac\n",
    )
    .context("write git askpass")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o700)).ok();
    }

    let clone_url = format!("https://github.com/{}/{}.git", repo.owner, repo.name);
    run_host_cmd(
        &root,
        &askpass,
        token,
        "git",
        &[
            "clone", "--depth", "1", "--branch", &base, &clone_url, "repo",
        ],
    )
    .await
    .context("clone repo")?;
    run_host_cmd(
        &repo_dir,
        &askpass,
        token,
        "git",
        &["checkout", "-b", &branch],
    )
    .await
    .context("create branch")?;
    run_host_cmd(
        &repo_dir,
        &askpass,
        token,
        "git",
        &["apply", "--index", "--binary", patch_path.to_str().unwrap()],
    )
    .await
    .context("apply agent patch")?;
    run_host_cmd(
        &repo_dir,
        &askpass,
        token,
        "git",
        &["config", "user.name", "workdir-agent"],
    )
    .await?;
    run_host_cmd(
        &repo_dir,
        &askpass,
        token,
        "git",
        &["config", "user.email", "agent@workdir.dev"],
    )
    .await?;
    let message = format!("{title}\n\nWorkdir-Agent-Run: {}", run.id);
    run_host_cmd(
        &repo_dir,
        &askpass,
        token,
        "git",
        &["commit", "-m", &message],
    )
    .await
    .context("commit agent patch")?;
    let commit = run_host_cmd(&repo_dir, &askpass, token, "git", &["rev-parse", "HEAD"])
        .await
        .context("read commit")?
        .trim()
        .to_string();
    run_host_cmd(
        &repo_dir,
        &askpass,
        token,
        "git",
        &["push", "origin", &format!("HEAD:refs/heads/{branch}")],
    )
    .await
    .context("push branch")?;

    let body = render_report_markdown(run);
    let res = state
        .http
        .post(format!(
            "https://api.github.com/repos/{}/{}/pulls",
            repo.owner, repo.name
        ))
        .bearer_auth(token)
        .header("User-Agent", "workdir-agent")
        .json(&json!({
            "title": title,
            "head": branch,
            "base": base,
            "body": body,
            "draft": github.draft.unwrap_or(true),
        }))
        .send()
        .await
        .context("create GitHub PR")?;
    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("create GitHub PR failed ({status}): {body}");
    }
    let pr: Value = res.json().await.context("parse GitHub PR response")?;
    let url = pr
        .get("html_url")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("GitHub PR response did not include html_url"))?
        .to_string();
    let _ = std::fs::remove_dir_all(&root);
    Ok(PullRequestResult {
        branch,
        commit,
        url,
    })
}

async fn run_host_cmd(
    cwd: &Path,
    askpass: &Path,
    token: &str,
    program: &str,
    args: &[&str],
) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", askpass)
        .env("GITHUB_TOKEN", token)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("run {program} {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "{} {} failed: {}",
            program,
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

struct GithubRepo {
    owner: String,
    name: String,
}

impl GithubRepo {
    fn parse(url: &str) -> Result<GithubRepo> {
        let mut s = url.trim().trim_end_matches('/').to_string();
        if let Some(rest) = s.strip_prefix("git@github.com:") {
            s = format!("https://github.com/{rest}");
        }
        let marker = "github.com/";
        let rest = s
            .split_once(marker)
            .map(|(_, rest)| rest)
            .ok_or_else(|| anyhow!("only GitHub repositories are supported for PR creation"))?;
        let mut parts = rest.split('/');
        let owner = parts
            .next()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow!("GitHub repo owner missing"))?;
        let name = parts
            .next()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow!("GitHub repo name missing"))?
            .trim_end_matches(".git");
        Ok(GithubRepo {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentRepoSpec, AgentTask};

    #[test]
    fn pr_helpers_use_descriptive_task_metadata() {
        let now = Utc::now();
        let run = AgentRun {
            id: "arun_abcdef123456".into(),
            org_id: "org1".into(),
            state: AgentRunState::Succeeded,
            sandbox_id: Some("sbx_1".into()),
            template: Some("node-app".into()),
            repo: AgentRepoSpec {
                url: "https://github.com/acme/app.git".into(),
                r#ref: Some("main".into()),
            },
            prompt: "Fix the very vague thing".into(),
            model: "gpt-5".into(),
            agent: AgentKind::Codex,
            api_key_secret: "OPENAI_API_KEY".into(),
            hardness: Hardness::Medium,
            r#loop: Default::default(),
            github: None,
            task: AgentTask {
                name: Some("Fix login timeout regression".into()),
                labels: vec!["auth".into()],
                ..Default::default()
            },
            mode: AgentRunMode::Change,
            constraints: Default::default(),
            context: Default::default(),
            verify: Vec::new(),
            verification_results: Vec::new(),
            artifacts: Vec::new(),
            constraint_result: None,
            report: None,
            stdout: String::new(),
            stderr: String::new(),
            diff: "diff --git a/auth.txt b/auth.txt\nnew file mode 100644\n--- /dev/null\n+++ b/auth.txt\n@@ -0,0 +1 @@\n+fixed\n".into(),
            logs_truncated: false,
            verification_result: None,
            branch: None,
            commit: None,
            pr_url: None,
            error: None,
            created_at: now,
            updated_at: now,
            finished_at: Some(now),
        };

        assert_eq!(task_title(&run), "Fix login timeout regression");
        assert_eq!(task_slug(&run), "fix-login-timeout-regression");
        let body = render_report_markdown(&run);
        assert!(body.contains("Fix login timeout regression"));
        assert!(body.contains("auth.txt"));
        assert!(body.contains("Workdir report"));
    }
}
