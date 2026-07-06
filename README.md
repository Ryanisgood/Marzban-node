# MarzbanX-node

Rust node runtime for [MarzbanX](../MarzbanX), forked from the original Marzban node work and kept compatible with the Marzban REST node protocol.

MarzbanX-node is intentionally small: it is a single Rust binary that accepts controller-pushed configs, chooses one proxy core, starts it, reports runtime diagnostics, and streams core logs back to the controller.

## Links

- Controller fork: [Ryanisgood/MarzbanX](https://github.com/Ryanisgood/MarzbanX)
- Node fork: [Ryanisgood/MarzbanX-node](https://github.com/Ryanisgood/MarzbanX-node)
- Original Marzban upstream: [Gozargah/Marzban](https://github.com/Gozargah/Marzban)

## What This Node Does

- Accepts controller REST requests from MarzbanX.
- Supports controller-managed inbound selection through active inbound tags.
- Starts Xray only when all selected inbounds are Xray-compatible.
- Starts sing-box when any selected inbound requires sing-box, such as HY2/Hysteria2 or AnyTLS.
- Injects and exposes Xray API only for Xray-selected configs.
- Translates supported sing-box-selected inbounds to sing-box.
- Reports node diagnostics: node version, installed cores, current core, memory, local sockets, configured inbound ports, and last core restart time.
- Streams core logs over `/logs`.

Only one core is started at a time. Unknown sing-box-selected protocols fail explicitly instead of silently dropping inbounds or starting both cores.

This node does not run Docker, Python, FastAPI, Uvicorn, RPyC, a database, a controller, a dashboard, or subscription logic.

## Supported Core Selection

MarzbanX-node chooses the runtime core from the selected inbounds:

| Selection | Core |
| --- | --- |
| HY2/Hysteria2 | `sing-box` |
| AnyTLS | `sing-box` |
| HY2/AnyTLS mixed with supported sing-box translations | `sing-box` |
| Xray-compatible selections only | `xray` |

The current sing-box translation layer covers the protocols implemented in this repository. Keep the controller wizard and node translator in sync before exposing additional protocols in the panel.

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

The binary name remains `marzban-node` for controller and service compatibility. The repository and project name are MarzbanX-node.

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
```

For panel-managed nodes created by MarzbanX provisioning, do not set `INBOUNDS`. The controller sends active inbound tags to the node.

For legacy/manual nodes, `INBOUNDS` is still available:

```env
INBOUNDS=hy2-rn1c1g,Shadowsocks TCP,VLESS TCP REALITY
```

The unused core does not need to exist on disk. A HY2 or AnyTLS node needs `sing-box`; a pure Xray-compatible node needs `xray`.

## Run

```bash
sudo cp marzban.service /etc/systemd/system/marzban-node.service
sudo systemctl daemon-reload
sudo systemctl enable --now marzban-node
```

When installed through MarzbanX Add Node, the controller-generated installer writes the env file, installs the service, starts the node, and finalizes the one-time provisioning token after successful startup.

## Diagnostics

The controller reads node state and can display:

- node version;
- installed Xray and sing-box versions;
- current core kind;
- Xray API availability;
- agent/core RSS memory;
- local listening sockets;
- configured inbound ports;
- last core restart time;
- feature flags such as `controller_inbounds`, `core_kind`, and `node_diagnostics`.

These fields are used by MarzbanX before protocol switching, especially to reject a sing-box-only selection when sing-box is not installed.

## Development Checks

```bash
cargo test
cargo build --release --locked
```

## Attribution

MarzbanX-node is part of the MarzbanX fork ecosystem and remains protocol-compatible with the original Marzban node flow where practical.

## License

Use the license inherited by this repository.
