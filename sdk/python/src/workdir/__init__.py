"""Minimal Python SDK for workdir.

The default path is one call::

    from workdir import Client
    client = Client("https://api.sandboxes.example.com", api_key="sk_live_...")
    sandbox = client.sandboxes.create()          # cheap default path
    print(sandbox.exec("echo ok").stdout)
    sandbox.delete()

Heavier sandboxes require explicit options (spec §3.4)::

    sandbox = client.sandboxes.create(
        image="browser",
        resources={"cpu": 2, "memory_mb": 4096, "disk_gb": 16},
        browser={"enabled": True, "vnc": True, "cdp": True},
        startup={
            "git": {"url": "https://github.com/acme/app.git", "ref": "main", "depth": 1},
            "commands": [{"name": "install", "run": "pnpm install --frozen-lockfile"}],
            "ports": [3000, 6080],
            "ready": {"http": "http://127.0.0.1:3000", "timeout_seconds": 30},
        },
    )
    print(sandbox.urls["vnc"])

Opt in to an in-sandbox coding agent (opencode), installed on demand::

    sandbox = client.sandboxes.create(
        coding_agent={"enabled": True},
        startup={"secrets": ["ANTHROPIC_API_KEY"]},
    )
    sandbox.exec("opencode run 'add a test for utils.py'")

Uses only the standard library (urllib), so it has zero dependencies.
"""

from __future__ import annotations

import json
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any, Optional, Union

__version__ = "0.3.2"
__all__ = [
    "AgentRun",
    "AgentRunLogs",
    "AgentRunReport",
    "Client",
    "ExecJob",
    "ExecLogs",
    "ExecResult",
    "Sandbox",
    "SandboxError",
    "SandboxTemplate",
]


class SandboxError(Exception):
    def __init__(self, status: int, code: str, message: str):
        super().__init__(f"[{status} {code}] {message}")
        self.status = status
        self.code = code
        self.message = message


@dataclass
class ExecResult:
    exit_code: int
    stdout: str
    stderr: str


@dataclass
class ExecJob:
    cmd_id: str
    state: str
    started_at: str
    exit_code: Optional[int] = None
    finished_at: Optional[str] = None
    error: Optional[str] = None
    logs_truncated: bool = False
    status_url: Optional[str] = None
    logs_url: Optional[str] = None


@dataclass
class ExecLogs:
    cmd_id: str
    state: str
    stdout: str
    stderr: str
    truncated: bool


@dataclass
class SandboxTemplate:
    id: str
    name: str
    create: dict
    created_at: str
    updated_at: str
    description: Optional[str] = None


@dataclass
class AgentRun:
    id: str
    state: str
    created_at: str
    updated_at: str
    sandbox_id: Optional[str] = None
    template: Optional[str] = None
    repo: Optional[dict] = None
    prompt: Optional[str] = None
    model: Optional[str] = None
    agent: Optional[str] = None
    api_key_secret: Optional[str] = None
    hardness: Optional[str] = None
    loop: Optional[dict] = None
    github: Optional[dict] = None
    task: Optional[dict] = None
    mode: Optional[str] = None
    constraints: Optional[dict] = None
    context: Optional[dict] = None
    verify: Optional[list[dict]] = None
    verification_result: Optional[str] = None
    verification_results: Optional[list[dict]] = None
    artifacts: Optional[list[dict]] = None
    constraint_result: Optional[dict] = None
    report: Optional["AgentRunReport"] = None
    branch: Optional[str] = None
    commit: Optional[str] = None
    pr_url: Optional[str] = None
    error: Optional[str] = None
    logs_truncated: bool = False
    finished_at: Optional[str] = None
    status_url: Optional[str] = None
    logs_url: Optional[str] = None
    report_url: Optional[str] = None
    children_url: Optional[str] = None


@dataclass
class AgentRunReport:
    run_id: str
    outcome: str
    summary: str
    task: dict
    mode: str
    agent: str
    model: str
    hardness: str
    diff_stats: dict
    changed_files: list[dict]
    constraints: dict
    verification: list[dict]
    artifacts: list[dict]
    stdout_tail: str
    stderr_tail: str
    logs_truncated: bool
    generated_at: str
    template: Optional[str] = None
    sandbox_id: Optional[str] = None
    branch: Optional[str] = None
    commit: Optional[str] = None
    pr_url: Optional[str] = None
    error: Optional[str] = None
    duration_ms: Optional[int] = None


