# 中转服务器部署指南（自托管）

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
docker compose up -d --build
```

构建分三阶段：Node 构建 PWA → Rust 构建中转 → 拷入最小运行时镜像（非 root 运行）。完成后中转监听在 `127.0.0.1:8080`（compose 默认只绑回环，由反代对外服务）。

验证：

```bash
curl http://127.0.0.1:8080/healthz   # 应返回 ok
```

### 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `RELAY_PORT` | `8080` | 容器内监听端口 |
| `RELAY_BIND` | `0.0.0.0` | 容器内监听地址 |
| `RELAY_PWA_DIR` | `/srv/pwa` | PWA 静态资源目录（镜像已内置，无需修改） |

对外地址（域名/端口）不需要配置进中转——桌面端与手机连接哪个地址由你在桌面端设置里填写的中转地址决定。

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

1. 桌面端 mini-term → 设置 → 移动端：中转服务器地址填 `wss://relay.example.com`，保存并连接，状态变为「已连接」。
2. 顶栏「移动端」入口 → 生成配对二维码。
3. 手机相机扫码 → 浏览器打开 PWA 自动完成配对，显示活跃 AI 会话列表。
4. 手机浏览器菜单「添加到主屏幕」，此后以独立窗口打开（iOS 必须添加到主屏幕才有独立窗口体验）。
5. 桌面端任一终端里启动 Claude / Codex → 手机列表实时出现 → 点进查看对话镜像 → 底部输入框发送指令，桌面终端原样写入。

## 五、升级与运维

```bash
cd mini-term && git pull
cd relay-server && docker compose up -d --build
```

注意事项：

- 中转重启（含升级重建容器）会丢失配对状态，手机需重新扫码。
- 协议带版本号：桌面端与中转版本不匹配时握手明确拒绝并提示升级，不会静默错乱。
- 1×1 拓扑：同一时刻只有一台桌面端、一部手机有效；新设备扫码配对会顶替旧设备。
- 手机丢失：桌面端「移动端」面板 → 重置配对，所有移动端凭证立即失效。
- 日志抽查：`docker logs mini-term-relay` 只应出现连接/鉴权/配对元数据，不含任何对话内容。
