# REST API Reference

Base URL: `https://api.<domain>` (dev: `http://127.0.0.1:8080`).
Auth: `Authorization: Bearer sk_live_...` on every `/v1` route.
All bodies are JSON. Errors are `{"error": {"code", "message", "reason?"}}`.

The default create is one call with no body and yields the cheapest, fastest path.

## Sandboxes

| Method | Path | Notes |
|---|---|---|
| `POST` | `/v1/sandboxes` | Create. Empty body = default cheap path. |
| `GET` | `/v1/sandboxes` | List the caller's sandboxes. |
| `GET` | `/v1/sandboxes/:id` | Get one (with timings, urls, price, uptime, cost). |
| `POST` | `/v1/sandboxes/:id/exec` | `{cmd, cwd?, env?, background?}` → sync `{exit_code, stdout, stderr}` or, with `background: true`, `202 {cmd_id, state, status_url, logs_url}`. |
| `GET` | `/v1/sandboxes/:id/exec/:cmd_id` | Poll background exec status → `{state, exit_code?, started_at, finished_at?, error?}`. |
| `GET` | `/v1/sandboxes/:id/exec/:cmd_id/logs` | Fetch captured background exec output → `{stdout, stderr, truncated}`. |
| `GET` | `/v1/sandboxes/:id/pty` | WebSocket interactive shell (a real in-guest TTY over vsock on Firecracker). |
| `GET` | `/v1/sandboxes/:id/metrics` | Working-set metrics: host RSS vs reserved shape, balloon target + guest memory stats, net counters. |
| `GET` | `/v1/sandboxes/:id/files?path=…` | Read a file → `{content, encoding}`. |
| `PUT` | `/v1/sandboxes/:id/files` | `{path, content, encoding?}` → write. |
| `POST` | `/v1/sandboxes/:id/ports/:port/expose` | → `{port, url}` preview route. |
| `GET`/`POST` | `/v1/sandboxes/:id/browser` | Browser readiness + VNC/CDP urls + screenshot url. |
| `GET` | `/v1/sandboxes/:id/browser/screenshot` | PNG of the live desktop (X root window). |
| `POST` | `/v1/sandboxes/:id/snapshot` | Snapshot (billed separately). |
| `POST` | `/v1/sandboxes/:id/fork` | Clone an instant sibling from the parent's live state (`boot_path: "fork"`). |
| `POST` | `/v1/sandboxes/:id/pause` | Stop (release CPU/mem; keep billing correct). |
| `POST` | `/v1/sandboxes/:id/resume` | Resume from stopped disk/snapshot. |
| `DELETE` | `/v1/sandboxes/:id` | Stop, delete ephemeral disk, remove routes. |

## Templates

Org-scoped named sandbox create configs. A template's `create` field is the same
JSON body accepted by `POST /v1/sandboxes`.

| Method | Path | Notes |
|---|---|---|
| `POST` | `/v1/templates` | `{name, description?, create}` → store a named create config. |
| `GET` | `/v1/templates` | List the caller's templates. |
| `GET` | `/v1/templates/:name` | Get one template. |
| `PUT` | `/v1/templates/:name` | Replace `description` + `create`. |
| `DELETE` | `/v1/templates/:name` | Delete one template. |
| `POST` | `/v1/templates/:name/sandboxes` | `{count?, overrides?}` → create 1–20 fresh sandboxes from the template. |

```jsonc
{
  "name": "node-app",
  "create": {
    "image": "node-python",
    "resources": { "cpu": 2, "memory_mb": 4096, "disk_gb": 16 },
    "startup": {
      "git": { "url": "https://github.com/acme/app.git", "ref": "main" },
      "commands": [{ "name": "install", "run": "pnpm install --frozen-lockfile" }],
      "ports": [3000]
    }
  }
}
```

## Agent runs

Agent runs create a sandbox from a template or hardness profile, run Codex or
Claude Code inside it, collect a staged git diff, and optionally open a GitHub
PR from the control plane. They also produce a backend-generated report for
main-agent orchestration. Provider keys and GitHub tokens are referenced by
secret name; secret values are never returned.

