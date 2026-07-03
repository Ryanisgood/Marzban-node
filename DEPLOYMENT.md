# Rust Marzban-node Deployment

This repository contains only the Rust node implementation. The old Python, Docker, FastAPI, Uvicorn, and RPyC runtime is intentionally removed.

## What The Rust Node Runs

- `marzban-node`: single Rust binary, REST node API on `SERVICE_PORT`.
- `xray`: launched only when every selected inbound is Xray-compatible.
- `sing-box`: launched when any selected inbound requires sing-box, such as HY2/Hysteria2.

Only one proxy core is started for a pushed config. If the selected inbound set contains HY2, the node chooses sing-box and does not start Xray or expose the Xray gRPC API. The sing-box translator currently supports HY2, VLESS, Shadowsocks, and Trojan, so HY2 can share one sing-box core with those protocols. Unknown sing-box-selected protocols fail explicitly instead of starting two cores.

HY2 must be configured in Marzban as:

```json
{
  "tag": "hy2-rn1c1g",
  "protocol": "hysteria",
  "settings": {
    "version": 2,
    "users": []
  },
  "streamSettings": {
    "network": "hysteria"
  }
}
```

The node converts this HY2 inbound to a sing-box `hysteria2` inbound and runs sing-box as the only proxy core for that config. If the same selected config also contains VLESS, Shadowsocks, or Trojan inbounds, those are converted to sing-box inbounds too.

## Ports

Minimum public ports:

- REST management: `SERVICE_PORT/tcp`, default `62050`, should be restricted to the controller IP when possible.
- SS node traffic: the configured Shadowsocks inbound port, commonly `443/tcp`.
- HY2 node traffic: the configured HY2 port, commonly `8443/udp`.

`XRAY_API_PORT` defaults to `62051/tcp`. It is used only when the node selected Xray for the current config. HY2/sing-box configs do not listen on this port.

## Build

Normal Linux build:

```bash
cargo build --release --locked
sudo install -m 0755 target/release/marzban-node /usr/local/bin/marzban-node
```

Portable static build for older glibc systems and Alpine:

```bash
sudo apt-get install -y musl-tools pkg-config build-essential
rustup target add x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --locked --target x86_64-unknown-linux-musl
sudo install -m 0755 target/x86_64-unknown-linux-musl/release/marzban-node /usr/local/bin/marzban-node
```

Use the musl build when copying a binary between servers. A binary built on a newer Debian can fail on an older Debian with errors like:

```text
/usr/local/bin/marzban-node: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.39' not found
```

## Required Files

Always install:

```text
/usr/local/bin/marzban-node
```

Install only the core required by this node's selected inbounds:

```text
# Xray-selected nodes
/usr/local/bin/xray
/usr/local/share/xray/geoip.dat
/usr/local/share/xray/geosite.dat

# HY2 or HY2-plus-VLESS/SS/Trojan sing-box-selected nodes
/usr/local/bin/sing-box
```

Install node TLS files:

```text
/var/lib/marzban-node/cert.pem
/var/lib/marzban-node/ssl_cert.pem
/var/lib/marzban-node/ssl_key.pem
```

Recommended permissions:

```bash
sudo chmod 644 /var/lib/marzban-node/cert.pem /var/lib/marzban-node/ssl_cert.pem
sudo chmod 600 /var/lib/marzban-node/ssl_key.pem
```

The REST API is protected with mTLS. Do not print or paste private key contents into logs or tickets.

