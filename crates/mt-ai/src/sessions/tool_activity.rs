//! 工具活动:四家会话记录里的工具调用 → (工具名, 一行目标摘要)。
//!
//! 供 mt-relay 对话镜像产出 kind = "tool" 的消息,让移动端看得到 agent 在读哪个
//! 文件、跑哪条命令;桌面 AI 历史面板不用它。全是行级纯函数,与同目录的提问解析
//! (`*_question_from_line`)同一口径:
//!
//! - **提问类工具一律排除**——它们已经出提问卡片:Claude `AskUserQuestion` /
//!   Codex `request_user_input` / Grok `ask_user_question` / OMP `ask`,
//!   判据与对应的 `*_question_from_line` 一致;
//! - 目标只给对象(路径 / 命令首行 / 搜索词),**不写任何自然语言文案**(工具名的
//!   展示文案归前端 i18n),统一过 [`summarize_tool_target`];
//! - 每家先做一次子串预筛再解析 JSON:换绑要把整份记录(实测可到 19 MB,工具输出
//!   行动辄几 MB)从头解析一遍,不含工具调用判别串的行不必再多付一次全量解析。
//!   预筛只会放过、不会误杀——判别串是 JSON 里必然逐字出现的键值。
//!
//! 目标字段核对过本机真实记录的有 Claude / Codex / Grok(见各段注释);OMP 本机
//! 没有记录,字段名按 oh-my-pi 源码的工具参数 schema 定。

use serde_json::Value;

use super::grok::{grok_acp_update, unix_to_iso};
use super::line_timestamp;

/// 目标摘要的字符上限(按 char 计,超出截断并补 `…`)。
pub const TOOL_TARGET_MAX_CHARS: usize = 160;

/// 会话记录里的一次工具调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiToolCall {
    /// 工具名原文(`Bash` / `Edit` / `shell_command` / `apply_patch` …)
    pub name: String,
    /// 一行目标摘要(已过 [`summarize_tool_target`]);该工具没有可代表的对象时为 None
    pub target: Option<String>,
    /// 所在行的 ISO 8601 时间戳,缺失时为空串
    pub timestamp: String,
}

/// 目标摘要的统一后处理:取第一个非空行 → 连续空白折叠成一个空格 →
/// 按字符边界截到 [`TOOL_TARGET_MAX_CHARS`],超出补 `…`。空串 → None。
pub fn summarize_tool_target(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let mut out: String = chars.by_ref().take(TOOL_TARGET_MAX_CHARS).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    Some(out)
}

/// `obj[key]` 是非空字符串 → 摘要。
fn str_target(obj: &Value, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .and_then(summarize_tool_target)
}

/// 按优先级取第一个有摘要的键。
fn first_target(obj: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| str_target(obj, key))
}

// ─── Claude ───────────────────────────────────────────────────
//
// 工具调用是 assistant 行 `message.content` 里的 `tool_use` 块(`name` + `input`)。
// 真实记录里 Claude Code 基本是一块一行,偶有 text + tool_use 同行。
// 核对过真实记录的字段:Bash/PowerShell.command、Read/Write/Edit.file_path、
// Grep/Glob.pattern、WebFetch.url、WebSearch.query、Agent.description、Skill.skill;
// MultiEdit.file_path / NotebookEdit.notebook_path / Task.description 按 Claude Code
// 工具 schema。

/// 解析 Claude JSONL 的一行为工具调用(assistant 行的 `tool_use` 块,按块序)。
/// 子代理行(`isSidechain: true`)/ 非 assistant 行 / AskUserQuestion → 不产出。
pub fn claude_tool_calls_from_line(line: &str) -> Vec<AiToolCall> {
    if !line.contains("\"tool_use\"") {
        return Vec::new();
    }
    let Ok(obj) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    if obj.get("type").and_then(Value::as_str) != Some("assistant")
        || obj.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        return Vec::new();
    }
    let Some(blocks) = obj.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let timestamp = line_timestamp(&obj);
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|b| {
            let name = b.get("name").and_then(Value::as_str)?;
            // 提问走提问卡片(判据同 claude_question_from_line)
            if name.is_empty() || name == "AskUserQuestion" {
                return None;
            }
            Some(AiToolCall {
                name: name.to_string(),
                target: b.get("input").and_then(|input| claude_target(name, input)),
                timestamp: timestamp.clone(),
            })
        })
        .collect()
}