| Method | Path | Notes |
|---|---|---|
| `POST` | `/v1/agent-runs` | Start an async run → `202 {id, state, status_url, logs_url}`. |
| `GET` | `/v1/agent-runs` | List runs with compact report fields. Supports `parent_run_id`, `label`, and `state` filters. |
| `GET` | `/v1/agent-runs/:id` | Status, sandbox id, report, branch/commit/PR URL, error. |
| `GET` | `/v1/agent-runs/:id/report` | Backend-generated run report. |
| `GET` | `/v1/agent-runs/:id/children` | Runs whose `task.parent_run_id` is this id, with compact report fields. |
| `POST` | `/v1/agent-runs/:id/cancel` | Cooperatively cancel a queued/running run. |
| `GET` | `/v1/agent-runs/:id/logs` | Captured stdout/stderr and staged diff. |

```jsonc
{
  "template": "node-app",
  "repo": { "url": "https://github.com/acme/app.git", "ref": "main" },
  "agent": "codex",                  // codex | claude_code
  "model": "gpt-5",
  "api_key_secret": "OPENAI_API_KEY",
  "prompt": "Fix the failing tests and keep the change minimal.",
  "hardness": "medium",              // easy | medium | hard
  "task": { "name": "Fix auth test failures", "labels": ["auth", "delegated"] },
  "mode": "change",                  // change | review
  "loop": { "goal": "Fix the failing tests and leave the repo with a clean diff.", "max_iterations": 2 },
  "constraints": { "allowed_paths": ["src/auth", "tests/auth"], "max_changed_files": 8 },
  "context": { "instructions": "Keep the public API stable." },
  "verify": [{ "name": "auth tests", "run": "cargo test auth", "fail_run": true }],
  "github": { "token_secret": "GITHUB_TOKEN", "base_branch": "main", "draft": true }
}
```

If `github.token_secret` is omitted, the run stops after collecting the diff.
When present, the token is used only by the control plane to create the branch,
commit, push, and PR; it is not injected into the sandbox.

`api_key_secret` names the stored provider key. Workdir maps that value to the
selected CLI's required environment variable (`CODEX_API_KEY` for Codex,
`ANTHROPIC_API_KEY` for Claude Code) when it starts the agent.

Agent prompts and loop goals should ask the agent to modify files in the cloned
repo. The sandbox agent should not commit, push, or create the PR; Workdir does
that headlessly from the collected diff.

Use `mode: "review"` for inspection-only subtasks. Review runs do not require a
diff or PR. Reports include outcome, summary, changed files, verification
results, artifacts from `.workdir/artifacts/`, constraints, branch/commit/PR,
and stdout/stderr tails.

List and children endpoints return compact report summaries for orchestration
dashboards. Use `GET /v1/agent-runs/:id/report` when a delegating agent needs
the full authoritative report.

### Lifecycle & perpetual standby

States: `creating → running → stopping → {stopped|standby} → resuming → running`,
plus `deleting → deleted` and `failed`.

- **`stopped`** is a user-initiated pause (`POST .../pause`) and requires an
  explicit `POST .../resume`.
- **`standby`** is automatic: when a sandbox is idle past `auto_stop_seconds`,
  the reaper snapshots it, frees its RAM, and parks it in `standby` at **$0**.
  The **next request** (`exec`, file read/write, `expose`, browser, `fork`)
  **transparently auto-resumes** it — the caller just sees a slightly slower
  first call. To the user the sandbox stays alive; it simply stops costing
  anything while idle. (Sandboxes with resident secrets are never snapshotted, so
  they fall back to a plain `stopped` instead.)
- Running background exec jobs keep the sandbox out of automatic standby until
  the tracked command exits.

### Create request

