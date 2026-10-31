// 在**本机装的真 pi** 运行时里验证扩展模板 `crates/mt-ai/assets/miniterm-pi.ts`:
// 用 pi 的 SDK 建会话、pi-ai 自带的 faux 假模型出回复(不联网、不花 token),
// `bindExtensions({ mode: "tui" })` 让扩展以为自己在交互式主会话里,收集它 POST 出来的
// hook 事件逐场景断言。比 `tools/pi-ext-check.ts` 的伪事件总线多验一层「pi 真的按这个
// 顺序发这些事件」——自动重试时 agent_end 一回合来两次、打断 / 报错的收场形态都是真跑出来的。
//
//   node tools/pi-ext-realrun.mjs [miniterm-pi.ts] [pi-coding-agent 包目录]
//
// 包目录缺省取 pi 托管安装的当前版本:`~/.pi/agent/install/releases/<current-version>/
// node_modules/@earendil-works/pi-coding-agent`(npm 全局装的就手动传路径)。
// 全程隔离:PI_CODING_AGENT_DIR 指到临时目录,不读写用户的 ~/.pi。
// 拿 omp 模板跑同一套场景可以看到它为什么不能直接给 pi 用(重试中途误报 StopFailure 等)。
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const extPath = path.resolve(process.argv[2] ?? path.join(here, "..", "crates", "mt-ai", "assets", "miniterm-pi.ts"));
const pkgDir =
	process.argv[3] ??
	(() => {
		const install = path.join(os.homedir(), ".pi", "agent", "install");
		const version = fs.readFileSync(path.join(install, "current-version"), "utf8").trim();
		return path.join(install, "releases", version, "node_modules", "@earendil-works", "pi-coding-agent");
	})();

const workDir = fs.mkdtempSync(path.join(os.tmpdir(), "mini-term-pi-realrun-"));
const agentDir = path.join(workDir, "agent");
const sessionDir = path.join(workDir, "sessions");
fs.mkdirSync(agentDir, { recursive: true });
fs.mkdirSync(sessionDir, { recursive: true });
process.env.PI_CODING_AGENT_DIR = agentDir;
process.env.PI_OFFLINE = "1";
process.env.PI_TELEMETRY = "0";
process.env.PI_SKIP_VERSION_CHECK = "1";