@dataclass
class AgentRunLogs:
    id: str
    state: str
    stdout: str
    stderr: str
    diff: str
    truncated: bool


class _Http:
    def __init__(self, base_url: str, api_key: str, timeout: float = 60.0):
        self.base = base_url.rstrip("/")
        self.key = api_key
        self.timeout = timeout

    def request(self, method: str, path: str, body: Optional[dict] = None) -> Any:
        url = f"{self.base}{path}"
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, method=method)
        req.add_header("Authorization", f"Bearer {self.key}")
        req.add_header("Content-Type", "application/json")
        req.add_header("User-Agent", f"mv37-workdir-python/{__version__}")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                raw = resp.read()
                return json.loads(raw) if raw else {}
        except urllib.error.HTTPError as e:
            raw = e.read()
            try:
                err = json.loads(raw)["error"]
                raise SandboxError(e.code, err.get("code", "error"), err.get("message", "")) from None
            except (ValueError, KeyError):
                raise SandboxError(e.code, "error", raw.decode(errors="replace")) from None


class Sandbox:
    def __init__(self, http: _Http, data: dict):
        self._http = http
        self._data = data

    @property
    def id(self) -> str:
        return self._data["id"]

    @property
    def state(self) -> str:
        return self._data["state"]

    @property
    def boot_path(self) -> str:
        return self._data["boot_path"]

    @property
    def timings(self) -> dict:
        return self._data.get("timings", {})

    @property
    def urls(self) -> dict:
        return self._data.get("urls", {})

    @property
    def price(self) -> dict:
        return self._data.get("price", {})

    @property
    def network(self) -> dict:
        return self._data.get("network", {})

    def refresh(self) -> "Sandbox":
        self._data = self._http.request("GET", f"/v1/sandboxes/{self.id}")
        return self

    def exec(self, cmd: str, cwd: Optional[str] = None, env: Optional[dict] = None,
             background: bool = False) -> Union[ExecResult, ExecJob]:
        body = {"cmd": cmd, "background": background}
        if cwd:
            body["cwd"] = cwd
        if env:
            body["env"] = env
        r = self._http.request("POST", f"/v1/sandboxes/{self.id}/exec", body)
        if background:
            return ExecJob(
                cmd_id=r["cmd_id"],
                state=r["state"],
                started_at=r["started_at"],
                exit_code=r.get("exit_code"),
                finished_at=r.get("finished_at"),
                error=r.get("error"),
                logs_truncated=bool(r.get("logs_truncated", False)),
                status_url=r.get("status_url"),
                logs_url=r.get("logs_url"),
            )
        return ExecResult(r["exit_code"], r["stdout"], r["stderr"])

    def exec_status(self, cmd_id: str) -> ExecJob:
        r = self._http.request("GET", f"/v1/sandboxes/{self.id}/exec/{cmd_id}")
        return ExecJob(
            cmd_id=r["cmd_id"],
            state=r["state"],
            started_at=r["started_at"],
            exit_code=r.get("exit_code"),
            finished_at=r.get("finished_at"),
            error=r.get("error"),
            logs_truncated=bool(r.get("logs_truncated", False)),
            status_url=r.get("status_url"),
            logs_url=r.get("logs_url"),
        )

    def exec_logs(self, cmd_id: str) -> ExecLogs:
        r = self._http.request("GET", f"/v1/sandboxes/{self.id}/exec/{cmd_id}/logs")
        return ExecLogs(
            cmd_id=r["cmd_id"],
            state=r["state"],
            stdout=r["stdout"],
            stderr=r["stderr"],
            truncated=bool(r.get("truncated", False)),
        )

    def write_file(self, path: str, content: str) -> None:
        self._http.request("PUT", f"/v1/sandboxes/{self.id}/files",
                           {"path": path, "content": content})

    def read_file(self, path: str) -> str:
        q = urllib.parse.urlencode({"path": path})
        r = self._http.request("GET", f"/v1/sandboxes/{self.id}/files?{q}")
        return r["content"]

    def expose_port(self, port: int) -> str:
        r = self._http.request("POST", f"/v1/sandboxes/{self.id}/ports/{port}/expose")
        return r["url"]

    def browser(self) -> dict:
        return self._http.request("GET", f"/v1/sandboxes/{self.id}/browser")

    def metrics(self) -> dict:
        return self._http.request("GET", f"/v1/sandboxes/{self.id}/metrics")

    def snapshot(self) -> dict:
        return self._http.request("POST", f"/v1/sandboxes/{self.id}/snapshot")

    def fork(self) -> "Sandbox":
        return Sandbox(self._http, self._http.request("POST", f"/v1/sandboxes/{self.id}/fork"))

    def pause(self) -> "Sandbox":
        self._data = self._http.request("POST", f"/v1/sandboxes/{self.id}/pause")
        return self

    def resume(self) -> "Sandbox":
        self._data = self._http.request("POST", f"/v1/sandboxes/{self.id}/resume")
        return self

    def delete(self) -> None:
        self._http.request("DELETE", f"/v1/sandboxes/{self.id}")