```jsonc
// default cheap path
{}

// or "startup": "none" explicitly
{ "startup": "none" }

// heavier path — explicit options required (spec §3.4)
{
  "image": "browser",
  "resources": { "cpu": 2, "memory_mb": 4096, "disk_gb": 16 },
  "browser": { "enabled": true, "vnc": true, "cdp": true },
  "auto_stop_seconds": 300,
  "snapshot": false,
  "startup": {
    "git": { "url": "https://github.com/acme/app.git", "ref": "main", "depth": 1 },
    "env": { "NODE_ENV": "development" },
    "secrets": ["OPENAI_API_KEY"],
    "cache": { "package_managers": ["npm", "pnpm", "pip", "uv"] },
    "commands": [
      { "name": "install", "run": "pnpm install --frozen-lockfile", "cache_key": "pnpm-lock.yaml" },
      { "name": "dev", "run": "pnpm dev --host 0.0.0.0", "background": true }
    ],
    "ports": [3000, 6080],
    "ready": { "http": "http://127.0.0.1:3000", "timeout_seconds": 30 },
    "network": {
      "egress": "allowlist",
      "allow": [
        { "type": "domain", "value": "api.openai.com", "protocol": "tcp", "ports": [443] },
        "93.184.216.34"
      ]
    }
  }
}
```

### Network policy

`startup.network` is create-time only and is enforced by the Firecracker host
against the sandbox tap. Omit it, or set `{ "egress": "default" }`, to keep the
backward-compatible default internet egress with baseline metadata/private/SMTP
blocks.

Modes:

| `egress` | Behavior |
|---|---|
| `default` | Current default egress; `allow`/`deny` must be empty. |
| `none` | Drop all forwarded sandbox egress. |
| `allowlist` | Allow only `allow` rules; at least one rule required. |
| `denylist` | Drop `deny` rules and allow the rest; at least one rule required. |

Rules may be shorthand strings (`"93.184.216.34"`, `"203.0.113.0/24"`,
`"api.example.com"`) or objects:

```json
{ "type": "domain", "value": "*.example.com", "protocol": "tcp", "ports": [443] }
```

`type` is `cidr` or `domain`; `protocol` is `tcp` or `udp`; `ports` is optional.
Domain rules support exact hostnames and one-label wildcards (`*.example.com`).
URLs, paths, raw `*`, invalid ports, IPv6 rules, and allowlisted private or
metadata ranges are rejected.

**Constrained knobs (spec §3.2)** — arbitrary values like 13 GB or 250 GB are
rejected with `400 bad_request`:

| Knob | Allowed | Default |
|---|---|---|
| `cpu` | 0.5, 1, 2, 4 | 1 |
| `memory_mb` | 512, 1024, 2048, 4096, 8192, 16384 | 2048 |
| `disk_gb` | 8, 16, 32, 64 | 8 |
| `image` | base, node-python, browser, heavy-build, custom/… | base |
| `auto_stop_seconds` | 30–3600 | 120 |

### Create / get response

```jsonc
{
  "id": "sbx_…",
  "runtime": "firecracker",
  "image": "base",
  "state": "running",
  "resources": { "cpu": "1 shared vCPU", "memory_mb": 2048, "disk_gb": 8 },
  "node_id": "node_…",
  "boot_path": "hot_pool",          // hot_pool | snapshot_restore | cold_boot | fork
  "boot_ms": 42,
  "browser_ready_ms": 1280,          // present only for browser sandboxes
  "coding_agent": "opencode",        // present only when the coding agent is opted in
  "auto_stop_seconds": 120,
  "snapshot_enabled": false,
  "timings": { "boot_ms": 42, "image_cache_ms": 0, "git_ms": 0,
               "install_ms": 0, "ready_ms": 0, "total_ms": 43 },
  "urls": { "ports": { "3000": "https://sbx-…-3000.<domain>" },
            "vnc": "https://sbx-…-6080.<domain>",
            "cdp": "https://sbx-…-9222.<domain>" },
  "mounts": [],
  "volumes": [],
  "network": { "egress": "default" },
  "price": { "resource_units": 1.0, "image_multiplier": 1.0,
             "unit_price_usd_hr": 0.009, "price_usd_hr": 0.009,
             "price_usd_second": 0.0000025 },
  "uptime_seconds": 0,
  "cost_estimate_usd": 0.0
}
```

