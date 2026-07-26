# 中转服务器部署指南（自托管）

<a href="deploy-relay.md">English</a> · <strong>简体中文</strong>

mini-term 移动端体系需要一台你自己的中转服务器（Relay Server）：桌面端主动出站连接它（穿 NAT），手机上的 PWA 也连接它，中转只做消息转发。本文面向「有一台 VPS 的独立开发者」，覆盖 Docker 一键部署与反代 + TLS 的典型配置。

## 架构一览

```
mini-term 桌面端 ──(wss 出站长连)──▶ ┌──────────────┐ ◀──(wss/https)── 手机 PWA
                                     │  中转服务器   │
                                     │  (Docker)    │  同时托管 PWA 静态资源
                                     └──────────────┘
```

- 全链路 TLS（wss/https），由前置反代终结证书。
- 中转纪律：消息体仅内存转发、**不落盘**；日志只记连接与鉴权元数据、不含对话内容；容器不挂任何数据卷。
- 配对状态（一次性配对码、移动端长期凭证）也仅存内存——**中转重启后需要在桌面端重新生成二维码扫码配对**。

## 一、前置要求

- 一台可公网访问的服务器（1C1G 足够），已装 Docker 与 Docker Compose 插件。
- 一个解析到该服务器的域名（例如 `relay.example.com`）。TLS 证书由 Caddy 自动签或 Nginx + certbot。

## 二、一键启动

```bash
git clone https://github.com/dreamlonglll/mini-term.git
cd mini-term/relay-server

# 生成桌面端接入密钥并写入 .env（必做，见下文）
echo "MT_RELAY_DESKTOP_KEY=$(openssl rand -hex 32)" > .env

docker compose up -d --build
```

构建分三阶段：Node 构建 PWA → Rust 构建中转 → 拷入最小运行时镜像（非 root 运行）。完成后中转监听在 `127.0.0.1:8080`（compose 默认只绑回环，由反代对外服务）。

验证：

```bash
curl http://127.0.0.1:8080/healthz   # 应返回 ok
docker logs mini-term-relay | grep 'desktop key'   # 应出现 "desktop key configured"
```

### 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `MT_RELAY_DESKTOP_KEY` | **无（必配）** | 桌面端接入共享密钥；未设置时拒绝一切桌面端连接 |
| `RELAY_PORT` | `8080` | 容器内监听端口 |
| `RELAY_BIND` | `0.0.0.0` | 容器内监听地址 |
| `RELAY_PWA_DIR` | `/srv/pwa` | PWA 静态资源目录（镜像已内置，无需修改） |

对外地址（域名/端口）不需要配置进中转——桌面端与手机连接哪个地址由你在桌面端设置里填写的中转地址决定。

### 桌面端接入密钥（`MT_RELAY_DESKTOP_KEY`）

中转地址不是秘密：PWA 就托管在它上面，地址早已躺在手机浏览器历史里。因此 `/ws/desktop` 端点用一个共享密钥鉴权，**未配置密钥的中转会拒绝一切桌面端连接**（fail-closed）——"能跑起来"不等于"配好了"，宁可让你立刻发现。

- 生成：`openssl rand -hex 32`，写入 `relay-server/.env` 的 `MT_RELAY_DESKTOP_KEY=`。
- 桌面端：mini-term 顶栏「移动端」→「桌面端接入密钥」填入**同一个值** → 保存并连接。
- 填错时桌面端状态显示「接入密钥不正确」并**停止重连**（配置问题不是网络问题，重试无意义）；中转未配置时显示「中转未配置 MT_RELAY_DESKTOP_KEY」。两种文案区分开，一眼看出该改哪一头。
- 中转日志只记鉴权失败这件事，**不记录密钥本身**。

## 三、反代 + TLS

中转的三类流量都走同一端口：`/ws/desktop`、`/ws/mobile`（WebSocket）与 PWA 静态资源（HTTP）。反代需开启 WebSocket 升级。

### Caddy（推荐，自动 HTTPS）

`/etc/caddy/Caddyfile`：

```caddy
relay.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Caddy 默认透传 WebSocket 升级，无需额外配置，证书自动签发续期。

### Nginx

```nginx
server {
    listen 443 ssl;
    server_name relay.example.com;

    ssl_certificate     /etc/letsencrypt/live/relay.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/relay.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        # 长连不掐断(默认 60s 会断开空闲 WebSocket)
        proxy_read_timeout 7d;
        proxy_send_timeout 7d;
    }
}
```

## 四、走通全流程

1. 桌面端 mini-term → 顶栏「移动端」：中转服务器地址填 `wss://relay.example.com`，接入密钥填与中转一致的 `MT_RELAY_DESKTOP_KEY`，保存并连接，状态变为「已连接」。
2. 同一面板内 → 生成配对二维码。
3. 手机相机扫码 → 浏览器打开 PWA 自动完成配对，显示活跃 AI 会话列表。
4. 手机浏览器菜单「添加到主屏幕」，此后以独立窗口打开（iOS 必须添加到主屏幕才有独立窗口体验）。
5. 桌面端任一终端里启动 Claude / Codex → 手机列表实时出现 → 点进查看对话镜像 → 底部输入框发送指令，桌面终端原样写入。
6. 手机首页右下角 **+** → 选项目 → 选 AI 启动器 → 桌面端在该项目新开一个 tab 并把 agent 拉起来，会话起来后手机自动进入它的对话镜像。项目按桌面端的分组层级展示、可折叠；启动器在桌面端「移动端」面板里维护（默认预置 Claude / Codex），命令与 shell 只保存在桌面端。
7. 会话列表行的 ✎ 或镜像页顶部标题 → 给会话改个看得懂的名字，桌面端的终端标签同步显示（留空恢复默认名）。

## 五、升级与运维

```bash
cd mini-term && git pull
cd relay-server && docker compose up -d --build
```

### 从 v0.7.0 升级到 v0.7.1（协议 v1 → v2）

协议 v2 引入了桌面端鉴权，**必须三件事同时做完**，缺一桌面端就连不上——这是 fail-closed 的有意代价：

1. 重新构建并部署中转（`docker compose up -d --build`）。
2. 在中转上配置 `MT_RELAY_DESKTOP_KEY`（`relay-server/.env`，见上文）。
3. 桌面端升级到 v0.7.1 或更高版本，并在「移动端」面板填入同一个密钥值，保存并连接。

升级顺序不敏感（旧桌面端连新中转会收到版本不匹配提示，反之亦然），但中途连不上属正常现象，三步做完即恢复。PWA 由中转托管，会自动跟上新版本、不必手动更新；但重建容器会清空配对状态，手机仍需重新扫码一次（见下方注意事项）。

注意事项：

- 中转重启（含升级重建容器）会丢失配对状态，手机需重新扫码。
- 协议带版本号：桌面端与中转版本不匹配时握手明确拒绝并提示升级，不会静默错乱。
- 1×1 拓扑：同一时刻只有一台桌面端、一部手机有效；新设备扫码配对会顶替旧设备。
- 手机丢失：桌面端「移动端」面板 → 重置配对，所有移动端凭证立即失效。
- 日志抽查：`docker logs mini-term-relay` 只应出现连接/鉴权/配对元数据，不含任何对话内容。