const received = [];
const server = http.createServer((req, res) => {
	let body = "";
	req.on("data", (c) => (body += c));
	req.on("end", () => {
		if (req.method === "POST" && req.url === "/hook") received.push(JSON.parse(body));
		res.end("OK");
	});
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
process.env.MINITERM_PTY_ID = "7";
process.env.MINITERM_HOOK_PORT = String(server.address().port);

const sdk = await import(pathToFileURL(path.join(pkgDir, "dist", "index.js")).href);
const ai = await import(pathToFileURL(path.join(pkgDir, "..", "pi-ai", "dist", "index.js")).href);
const { createAgentSession, DefaultResourceLoader, SessionManager, ModelRuntime } = sdk;
const { fauxProvider, fauxAssistantMessage, fauxText, fauxToolCall } = ai;
console.log(`pi ${JSON.parse(fs.readFileSync(path.join(pkgDir, "package.json"), "utf8")).version} · ${extPath}`);

let failures = 0;
const check = (cond, msg, extra) => {
	console.log(`  ${cond ? "ok  " : "FAIL"} ${msg}${cond || extra === undefined ? "" : "  " + JSON.stringify(extra)}`);
	if (!cond) failures++;
};
const take = async () => {
	await new Promise((r) => setTimeout(r, 150)); // 等最后一条 POST 落进收集器
	return received.splice(0, received.length);
};
const names = (got) => got.map((b) => (b.reason ? `${b.event}(${b.reason})` : b.event)).join(",");

// 内联扩展:一个执行时弹 confirm 的工具(模拟权限确认类扩展),外加 agent_end 计数
let agentEnds = 0;
const helper = (pi) => {
	pi.on("agent_end", () => {
		agentEnds++;
	});
	pi.registerTool({
		name: "needs_ok",
		label: "needs_ok",
		description: "asks the user before doing nothing",
		parameters: { type: "object", properties: {}, additionalProperties: false },
		async execute(_id, _args, _signal, _onUpdate, ctx) {
			const ok = await ctx.ui.confirm("Allow?", "needs_ok wants to run");
			return { content: [{ type: "text", text: ok ? "approved" : "denied" }], details: undefined };
		},
	});
};

const resourceLoader = new DefaultResourceLoader({
	cwd: workDir,
	agentDir,
	additionalExtensionPaths: [extPath],
	extensionFactories: [helper],
});
await resourceLoader.reload();
const extErrors = resourceLoader.getExtensions?.().errors ?? [];
check(extErrors.length === 0, "extension loads in real pi without errors", extErrors);

const faux = fauxProvider({ tokensPerSecond: 2000 });
const modelRuntime = await ModelRuntime.create({
	authPath: path.join(agentDir, "auth.json"),
	modelsPath: null,
	refreshOnCreate: false,
});
modelRuntime.registerNativeProvider(faux.provider);

const { session } = await createAgentSession({
	cwd: workDir,
	agentDir,
	resourceLoader,
	modelRuntime,
	model: faux.getModel(),
	sessionManager: SessionManager.create(workDir, sessionDir),
	noTools: "builtin",
});
const sessionId = session.sessionManager.getSessionId();

// 交互模式的 UI 上下文:confirm 直接同意,其余 no-op
const ui = new Proxy(
	{ confirm: async () => true, select: async () => undefined, input: async () => undefined, notify: () => {} },
	{ get: (t, k) => (k in t ? t[k] : () => undefined) },
);
await session.bindExtensions({ mode: "tui", uiContext: ui });

console.log("A. 启动");
let got = await take();
check(names(got) === "SessionStart(startup)", "bindExtensions → SessionStart(startup)", names(got));
check(got[0]?.agent === "pi" && got[0]?.pty_id === 7 && got[0]?.session_id === sessionId, "payload: agent=pi, pty_id, real session id", got[0]);

console.log("B. 带工具 + 权限对话框的完整回合");
faux.setResponses([
	fauxAssistantMessage([fauxText("checking"), fauxToolCall("needs_ok", {}, { id: "call-1" })], { stopReason: "toolUse" }),
	fauxAssistantMessage("all done"),
]);
await session.prompt("do it");
got = await take();
check(
	names(got) === "UserPromptSubmit,PreToolUse,Elicitation,ElicitationResult,PostToolUse,Stop",
	"prompt → UserPromptSubmit,PreToolUse,Elicitation,ElicitationResult,PostToolUse,Stop",
	names(got),
);
check(got.find((b) => b.event === "PreToolUse")?.tool_name === "needs_ok", "PreToolUse carries tool_name");

console.log("C. API 错误 → 自动重试 → 成功:中途不能报完成");
agentEnds = 0;
faux.setResponses([
	fauxAssistantMessage([], { stopReason: "error", errorMessage: "503 Service Unavailable: overloaded" }),
	fauxAssistantMessage("recovered"),
]);
await session.prompt("again");
got = await take();
check(agentEnds >= 2, `agent_end fired ${agentEnds}x in one turn (retry)`);
check(
	got.filter((b) => b.event === "Stop" || b.event === "StopFailure").length === 1 && got.at(-1)?.event === "Stop" && !got.at(-1)?.reason,
	"exactly one Stop, at the very end",
	names(got),
);

console.log("D. 不可重试的错误 → StopFailure");
faux.setResponses([fauxAssistantMessage([], { stopReason: "error", errorMessage: "400 invalid request: bad schema" })]);
await session.prompt("break");
got = await take();
check(got.at(-1)?.event === "StopFailure" && String(got.at(-1)?.error_type).includes("400"), "turn ending in error → StopFailure(error_type)", names(got));

console.log("E. 用户打断(Esc)→ Stop(aborted)");
faux.setResponses([
	async () => {
		await new Promise((r) => setTimeout(r, 1500));
		return fauxAssistantMessage("too late");
	},
]);
const running = session.prompt("slow");
await new Promise((r) => setTimeout(r, 300));
await session.abort();
await running.catch(() => {});
got = await take();
check(got.at(-1)?.event === "Stop" && got.at(-1)?.reason === "aborted", "abort → Stop(reason=aborted)", names(got));

session.dispose();
server.close();
fs.rmSync(workDir, { recursive: true, force: true });
console.log(failures === 0 ? "\nALL PASSED" : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
