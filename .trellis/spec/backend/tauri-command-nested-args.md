# Tauri command nested struct 参数 invoke 约定

> Rust 端 `#[tauri::command] fn xxx(req: T)` 参数是 struct 时,前端
> `invoke('xxx', { ??? })` 必须传 **`{ req: {...} }`**,而**不是**把字段
> 散开。这是 Tauri + serde 参数传递机制的非显然行为,不遵守会得到
> cryptic 的 `failed to deserialize parameter` 错误。

## Scope / Trigger

新增 Tauri command 时,只要参数含 struct(非 primitive / `String` / `Vec<primitive>` /
`Option<primitive>`),就必须读本 spec。

参考实现:`src-tauri/src/cc_connect.rs::cc_connect_import_project` 的 `req: ImportProjectRequest`
+ 前端 `src/utils/ccConnectActions.ts` 的 invoke 调用。

## Signatures

```rust
// 后端
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProjectRequest {
    pub name: String,
    pub work_dir: String,
    pub agent_type: Option<String>,
}

#[tauri::command]
pub fn cc_connect_import_project(
    req: ImportProjectRequest,
    config_path: Option<String>,
) -> Result<(), String> { ... }
```

## Contracts

### Tauri invoke 参数映射规则

Tauri command 的**每个参数名**会成为 invoke payload 的 **top-level key**;参数类型决定该 key 的 value 形态:

| Rust 参数声明 | 前端 invoke payload |
|---|---|
| `name: String` | `{ name: "foo" }` |
| `config_path: Option<String>` | `{ configPath: "..." }` 或省略 |
| `req: ImportProjectRequest`(struct) | **`{ req: { name, workDir, agentType } }`** ← `req` 是 wrapping key |
| `req: T, config_path: Option<String>` | `{ req: {...}, configPath: "..." }` |

参数名本身遵循 Tauri 自动 **snake_case → camelCase** 转换(`config_path` → `configPath`)。

### Rust struct 字段命名

每个跨边界 struct **必须**显式 `#[serde(rename_all = "camelCase")]`。否则字段在 invoke 时前端
不知道该传 `work_dir` 还是 `workDir`,容易踩 mismatch。mini-term 全 codebase 既有约定是 camelCase。

## Validation & Error Matrix

| 错误现象 | 原因 |
|---|---|
| `failed to deserialize parameter 'req'` | 前端传了 flat `{ name, workDir }` 而不是 `{ req: { name, workDir } }` |
| `missing field 'workDir'` | 前端字段写 `work_dir` 但 struct 已加 `#[serde(rename_all = "camelCase")]` |
| `unknown variant ...` | TS interface 字段拼写不对齐 Rust struct(常见:`ownPid` vs `own_pid`、`workDir` vs `work_dir`) |
| `invalid type: null, expected struct` | Option<T>(struct) 前端没传时正常;非 Option 前端没传则错 |

## Good / Base / Bad Cases

- **Good**:Rust 端用 struct + `#[serde(rename_all = "camelCase")]`,TS interface 字段名严格 camel,invoke 时显式 wrap `{ req: {...} }`,Option<T> 字段允许 omit
- **Base**:参数全部 primitive(`name: String, work_dir: String, agent_type: String`),无 wrapping 烦恼,但参数多了 invoke payload 难读且 IDE 自动补全弱
- **Bad**:Rust struct 用默认 snake_case 字段 + TS interface 也 snake_case → 短期能跑,破坏 mini-term 既有 camelCase 跨边界约定,后续新 command 风格不一致;前端 invoke 时漏 `req:` wrapping → 反序列化静默失败

## Tests Required

新增带 struct 参数的 command 时,**必须**做端到端 invoke 验证:

1. **前端 invoke 后端**:后端 deserialize 出的 struct 字段值正确(可用临时 `println!` 或最简 manual smoke 验证一次,落地后删除)
2. **TS 类型定义**:在 `src/types.ts` 显式定义 `interface XxxRequest`,**严格对齐** Rust serde camelCase 产物;不要在 invoke 里现写 inline type
3. **camelCase 一致性**:跨边界结构(`CcConnectStatus / CcProject / ImportProjectRequest` 等)字段名 review 时必须明确 1:1 对齐 Rust `#[serde(rename_all = "camelCase")]` 产物 ── 重点 `ownPid` `workDir` `agentType` `hasPlatform` 这种容易拼错的

## Wrong vs Correct

### Wrong

```typescript
// ❌ 散开字段,Tauri 在 req 参数找不到 struct
await invoke('cc_connect_import_project', {
  name: 'foo',
  workDir: '/path',
  agentType: 'claudecode',
  configPath: '~/.cc-connect/config.toml',
});
// 报错: failed to deserialize parameter `req`: missing field `name`

// ❌ 漏 camelCase 转换(struct 已加 rename_all)
await invoke('cc_connect_import_project', {
  req: { name: 'foo', work_dir: '/path' },   // 应该 workDir
  config_path: '...',                         // 应该 configPath
});

// ❌ inline type 写 invoke,容易和 Rust 不对齐
await invoke<{ name: string; work_dir: string }>('xxx', { ... });
```

### Correct

```typescript
// types.ts
export interface ImportProjectRequest {
  name: string;
  workDir: string;
  agentType?: string;
}

// 调用处
import type { ImportProjectRequest } from '../types';
await invoke('cc_connect_import_project', {
  req: {
    name: 'foo',
    workDir: '/path',
    agentType: 'claudecode',   // Option<String> 可省
  } satisfies ImportProjectRequest,
  configPath: '~/.cc-connect/config.toml',   // Option<String> 可传 undefined
});
```

## Related

- [cc-connect-integration.md](./cc-connect-integration.md) ── 8 个 cc-connect command 都遵循本约定
- [../frontend/type-safety.md](../frontend/type-safety.md) ── 前端跨边界类型约定
- [../guides/cross-layer-thinking-guide.md](../guides/cross-layer-thinking-guide.md) ── Boundary 思考清单