fn claude_target(name: &str, input: &Value) -> Option<String> {
    let key = match name {
        // PowerShell 是 Windows 上与 Bash 并列的 shell 工具,入参同为 command
        "Bash" | "PowerShell" => "command",
        "Read" | "Write" | "Edit" | "MultiEdit" => "file_path",
        "NotebookEdit" => "notebook_path",
        "Grep" | "Glob" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Task" | "Agent" => "description",
        "Skill" => "skill",
        // TodoWrite / mcp__* / 其余工具:没有能一行概括的对象
        _ => return None,
    };
    str_target(input, key)
}

// ─── Codex ────────────────────────────────────────────────────
//
// 工具调用是 `response_item` 行,payload.type 分几种(真实记录核对过):
// - `function_call`:`name` + `arguments`(JSON **字符串**)。`shell_command.command`
//   是字符串、`exec_command.cmd` 是字符串、旧版 `shell.command` 是 argv 数组
//   (见过 `["powershell.exe","-Command",…]` 与 `["apply_patch","*** Begin Patch…"]`);
// - `custom_tool_call`:`name` + `input`(纯文本),`apply_patch` 的补丁正文在 input;
//   `exec`(代码模式)的 input 是一段 JS 脚本,经 `tools.<内置工具>(…)` 代调
//   shell_command / apply_patch 等,八成以上只调一个;
// - `web_search_call`:没有 name,`action` 是 search(query/queries)/ open_page(url)/
//   find_in_page(pattern + url);
// - `tool_search_call`:工具发现,没有对象。
// `local_shell_call`(action.command 为 argv)按协议定义一并认,本机记录里没见过。

/// 解析 Codex JSONL 的一行为工具调用(`response_item` 的各类 `*_call`)。
/// request_user_input → 不产出(走提问卡片)。
pub fn codex_tool_calls_from_line(line: &str) -> Vec<AiToolCall> {
    // `function_call"` / `custom_tool_call"` / `web_search_call"` … 都以 `_call"` 收尾
    if !line.contains("_call\"") {
        return Vec::new();
    }
    let Ok(obj) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    if obj.get("type").and_then(Value::as_str) != Some("response_item") {
        return Vec::new();
    }
    let Some((name, target)) = obj.get("payload").and_then(codex_tool_call) else {
        return Vec::new();
    };
    // 提问走提问卡片(判据同 codex_question_from_line)
    if name.is_empty() || name == "request_user_input" {
        return Vec::new();
    }
    vec![AiToolCall {
        name,
        target,
        timestamp: line_timestamp(&obj),
    }]
}

/// payload → (工具名, 目标)。不是工具调用的 payload → None。
fn codex_tool_call(payload: &Value) -> Option<(String, Option<String>)> {
    match payload.get("type").and_then(Value::as_str)? {
        "function_call" => {
            let name = payload.get("name").and_then(Value::as_str)?;
            // arguments 解析失败只丢目标,调用本身照出
            let args = payload
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Null);
            Some(codex_function_target(name, &args))
        }
        "custom_tool_call" => {
            let name = payload.get("name").and_then(Value::as_str)?;
            let input = payload.get("input").and_then(Value::as_str).unwrap_or("");
            let target = match name {
                "apply_patch" => patch_target(input),
                "exec" => exec_inner_tool(input),
                _ => None,
            };
            Some((name.to_string(), target))
        }
        "local_shell_call" => {
            let command = payload.pointer("/action/command").and_then(command_text);
            Some(shell_call("local_shell", command))
        }
        "web_search_call" => {
            let target = payload.get("action").and_then(|action| {
                first_target(action, &["query", "pattern", "url"]).or_else(|| {
                    action
                        .get("queries")?
                        .as_array()?
                        .iter()
                        .find_map(|q| q.as_str().and_then(summarize_tool_target))
                })
            });
            Some(("web_search".to_string(), target))
        }
        "tool_search_call" => Some(("tool_search".to_string(), None)),
        _ => None,
    }
}