## Systemd Deployment

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
SSL_CLIENT_CERT_FILE=/var/lib/marzban-node/cert.pem
INBOUNDS="Shadowsocks TCP,hy2-rn1c1g"
```

Quote `INBOUNDS` when an inbound tag contains spaces, for example `Shadowsocks TCP`.

Install and start:

```bash
sudo cp marzban.service /etc/systemd/system/marzban-node.service
sudo systemctl daemon-reload
sudo systemctl enable --now marzban-node
```

## Alpine / OpenRC Deployment

Alpine does not use systemd. Use a small wrapper so tags with spaces are loaded correctly.

`/usr/local/bin/marzban-node-start`:

```sh
#!/bin/sh
set -a
. /etc/marzban-node.env
set +a
exec /usr/local/bin/marzban-node
```

`/etc/init.d/marzban-node`:

```sh
#!/sbin/openrc-run
name="marzban-node"
description="Marzban Rust Node Service"
command="/usr/local/bin/marzban-node-start"
command_background="yes"
pidfile="/run/${RC_SVCNAME}.pid"
output_log="/var/log/marzban-node.log"
error_log="/var/log/marzban-node.log"
depend() {
    need net
    after firewall
}
```

Enable and start:

```bash
chmod +x /usr/local/bin/marzban-node-start /etc/init.d/marzban-node
rc-update add marzban-node default
rc-service marzban-node restart
```

For shell-loaded env files on Alpine, quote tags with spaces:

```env
INBOUNDS="Shadowsocks TCP"
```

If it is not quoted, the wrapper may log:

```text
/usr/local/bin/marzban-node-start: /etc/marzban-node.env: line 10: TCP: not found
```

## Controller Configuration Checklist

For each node:

1. Add or update the node row with:
   - `address`
   - `port=62050`
   - `api_port=62051`
2. Add `inbounds` rows for every selected inbound tag.
3. Add `hosts` rows for user-facing subscription addresses.
4. Make sure `/var/lib/marzban/xray_config.json` contains the matching inbound tag.
5. Restart Marzban so runtime config reloads.

For HY2, both places must include the tag:

- raw config: `/var/lib/marzban/xray_config.json`
- database: `inbounds` and `hosts`

If a tag exists in the database but not in the raw config, users may see a host entry but the node will not start sing-box.

## Migration From Old Python/Docker Node

Before starting Rust node, check who owns the management port:

```bash
sudo ss -lntup | grep -E ':62050|:62051'
sudo ps -eo pid,ppid,rss,args | grep -E 'python main.py|marzban-node|xray run -config stdin:'
```

If `62050` is owned by `python main.py`, find whether it is Docker:

```bash
sudo cat /proc/<pid>/cgroup
sudo docker ps --format '{{.ID}} {{.Names}} {{.Image}} {{.Ports}}'
```

Stop and disable the old Docker node:

```bash
sudo docker stop marzban-node
sudo docker update --restart=no marzban-node
```

Then start the Rust node:

```bash
sudo systemctl restart marzban-node
```

## Verification

Node service:

```bash
sudo systemctl is-active marzban-node
sudo ss -lntup | grep -E ':62050|:62051|:8443|:443'
sudo ss -lunp | grep -E ':8443'
sudo ps -eo pid,rss,args | grep -E 'marzban-node|sing-box run|xray run -config stdin:' | grep -v grep
```

Expected for HY2 or HY2-plus-VLESS/SS/Trojan:

- `marzban-node` listens on `62050/tcp`.
- `sing-box` owns `8443/udp`.
- `sing-box` owns any selected VLESS, Shadowsocks, or Trojan ports.
- No `xray run -config stdin:` process is required.
- No `62051/tcp` listener is required.

Expected for SS-only:

- `marzban-node` listens on `62050/tcp`.
- `xray` owns the Shadowsocks inbound port after the controller starts the node.
- No sing-box process is required.

Controller DB status:

```bash
sudo docker exec marzban-marzban-1 python -c 'import sqlite3,json; con=sqlite3.connect("/var/lib/marzban/db.sqlite3"); con.row_factory=sqlite3.Row; print(json.dumps([dict(r) for r in con.execute("select id,name,address,status,message,xray_version from nodes order by id")], ensure_ascii=False))'
```

Controller runtime config:

```bash
sudo docker exec -e PYTHONPATH=/code marzban-marzban-1 python -c 'from app import xray; print([t for t in xray.config.inbounds_by_tag if "hy2" in t])'
```

## Pitfalls From The RN1C1G Rollout

- **Old Docker node can hide behind the same name.** RN1C1G had a Docker container named `marzban-node` running `python main.py` and owning `62050`. Checking only systemd was not enough.
- **Do not copy a glibc binary blindly.** The RN3C4G binary required glibc 2.39 and failed on RN1C1G with glibc 2.36. Use the musl static build for portable deployment.
- **Old TLS certs may fail Rust TLS.** RN1C1G's old cert failed with `UnsupportedCertVersion`. Copying or generating v3-compatible node TLS certs fixed the Rust node startup.
- **Quote inbound tags with spaces.** `Shadowsocks TCP` must be quoted in env files loaded by a shell and is safer quoted in systemd env files too.
- **HY2 requires raw config plus DB rows.** `hy2-rn1c1g` initially existed only in intent, not in runtime/raw config, so the node did not receive the HY2 inbound needed to select sing-box.
- **One core per node config.** HY2 plus VLESS, Shadowsocks, or Trojan runs on one sing-box process. If `INBOUNDS` selects HY2 plus a protocol whose sing-box translator is not implemented yet, the node rejects the config until that translator is added.
- **Manual REST `/start` can isolate failures.** If an Xray-selected config starts but Marzban marks the node error, check `62051` gRPC reachability and Xray API startup. HY2/sing-box configs should return `xray_api=false` and skip that gRPC check.
- **Home broadband nodes may not expose management ports.** If `62050/tcp` is not reachable from the controller, Marzban cannot manage the node even if the Rust process is running locally.
