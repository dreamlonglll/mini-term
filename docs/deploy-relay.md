# Relay Server Deployment Guide (Self-Hosted)

<strong>English</strong> · <a href="deploy-relay.zh-CN.md">简体中文</a>

The mini-term mobile stack requires a relay server of your own: the desktop app dials out to it (punching through NAT), and the PWA on your phone connects to it too — the relay only forwards messages. This guide targets the "solo developer with one VPS" case, covering one-command Docker deployment plus a typical reverse proxy + TLS setup.

## Architecture at a Glance

```
mini-term desktop ──(outbound wss)──▶ ┌──────────────┐ ◀──(wss/https)── phone PWA
                                      │ relay server │
                                      │   (Docker)   │  also serves the PWA assets
                                      └──────────────┘
```

- TLS end to end (wss/https), with certificates terminated by the front reverse proxy.
- Relay discipline: message bodies are forwarded in memory only and **never written to disk**; logs record connection and auth metadata only, never conversation content; the container mounts no volumes.
- Pairing state (the one-time pairing code and the mobile long-lived credential) is also in memory only — **after a relay restart you must generate a fresh QR code on the desktop and pair again**.

## 1. Prerequisites

- A publicly reachable server (1 vCPU / 1 GB RAM is plenty) with Docker and the Docker Compose plugin installed.
- A domain name resolving to that server (e.g. `relay.example.com`). Certificates come from Caddy's automatic issuance or Nginx + certbot.

## 2. One-Command Start

```bash
git clone https://github.com/dreamlonglll/mini-term.git
cd mini-term/relay-server
docker compose up -d --build
```

The build runs in three stages: Node builds the PWA → Rust builds the relay → both are copied into a minimal runtime image (running as non-root). Once up, the relay listens on `127.0.0.1:8080` (the compose file binds loopback only by default, leaving the reverse proxy to serve the public).

Verify:

```bash
curl http://127.0.0.1:8080/healthz   # should return ok
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RELAY_PORT` | `8080` | Listen port inside the container |
| `RELAY_BIND` | `0.0.0.0` | Listen address inside the container |
| `RELAY_PWA_DIR` | `/srv/pwa` | PWA asset directory (baked into the image; no need to change) |

The public address (domain/port) is not configured on the relay — which address the desktop and phone connect to is determined by the relay URL you enter in the desktop settings.

## 3. Reverse Proxy + TLS

All three kinds of relay traffic share one port: `/ws/desktop` and `/ws/mobile` (WebSocket) plus the PWA static assets (HTTP). The reverse proxy must allow WebSocket upgrades.

### Caddy (recommended — automatic HTTPS)

`/etc/caddy/Caddyfile`:

```caddy
relay.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Caddy passes WebSocket upgrades through by default with no extra configuration, and issues and renews certificates automatically.

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
        # Keep long-lived connections alive (the 60s default drops idle WebSockets)
        proxy_read_timeout 7d;
        proxy_send_timeout 7d;
    }
}
```

## 4. Walking Through the Full Flow

1. Desktop mini-term → "Mobile" in the title bar: set the relay server address to `wss://relay.example.com`, save and connect, and wait for the status to turn "Connected".
2. In the same panel → generate the pairing QR code.
3. Scan it with your phone camera → the PWA opens in the browser, pairs automatically, and shows the list of active AI sessions.
4. Use the browser menu's "Add to Home Screen" so it opens as a standalone window from then on (on iOS, adding to the home screen is required for the standalone window experience).
5. Start Claude / Codex in any desktop terminal → it appears in the phone's list in real time → tap in to watch the conversation mirror → send a command from the input box at the bottom, and it is written verbatim into the desktop terminal.

## 5. Upgrades and Operations

```bash
cd mini-term && git pull
cd relay-server && docker compose up -d --build
```

Things to keep in mind:

- Restarting the relay (including recreating the container on upgrade) loses pairing state, so the phone must scan again.
- The protocol is versioned: if the desktop and relay versions do not match, the handshake is rejected explicitly with an upgrade prompt rather than failing silently.
- 1×1 topology: only one desktop and one phone are active at a time; pairing a new device supersedes the old one.
- Lost phone: desktop "Mobile" panel → reset pairing, and every mobile credential is invalidated immediately.
- Log spot-check: `docker logs mini-term-relay` should show only connection / auth / pairing metadata, never any conversation content.