fn codex_function_target(name: &str, args: &Value) -> (String, Option<String>) {
    match name {
        "shell" | "container.exec" | "shell_command" => {
            shell_call(name, args.get("command").and_then(command_text))
        }
        "exec_command" => shell_call(name, args.get("cmd").and_then(command_text)),
        // 旧版 JSON 形态的 apply_patch:补丁正文在 input
        "apply_patch" => (
            name.to_string(),
            args.get("input")
                .and_then(Value::as_str)
                .and_then(patch_target),
        ),
        "view_image" => (name.to_string(), str_target(args, "path")),
        "spawn_agent" => (name.to_string(), str_target(args, "task_name")),
        // update_plan 及其余工具:没有能一行概括的对象
        _ => (name.to_string(), None),
    }
}

/// shell 类调用。命令里带补丁时(apply_patch 经 shell 下发,真实记录见过)按补丁
/// 处理:工具名记作 `apply_patch`、目标取补丁里的文件路径——与 Codex 自家 TUI 把它
/// 渲染成改文件同一口径;否则取命令首行。
fn shell_call(name: &str, command: Option<String>) -> (String, Option<String>) {
    let Some(command) = command else {
        return (name.to_string(), None);
    };
    if command.contains("*** Begin Patch")
        && let Some(path) = patch_target(&command)
    {
        return ("apply_patch".to_string(), Some(path));
    }
    (name.to_string(), summarize_tool_target(&command))
}

/// command 字段:字符串原样;argv 数组用空格拼接。
fn command_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(argv) => Some(
            argv.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// 补丁正文里第一个 `*** Update/Add/Delete File: <path>` 的路径。
fn patch_target(patch: &str) -> Option<String> {
    const HEADERS: [&str; 3] = ["*** Update File:", "*** Add File:", "*** Delete File:"];
    patch.lines().find_map(|line| {
        let line = line.trim_start();
        HEADERS
            .iter()
            .find_map(|header| line.strip_prefix(header))
            .and_then(summarize_tool_target)
    })
}

/// 代码模式 exec 脚本里第一个被调的内置工具名(`tools.shell_command(` → shell_command)。
/// 只认工具名、不去抠 JS 字面量里的命令——转义 / 模板串一抠就碎;同 grok 的
/// use_tool,目标就是被代调的工具。
fn exec_inner_tool(script: &str) -> Option<String> {
    const PREFIX: &str = "tools.";
    script.match_indices(PREFIX).find_map(|(at, _)| {
        let rest = &script[at + PREFIX.len()..];
        let ident_len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let (ident, after) = rest.split_at(ident_len);
        (!ident.is_empty() && after.trim_start().starts_with('(')).then(|| ident.to_string())
    })
}

// ─── Grok ─────────────────────────────────────────────────────
//
// 真实记录里每次调用落三行:`tool_call` 首行(title = 工具名、`_meta["x.ai/tool"].name`、
// rawInput = 模型给的参数)→ `tool_call_update` 富化行(无 status,title 换成给人看的
// 描述,rawInput 补默认值)→ 终态行(status = completed/failed)。只认首行:它必在且
// 最早落盘(交互式工具的富化行能晚几十秒),一次调用恰好出一条,不必去重。
// 核对过真实记录的 rawInput 字段:run_terminal_command.command、read_file.target_file、
// list_dir.target_directory、grep.pattern、use_tool.tool_name、search_tool.query。

/// 解析 grok updates.jsonl 的一行为工具调用(只认 `tool_call` 首行,见上)。
/// ask_user_question → 不产出(走提问卡片)。
pub fn grok_tool_calls_from_line(line: &str) -> Vec<AiToolCall> {
    // `"tool_call_update"` 不含这个判别串,富化行与终态行在这里就被筛掉
    if !line.contains("\"tool_call\"") {
        return Vec::new();
    }
    let Some((update, ts)) = grok_acp_update(line) else {
        return Vec::new();
    };
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("tool_call") {
        return Vec::new();
    }
    let title = update.get("title").and_then(Value::as_str).unwrap_or("");
    let name = update
        .pointer("/_meta/x.ai~1tool/name")
        .and_then(Value::as_str)
        .unwrap_or(title);
    // 提问走提问卡片(判据同 grok_question_from_line 认首行的 title)
    if name.is_empty() || name == "ask_user_question" || title == "ask_user_question" {
        return Vec::new();
    }
    vec![AiToolCall {
        name: name.to_string(),
        target: update
            .get("rawInput")
            .and_then(|input| grok_target(name, input)),
        timestamp: unix_to_iso(ts),
    }]
}

fn grok_target(name: &str, input: &Value) -> Option<String> {
    match name {
        "run_terminal_command" => str_target(input, "command"),
        "read_file" => str_target(input, "target_file"),
        "list_dir" => str_target(input, "target_directory"),
        "grep" => str_target(input, "pattern"),
        // use_tool 是代调别的(MCP 等)工具:目标就是被调的工具名
        "use_tool" => str_target(input, "tool_name"),
        // search_tool 是工具发现(同 Claude 的 ToolSearch),没有对象
        "search_tool" => None,
        // 没见过的工具:按常见目标键的优先级取一个
        _ => first_target(
            input,
            &[
                "command",
                "target_file",
                "file_path",
                "path",
                "target_directory",
                "pattern",
                "query",
                "url",
            ],
        ),
    }
}

// ─── OMP ──────────────────────────────────────────────────────
//
// 工具调用是 assistant 消息行 `message.content` 里的 `toolCall` 块(`name` +
// `arguments` 对象),与 ask 提问同形。参数名按 oh-my-pi 源码的工具 schema:
// bash.command、read/write/edit.path、glob.path(通配式写在 path 里)、
// grep.pattern(`search` 是 grep 的旧名)、web_search.query、task.name。

/// 解析 OMP JSONL 的一行为工具调用(assistant 消息里的 `toolCall` 块,按块序)。
/// ask → 不产出(走提问卡片)。
pub fn omp_tool_calls_from_line(line: &str) -> Vec<AiToolCall> {
    if !line.contains("\"toolCall\"") {
        return Vec::new();
    }
    let Ok(obj) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    if obj.get("type").and_then(Value::as_str) != Some("message")
        || obj.pointer("/message/role").and_then(Value::as_str) != Some("assistant")
    {
        return Vec::new();
    }
    let Some(blocks) = obj.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let timestamp = line_timestamp(&obj);
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("toolCall"))
        .filter_map(|b| {
            let name = b.get("name").and_then(Value::as_str)?;
            // 提问走提问卡片(判据同 omp_question_from_line)
            if name.is_empty() || name == "ask" {
                return None;
            }
            Some(AiToolCall {
                name: name.to_string(),
                target: b.get("arguments").and_then(|args| omp_target(name, args)),
                timestamp: timestamp.clone(),
            })
        })
        .collect()
}