## Images (spec §10, §11)

| Method | Path | Notes |
|---|---|---|
| `GET` | `/v1/images` | Curated catalog + the org's custom images. |
| `POST` | `/v1/images` | Build/import **asynchronously** (`202 Accepted`). |
| `GET` | `/v1/images/:id` | Status + `build_log` + cache-miss time. |
| `DELETE` | `/v1/images/:id` | Soft delete: blocks new creates, keeps running sandboxes. |

```jsonc
// POST /v1/images
{
  "source": { "type": "dockerfile",
              "context_url": "https://github.com/acme/app/archive/main.tar.gz",
              "dockerfile": "Dockerfile" },
  "name": "custom/acme/app",
  "resources_hint": { "cpu": 2, "memory_mb": 4096, "disk_gb": 16 }
}
// or  "source": { "type": "oci", "image_ref": "ghcr.io/acme/app:1.2.3" }
```

Use a published custom image: `{"image": "custom/acme/app", "image_version": "2026-06-10-ab12cd"}`.

## Secrets (feature)

Org-scoped, encrypted at rest, never returned over the API. See
[FEATURES.md](FEATURES.md#1-secret-management).

| Method | Path | Notes |
|---|---|---|
| `GET` | `/v1/secrets` | List secret names + timestamps (no values). |
| `PUT` | `/v1/secrets/:name` | `{value}` → store/replace (encrypted). |
| `DELETE` | `/v1/secrets/:name` | Remove a secret. |

Reference secrets in `startup.secrets: ["NAME", ...]`; values are injected into
the sandbox env after assignment. A sandbox with resident secrets cannot be
snapshotted (`409`).

## Persistent volumes

Org-scoped block storage that **survives sandbox deletion**, so workspace state
persists across sessions. A volume attaches to at most one running sandbox at a
time.

| Method | Path | Notes |
|---|---|---|
| `GET` | `/v1/volumes` | List the org's volumes. |
| `POST` | `/v1/volumes` | `{name, size_gb}` → create. `size_gb` ∈ {1,5,10,20,50,100,250}. |
| `GET` | `/v1/volumes/:id` | Get one (incl. `attached_to`). |
| `DELETE` | `/v1/volumes/:id` | Delete + free storage; `409` while attached. |

Attach at sandbox-create with `volumes: [{ "volume_id": "vol_…",
"mount_path": "/mnt/data" }]`. The volume is mounted (ext4) at `mount_path` in
the guest; attaching forces a cold boot. Deleting the sandbox detaches the
volume (data intact) so it can be re-attached to a new one.

## Extended create options (features)

Added to `POST /v1/sandboxes` (all optional, default-off):

```jsonc
{
  "docker": { "enabled": true },                 // dockerd inside the guest VM (heavy-build/custom image)
  "coding_agent": { "enabled": true },           // install opencode CLI into the guest (opt-in; any image)
  "mounts": [ { "type": "s3", "bucket": "my-data", "mount_path": "/mnt/data",
                "read_only": true, "prefix": "p/", "region": "us-east-1" } ],
  "volumes": [ { "volume_id": "vol_...", "mount_path": "/mnt/project" } ],
  "files":  [ { "path": "config.json", "content": "{}", "encoding": "utf8" } ],
  "startup": {
    "network": {
      "egress": "allowlist",
      "allow": [{ "type": "domain", "value": "api.openai.com", "protocol": "tcp", "ports": [443] }]
    }
  }
}
```

Custom images accept `"ephemeral": true` + `"ttl_seconds": N` for auto-GC'd
one-off images. Full reference: [FEATURES.md](FEATURES.md).

## Nodes (spec §8)

| Method | Path | Notes |
|---|---|---|
| `GET` | `/v1/nodes` | Nodes + capacity in default-equivalent units + add-node command. |
| `POST` | `/v1/nodes/join-token` | Admin: mint/rotate a worker join token. |
| `POST` | `/v1/nodes/:id/drain` | Admin: mark unschedulable + draining. |

## Usage, billing, benchmarks

| Method | Path | Notes |
|---|---|---|
| `GET` | `/v1/usage` | Org cost, delivered unit-seconds, prepaid balance, per-sandbox. |
| `GET` | `/v1/admin/overview` | Admin: nodes, hot pools, reconciled at-cost price, abuse alerts. |
| `GET` | `/v1/benchmarks` | Latency table: p50/p90/p95 by image **and boot path** (`cold_boot`/`hot_pool`/`snapshot_restore`/`fork`), reported separately and never merged. |
| `POST` | `/v1/benchmarks/run` | Admin: run a fresh harness sweep `{image?, iterations?}` and return the recomputed table. |
| `GET` | `/healthz` | Liveness (no auth). |

The benchmark harness (roadmap Phase 0) drives the runtime directly with
throwaway VMs — not billable sandboxes — so the baseline measures the boot
machinery honestly. `snapshot_restore` is the perpetual-standby resume path and
carries the Phase 2 targets (`p50 < 25ms`, `p90 < 50ms`), surfaced under
`targets` in the response.

## Preview proxy (spec §16.2)

Host-routed: `https://<hostname-safe-id>-<port>.<domain>/…` (sandbox ids render
their `sbx_` prefix as `sbx-` in the DNS label). HTTP is forwarded;
WebSocket/CDP/VNC upgrades are bridged. Requires a valid API key (header,
`?key=`, or the preview cookie set by an earlier `?key=` request) belonging to
the sandbox's org. A path-based form `/_preview/<id>/<port>/<rest>` exists for
environments without wildcard DNS; use `/_preview/<id>/<port>` for the upstream
root path.

### Driving the browser over CDP

For `browser` sandboxes the create/get response includes `urls.cdp`
(`https://<id>-9222.<domain>`). It speaks the Chrome DevTools Protocol, so any
CDP client — Playwright, Puppeteer, `chrome-remote-interface` — can drive the
live Chrome.

Like every preview route it **requires your API key** (any key in the sandbox's
org, or admin), passed as a `?key=` query param or an `Authorization: Bearer`
header. An unauthenticated request returns `404` by design — existence is never
leaked across orgs. The `key=` param is stripped before the request reaches
Chrome and redacted from logs, so it is safe in the URL; prefer it for WebSocket
clients that cannot set headers on the upgrade.

When a browser request authenticates with `?key=`, the proxy also returns an
HttpOnly, Secure, SameSite preview cookie scoped to that preview host. Follow-up
subresource requests and WebSocket upgrades can then authenticate without
rewriting every URL. The proxy strips that preview cookie before forwarding to
the sandbox, and it also blocks a sandbox from setting the reserved preview
cookie name in upstream responses.

```js
import { chromium } from "playwright";

// query-param auth — most portable; also covers the raw WebSocket upgrade
const browser = await chromium.connectOverCDP(`${cdpUrl}?key=${apiKey}`);

// or header auth
// const browser = await chromium.connectOverCDP(cdpUrl, {
//   headers: { Authorization: `Bearer ${apiKey}` },
// });

const page = await browser.newPage();
await page.goto("https://example.com");
```

Chrome advertises an internal `webSocketDebuggerUrl` (the guest IP); Playwright
and Puppeteer rewrite it to the endpoint host automatically, so no extra config
is needed. `GET /json/version` and `/json/list` work the same way for manual
target discovery.

## Error codes

`bad_request` (400), `unauthorized` (401), `forbidden` (403), `not_found` (404),
`conflict` (409), `rejected` (422), `no_capacity` (503, with `reason`),
`internal` (500). `no_capacity.reason` is one of `no_nodes`,
`no_schedulable_nodes`, `no_browser_capable_node`, `memory_admission`,
`no_fit`. Hosted deployments may append operator-configured guidance to the
human `message`, while the stable `code` and `reason` remain unchanged.
