// 离线验证 oh-my-pi 扩展模板 `crates/mt-ai/assets/miniterm-omp.ts`:
// 伪造 omp 的事件总线与 ctx,起一个本地 HTTP 收集器,逐事件断言 POST 出去的 body 形状。
// omp 本身不必安装;跑在与 omp 相同的 Bun 运行时上,模块导入 / 工厂契约 / fetch 行为一并覆盖。
//
//   bun run tools/omp-ext-check.ts [path-to-miniterm-omp.ts]
//
// 改模板后跑一遍;Rust 侧另有单测钉住 `pi.on(...)` 事件集与上报事件名的对账(hook_registry.rs)。
import path from "node:path";

const EXT = process.argv[2] ?? path.join(import.meta.dir, "..", "crates", "mt-ai", "assets", "miniterm-omp.ts");

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

type Handler = (event: unknown, ctx: unknown) => Promise<void> | void;
const handlers = new Map<string, Handler[]>();
const pi = {
	on(ev: string, h: Handler) {
		const list = handlers.get(ev) ?? [];
		list.push(h);
		handlers.set(ev, list);
	},
};
const emit = async (ev: string, event: unknown, ctx: unknown) => {
	for (const h of handlers.get(ev) ?? []) await h(event, ctx);
};
const tui = (sid: string) => ({
	mode: "tui",
	hasUI: true,
	cwd: "D:/proj",
	sessionManager: { getSessionId: () => sid },
});
const sub = (sid: string) => ({
	mode: "print",
	hasUI: false,
	cwd: "D:/proj",
	sessionManager: { getSessionId: () => sid },
});
const legacyTui = (sid: string) => ({ hasUI: true, cwd: "D:/proj", sessionManager: { getSessionId: () => sid } });

// ── 1. 没有 MINITERM_PTY_ID:整个扩展是空操作 ──
delete process.env.MINITERM_PTY_ID;
process.env.MINITERM_HOOK_PORT = String(server.port);
const mod = await import(path.resolve(EXT));
console.log("factory export:", typeof mod.default);
mod.default(pi);
console.log("subscribed:", [...handlers.keys()].sort().join(","));

await emit("session_start", { type: "session_start" }, tui("s1"));
await emit("agent_start", { type: "agent_start" }, tui("s1"));
check(takeAll().length === 0, "no MINITERM_PTY_ID → nothing posted");

// ── 2. 正常主会话 ──
process.env.MINITERM_PTY_ID = "7";
handlers.clear();
mod.default(pi);

await emit("session_start", { type: "session_start" }, tui("s1"));
let got = takeAll();
check(got.length === 1 && got[0].event === "SessionStart" && got[0].session_id === "s1", "session_start → SessionStart(s1)");
check(got[0]?.pty_id === 7 && got[0]?.agent === "omp" && got[0]?.cwd === "D:/proj", "payload carries pty_id/agent/cwd");
check(got[0]?.reason === "startup", "SessionStart reason=startup");

await emit("session_start", { type: "session_start" }, tui("s1"));
check(takeAll().length === 0, "repeated session_start with same id → no duplicate");

await emit("agent_start", { type: "agent_start" }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "UserPromptSubmit", "agent_start → UserPromptSubmit");

await emit("tool_call", { type: "tool_call", toolName: "bash", toolCallId: "t1", input: {} }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "PreToolUse" && got[0].tool_name === "bash", "tool_call bash → PreToolUse");

await emit("tool_result", { type: "tool_result", toolName: "bash", isError: true, content: [] }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "PostToolUseFailure", "tool_result isError → PostToolUseFailure");

await emit("tool_result", { type: "tool_result", toolName: "bash", isError: false, content: [] }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "PostToolUse", "tool_result ok → PostToolUse");

await emit("tool_call", { type: "tool_call", toolName: "ask", toolCallId: "t2", input: {} }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "Elicitation", "tool_call ask → Elicitation");
await emit("tool_result", { type: "tool_result", toolName: "ask", isError: false, content: [] }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "ElicitationResult", "tool_result ask → ElicitationResult");

await emit("tool_approval_requested", { type: "tool_approval_requested", toolName: "bash", sessionId: "s1", toolCallId: "t3", approvalMode: "ask" }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "PermissionRequest" && got[0].tool_name === "bash", "approval requested → PermissionRequest");
await emit("tool_approval_resolved", { type: "tool_approval_resolved", toolName: "bash", approved: true, sessionId: "s1", toolCallId: "t3" }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "PreToolUse", "approval approved → PreToolUse");
await emit("tool_approval_resolved", { type: "tool_approval_resolved", toolName: "bash", approved: false, sessionId: "s1", toolCallId: "t3" }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "PermissionDenied", "approval denied → PermissionDenied");

