// 离线验证 pi 扩展模板 `crates/mt-ai/assets/miniterm-pi.ts`:
// 伪造 pi 的事件总线与 ctx,起一个本地 HTTP 收集器,逐事件断言 POST 出去的 body 形状。
// pi 本身不必安装;用 Bun 跑(pi 自己是 Node + jiti 加载,模板只用 node:fs/os/path 与全局 fetch,两边一致)。
//
//   bun run tools/pi-ext-check.ts [path-to-miniterm-pi.ts]
//
// 改模板后跑一遍;Rust 侧另有单测钉住 `pi.on(...)` 事件集与上报事件名的对账(hook_registry.rs)。
// 事件形状按 pi 1.4.0 的 `dist/core/extensions/types.d.ts`。
import path from "node:path";

const EXT = process.argv[2] ?? path.join(import.meta.dir, "..", "crates", "mt-ai", "assets", "miniterm-pi.ts");

type Body = Record<string, unknown>;
const received: Body[] = [];
const server = Bun.serve({
	port: 0,
	hostname: "127.0.0.1",
	async fetch(req) {
		if (req.method === "POST" && new URL(req.url).pathname === "/hook") {
			received.push((await req.json()) as Body);
			return new Response("OK");
		}
		return new Response("nf", { status: 404 });
	},
});

let failures = 0;
function check(cond: boolean, msg: string) {
	if (cond) console.log("  ok   " + msg);
	else {
		failures++;
		console.log("  FAIL " + msg);
	}
}
function takeAll(): Body[] {
	return received.splice(0, received.length);
}
const events = (got: Body[]) => got.map((b) => b.event).join(",");

type Handler = (event: unknown, ctx: unknown) => Promise<void> | void;
const handlers = new Map<string, Handler[]>();
const pi = {
	on(ev: string, h: Handler) {
		const list = handlers.get(ev) ?? [];
		list.push(h);
		handlers.set(ev, list);
		return () => {};
	},
};
const emit = async (ev: string, event: Record<string, unknown>, ctx: unknown) => {
	for (const h of handlers.get(ev) ?? []) await h({ type: ev, ...event }, ctx);
};
/** pi 每次 /new /resume /fork /reload 都换一个扩展运行时:旧工厂的状态全丢,新工厂重新绑定 */
const freshRuntime = (mod: { default: (api: typeof pi) => void }) => {
	handlers.clear();
	mod.default(pi);
};
const ctxOf = (mode: string | undefined, sid: string) => ({
	...(mode === undefined ? {} : { mode }),
	hasUI: mode === undefined || mode === "tui" || mode === "rpc",
	cwd: "D:/proj",
	sessionManager: { getSessionId: () => sid },
});
const tui = (sid: string) => ctxOf("tui", sid);

// ── 1. 没有 MINITERM_PTY_ID:整个扩展是空操作 ──
delete process.env.MINITERM_PTY_ID;
process.env.MINITERM_HOOK_PORT = String(server.port);
const mod = await import(path.resolve(EXT));
console.log("factory export:", typeof mod.default);
freshRuntime(mod);
console.log("subscribed:", [...handlers.keys()].sort().join(","));
await emit("session_start", { reason: "startup" }, tui("s1"));
await emit("agent_start", {}, tui("s1"));
check(takeAll().length === 0, "no MINITERM_PTY_ID → nothing posted");

// ── 2. 正常主会话 ──
process.env.MINITERM_PTY_ID = "7";
freshRuntime(mod);

await emit("session_start", { reason: "startup" }, tui("s1"));
let got = takeAll();
check(got.length === 1 && got[0].event === "SessionStart" && got[0].session_id === "s1", "session_start → SessionStart(s1)");
check(got[0]?.pty_id === 7 && got[0]?.agent === "pi" && got[0]?.cwd === "D:/proj", "payload carries pty_id/agent=pi/cwd");
check(got[0]?.reason === "startup", "SessionStart reason=startup");

