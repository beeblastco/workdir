# mv37-workdir

Python SDK for [workdir](https://workdir.dev).

```bash
pip install mv37-workdir
```

```python
import time
from workdir import Client

workdir = Client("https://api.workdir.dev", api_key="...")

box = workdir.sandboxes.create()
print(box.exec("echo hello").stdout)

job = box.exec("pytest", background=True)
status = box.exec_status(job.cmd_id)
while status.state == "running":
    time.sleep(1)
    status = box.exec_status(job.cmd_id)
print(box.exec_logs(job.cmd_id))
box.delete()
```

```python
workdir.templates.create(
    "node-app",
    create={
        "image": "node-python",
        "resources": {"cpu": 2, "memory_mb": 4096, "disk_gb": 16},
        "startup": {"git": {"url": "https://github.com/acme/app.git", "ref": "main"}},
    },
)
boxes = workdir.templates.spawn("node-app", count=2)

run = workdir.agent_runs.create(
    template="node-app",
    repo={"url": "https://github.com/acme/app.git", "ref": "main"},
    agent="codex",
    model="gpt-5",
    api_key_secret="OPENAI_API_KEY",
    prompt="Fix the failing tests and leave a clean diff.",
    task={"name": "Fix failing tests", "labels": ["delegated"]},
    verify=[{"name": "tests", "run": "pnpm test", "fail_run": True}],
    github={"token_secret": "GITHUB_TOKEN", "draft": True},
)
finished = workdir.agent_runs.wait(run.id)
print(workdir.agent_runs.report(finished.id).summary)
```

The SDK uses only the Python standard library.