await emit("auto_retry_start", { type: "auto_retry_start", attempt: 2, maxAttempts: 5, delayMs: 100, errorMessage: "529 overloaded" }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "Notification" && String(got[0].message).includes("retrying"), "auto_retry_start → retrying Notification");

await emit("auto_compaction_start", { type: "auto_compaction_start" }, tui("s1"));
await emit("auto_compaction_end", { type: "auto_compaction_end" }, tui("s1"));
got = takeAll();
check(got.length === 2 && got[0].event === "PreCompact" && got[1].event === "PostCompact", "compaction → PreCompact/PostCompact");

await emit("agent_end", { type: "agent_end", willContinue: true, messages: [] }, tui("s1"));
check(takeAll().length === 0, "agent_end willContinue → nothing");

await emit("agent_end", { type: "agent_end", messages: [{ role: "user" }, { role: "assistant", stopReason: "aborted" }, { role: "toolResult" }] }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "Stop" && got[0].reason === "aborted", "agent_end aborted → Stop reason=aborted");

await emit("agent_end", { type: "agent_end", messages: [{ role: "assistant", stopReason: "error", errorMessage: "boom" }] }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "StopFailure" && got[0].error_type === "boom", "agent_end error → StopFailure error_type");

await emit("agent_end", { type: "agent_end", messages: [{ role: "assistant", stopReason: "stop" }] }, tui("s1"));
got = takeAll();
check(got.length === 1 && got[0].event === "Stop" && got[0].reason === undefined, "agent_end normal → Stop");

// ── 3. 子代理(同进程、print 模式)全部静默 ──
for (const ev of ["session_start", "agent_start", "tool_call", "agent_end", "session_shutdown"]) {
	await emit(ev, { type: ev, toolName: "bash", messages: [{ role: "assistant", stopReason: "stop" }] }, sub("child-1"));
}
check(takeAll().length === 0, "subagent (mode=print) events → nothing");

// ── 4. 换会话:旧会话 clear 收尾 + 新会话 SessionStart ──
await emit("session_switch", { type: "session_switch", reason: "new", previousSessionFile: "x" }, tui("s2"));
got = takeAll();
check(
	got.length === 2 && got[0].event === "SessionEnd" && got[0].session_id === "s1" && got[0].reason === "clear" && got[1].event === "SessionStart" && got[1].session_id === "s2" && got[1].reason === "new",
	"session_switch → SessionEnd(clear,s1) + SessionStart(s2)",
);
await emit("agent_start", { type: "agent_start" }, tui("s2"));
got = takeAll();
check(got.length === 1 && got[0].session_id === "s2", "events after switch carry the new session id");

await emit("session_branch", { type: "session_branch" }, tui("s3"));
got = takeAll();
check(got.length === 2 && got[0].event === "SessionEnd" && got[1].event === "SessionStart" && got[1].reason === "fork", "session_branch → clear + SessionStart(fork)");

// ── 5. 退出 ──
await emit("session_shutdown", { type: "session_shutdown" }, tui("s3"));
got = takeAll();
check(got.length === 1 && got[0].event === "SessionEnd" && got[0].session_id === "s3" && got[0].reason === "exit", "session_shutdown → SessionEnd(exit)");

// ── 6. 旧版 ctx(没有 mode)按 hasUI 判 ──
handlers.clear();
mod.default(pi);
await emit("session_start", { type: "session_start" }, legacyTui("s9"));
got = takeAll();
check(got.length === 1 && got[0].event === "SessionStart", "legacy ctx without mode but hasUI → reports");
await emit("agent_start", { type: "agent_start" }, { hasUI: false, sessionManager: { getSessionId: () => "s9" } });
check(takeAll().length === 0, "legacy ctx hasUI=false → silent");

// ── 7. 服务器不在:不能抛 ──
process.env.MINITERM_HOOK_PORT = "1";
let threw = false;
try {
	await emit("agent_start", { type: "agent_start" }, legacyTui("s9"));
} catch {
	threw = true;
}
check(!threw, "dead port → swallowed, no throw");

server.stop(true);
console.log(failures === 0 ? "\nALL PASSED" : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
