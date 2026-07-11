# @mv37/workdir

TypeScript SDK for [workdir](https://workdir.dev).

```bash
npm install @mv37/workdir
```

```ts
import { Client } from "@mv37/workdir";

const workdir = new Client("https://api.workdir.dev", process.env.WORKDIR_API_KEY!);

const box = await workdir.sandboxes.create();
const { stdout } = await box.exec("echo hello");
console.log(stdout);

const job = await box.exec("npm test", { background: true });
let status = await box.execStatus(job.cmd_id);
while (status.state === "running") {
  await new Promise((resolve) => setTimeout(resolve, 1000));
  status = await box.execStatus(job.cmd_id);
}
console.log(await box.execLogs(job.cmd_id));
await box.delete();
```

```ts
await workdir.templates.create({
  name: "node-app",
  create: {
    image: "node-python",
    resources: { cpu: 2, memory_mb: 4096, disk_gb: 16 },
    startup: { git: { url: "https://github.com/acme/app.git", ref: "main" } },
  },
});
const boxes = await workdir.templates.spawn("node-app", { count: 2 });

const run = await workdir.agentRuns.create({
  template: "node-app",
  repo: { url: "https://github.com/acme/app.git", ref: "main" },
  agent: "codex",
  model: "gpt-5",
  api_key_secret: "OPENAI_API_KEY",
  prompt: "Fix the failing tests and leave a clean diff.",
  task: { name: "Fix failing tests", labels: ["delegated"] },
  verify: [{ name: "tests", run: "pnpm test", fail_run: true }],
  github: { token_secret: "GITHUB_TOKEN", draft: true },
});
const finished = await workdir.agentRuns.wait(run.id);
console.log((await workdir.agentRuns.report(finished.id)).summary);
```

The SDK uses the global `fetch` API and supports Node.js 18+, Deno, Bun, and browsers.
