# CONTEXT.md — 领域术语表

本文件只是术语表（ubiquitous language），不写实现细节、不当规格书用。

## 会话（Session）

Claude Code / Codex 在某个项目目录下运行时留下的历史对话记录。每条会话归属于唯一的**会话来源**。

## 会话来源（Session Source）

会话文件所在的文件系统位置，三选一：

- **Windows 宿主**：Windows 用户目录下的 claude/codex 数据目录。
- **WSL 发行版**：某个 WSL 发行版文件系统内、发行版用户 home 下的 claude/codex 数据目录。不同发行版是彼此隔离的独立来源。
- **SSH 远程机器**：某条 SSH 连接指向的远程机器上、远程用户 home 下的 claude/codex 数据目录。不同连接是彼此隔离的独立来源。

同一个项目可以同时在两个来源里有会话（在 Windows 和 WSL 里都跑过 AI CLI）。

## WSL 会话（WSL Session）

会话来源为某 WSL 发行版的会话。与 Windows 会话在列表中合并按时间混排展示，带来源标识。

## WSL 根项目

项目根路径本身是 WSL UNC 路径（`\\wsl$\...` / `\\wsl.localhost\...`）的项目。其 WSL 会话来源由路径**自动推导**（发行版名就在路径里），无需用户配置。

## WSL 关联项目

项目根是普通 Windows 路径，但用户显式声明"我在 WSL 里对这个目录干活"的项目。声明内容是一个发行版名；声明后该项目的会话列表额外加载对应发行版内的 WSL 会话。未声明的 Windows 路径项目只加载 Windows 宿主来源。

## SSH 远程项目（Remote Project）

项目根位于 SSH 远程机器上的项目：引用一条已保存的 SSH 连接 + 一个远程 POSIX 绝对路径。文件树、终端、会话全部走远程链路；Git 状态、文件监听等本地能力对其不可用。与 WSL 根项目同为"项目根不在 Windows 本地"的形态，但访问通道是 SSH/SFTP 而非 UNC。

## 断链（Broken Link）

SSH 远程项目引用的连接已被删除的状态。项目在列表中仍可见、可删除，但文件树/终端/会话等功能入口给出明确错误提示。
