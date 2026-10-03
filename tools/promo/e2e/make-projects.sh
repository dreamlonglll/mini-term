#!/usr/bin/env bash
# 生成宣传片里出镜的演示项目(全是虚构的小样例)。用法:make-projects.sh <demo_home> <repo_root>
# - mini-term:本仓库的本地克隆(真实提交历史,给文件树 / Git 面板用)
# - 其余五个:不同技术栈的小项目,各带几条提交与未提交改动(给 Git 变更面板与技术栈图标用)
set -euo pipefail
H=$1; REPO=$2; C=$H/code
mkdir -p "$C"

commit() { # commit <dir> <date> <author> <msg>
  (cd "$1" && git add -A && GIT_AUTHOR_DATE="$2" GIT_COMMITTER_DATE="$2" \
    git -c user.name="$3" -c user.email="$(echo "$3" | tr 'A-Z ' 'a-z.')@example.dev" commit -q -m "$4")
}
merge() { # merge <dir> <date> <author> <branch>
  (cd "$1" && GIT_AUTHOR_DATE="$2" GIT_COMMITTER_DATE="$2" \
    git -c user.name="$3" -c user.email="$(echo "$3" | tr 'A-Z ' 'a-z.')@example.dev" merge -q --no-ff "$4" -m "merge: $4")
}
fresh() { rm -rf "$1"; mkdir -p "$1"; git -C "$1" init -q -b main; }

# ---------------------------------------------------------------- mini-term
[ -d "$C/mini-term/.git" ] || git clone -q "$REPO" "$C/mini-term"

# ---------------------------------------------------------------- orbit-api(主场景)
P=$C/orbit-api; fresh "$P"; mkdir -p "$P/src/routes" "$P/migrations" "$P/k8s"
cat > "$P/Cargo.toml" <<'EOF'
[package]
name = "orbit-api"
version = "0.9.2"
edition = "2024"