class _Sandboxes:
    def __init__(self, http: _Http):
        self._http = http

    def create(self, **options) -> Sandbox:
        # `create()` with no args yields the cheapest, fastest default path.
        body = {k: v for k, v in options.items() if v is not None}
        data = self._http.request("POST", "/v1/sandboxes", body)
        return Sandbox(self._http, data)

    def get(self, sandbox_id: str) -> Sandbox:
        return Sandbox(self._http, self._http.request("GET", f"/v1/sandboxes/{sandbox_id}"))

    def list(self) -> list[Sandbox]:
        data = self._http.request("GET", "/v1/sandboxes")
        return [Sandbox(self._http, s) for s in data.get("sandboxes", [])]


class _Images:
    def __init__(self, http: _Http):
        self._http = http

    def create(
        self,
        name: str,
        source: dict,
        resources_hint: Optional[dict] = None,
        ephemeral: bool = False,
        ttl_seconds: Optional[int] = None,
    ) -> dict:
        body = {"name": name, "source": source}
        if resources_hint:
            body["resources_hint"] = resources_hint
        if ephemeral:
            body["ephemeral"] = True
        if ttl_seconds is not None:
            body["ttl_seconds"] = ttl_seconds
        return self._http.request("POST", "/v1/images", body)

    def get(self, image_id: str) -> dict:
        return self._http.request("GET", f"/v1/images/{image_id}")

    def list(self) -> dict:
        return self._http.request("GET", "/v1/images")

    def delete(self, image_id: str) -> dict:
        return self._http.request("DELETE", f"/v1/images/{image_id}")


class _Templates:
    def __init__(self, http: _Http):
        self._http = http

    def create(self, name: str, create: Optional[dict] = None,
               description: Optional[str] = None) -> SandboxTemplate:
        r = self._http.request("POST", "/v1/templates", {
            "name": name,
            "description": description,
            "create": create or {},
        })
        return _template(r)

    def get(self, name: str) -> SandboxTemplate:
        q = urllib.parse.quote(name, safe="")
        return _template(self._http.request("GET", f"/v1/templates/{q}"))

    def list(self) -> list[SandboxTemplate]:
        data = self._http.request("GET", "/v1/templates")
        return [_template(t) for t in data.get("templates", [])]

    def update(self, name: str, create: Optional[dict] = None,
               description: Optional[str] = None) -> SandboxTemplate:
        q = urllib.parse.quote(name, safe="")
        r = self._http.request("PUT", f"/v1/templates/{q}", {
            "description": description,
            "create": create or {},
        })
        return _template(r)

    def delete(self, name: str) -> dict:
        q = urllib.parse.quote(name, safe="")
        return self._http.request("DELETE", f"/v1/templates/{q}")

    def spawn(self, name: str, count: Optional[int] = None,
              overrides: Optional[dict] = None) -> list[Sandbox]:
        q = urllib.parse.quote(name, safe="")
        body = {}
        if count is not None:
            body["count"] = count
        if overrides is not None:
            body["overrides"] = overrides
        data = self._http.request("POST", f"/v1/templates/{q}/sandboxes", body)
        return [Sandbox(self._http, s) for s in data.get("sandboxes", [])]


