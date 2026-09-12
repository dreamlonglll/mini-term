// miniterm-hook — mini-term 的 oh-my-pi(omp)状态上报扩展
// miniterm-hook — mini-term status bridge for oh-my-pi (omp)
//
// 本文件由 mini-term 生成并维护(设置 → AI Hook 事件 → 注入目标 oh-my-pi)。mini-term 每次
// 启动都会把它刷成当前版本,手改会被覆盖;要停用请在 mini-term 里卸载,或直接删掉本文件。
// Generated and kept up to date by mini-term; edits are overwritten on its next launch.
// Uninstall from mini-term's settings, or simply delete this file.
//
// 工作方式:omp 在进程内派发生命周期事件,本扩展把它们翻译成 mini-term hook 服务器认识的
// 事件名(与 Claude Code 的 hook 事件同名),POST 到 127.0.0.1 上的本地端口。端口取 mini-term
// 注入给终端的 MINITERM_HOOK_PORT,缺失时读数据目录里的 hook-server.json。不在 mini-term 的
// 终端里(没有 MINITERM_PTY_ID)时整个扩展是空操作,不影响 omp 本身。
//
// 只有交互式主会话(ctx.mode === "tui")上报:omp 的子代理(task 工具)是同一进程里的独立
// 会话,会把本扩展工厂再绑定一遍;若照常上报,子代理跑完的 agent_end 会把父会话误报成
// 「已完成」。父会话自己的 tool_call / tool_result 已覆盖子代理运行的整段时间。

import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const AGENT = "omp";
const APP_ID = "com.mini-term.app";
/** 单次 POST 的上限:hook 服务器在本机回环上,正常几毫秒;超时只丢这一条,不拖慢 omp */
const POST_TIMEOUT_MS = 800;
/** omp 的提问工具:等用户作答期间点黄灯(对应 Claude 的 Elicitation) */
const ASK_TOOL = "ask";

type SessionManagerLike = { getSessionId?: () => unknown };
type Ctx = {
	mode?: string;
	hasUI?: boolean;
	cwd?: string;
	sessionManager?: SessionManagerLike;
};
// 事件 payload 按需取字段,不绑定 omp 的类型包(pi 与 omp 的类型发布在不同包名下)
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type AnyEvent = any;
type Api = {
	on(event: string, handler: (event: AnyEvent, ctx: Ctx) => Promise<void> | void): void;
};

function isLeadSession(ctx: Ctx | undefined): boolean {
	if (!ctx) return false;
	// 新版 omp 带 mode;旧版没有,退回「有没有 UI」——子代理与 print 模式都没有
	return typeof ctx.mode === "string" ? ctx.mode === "tui" : ctx.hasUI === true;
}

function ptyId(): number | undefined {
	const raw = process.env.MINITERM_PTY_ID;
	return raw && /^\d+$/.test(raw) ? Number(raw) : undefined;
}

/** hook-server.json 的位置,与 miniterm-hook sidecar 的口径逐字一致 */
function portFilePath(): string | undefined {
	switch (process.platform) {
		case "win32": {
			const appdata = process.env.APPDATA;
			return appdata ? join(appdata, APP_ID, "hook-server.json") : undefined;
		}
		case "darwin":
			return join(homedir(), "Library", "Application Support", APP_ID, "hook-server.json");
		case "linux": {
			const data = process.env.XDG_DATA_HOME || join(homedir(), ".local", "share");
			return join(data, APP_ID, "hook-server.json");
		}
		default:
			return undefined;
	}
}

function resolvePort(): number | undefined {
	const env = process.env.MINITERM_HOOK_PORT;
	if (env && /^\d+$/.test(env)) return Number(env);
	const file = portFilePath();
	if (!file) return undefined;
	try {
		const port = JSON.parse(readFileSync(file, "utf8"))?.port;
		return typeof port === "number" && port > 0 ? port : undefined;
	} catch {
		return undefined;
	}
}

function sessionIdOf(ctx: Ctx | undefined): string | undefined {
	try {
		const id = ctx?.sessionManager?.getSessionId?.();
		return typeof id === "string" && id ? id : undefined;
	} catch {
		return undefined;
	}
}

function lastAssistant(messages: unknown): AnyEvent | undefined {
	if (!Array.isArray(messages)) return undefined;
	for (let i = messages.length - 1; i >= 0; i--) {
		const m = messages[i];
		if (m && typeof m === "object" && (m as { role?: unknown }).role === "assistant") return m;
	}
	return undefined;
}

async function post(
	event: string,
	ctx: Ctx,
	sessionId: string | undefined,
	fields: Record<string, unknown> = {},
): Promise<void> {
	const pty = ptyId();
	if (pty === undefined) return;
	const port = resolvePort();
	if (port === undefined) return;
	const body = JSON.stringify({
		pty_id: pty,
		event,
		agent: AGENT,
		session_id: sessionId,
		cwd: ctx.cwd || process.cwd(),
		...fields,
	});
	try {
		await fetch(`http://127.0.0.1:${port}/hook`, {
			method: "POST",
			headers: { "content-type": "application/json" },
			body,
			signal: AbortSignal.timeout(POST_TIMEOUT_MS),
		});
	} catch {
		// mini-term 没开 / 端口失效:静默丢弃,状态上报绝不能影响 omp 本身
	}
}