fn omp_target(name: &str, args: &Value) -> Option<String> {
    match name {
        "bash" => str_target(args, "command"),
        "read" | "write" | "edit" | "ls" => first_target(args, &["path", "file_path"]),
        "glob" | "find" => first_target(args, &["path", "pattern"]),
        "grep" | "search" => str_target(args, "pattern"),
        "web_search" => str_target(args, "query"),
        "fetch" => str_target(args, "url"),
        "task" => str_target(args, "name"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, target: Option<&str>, timestamp: &str) -> AiToolCall {
        AiToolCall {
            name: name.to_string(),
            target: target.map(String::from),
            timestamp: timestamp.to_string(),
        }
    }

    // ---- 目标摘要的统一后处理 ----

    #[test]
    fn summarize_takes_first_non_blank_line_and_collapses_whitespace() {
        assert_eq!(
            summarize_tool_target("\n  \r\n  cargo   test\t-p  mt-ai  \nsecond line").as_deref(),
            Some("cargo test -p mt-ai")
        );
        assert_eq!(
            summarize_tool_target("src/main.rs").as_deref(),
            Some("src/main.rs")
        );
        // 空串 / 纯空白 → None
        assert_eq!(summarize_tool_target(""), None);
        assert_eq!(summarize_tool_target(" \n\t\r\n "), None);
    }

    #[test]
    fn summarize_truncates_on_char_boundary_with_ellipsis() {
        // 恰好 160 字符:原样,不补省略号
        let exact = "a".repeat(TOOL_TARGET_MAX_CHARS);
        assert_eq!(
            summarize_tool_target(&exact).as_deref(),
            Some(exact.as_str())
        );

        // 多一个字符:截到 160 再补 `…`
        let over = "b".repeat(TOOL_TARGET_MAX_CHARS + 1);
        let got = summarize_tool_target(&over).unwrap();
        assert_eq!(got.chars().count(), TOOL_TARGET_MAX_CHARS + 1);
        assert!(got.ends_with('…'));
        assert_eq!(got.trim_end_matches('…'), "b".repeat(TOOL_TARGET_MAX_CHARS));

        // 多字节字符按 char 计数,不会切在 UTF-8 中间
        let wide = "镜".repeat(TOOL_TARGET_MAX_CHARS + 5);
        let got = summarize_tool_target(&wide).unwrap();
        assert_eq!(got, format!("{}…", "镜".repeat(TOOL_TARGET_MAX_CHARS)));

        // 折叠空白先于截断:截的是折叠后的文本
        let spaced = format!("x{}y", " ".repeat(500));
        assert_eq!(summarize_tool_target(&spaced).as_deref(), Some("x y"));
    }

    // ---- Claude ----

    /// 形状取自真实记录:text 在前、两个 tool_use 在后,按块序产出。
    #[test]
    fn claude_text_and_tools_in_block_order() {
        let line = r#"{"type":"assistant","timestamp":"2026-10-11T08:00:00.000Z","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"先看看。"},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"D:\\proj\\src\\main.rs","limit":40}},{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"cargo build\ncargo test","description":"构建"}}]}}"#;
        assert_eq!(
            claude_tool_calls_from_line(line),
            vec![
                call(
                    "Read",
                    Some(r"D:\proj\src\main.rs"),
                    "2026-10-11T08:00:00.000Z"
                ),
                call("Bash", Some("cargo build"), "2026-10-11T08:00:00.000Z"),
            ]
        );
    }

    #[test]
    fn claude_excludes_question_sidechain_and_non_assistant_lines() {
        // AskUserQuestion 走提问卡片;同行的别家工具照出
        let ask = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_q","name":"AskUserQuestion","input":{"questions":[]}},{"type":"tool_use","id":"toolu_t","name":"TodoWrite","input":{"todos":[]}}]}}"#;
        assert_eq!(
            claude_tool_calls_from_line(ask),
            vec![call("TodoWrite", None, "")]
        );

        // 子代理行整行跳过
        let sidechain = r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_s","name":"Bash","input":{"command":"ls"}}]}}"#;
        assert!(claude_tool_calls_from_line(sidechain).is_empty());

        // user 行(tool_result 正文里出现判别串)不是工具调用
        let user = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"\"tool_use\" mentioned"}]}}"#;
        assert!(claude_tool_calls_from_line(user).is_empty());

        // 纯文本 / 非 JSON
        let text = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#;
        assert!(claude_tool_calls_from_line(text).is_empty());
        assert!(claude_tool_calls_from_line("not json \"tool_use\"").is_empty());
    }

    #[test]
    fn claude_target_per_tool() {
        let cases = [
            (
                r#""Write","input":{"file_path":"a.txt","content":"x"}"#,
                Some("a.txt"),
            ),
            (
                r#""Edit","input":{"file_path":"b.rs","old_string":"x","new_string":"y"}"#,
                Some("b.rs"),
            ),
            (
                r#""MultiEdit","input":{"file_path":"c.rs","edits":[]}"#,
                Some("c.rs"),
            ),
            (
                r#""NotebookEdit","input":{"notebook_path":"n.ipynb","new_source":""}"#,
                Some("n.ipynb"),
            ),
            (
                r#""Grep","input":{"pattern":"fn main","path":"src"}"#,
                Some("fn main"),
            ),
            (r#""Glob","input":{"pattern":"**/*.rs"}"#, Some("**/*.rs")),
            (
                r#""WebFetch","input":{"url":"https://example.com","prompt":"p"}"#,
                Some("https://example.com"),
            ),
            (
                r#""WebSearch","input":{"query":"rust serde"}"#,
                Some("rust serde"),
            ),
            (
                r#""Agent","input":{"description":"查调用点","prompt":"…"}"#,
                Some("查调用点"),
            ),
            (
                r#""Task","input":{"description":"审查","prompt":"…"}"#,
                Some("审查"),
            ),
            (r#""Skill","input":{"skill":"tdd","args":"x"}"#, Some("tdd")),
            (
                r#""PowerShell","input":{"command":"Get-ChildItem","description":"d"}"#,
                Some("Get-ChildItem"),
            ),
            (r#""TodoWrite","input":{"todos":[]}"#, None),
            (r#""mcp__ctx__query","input":{"query":"x"}"#, None),
            // 目标字段缺失 / 为空串 → None,调用照出
            (r#""Bash","input":{"command":"   "}"#, None),
            (r#""Read","input":{}"#, None),
        ];
        for (block, expected) in cases {
            let line = format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t","name":{block}}}]}}}}"#
            );
            let calls = claude_tool_calls_from_line(&line);
            assert_eq!(calls.len(), 1, "{line}");
            assert_eq!(calls[0].target.as_deref(), expected, "{line}");
        }
    }

    // ---- Codex ----

    fn codex_function_line(name: &str, arguments: &serde_json::Value) -> String {
        serde_json::json!({
            "timestamp": "2026-10-11T08:00:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": name,
                "arguments": arguments.to_string(),
                "call_id": "call_1"
            }
        })
        .to_string()
    }

    /// 形状取自真实 rollout:shell_command.command 字符串、exec_command.cmd、
    /// 旧版 shell.command argv 数组。
    #[test]
    fn codex_shell_like_calls_take_first_command_line() {
        let line = codex_function_line(
            "shell_command",
            &serde_json::json!({"command": "git status\ngit diff", "workdir": "D:\\proj", "timeout_ms": 10000}),
        );
        assert_eq!(
            codex_tool_calls_from_line(&line),
            vec![call(
                "shell_command",
                Some("git status"),
                "2026-10-11T08:00:00.000Z"
            )]
        );

        let line = codex_function_line(
            "exec_command",
            &serde_json::json!({"cmd": "cargo  test", "yield_time_ms": 1000}),
        );
        assert_eq!(
            codex_tool_calls_from_line(&line)[0].target.as_deref(),
            Some("cargo test")
        );

        let line = codex_function_line(
            "shell",
            &serde_json::json!({"command": ["powershell.exe", "-Command", "Get-ChildItem src"]}),
        );
        assert_eq!(
            codex_tool_calls_from_line(&line)[0].target.as_deref(),
            Some("powershell.exe -Command Get-ChildItem src")
        );
    }

    /// apply_patch 三种来路:custom_tool_call(input 是补丁)、shell argv 下发、
    /// 旧版 JSON function_call;一律取补丁里第一个文件头的路径。
    #[test]
    fn codex_apply_patch_targets_first_file_header() {
        let custom = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "apply_patch",
                "input": "*** Begin Patch\n*** Add File: docs/new.md\n+hello\n*** Update File: src/lib.rs\n@@\n*** End Patch",
                "call_id": "call_p"
            }
        })
        .to_string();
        assert_eq!(
            codex_tool_calls_from_line(&custom),
            vec![call("apply_patch", Some("docs/new.md"), "")]
        );

        let via_shell = codex_function_line(
            "shell",
            &serde_json::json!({"command": ["apply_patch", "*** Begin Patch\n*** Delete File: old.txt\n*** End Patch"]}),
        );
        let calls = codex_tool_calls_from_line(&via_shell);
        assert_eq!(calls[0].name, "apply_patch");
        assert_eq!(calls[0].target.as_deref(), Some("old.txt"));

        let legacy = codex_function_line(
            "apply_patch",
            &serde_json::json!({"input": "*** Begin Patch\n*** Update File: a/b.rs\n*** End Patch"}),
        );
        assert_eq!(
            codex_tool_calls_from_line(&legacy)[0].target.as_deref(),
            Some("a/b.rs")
        );

        // 只是提到补丁标记、找不到文件头的命令:仍按普通 shell 命令处理
        let mention = codex_function_line(
            "shell_command",
            &serde_json::json!({"command": "rg \"*** Begin Patch\" docs"}),
        );
        let calls = codex_tool_calls_from_line(&mention);
        assert_eq!(calls[0].name, "shell_command");
        assert_eq!(
            calls[0].target.as_deref(),
            Some("rg \"*** Begin Patch\" docs")
        );
    }

    /// exec(代码模式)的 input 是 JS 脚本:目标取第一个被代调的内置工具名。
    #[test]
    fn codex_exec_targets_first_inner_tool() {
        let exec = |script: &str| {
            let line = serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "exec",
                    "input": script,
                    "call_id": "c"
                }
            })
            .to_string();
            let calls = codex_tool_calls_from_line(&line);
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "exec");
            calls[0].target.clone()
        };
        assert_eq!(
            exec("const r = await tools.shell_command({command: \"git status\"});\ntext(r);")
                .as_deref(),
            Some("shell_command")
        );
        // 属性访问(不是调用)跳过,取真正被调的那个
        assert_eq!(
            exec("const t = tools.list;\nawait tools.apply_patch (patch);").as_deref(),
            Some("apply_patch")
        );
        // 不调内置工具的脚本:没有目标
        assert_eq!(exec("const x = 1 + 1;\ntext(x);"), None);
    }

    /// web_search_call 没有 name;三种 action 各取最具体的对象(形状取自真实 rollout)。
    #[test]
    fn codex_web_search_actions() {
        let line = |action: &str| {
            format!(
                r#"{{"type":"response_item","payload":{{"type":"web_search_call","status":"completed","action":{action}}}}}"#
            )
        };
        let target = |action: &str| {
            let calls = codex_tool_calls_from_line(&line(action));
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "web_search");
            calls[0].target.clone()
        };
        assert_eq!(
            target(r#"{"type":"search","query":"gpui docs","queries":["gpui docs"]}"#).as_deref(),
            Some("gpui docs")
        );
        assert_eq!(
            target(r#"{"type":"search","queries":["","serde flatten"]}"#).as_deref(),
            Some("serde flatten")
        );
        assert_eq!(
            target(r#"{"type":"open_page","url":"https://example.com/a"}"#).as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(
            target(r#"{"type":"find_in_page","url":"https://example.com","pattern":"limit"}"#)
                .as_deref(),
            Some("limit")
        );
    }

    #[test]
    fn codex_excludes_question_and_non_call_items() {
        // request_user_input 走提问卡片
        let ask = codex_function_line("request_user_input", &serde_json::json!({"questions": []}));
        assert!(codex_tool_calls_from_line(&ask).is_empty());

        // 回执 / 消息 / 非 response_item 都不是工具调用
        for line in [
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c","output":"ok"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"function_call\" here"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"web_search_call"}}"#,
            "garbage _call\"",
        ] {
            assert!(codex_tool_calls_from_line(line).is_empty(), "{line}");
        }

        // update_plan 无对象;tool_search 是工具发现;arguments 解析失败只丢目标
        let plan = codex_function_line("update_plan", &serde_json::json!({"plan": []}));
        assert_eq!(codex_tool_calls_from_line(&plan)[0].target, None);
        let search = r#"{"type":"response_item","payload":{"type":"tool_search_call","arguments":{"query":"x"},"call_id":"c"}}"#;
        assert_eq!(
            codex_tool_calls_from_line(search),
            vec![call("tool_search", None, "")]
        );
        let bad = r#"{"type":"response_item","payload":{"type":"function_call","name":"shell_command","arguments":"not json","call_id":"c"}}"#;
        assert_eq!(
            codex_tool_calls_from_line(bad),
            vec![call("shell_command", None, "")]
        );
    }

    // ---- Grok ----

    /// 形状取自真实 updates.jsonl:一次调用三行,只有首行出工具。
    #[test]
    fn grok_only_first_tool_call_line_counts() {
        let first = r#"{"timestamp":1788251329,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"call-1","title":"read_file","rawInput":{"target_file":"src/lib.rs","limit":50},"_meta":{"x.ai/tool":{"name":"read_file"}}}}}"#;
        assert_eq!(
            grok_tool_calls_from_line(first),
            vec![call(
                "read_file",
                Some("src/lib.rs"),
                "2026-09-01T08:28:49Z"
            )]
        );

        let enriched = r#"{"timestamp":1788251329,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-1","kind":"read","title":"Read src/lib.rs","rawInput":{"variant":"ReadFile","target_file":"src/lib.rs"},"locations":[{"path":"src/lib.rs"}],"_meta":{"x.ai/tool":{"name":"read_file"}}}}}"#;
        let terminal = r#"{"timestamp":1788251351,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-1","status":"completed","rawOutput":null,"content":[]}}}"#;
        assert!(grok_tool_calls_from_line(enriched).is_empty());
        assert!(grok_tool_calls_from_line(terminal).is_empty());
    }

    #[test]
    fn grok_targets_and_question_exclusion() {
        let line = |name: &str, raw_input: &str| {
            format!(
                r#"{{"timestamp":1,"method":"session/update","params":{{"update":{{"sessionUpdate":"tool_call","toolCallId":"c","title":"{name}","rawInput":{raw_input},"_meta":{{"x.ai/tool":{{"name":"{name}"}}}}}}}}}}"#
            )
        };
        let target = |name: &str, raw_input: &str| {
            let calls = grok_tool_calls_from_line(&line(name, raw_input));
            assert_eq!(calls.len(), 1, "{name}");
            assert_eq!(calls[0].name, name);
            calls[0].target.clone()
        };
        assert_eq!(
            target(
                "run_terminal_command",
                r#"{"command":"npm test","description":"d"}"#
            )
            .as_deref(),
            Some("npm test")
        );
        assert_eq!(
            target("list_dir", r#"{"target_directory":"crates"}"#).as_deref(),
            Some("crates")
        );
        assert_eq!(
            target("grep", r#"{"pattern":"TODO","glob":"*.rs"}"#).as_deref(),
            Some("TODO")
        );
        assert_eq!(
            target(
                "use_tool",
                r#"{"tool_name":"browser_click","tool_input":{}}"#
            )
            .as_deref(),
            Some("browser_click")
        );
        assert_eq!(target("search_tool", r#"{"query":"browser"}"#), None);
        // 没见过的工具按常见目标键取
        assert_eq!(
            target("edit_file", r#"{"file_path":"a.rs","old":"x"}"#).as_deref(),
            Some("a.rs")
        );

        // ask_user_question 走提问卡片
        assert!(
            grok_tool_calls_from_line(&line("ask_user_question", r#"{"questions":[]}"#)).is_empty()
        );
        // 没有 _meta 时 title 兜底当工具名
        let bare = r#"{"timestamp":0,"method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"c","title":"read_file","rawInput":{"target_file":"x"}}}}"#;
        assert_eq!(
            grok_tool_calls_from_line(bare),
            vec![call("read_file", Some("x"), "")]
        );
        // xAI 扩展轨不是 ACP 工具调用
        let xai = r#"{"timestamp":1,"method":"_x.ai/session/update","params":{"update":{"sessionUpdate":"tool_call","title":"read_file"}}}"#;
        assert!(grok_tool_calls_from_line(xai).is_empty());
    }

    // ---- OMP ----

    #[test]
    fn omp_text_and_tool_calls_with_question_excluded() {
        let line = r#"{"type":"message","timestamp":"2026-10-11T08:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"先跑测试"},{"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"bun test\nbun lint","timeout":60}},{"type":"toolCall","id":"c2","name":"read","arguments":{"path":"src/index.ts"}},{"type":"toolCall","id":"c3","name":"ask","arguments":{"questions":[]}}]}}"#;
        assert_eq!(
            omp_tool_calls_from_line(line),
            vec![
                call("bash", Some("bun test"), "2026-10-11T08:00:00Z"),
                call("read", Some("src/index.ts"), "2026-10-11T08:00:00Z"),
            ]
        );

        // toolResult / user 行不是工具调用
        let result = r#"{"type":"message","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"\"toolCall\""}]}}"#;
        assert!(omp_tool_calls_from_line(result).is_empty());
    }

    #[test]
    fn omp_target_per_tool() {
        let cases = [
            (
                r#""edit","arguments":{"path":"a.ts","edits":[]}"#,
                Some("a.ts"),
            ),
            (
                r#""write","arguments":{"path":"b.ts","content":"x"}"#,
                Some("b.ts"),
            ),
            (
                r#""glob","arguments":{"path":"src/**/*.ts"}"#,
                Some("src/**/*.ts"),
            ),
            (
                r#""grep","arguments":{"pattern":"useState","path":"src"}"#,
                Some("useState"),
            ),
            (
                r#""web_search","arguments":{"query":"bun ffi"}"#,
                Some("bun ffi"),
            ),
            (
                r#""task","arguments":{"name":"refactor-auth","agent":"task","task":"…"}"#,
                Some("refactor-auth"),
            ),
            (r#""todo","arguments":{"items":[]}"#, None),
        ];
        for (block, expected) in cases {
            let line = format!(
                r#"{{"type":"message","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"c","name":{block}}}]}}}}"#
            );
            let calls = omp_tool_calls_from_line(&line);
            assert_eq!(calls.len(), 1, "{line}");
            assert_eq!(calls[0].target.as_deref(), expected, "{line}");
        }
    }
}
