# Marzban-node

Lightweight Marzban node service implemented as a single Rust binary.

The node keeps Marzban's REST node protocol and starts one proxy core per pushed config:

- accepts controller REST requests
- starts Xray only when all selected inbounds are Xray-compatible
- starts sing-box when any selected inbound requires sing-box, such as Hysteria2/HY2
- injects and exposes the Xray API only for Xray-selected configs
- forwards core logs over `/logs`

Only one core is started at a time. When sing-box is selected, the node translates supported selected inbounds to sing-box. Current sing-box translations cover Hysteria2/HY2, VLESS, Shadowsocks, and Trojan. Unknown sing-box-selected protocols fail explicitly instead of silently dropping inbounds or starting two cores.

It does not run Docker, Python, FastAPI, Uvicorn, RPyC, a database, a controller, or subscription logic.

## Build

```bash
cargo build --release
sudo install -m 0755 target/release/marzban-node /usr/local/bin/marzban-node
```

For older glibc systems or Alpine, build a static musl binary:

```bash
rustup target add x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --locked --target x86_64-unknown-linux-musl
```

## Configure

Create `/etc/marzban-node.env`:

```env
SERVICE_HOST=0.0.0.0
SERVICE_PORT=62050

XRAY_API_HOST=0.0.0.0
XRAY_API_PORT=62051
XRAY_EXECUTABLE_PATH=/usr/local/bin/xray
XRAY_ASSETS_PATH=/usr/local/share/xray
SING_BOX_EXECUTABLE_PATH=/usr/local/bin/sing-box

SSL_CERT_FILE=/var/lib/marzban-node/ssl_cert.pem
SSL_KEY_FILE=/var/lib/marzban-node/ssl_key.pem
SSL_CLIENT_CERT_FILE=/var/lib/marzban-node/ssl_client_cert.pem

# Optional: only run selected inbound tags from the controller config.
# INBOUNDS=hy2-rn1c1g,Shadowsocks TCP,VLESS TCP REALITY
# INBOUNDS=VLESS TCP REALITY
```

The unused core does not need to exist on disk. A HY2 or HY2-plus-SS/VLESS/Trojan node needs `sing-box` but not `xray`; a pure Xray-compatible node needs `xray` but not `sing-box`.

## Run

```bash
sudo cp marzban.service /etc/systemd/system/marzban-node.service
sudo systemctl daemon-reload
sudo systemctl enable --now marzban-node
```

See [DEPLOYMENT.md](DEPLOYMENT.md) for the production rollout checklist and known pitfalls.
