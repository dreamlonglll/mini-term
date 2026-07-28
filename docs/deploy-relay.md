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

# Generate the desktop access key and write it to .env (required, see below)
echo "MT_RELAY_DESKTOP_KEY=$(openssl rand -hex 32)" > .env

docker compose up -d --build
```

The build runs in three stages: Node builds the PWA → Rust builds the relay → both are copied into a minimal runtime image (running as non-root). Once up, the relay listens on `127.0.0.1:8080` (the compose file binds loopback only by default, leaving the reverse proxy to serve the public).

Verify:

```bash
curl http://127.0.0.1:8080/healthz   # should return ok
docker logs mini-term-relay | grep 'desktop key'   # expect "desktop key configured"
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `MT_RELAY_DESKTOP_KEY` | **none (required)** | Shared desktop access key; without it the relay rejects every desktop connection |
| `RELAY_PORT` | `8080` | Listen port inside the container |
| `RELAY_BIND` | `0.0.0.0` | Listen address inside the container |
| `RELAY_PWA_DIR` | `/srv/pwa` | PWA asset directory (baked into the image; no need to change) |

The public address (domain/port) is not configured on the relay — which address the desktop and phone connect to is determined by the relay URL you enter in the desktop settings.

### Desktop access key (`MT_RELAY_DESKTOP_KEY`)

The relay address is not a secret: the PWA is hosted on it, so the URL already sits in your phone's browser history. The `/ws/desktop` endpoint therefore authenticates with a shared key, and **a relay without a configured key rejects every desktop connection** (fail-closed) — "it starts up" must not be mistaken for "it's configured".

- Generate: `openssl rand -hex 32`, then set `MT_RELAY_DESKTOP_KEY=` in `relay-server/.env`.
- Desktop: mini-term title bar → **Mobile** → "Desktop access key" → enter **the same value** → Save & connect.
- On a wrong key the desktop shows "Access key rejected" and **stops reconnecting** (a configuration problem is not a network problem, so retrying is pointless); when the relay has no key configured it shows "The relay has no MT_RELAY_DESKTOP_KEY set". The two messages are distinct so you can tell at a glance which end to fix.
- Relay logs record the fact of an auth failure only — **never the key itself**.

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

1. Desktop mini-term → "Mobile" in the title bar: set the relay server address to `wss://relay.example.com`, enter the same access key as the relay's `MT_RELAY_DESKTOP_KEY`, save and connect, and wait for the status to turn "Connected".
2. In the same panel → generate the pairing QR code.
3. Scan it with your phone camera → the PWA opens in the browser, pairs automatically, and shows the list of active AI sessions.
4. Use the browser menu's "Add to Home Screen" so it opens as a standalone window from then on (on iOS, adding to the home screen is required for the standalone window experience).
5. Start Claude / Codex in any desktop terminal → it appears in the phone's list in real time → tap in to watch the conversation mirror → send a command from the input box at the bottom, and it is written verbatim into the desktop terminal.
6. Tap **+** at the bottom right of the phone's home screen → pick a project → pick an AI launcher → the desktop opens a new tab in that project and brings the agent up; once the session is live the phone enters its conversation mirror automatically. Projects are listed with the desktop's group hierarchy and can be collapsed; launchers are managed in the desktop "Mobile" panel (Claude / Codex are preset), and commands and shells never leave the desktop.
7. Tap the ✎ on a session row, or the title at the top of the mirror page, to give the session a name you will recognise — the desktop terminal tab shows it too (leave it empty to restore the default).

## 5. Upgrades and Operations

```bash
cd mini-term && git pull
cd relay-server && docker compose up -d --build
```

### Upgrading from v0.7.0 to v0.7.1 (protocol v1 → v2)

Protocol v2 adds desktop authentication. **All three steps must be completed**, or the desktop simply cannot connect — that is the deliberate price of fail-closed:

1. Rebuild and redeploy the relay (`docker compose up -d --build`).
2. Configure `MT_RELAY_DESKTOP_KEY` on the relay (`relay-server/.env`, see above).
3. Upgrade the desktop app to v0.7.1 or later and enter the same key value in the "Mobile" panel, then save and connect.

The order does not matter (an old desktop against a new relay gets a version-mismatch prompt, and vice versa); being disconnected mid-upgrade is expected and resolves once all three steps are done. The PWA is served by the relay and picks up the new version on its own, so it never needs a manual update; recreating the container does clear the pairing state, though, so the phone has to scan once more (see the notes below).

Things to keep in mind:

- Restarting the relay (including recreating the container on upgrade) loses pairing state, so the phone must scan again.
- The protocol is versioned: if the desktop and relay versions do not match, the handshake is rejected explicitly with an upgrade prompt rather than failing silently.
- 1×1 topology: only one desktop and one phone are active at a time; pairing a new device supersedes the old one.
- Lost phone: desktop "Mobile" panel → reset pairing, and every mobile credential is invalidated immediately.
- Log spot-check: `docker logs mini-term-relay` should show only connection / auth / pairing metadata, never any conversation content.
