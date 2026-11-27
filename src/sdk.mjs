import { createInterface } from "node:readline";
import { join } from "node:path";

const write = process.stdout.write.bind(process.stdout);
process.stdout.write = process.stderr.write.bind(process.stderr);
const emit = (frame) => write(`${JSON.stringify(frame)}\n`);
const key = process.env.CURSOR_API_KEY;
const fail = (error) => {
  let message = typeof error?.message === "string" ? error.message : "unknown error";
  if (key) message = message.replaceAll(key, "[redacted]");
  emit({ type: "error", message: message.slice(0, 1500) });
};

let sdk;
try {
  const [major, minor] = process.versions.node.split(".").map(Number);
  if (major < 22 || (major === 22 && minor < 13)) throw new Error("Node 22.13 or later is required");
  sdk = await import("@cursor/sdk");
  if (!sdk.Agent?.create || !sdk.Cursor?.models?.list || !sdk.JsonlLocalAgentStore || !sdk.emptyFs) {
    throw new Error("incompatible @cursor/sdk package");
  }
  emit({ type: "ready" });
} catch (error) {
  fail(error);
  process.exit(1);
}

const { Agent, Cursor, JsonlLocalAgentStore, emptyFs } = sdk;
const listModels = () => Cursor.models.list({ apiKey: key });

async function selectModel(id, overrides, effort) {
  id = !id || id === "auto" ? "default" : id;
  const catalog = await listModels().catch(() => []);
  const entry = catalog.find((m) => m.id === id || m.aliases?.includes(id));
  const names = (entry?.parameters ?? []).map((p) => p.id);
  const effortId = ["effort", "reasoning", "reasoning_effort", "thought_level"].find((id) => names.includes(id));
  const namesForGray = Object.assign(Object.create(null), {
    contextWindow: "context",
    fastMode: "fast",
    effort: effortId ?? "effort",
  });
  const params = new Map(overrides.map(([name, value]) => [namesForGray[name] ?? name, value]));
  if (effort && !params.has(effortId) && (effortId || catalog.length === 0)) {
    params.set(effortId ?? "effort", effort);
  }
  const variants = entry?.variants ?? [];
  const defaultVariant = variants.find((variant) => variant.isDefault);
  for (const id of names) {
    if (params.has(id)) continue;
    const parameter = entry.parameters.find((p) => p.id === id);
    const value = id === "fast" && parameter.values.some((v) => v.value === "false")
      ? "false" : defaultVariant?.params.find((p) => p.id === id)?.value;
    if (value !== undefined) params.set(id, value);
  }
  const selection = [...params].map(([id, value]) => ({ id, value }));
  const matching = variants.filter((variant) => selection.every((p) =>
    variant.params.some((value) => value.id === p.id && value.value === p.value)));
  return {
    id: entry?.id ?? id,
    ...(selection.length ? { params: selection } : {}),
    ...(selection.length && matching.length && matching.every((v) => v.isMaxMode) ? { maxMode: true } : {}),
  };
}

let agent;
let run;
let running = false;
let ending = false;
let cancelTask;
const pending = new Map();
let nextCall = 0;

function cancelRun() {
  if (!run) return Promise.resolve();
  cancelTask ??= run.cancel().catch(() => {});
  return cancelTask;
}

async function close() {
  if (ending) return;
  ending = true;
  for (const resolve of pending.values()) {
    resolve({ content: [{ type: "text", text: "Turn interrupted" }], isError: true });
  }
  pending.clear();
  if (run) await cancelRun();
  if (!running || run) {
    agent?.close();
    emit({ type: "closed" });
    process.exit(0);
  }
}

async function turn(frame) {
  if (running) throw new Error("run already active");
  running = true;
  let sentText = false;
  try {
    if (!agent) {
      const { cwd, model, overrides, effort, mode } = frame;
      const selection = await selectModel(model, overrides, effort);
      if (ending) return;
      agent = await Agent.create({
        apiKey: key,
        model: selection,
        mode: mode === "plan" ? "plan" : "agent",
        tools: ["mcp"],
        local: {
          cwd,
          settingSources: [],
          fs: emptyFs(),
          store: new JsonlLocalAgentStore(join(cwd, "state")),
        },
      });
    }
    if (ending) return;
    const customTools = Object.fromEntries(frame.tools.map(({ name, description, inputSchema }) => [
      name,
      { description, inputSchema, execute: (args) => {
        if (ending) return { content: [{ type: "text", text: "Turn interrupted" }], isError: true };
        const id = `sdk-${++nextCall}`;
        return new Promise((resolve) => {
          pending.set(id, resolve);
          emit({ type: "call", id, name, args });
        });
      } },
    ]));
    run = await agent.send(frame.message, {
      local: { customTools },
      onDelta: ({ update }) => {
        if (ending || !update.text) return;
        if (update.type === "text-delta") {
          sentText = true;
          emit({ type: "text", text: update.text });
        } else if (update.type === "thinking-delta") {
          emit({ type: "reasoning", text: update.text });
        }
      },
    });
    if (ending) {
      await cancelRun();
      return;
    }
    const result = await run.wait();
    if (ending) return;
    if (result.status !== "finished") throw result.error ?? new Error(`run ${result.status}`);
    if (!sentText && result.result) emit({ type: "text", text: result.result });
    emit({ type: "done", usage: result.usage });
  } finally {
    running = false;
    if (ending) {
      agent?.close();
      emit({ type: "closed" });
      process.exit(0);
    }
    run = undefined;
    cancelTask = undefined;
  }
}

const lines = createInterface({ input: process.stdin });
lines.on("line", (line) => {
  Promise.resolve().then(async () => {
    const frame = JSON.parse(line);
    switch (frame.type) {
      case "catalog": emit({ type: "catalog", models: await listModels() }); break;
      case "turn": await turn(frame); break;
      case "output": {
        const resolve = pending.get(frame.id);
        pending.delete(frame.id);
        resolve?.({ content: [{ type: "text", text: frame.output }] });
        break;
      }
      case "close": await close(); break;
    }
  }).catch(fail);
});
lines.on("close", () => {
  void close();
  setTimeout(() => process.exit(0), 1000).unref();
});