[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
sqlx = { version = "0.8", features = ["postgres", "runtime-tokio"] }
tracing = "0.1"
EOF
cat > "$P/src/main.rs" <<'EOF'
mod routes;

use axum::Router;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let app = Router::new()
        .nest("/api/orders", routes::orders::router())
        .nest("/api/health", routes::health::router());
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!("orbit-api listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}
EOF
printf 'pub mod health;\npub mod orders;\n' > "$P/src/routes/mod.rs"
cat > "$P/src/routes/health.rs" <<'EOF'
use axum::{routing::get, Router};

pub fn router() -> Router {
    Router::new().route("/", get(|| async { "ok" }))
}
EOF
cat > "$P/src/routes/orders.rs" <<'EOF'
use axum::{extract::Path, routing::get, Json, Router};
use serde::Serialize;

#[derive(Serialize)]
pub struct Order { id: u64, status: &'static str, total_cents: i64 }

pub fn router() -> Router {
    Router::new().route("/{id}", get(get_order))
}

async fn get_order(Path(id): Path<u64>) -> Json<Order> {
    Json(Order { id, status: "delivered", total_cents: 12_900 })
}
EOF
printf '# Orbit API\n\nOrder service written in Rust (axum + sqlx).\n' > "$P/README.md"
printf 'target\n' > "$P/.gitignore"
commit "$P" "2026-08-28T11:00:00+08:00" "Zhou Ming" "init: axum skeleton"
echo "CREATE TABLE orders (id BIGSERIAL PRIMARY KEY, status TEXT NOT NULL, total_cents BIGINT NOT NULL);" > "$P/migrations/0001_orders.sql"
commit "$P" "2026-09-02T15:20:00+08:00" "Zhou Ming" "feat: order model and sqlx migrations"
git -C "$P" checkout -q -b feat/rate-limit
printf '//! token bucket rate limiter\npub struct Limiter { capacity: u32, refill_per_sec: u32 }\n' > "$P/src/limit.rs"
commit "$P" "2026-09-10T10:05:00+08:00" "Chen Yu" "feat: token bucket rate limiter"
printf '\n#[cfg(test)]\nmod tests {}\n' >> "$P/src/limit.rs"
commit "$P" "2026-09-12T16:40:00+08:00" "Chen Yu" "test: limiter edge cases"
git -C "$P" checkout -q main
sed -i 's/"ok"/"ok (v1)"/' "$P/src/routes/health.rs"
commit "$P" "2026-09-11T09:30:00+08:00" "Zhou Ming" "fix: health check timeout"
merge "$P" "2026-09-15T17:22:00+08:00" "Zhou Ming" feat/rate-limit
git -C "$P" checkout -q -b feat/order-events
printf 'apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: orbit-api\nspec:\n  replicas: 3\n' > "$P/k8s/deployment.yaml"
commit "$P" "2026-09-18T14:10:00+08:00" "Lin Xia" "feat: publish order events to queue"
git -C "$P" checkout -q main
merge "$P" "2026-09-22T11:45:00+08:00" "Zhou Ming" feat/order-events
cat > "$P/ARCHITECTURE.md" <<'EOF'
# Orbit 架构总览

> 订单服务的请求链路、数据流与部署拓扑。改动核心链路前请先读这一页。

## 请求链路

```mermaid
flowchart LR
    U[Web / Mobile] --> G[API Gateway]
    G --> A[orbit-api]
    A --> C[(Redis 缓存)]
    A --> D[(PostgreSQL)]
    A --> Q[[订单事件队列]]
    Q --> W[结算 Worker]
    W --> D
```

## 服务清单

| 服务 | 语言 | 端口 | 负责人 |
|---|---|---|---|
| orbit-api | Rust · axum | 8080 | @zhouming |
| aurora-web | TypeScript · React | 5173 | @linxia |
| nebula | Go | 9090 | @liufang |

## 本地启动

```bash
docker compose up -d db
cargo run -p orbit-api
```

- [x] 订单查询接口
- [x] 请求限流
- [ ] 结算对账任务
EOF
commit "$P" "2026-09-26T16:00:00+08:00" "Zhou Ming" "docs: architecture overview"
sed -i 's/axum = "0.8"/axum = "0.8.4"/' "$P/Cargo.toml"
commit "$P" "2026-09-29T10:15:00+08:00" "Chen Yu" "chore: bump axum to 0.8.4"
# 未提交改动:一份已暂存、两份未暂存、一份未跟踪
printf '\npub const DEFAULT_BURST: u32 = 20;\n' >> "$P/src/limit.rs"; git -C "$P" add src/limit.rs
sed -i 's/"delivered"/"shipped"/' "$P/src/routes/orders.rs"
sed -i 's/0.0.0.0:8080/0.0.0.0:8081/' "$P/src/main.rs"
printf 'pub async fn find_order_with_items() {}\n' > "$P/src/repo.rs"

# ---------------------------------------------------------------- aurora-web(React + Vite)
P=$C/aurora-web; fresh "$P"; mkdir -p "$P/src/components" "$P/src/hooks" "$P/public"
cat > "$P/package.json" <<'EOF'
{
  "name": "aurora-web",
  "version": "2.4.0",
  "private": true,
  "type": "module",
  "scripts": { "dev": "vite", "build": "tsc -b && vite build", "test": "vitest run" },
  "dependencies": { "react": "^19.1.0", "react-dom": "^19.1.0", "@tanstack/react-query": "^5.80.0" },
  "devDependencies": { "@vitejs/plugin-react": "^5.0.0", "typescript": "^5.8.3", "vite": "^7.0.0", "vitest": "^3.2.0" }
}
EOF
printf "import { defineConfig } from 'vite';\nimport react from '@vitejs/plugin-react';\n\nexport default defineConfig({ plugins: [react()] });\n" > "$P/vite.config.ts"
printf '{ "compilerOptions": { "target": "ES2022", "jsx": "react-jsx", "strict": true } }\n' > "$P/tsconfig.json"
printf '<!doctype html>\n<html><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>\n' > "$P/index.html"
printf "import { createRoot } from 'react-dom/client';\nimport { App } from './App';\n\ncreateRoot(document.getElementById('root')!).render(<App />);\n" > "$P/src/main.tsx"
printf "import { Dashboard } from './components/Dashboard';\nimport { useMetrics } from './hooks/useMetrics';\n\nexport function App() {\n  const { data } = useMetrics();\n  return <Dashboard metrics={data ?? []} />;\n}\n" > "$P/src/App.tsx"
printf "export interface Metric { name: string; value: number; delta: number }\n\nexport function Dashboard({ metrics }: { metrics: Metric[] }) {\n  return <section>{metrics.map((m) => <article key={m.name}>{m.name}</article>)}</section>;\n}\n" > "$P/src/components/Dashboard.tsx"
printf "import { useQuery } from '@tanstack/react-query';\n\nexport function useMetrics() {\n  return useQuery({\n    queryKey: ['metrics'],\n    queryFn: () => fetch('/api/metrics').then((r) => r.json()),\n    refetchInterval: 15_000,\n  });\n}\n" > "$P/src/hooks/useMetrics.ts"
printf '# Aurora Web\n\nRealtime analytics dashboard built with React 19 + Vite.\n' > "$P/README.md"
printf 'node_modules\ndist\n' > "$P/.gitignore"
commit "$P" "2026-09-02T10:12:00+08:00" "Lin Xia" "chore: scaffold vite + react app"
echo "export const API_BASE = '/api';" > "$P/src/config.ts"; commit "$P" "2026-09-11T15:40:00+08:00" "Chen Yu" "feat: add metrics dashboard"
echo "/* theme tokens */" > "$P/src/theme.css"; commit "$P" "2026-09-24T09:05:00+08:00" "Lin Xia" "style: dark theme tokens"
sed -i 's/refetchInterval: 15_000/refetchInterval: 5_000/' "$P/src/hooks/useMetrics.ts"
echo "export const FEATURE_FLAGS = { liveChart: true };" > "$P/src/flags.ts"

# ---------------------------------------------------------------- pulse-ml(Python)
P=$C/pulse-ml; fresh "$P"; mkdir -p "$P/src/pulse" "$P/tests"
printf '[project]\nname = "pulse-ml"\nversion = "0.3.0"\nrequires-python = ">=3.11"\ndependencies = ["torch>=2.4", "polars>=1.0"]\n' > "$P/pyproject.toml"
printf '"""Pulse — anomaly detection for time-series metrics."""\n__version__ = "0.3.0"\n' > "$P/src/pulse/__init__.py"
printf 'import torch\nfrom torch import nn\n\n\nclass PulseNet(nn.Module):\n    def __init__(self, features: int = 32, hidden: int = 128) -> None:\n        super().__init__()\n        self.encoder = nn.LSTM(features, hidden, batch_first=True)\n        self.head = nn.Linear(hidden, 1)\n' > "$P/src/pulse/model.py"
printf 'def test_smoke():\n    assert True\n' > "$P/tests/test_model.py"
printf '# Pulse ML\n\nTime-series anomaly detection.\n' > "$P/README.md"
commit "$P" "2026-09-05T20:10:00+08:00" "Wang Lei" "feat: LSTM baseline"

# ---------------------------------------------------------------- nebula-go(Go)
P=$C/nebula-go; fresh "$P"; mkdir -p "$P/cmd/nebula" "$P/internal/store"
printf 'module github.com/example/nebula\n\ngo 1.24\n' > "$P/go.mod"
printf 'package main\n\nimport (\n\t"log"\n\t"net/http"\n)\n\nfunc main() {\n\tlog.Println("nebula listening on :9090")\n\tlog.Fatal(http.ListenAndServe(":9090", nil))\n}\n' > "$P/cmd/nebula/main.go"
printf 'package store\n' > "$P/internal/store/store.go"
printf '# Nebula\n\nA tiny key-value gateway in Go.\n' > "$P/README.md"
commit "$P" "2026-09-18T13:30:00+08:00" "Liu Fang" "init"

# ---------------------------------------------------------------- infra-k8s(Docker / K8s)
P=$C/infra-k8s; fresh "$P"; mkdir -p "$P/k8s" "$P/terraform"
printf 'FROM rust:1.98-slim AS build\nWORKDIR /app\nCOPY . .\nRUN cargo build --release\n\nFROM debian:bookworm-slim\nCOPY --from=build /app/target/release/orbit-api /usr/local/bin/\nCMD ["orbit-api"]\n' > "$P/Dockerfile"
printf 'services:\n  api:\n    build: .\n    ports: ["8080:8080"]\n  db:\n    image: postgres:17\n' > "$P/docker-compose.yml"
printf 'apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: orbit-api\nspec:\n  replicas: 3\n' > "$P/k8s/deployment.yaml"
printf 'terraform { required_version = ">= 1.9" }\n' > "$P/terraform/main.tf"
printf '# Infra\n\nDocker / Kubernetes / Terraform for the orbit stack.\n' > "$P/README.md"
commit "$P" "2026-09-20T10:00:00+08:00" "Zhou Ming" "infra: compose + k8s manifests"

echo "projects ready under $C"