// 一个回合:API 出错 → 自动重试 → 工具 → 完成。agent_end 出现两次,只有 agent_settled 才算完成
await emit("agent_start", {}, tui("s1"));
check(events(takeAll()) === "UserPromptSubmit", "agent_start → UserPromptSubmit");
await emit("agent_end", { messages: [{ role: "user" }, { role: "assistant", stopReason: "error", errorMessage: "Connection error." }] }, tui("s1"));
check(takeAll().length === 0, "agent_end (retry pending) → nothing, not a completion");
await emit("agent_start", {}, tui("s1"));
check(events(takeAll()) === "UserPromptSubmit", "retry run agent_start → UserPromptSubmit again (still working)");

await emit("tool_call", { toolName: "bash", toolCallId: "t1", input: { command: "ls" } }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "PreToolUse" && got[0].tool_name === "bash", "tool_call bash → PreToolUse");
await emit("tool_result", { toolName: "bash", toolCallId: "t1", isError: true, content: [] }, tui("s1"));
check(events(takeAll()) === "PostToolUseFailure", "tool_result isError → PostToolUseFailure");
await emit("tool_result", { toolName: "bash", toolCallId: "t1", isError: false, content: [] }, tui("s1"));
check(events(takeAll()) === "PostToolUse", "tool_result ok → PostToolUse");

// 回合里扩展弹对话框(权限确认 / 提问)→ 黄灯,关掉后熄灭
await emit("ui_prompt_start", { reason: "ui_prompt", kind: "confirm", title: "Allow tool call?" }, tui("s1"));
check(events(takeAll()) === "Elicitation", "ui_prompt_start during a turn → Elicitation");
await emit("ui_prompt_end", { reason: "ui_prompt", kind: "confirm", title: "Allow tool call?" }, tui("s1"));
check(events(takeAll()) === "ElicitationResult", "ui_prompt_end during a turn → ElicitationResult");

await emit("agent_end", { messages: [{ role: "assistant", stopReason: "stop" }] }, tui("s1"));
check(takeAll().length === 0, "agent_end before settle → nothing");
await emit("agent_settled", { aborted: false }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "Stop" && got[0].reason === undefined, "agent_settled → Stop (completion)");

// 空闲时斜杠命令弹的对话框:不报
await emit("ui_prompt_start", { reason: "ui_prompt", kind: "select" }, tui("s1"));
await emit("ui_prompt_end", { reason: "ui_prompt", kind: "select" }, tui("s1"));
check(takeAll().length === 0, "ui_prompt while idle → nothing");

// 打断:aborted 位
await emit("agent_start", {}, tui("s1"));
takeAll();
await emit("agent_end", { messages: [{ role: "assistant", stopReason: "aborted" }] }, tui("s1"));
await emit("agent_settled", { aborted: true }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "Stop" && got[0].reason === "aborted", "agent_settled aborted → Stop reason=aborted");

// 打断:更早的 pi 没有 aborted 位,看最后一次运行的 stopReason
await emit("agent_start", {}, tui("s1"));
takeAll();
await emit("agent_end", { messages: [{ role: "assistant", stopReason: "aborted" }] }, tui("s1"));
await emit("agent_settled", {}, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].reason === "aborted", "agent_settled without aborted flag → falls back to stopReason");

// 重试用尽:最后一次运行以 error 收场
await emit("agent_start", {}, tui("s1"));
takeAll();
await emit("agent_end", { messages: [{ role: "assistant", stopReason: "error", errorMessage: "529 overloaded" }] }, tui("s1"));
await emit("agent_settled", { aborted: false }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "StopFailure" && got[0].error_type === "529 overloaded", "settled after error → StopFailure error_type");

// 上一轮的 error 不能漏到下一轮
await emit("agent_start", {}, tui("s1"));
takeAll();
await emit("agent_settled", { aborted: false }, tui("s1"));
check(events(takeAll()) === "Stop", "stale error from previous turn is reset");

