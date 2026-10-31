// miniterm-hook — mini-term 的 pi 状态上报扩展
// miniterm-hook — mini-term status bridge for pi (earendil-works/pi)
//
// 本文件由 mini-term 生成并维护(设置 → AI Hook 事件 → 注入目标 pi)。mini-term 每次
// 启动都会把它刷成当前版本,手改会被覆盖;要停用请在 mini-term 里卸载,或直接删掉本文件。
// Generated and kept up to date by mini-term; edits are overwritten on its next launch.
// Uninstall from mini-term's settings, or simply delete this file.
//
// 工作方式:pi 在进程内派发生命周期事件,本扩展把它们翻译成 mini-term hook 服务器认识的
// 事件名(与 Claude Code 的 hook 事件同名),POST 到 127.0.0.1 上的本地端口。端口取 mini-term
// 注入给终端的 MINITERM_HOOK_PORT,缺失时读数据目录里的 hook-server.json。不在 mini-term 的
// 终端里(没有 MINITERM_PTY_ID)时整个扩展是空操作,不影响 pi 本身。
//
// 只有交互式主会话(ctx.mode === "tui")上报:print / json / rpc 模式(含扩展拉起的子 pi
// 进程,它们继承了终端的环境变量)若照常上报,跑完就会把这个 pane 误报成「已完成」。
//
// pi 的扩展 API 已与 oh-my-pi 分叉,两份模板不能互换:
// - 回合结束看 agent_settled(pi ≥ 0.80.4):agent_end 只是一次低层运行结束,自动重试 /
//   压缩重试 / 排队消息都会在它之后再起一次运行,拿它当完成会在每次重试时误报;
// - 换会话没有 session_switch:/new /resume /fork 与 /reload 都是先 session_shutdown 拆掉旧
//   扩展运行时,再在新运行时里 session_start,靠 shutdown 的 reason 区分退出与换会话;
// - 没有工具审批事件:扩展弹出的阻塞式对话框(权限确认 / 提问 / 选择)由 ui_prompt_start /
//   ui_prompt_end 报出来(pi ≥ 0.84.4),回合进行中才映射成「等你处理」。

import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const AGENT = "pi";
const APP_ID = "com.mini-term.app";
/** 单次 POST 的上限:hook 服务器在本机回环上,正常几毫秒;超时只丢这一条,不拖慢 pi */
const POST_TIMEOUT_MS = 800;

type SessionManagerLike = { getSessionId?: () => unknown };
type Ctx = {
	mode?: string;
	hasUI?: boolean;
	cwd?: string;
	sessionManager?: SessionManagerLike;
};
// 事件 payload 按需取字段,不绑定 pi 的类型包(包名在 @mariozechner 与 @earendil-works 之间换过)
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type AnyEvent = any;
type Api = {
	on(event: string, handler: (event: AnyEvent, ctx: Ctx) => Promise<void> | void): unknown;
};

function isLeadSession(ctx: Ctx | undefined): boolean {
	if (!ctx) return false;
	// pi ≥ 0.78.1 带 mode;更早的版本没有,退回「有没有 UI」——print / json 模式都没有
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
		// mini-term 没开 / 端口失效:静默丢弃,状态上报绝不能影响 pi 本身
	}
}

