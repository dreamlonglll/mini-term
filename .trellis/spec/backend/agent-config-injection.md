# Agent 配置注入 —— 幂等读写外部 AI agent 配置

> mini-term 经常要把自己的能力注册进 Claude Code / Codex 的配置文件（hook、MCP server）。这类「写别人的配置文件」的代码必须遵守本文约定，否则会破坏用户 / 团队的既有配置。

---

## 适用场景

mini-term 代码要写入 / 删除以下任一文件的条目时：

- `~/.claude/settings.json`（Claude 全局 hooks、`enabledMcpjsonServers` 等）
- `~/.codex/config.toml`、`~/.codex/hooks.json`（Codex 全局配置）
- `<project>/.mcp.json`、`<project>/.codex/config.toml`、`<project>/.gitignore`（项目级配置）

现有实现：`src-tauri/src/hook_registry.rs`（hooks）、`src-tauri/src/ssh_mcp_registry.rs`（SSH MCP）。新增同类功能时对标这两个文件。

---

## Pattern：读-改-写 + 命名 marker 幂等

**Problem**：这些配置文件属于用户 / 团队，可能已有别的内容（其它 MCP server、团队共享的 hook、其它 TOML 段）。整文件覆盖、或盲目增删，会破坏用户数据。

**Solution**：

1. **读-改-写**：读入现有文件 → 解析 → 只动自己的条目 → 写回。文件不存在时以空对象 / 空文档起步。
2. **命名 marker**：自己写入的条目用一个固定、可识别的名字（如 MCP server 名 `mini-term-ssh`、hook 命令字符串里含 `miniterm-hook`）。
3. **幂等启用**：启用 = upsert。已存在自己的条目就替换，不存在才追加 —— 重复启用不产生重复条目。
4. **精确停用**：停用 = 只按 marker 删自己的条目，绝不动其它内容。
5. **失败可读**：每步返回 `Result`，错误信息为可读中文；错误信息**不得含密码等敏感值**。

**只删自己加的**：停用时，只删能用 marker 唯一识别归属的条目（如具名 MCP server）。**无法证明归属的共享设置不要在停用时动** —— 例如 Codex 的 `[projects."<path>"] trust_level`，无法区分是 mini-term 加的还是用户自己加的：启用时若已存在则保留不覆盖，停用时不移除。

---

## Gotcha：`toml_edit` 写嵌套表会生成 inline table

**Symptom**：想要 `[mcp_servers.mini-term-ssh]` 这种点路径表头，结果写出来是 `mini-term-ssh = { command = "..." }`（inline table）。

**Cause**：`doc["mcp_servers"]["mini-term-ssh"]["command"] = value` 这种链式赋值，`toml_edit` 默认把新建的中间表建成 inline table。

**Fix**：把目标 key 显式插成非-inline 的 `Table` 再赋字段（示意，确切写法见 `ssh_mcp_registry.rs` 的 `apply_codex_mcp_server`）：

```rust
// 显式建独立表头，而非 inline table
doc["mcp_servers"]["mini-term-ssh"] = toml_edit::Item::Table(toml_edit::Table::new());
doc["mcp_servers"]["mini-term-ssh"]["command"] = toml_edit::value(path);
```

两种形式 TOML 语义等价、Codex 都能解析；但若目标格式要求点路径表头，须显式建 `Table`。

**附注**：`toml_edit` 对含反斜杠的 Windows 路径会自动选用 TOML literal string（单引号 `'C:\...\x.exe'`），免转义且正确 —— 不要再手动转义成 `\\`。

---

## Wrong vs Correct

### Wrong

```rust
// 整文件覆盖 —— 抹掉用户既有的其它 MCP server
let json = serde_json::json!({ "mcpServers": { "mini-term-ssh": server_entry } });
fs::write(&mcp_json_path, json.to_string())?;
```

### Correct

```rust
// 读-改-写，只 upsert 自己那一个 key
let mut doc: Value = if path.exists() {
    serde_json::from_str(&fs::read_to_string(&path)?)?
} else {
    serde_json::json!({})
};
let servers = doc["mcpServers"].as_object_mut().ok_or("mcpServers 不是对象")?;
servers.insert("mini-term-ssh".into(), server_entry); // upsert，重复启用不重复
fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
```

---

## Tests Required

- **启用→停用 round-trip**：用户既有的其它 server / 配置段，在一轮启用-停用后**逐字节无损**。
- **重复启用**：连续两次启用，自己的条目只有一份（无重复）。
- **停用幂等**：对未启用的项目停用、或重复停用，不报错、不破坏文件。
- **空文件 / 文件不存在**：正常起步，不 panic。
- **`.mcp.json` 删空**：移除自己条目后若 `mcpServers` 空且无其它顶层 key，删整个文件而非留空壳。

assertion 点可对照 `ssh_mcp_registry.rs` 的单测集。