// ── 3. 非交互模式(扩展拉起的子 pi:print / json / rpc)全部静默 ──
for (const mode of ["print", "json", "rpc"]) {
	freshRuntime(mod);
	for (const ev of ["session_start", "agent_start", "tool_call", "agent_end", "agent_settled", "ui_prompt_start", "session_shutdown"]) {
		await emit(ev, { reason: "quit", toolName: "bash", aborted: false, messages: [{ role: "assistant", stopReason: "stop" }] }, ctxOf(mode, "child-1"));
	}
	check(takeAll().length === 0, `mode=${mode} → nothing`);
}

// ── 4. 换会话:旧运行时 shutdown(new) 打 clear 墓碑,新运行时 SessionStart ──
freshRuntime(mod);
await emit("session_start", { reason: "startup" }, tui("s1"));
takeAll();
await emit("session_shutdown", { reason: "new" }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "SessionEnd" && got[0].session_id === "s1" && got[0].reason === "clear", "session_shutdown(new) → SessionEnd(clear,s1)");
freshRuntime(mod);
await emit("session_start", { reason: "new", previousSessionFile: "x.jsonl" }, tui("s2"));
got = takeAll();
check(got.length === 1 && got[0].event === "SessionStart" && got[0].session_id === "s2" && got[0].reason === "new", "new runtime session_start(new) → SessionStart(s2)");
await emit("agent_start", {}, tui("s2"));
check(takeAll()[0]?.session_id === "s2", "events after switch carry the new session id");
await emit("agent_settled", { aborted: false }, tui("s2"));
takeAll();

for (const reason of ["resume", "fork"]) {
	await emit("session_shutdown", { reason, targetSessionFile: "y.jsonl" }, tui("s2"));
	got = takeAll();
	check(got.length === 1 && got[0].reason === "clear", `session_shutdown(${reason}) → SessionEnd(clear)`);
	freshRuntime(mod);
	await emit("session_start", { reason }, tui("s2"));
	takeAll();
}

// ── 5. /reload:同一会话,shutdown 不报,新运行时照常 SessionStart ──
await emit("session_shutdown", { reason: "reload" }, tui("s2"));
check(takeAll().length === 0, "session_shutdown(reload) → nothing");
freshRuntime(mod);
await emit("session_start", { reason: "reload" }, tui("s2"));
got = takeAll();
check(got.length === 1 && got[0].event === "SessionStart" && got[0].reason === "reload", "session_start(reload) → SessionStart(reload)");

// ── 6. 退出:quit 与不带 reason 的老版本都是 exit ──
await emit("session_shutdown", { reason: "quit" }, tui("s2"));
got = takeAll();
check(got.length === 1 && got[0].event === "SessionEnd" && got[0].session_id === "s2" && got[0].reason === "exit", "session_shutdown(quit) → SessionEnd(exit)");
freshRuntime(mod);
await emit("session_shutdown", {}, tui("s3"));
check(takeAll()[0]?.reason === "exit", "session_shutdown without reason (old pi) → SessionEnd(exit)");

// ── 7. 更早的 pi(< 0.78.1)ctx 没有 mode:按 hasUI 判 ──
freshRuntime(mod);
await emit("session_start", { reason: "startup" }, ctxOf(undefined, "s9"));
check(events(takeAll()) === "SessionStart", "legacy ctx without mode but hasUI → reports");
await emit("agent_start", {}, { hasUI: false, sessionManager: { getSessionId: () => "s9" } });
check(takeAll().length === 0, "legacy ctx hasUI=false → silent");

// ── 8. 服务器不在:不能抛(tool_call 是闸门事件,抛了会拦下工具) ──
process.env.MINITERM_HOOK_PORT = "1";
let threw = false;
try {
	await emit("tool_call", { toolName: "bash", toolCallId: "t9", input: {} }, tui("s9"));
	await emit("agent_settled", { aborted: false }, tui("s9"));
} catch {
	threw = true;
}
check(!threw, "dead port → swallowed, no throw");

server.stop(true);
console.log(failures === 0 ? "\nALL PASSED" : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