export default function minitermHook(pi: Api): void {
	/** 当前会话 id:每个事件都带上,mini-term 靠它把 pane 绑到确切的会话 */
	let sessionId: string | undefined;
	/** 一轮任务进行中(agent_start → agent_settled)。只有这段时间里弹出的对话框才算「AI 停下来等你」 */
	let busy = false;
	/** 本轮最近一次 agent_end 里最后一条 assistant 的收场方式;agent_settled 据此区分完成 / 出错 */
	let lastStop: string | undefined;
	let lastError: string | undefined;

	/** 该不该上报;顺带刷新会话 id */
	const track = (ctx: Ctx): boolean => {
		if (!isLeadSession(ctx)) return false;
		const id = sessionIdOf(ctx);
		if (id) sessionId = id;
		return true;
	};

	// 会话开始:进程启动、/reload,或 /new /resume /fork 换进来的新会话(各是一个新的扩展运行时)
	pi.on("session_start", async (event, ctx) => {
		if (!track(ctx)) return;
		await post("SessionStart", ctx, sessionId, { reason: String(event?.reason ?? "startup") });
	});

	// 扩展运行时拆除:/reload 是同一会话接着用,什么都不报;/new /resume /fork 只给旧会话打墓碑
	// (clear,不算退出,紧接着新运行时会 SessionStart);其余(quit、信号、更早的 pi 不带
	// reason)按退出——mini-term 只认 SessionEnd 作为退出信号
	pi.on("session_shutdown", async (event, ctx) => {
		if (!track(ctx)) return;
		const reason = String(event?.reason ?? "quit");
		if (reason === "reload") return;
		const ending = sessionId;
		sessionId = undefined;
		busy = false;
		const replaced = reason === "new" || reason === "resume" || reason === "fork";
		await post("SessionEnd", ctx, ending, { reason: replaced ? "clear" : "exit" });
	});

	// 一次运行开始(用户提交 prompt,或重试 / 压缩重试 / 排队消息接着跑)
	pi.on("agent_start", async (_event, ctx) => {
		if (!track(ctx)) return;
		busy = true;
		lastStop = undefined;
		lastError = undefined;
		await post("UserPromptSubmit", ctx, sessionId);
	});

	// 一次运行结束:后面可能还有自动续跑,不是回合结束,只记下收场方式
	pi.on("agent_end", async (event, ctx) => {
		if (!track(ctx)) return;
		const last = lastAssistant(event?.messages);
		lastStop = typeof last?.stopReason === "string" ? last.stopReason : undefined;
		lastError = last?.errorMessage ? String(last.errorMessage) : undefined;
	});

	// 回合彻底结束(不会再自动续跑):aborted 是用户打断(不算完成),最后一次运行以 error 收场
	// 是回合出错(点黄灯),其余才是完成。aborted 字段 pi ≥ 1.1.0 才有,更早的看 stopReason
	pi.on("agent_settled", async (event, ctx) => {
		if (!track(ctx)) return;
		busy = false;
		const stop = lastStop;
		const error = lastError;
		lastStop = undefined;
		lastError = undefined;
		if (event?.aborted === true || stop === "aborted") {
			await post("Stop", ctx, sessionId, { reason: "aborted" });
		} else if (stop === "error") {
			await post("StopFailure", ctx, sessionId, { error_type: error ?? "error" });
		} else {
			await post("Stop", ctx, sessionId);
		}
	});

	// 工具调用前后
	pi.on("tool_call", async (event, ctx) => {
		if (!track(ctx)) return;
		await post("PreToolUse", ctx, sessionId, { tool_name: String(event?.toolName ?? "") });
	});
	pi.on("tool_result", async (event, ctx) => {
		if (!track(ctx)) return;
		await post(event?.isError ? "PostToolUseFailure" : "PostToolUse", ctx, sessionId, {
			tool_name: String(event?.toolName ?? ""),
		});
	});

	// 扩展弹出的阻塞式对话框(权限确认 / 提问 / 选择)→ 黄灯,关掉后 AI 继续跑、黄灯随之熄灭。
	// 空闲时斜杠命令弹的对话框不报:用户本就在跟前,报了反而会把空闲的 pane 抬成「工作中」
	pi.on("ui_prompt_start", async (_event, ctx) => {
		if (!track(ctx) || !busy) return;
		await post("Elicitation", ctx, sessionId);
	});
	pi.on("ui_prompt_end", async (_event, ctx) => {
		if (!track(ctx) || !busy) return;
		await post("ElicitationResult", ctx, sessionId);
	});
}