class _AgentRuns:
    def __init__(self, http: _Http):
        self._http = http

    def create(self, **request) -> AgentRun:
        return _agent_run(self._http.request("POST", "/v1/agent-runs", request))

    def get(self, run_id: str) -> AgentRun:
        return _agent_run(self._http.request("GET", f"/v1/agent-runs/{run_id}"))

    def list(
        self,
        parent_run_id: Optional[str] = None,
        label: Optional[str] = None,
        state: Optional[str] = None,
    ) -> list[AgentRun]:
        query = {
            k: v
            for k, v in {
                "parent_run_id": parent_run_id,
                "label": label,
                "state": state,
            }.items()
            if v is not None
        }
        suffix = f"?{urllib.parse.urlencode(query)}" if query else ""
        data = self._http.request("GET", f"/v1/agent-runs{suffix}")
        return [_agent_run(r) for r in data.get("agent_runs", [])]

    def logs(self, run_id: str) -> AgentRunLogs:
        r = self._http.request("GET", f"/v1/agent-runs/{run_id}/logs")
        return AgentRunLogs(
            id=r["id"],
            state=r["state"],
            stdout=r["stdout"],
            stderr=r["stderr"],
            diff=r["diff"],
            truncated=bool(r.get("truncated", False)),
        )

    def report(self, run_id: str) -> AgentRunReport:
        return _agent_run_report(self._http.request("GET", f"/v1/agent-runs/{run_id}/report"))

    def children(self, run_id: str) -> list[AgentRun]:
        data = self._http.request("GET", f"/v1/agent-runs/{run_id}/children")
        return [_agent_run(r) for r in data.get("agent_runs", [])]

    def cancel(self, run_id: str) -> AgentRun:
        return _agent_run(self._http.request("POST", f"/v1/agent-runs/{run_id}/cancel"))

    def wait(
        self,
        run_id: str,
        interval_seconds: float = 2.0,
        timeout_seconds: float = 30 * 60,
    ) -> AgentRun:
        deadline = time.time() + timeout_seconds
        while True:
            run = self.get(run_id)
            if run.state not in {"queued", "running"}:
                return run
            if time.time() >= deadline:
                raise TimeoutError(f"timed out waiting for agent run {run_id}")
            time.sleep(interval_seconds)


def _template(r: dict) -> SandboxTemplate:
    return SandboxTemplate(
        id=r["id"],
        name=r["name"],
        description=r.get("description"),
        create=r.get("create", {}),
        created_at=r["created_at"],
        updated_at=r["updated_at"],
    )


def _agent_run(r: dict) -> AgentRun:
    return AgentRun(
        id=r["id"],
        state=r["state"],
        created_at=r.get("created_at", ""),
        updated_at=r.get("updated_at", ""),
        sandbox_id=r.get("sandbox_id"),
        template=r.get("template"),
        repo=r.get("repo"),
        prompt=r.get("prompt"),
        model=r.get("model"),
        agent=r.get("agent"),
        api_key_secret=r.get("api_key_secret"),
        hardness=r.get("hardness"),
        loop=r.get("loop"),
        github=r.get("github"),
        task=r.get("task"),
        mode=r.get("mode"),
        constraints=r.get("constraints"),
        context=r.get("context"),
        verify=r.get("verify"),
        verification_result=r.get("verification_result"),
        verification_results=r.get("verification_results"),
        artifacts=r.get("artifacts"),
        constraint_result=r.get("constraint_result"),
        report=_agent_run_report(r["report"]) if isinstance(r.get("report"), dict) else None,
        branch=r.get("branch"),
        commit=r.get("commit"),
        pr_url=r.get("pr_url"),
        error=r.get("error"),
        logs_truncated=bool(r.get("logs_truncated", False)),
        finished_at=r.get("finished_at"),
        status_url=r.get("status_url"),
        logs_url=r.get("logs_url"),
        report_url=r.get("report_url"),
        children_url=r.get("children_url"),
    )