export default function minitermHook(pi: Api): void {
	/** 当前会话 id:每个事件都带上,mini-term 靠它把 pane 绑到确切的会话 */
	let sessionId: string | undefined;

	/** 该不该上报;顺带刷新会话 id(扩展在会话中途被 /reload-plugins 装入时没有 session_start) */
	const track = (ctx: Ctx): boolean => {
		if (!isLeadSession(ctx)) return false;
		const id = sessionIdOf(ctx);
		if (id) sessionId = id;
		return true;
	};

	/** 换会话:旧会话先以 clear 收尾(只打墓碑、不算退出),新会话再 SessionStart */
	const switchSession = async (ctx: Ctx, reason: string): Promise<void> => {
		if (!isLeadSession(ctx)) return;
		const next = sessionIdOf(ctx);
		if (!next || next === sessionId) return;
		const prev = sessionId;
		sessionId = next;
		if (prev) await post("SessionEnd", ctx, prev, { reason: "clear" });
		await post("SessionStart", ctx, next, { reason });
	};

	pi.on("session_start", async (_event, ctx) => switchSession(ctx, "startup"));
	pi.on("session_switch", async (event, ctx) => switchSession(ctx, String(event?.reason ?? "switch")));
	pi.on("session_branch", async (_event, ctx) => switchSession(ctx, "fork"));

	// 用户提交 prompt → 开跑
	pi.on("agent_start", async (_event, ctx) => {
		if (!track(ctx)) return;
		await post("UserPromptSubmit", ctx, sessionId);
	});

	// 回合结束:aborted 是用户打断(不算完成),error 是回合因错误收场(点黄灯),其余才是完成
	pi.on("agent_end", async (event, ctx) => {
		if (!track(ctx)) return;
		if (event?.willContinue) return; // 已排定自动续跑(重试/续写),回合没真的结束
		const last = lastAssistant(event?.messages);
		const stop = last?.stopReason;
		if (stop === "aborted") {
			await post("Stop", ctx, sessionId, { reason: "aborted" });
		} else if (stop === "error") {
			await post("StopFailure", ctx, sessionId, { error_type: String(last?.errorMessage ?? "error") });
		} else {
			await post("Stop", ctx, sessionId);
		}
	});

	// 工具调用前后;提问工具单独映射成 Elicitation / ElicitationResult(等作答期间点黄灯)
	pi.on("tool_call", async (event, ctx) => {
		if (!track(ctx)) return;
		const tool = String(event?.toolName ?? "");
		await post(tool === ASK_TOOL ? "Elicitation" : "PreToolUse", ctx, sessionId, { tool_name: tool });
	});
	pi.on("tool_result", async (event, ctx) => {
		if (!track(ctx)) return;
		const tool = String(event?.toolName ?? "");
		const name = tool === ASK_TOOL ? "ElicitationResult" : event?.isError ? "PostToolUseFailure" : "PostToolUse";
		await post(name, ctx, sessionId, { tool_name: tool });
	});

	// 等用户批准工具 → 黄灯;批准/拒绝后 AI 继续跑,黄灯随之熄灭
	pi.on("tool_approval_requested", async (event, ctx) => {
		if (!track(ctx)) return;
		await post("PermissionRequest", ctx, sessionId, { tool_name: String(event?.toolName ?? "") });
	});
	pi.on("tool_approval_resolved", async (event, ctx) => {
		if (!track(ctx)) return;
		await post(event?.approved ? "PreToolUse" : "PermissionDenied", ctx, sessionId, {
			tool_name: String(event?.toolName ?? ""),
		});
	});

	// API 错误自动重试中:仍在工作,不是等用户(文案含 retrying,mini-term 据此判为重试类)
	pi.on("auto_retry_start", async (event, ctx) => {
		if (!track(ctx)) return;
		await post("Notification", ctx, sessionId, {
			message: `API error, retrying (attempt ${event?.attempt ?? "?"}/${event?.maxAttempts ?? "?"}): ${event?.errorMessage ?? ""}`,
		});
	});

	// 自动压缩上下文期间仍在工作
	pi.on("auto_compaction_start", async (_event, ctx) => {
		if (!track(ctx)) return;
		await post("PreCompact", ctx, sessionId);
	});
	pi.on("auto_compaction_end", async (_event, ctx) => {
		if (!track(ctx)) return;
		await post("PostCompact", ctx, sessionId);
	});

	// 进程退出(Ctrl+C / Ctrl+D / /exit / 信号):mini-term 只认 SessionEnd 作为退出信号
	pi.on("session_shutdown", async (_event, ctx) => {
		if (!track(ctx)) return;
		const ending = sessionId;
		sessionId = undefined;
		await post("SessionEnd", ctx, ending, { reason: "exit" });
	});
}
