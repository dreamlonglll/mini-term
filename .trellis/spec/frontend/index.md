# Frontend Development Guidelines

> Best practices for frontend development in this project.

---

## Overview

This directory contains guidelines for frontend development. Fill in each file with your project's specific conventions.

---

## Guidelines Index

| Guide | Description | Status |
|-------|-------------|--------|
| [Directory Structure](./directory-structure.md) | Module organization and file layout | To fill |
| [Component Guidelines](./component-guidelines.md) | Component patterns, props, composition | To fill |
| [Hook Guidelines](./hook-guidelines.md) | Custom hooks, data fetching patterns | To fill |
| [State Management](./state-management.md) | Local state, global state, server state | Partial |
| [Quality Guidelines](./quality-guidelines.md) | Code standards, forbidden patterns | To fill |
| [Type Safety](./type-safety.md) | Type patterns, validation | To fill |
| [xterm Ligatures + WebGL Order](./xterm-ligatures-with-webgl-order.md) | LigaturesAddon 必须先于 WebglAddon、热切换同步无 await、平台差异 | Filled |
| [xterm.js WebGL TextureAtlas 跨实例共享](./xterm-webgl-atlas-sharing.md) | atlas page merge 后必须广播 `term.refresh` 唤醒 dormant 终端;否则多 AI 并发出现同形乱码 | Active |
| [Fluent 2 + backdrop-filter Modal/iframe Portal Convention](./fluent2-portal-modal.md) | `[data-panel]` 的 `backdrop-filter` 形成 containing block,导致 `position: fixed` 全屏 modal/iframe 被拽进面板边界;统一走 `createPortal(node, document.body)` 规避(沿用 commit `e7316e5` 七 modal 修复范式) | Active |

---

## How to Fill These Guidelines

For each guideline file:

1. Document your project's **actual conventions** (not ideals)
2. Include **code examples** from your codebase
3. List **forbidden patterns** and why
4. Add **common mistakes** your team has made

The goal is to help AI assistants and new team members understand how YOUR project works.

---

**Language**: All documentation should be written in **English**.