def _agent_run_report(r: dict) -> AgentRunReport:
    diff_stats = r.get("diff_stats") or {}
    if not diff_stats and isinstance(r.get("changed_files"), int):
        diff_stats = {"files_changed": r.get("changed_files")}
    changed_files = r.get("changed_files") if isinstance(r.get("changed_files"), list) else []
    artifacts = r.get("artifacts") if isinstance(r.get("artifacts"), list) else []
    return AgentRunReport(
        run_id=r.get("run_id", ""),
        outcome=r.get("outcome", ""),
        summary=r.get("summary", ""),
        task=r.get("task") or {},
        mode=r.get("mode", ""),
        agent=r.get("agent", ""),
        model=r.get("model", ""),
        template=r.get("template"),
        hardness=r.get("hardness", ""),
        sandbox_id=r.get("sandbox_id"),
        branch=r.get("branch"),
        commit=r.get("commit"),
        pr_url=r.get("pr_url"),
        diff_stats=diff_stats,
        changed_files=changed_files,
        constraints=r.get("constraints") or {},
        verification=r.get("verification") or [],
        artifacts=artifacts,
        error=r.get("error"),
        stdout_tail=r.get("stdout_tail", ""),
        stderr_tail=r.get("stderr_tail", ""),
        logs_truncated=bool(r.get("logs_truncated", False)),
        duration_ms=r.get("duration_ms"),
        generated_at=r.get("generated_at", ""),
    )


class _Volumes:
    def __init__(self, http: _Http):
        self._http = http

    def create(self, name: str, size_gb: int) -> dict:
        return self._http.request("POST", "/v1/volumes", {"name": name, "size_gb": size_gb})

    def get(self, volume_id: str) -> dict:
        return self._http.request("GET", f"/v1/volumes/{volume_id}")

    def list(self) -> list[dict]:
        return self._http.request("GET", "/v1/volumes").get("volumes", [])

    def delete(self, volume_id: str) -> dict:
        return self._http.request("DELETE", f"/v1/volumes/{volume_id}")


class _Nodes:
    def __init__(self, http: _Http):
        self._http = http

    def list(self) -> dict:
        return self._http.request("GET", "/v1/nodes")

    def join_token(self) -> dict:
        return self._http.request("POST", "/v1/nodes/join-token")

    def drain(self, node_id: str) -> dict:
        return self._http.request("POST", f"/v1/nodes/{node_id}/drain")


class _Secrets:
    """Org-scoped secrets. Values are encrypted at rest and never returned."""

    def __init__(self, http: _Http):
        self._http = http

    def set(self, name: str, value: str) -> dict:
        return self._http.request("PUT", f"/v1/secrets/{name}", {"value": value})

    def list(self) -> list[dict]:
        return self._http.request("GET", "/v1/secrets").get("secrets", [])

    def delete(self, name: str) -> dict:
        return self._http.request("DELETE", f"/v1/secrets/{name}")


class Client:
    def __init__(self, base_url: str, api_key: str, timeout: float = 60.0):
        self._http = _Http(base_url, api_key, timeout)
        self.sandboxes = _Sandboxes(self._http)
        self.images = _Images(self._http)
        self.templates = _Templates(self._http)
        self.agent_runs = _AgentRuns(self._http)
        self.volumes = _Volumes(self._http)
        self.nodes = _Nodes(self._http)
        self.secrets = _Secrets(self._http)

    def usage(self) -> dict:
        return self._http.request("GET", "/v1/usage")


if __name__ == "__main__":
    import os
    client = Client(os.environ.get("WORKDIR_URL", "http://127.0.0.1:8080"),
                    os.environ["WORKDIR_API_KEY"])
    sb = client.sandboxes.create()
    print("created", sb.id, "boot_path", sb.boot_path, "boot_ms", sb.timings.get("boot_ms"))
    print("echo:", sb.exec("echo ok").stdout.strip())
    sb.delete()
    print("deleted")
