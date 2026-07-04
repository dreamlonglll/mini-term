# CONTEXT.md — 领域术语表

本文件只是术语表（ubiquitous language），不写实现细节、不当规格书用。

## 会话（Session）

Claude Code / Codex 在某个项目目录下运行时留下的历史对话记录。每条会话归属于唯一的**会话来源**。

## 会话来源（Session Source）

会话文件所在的文件系统位置，二选一：

- **Windows 宿主**：Windows 用户目录下的 claude/codex 数据目录。
- **WSL 发行版**：某个 WSL 发行版文件系统内、发行版用户 home 下的 claude/codex 数据目录。不同发行版是彼此隔离的独立来源。

同一个项目可以同时在两个来源里有会话（在 Windows 和 WSL 里都跑过 AI CLI）。

## WSL 会话（WSL Session）

会话来源为某 WSL 发行版的会话。与 Windows 会话在列表中合并按时间混排展示，带来源标识。

## WSL 根项目

项目根路径本身是 WSL UNC 路径（`\\wsl$\...` / `\\wsl.localhost\...`）的项目。其 WSL 会话来源由路径**自动推导**（发行版名就在路径里），无需用户配置。

## WSL 关联项目

项目根是普通 Windows 路径，但用户显式声明"我在 WSL 里对这个目录干活"的项目。声明内容是一个发行版名；声明后该项目的会话列表额外加载对应发行版内的 WSL 会话。未声明的 Windows 路径项目只加载 Windows 宿主来源。
