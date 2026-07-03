use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fmt;

#[derive(Debug)]
pub enum ConfigError {
    InvalidJson(serde_json::Error),
    RootIsNotObject,
    UnsupportedSingBoxInbound(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson(error) => write!(f, "Failed to decode config: {error}"),
            Self::RootIsNotObject => write!(f, "Xray config root must be a JSON object"),
            Self::UnsupportedSingBoxInbound(tag) => {
                write!(
                    f,
                    "Inbound {tag} is not supported by the sing-box translator yet"
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiSettings {
    pub host: String,
    pub port: u16,
    pub ssl_cert_file: String,
    pub ssl_key_file: String,
    pub peer_ip: String,
    pub allowed_inbounds: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreKind {
    Xray,
    SingBox,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConfiguredInboundPort {
    pub tag: String,
    pub protocol: String,
    pub network: String,
    pub transport: String,
    pub listen: String,
    pub port: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub struct XrayConfig {
    value: Value,
    sing_box_value: Option<Value>,
    core_kind: Option<CoreKind>,
}

impl XrayConfig {
    pub fn from_controller_json(raw: &str, api: &ApiSettings) -> Result<Self, ConfigError> {
        let mut value: Value = serde_json::from_str(raw).map_err(ConfigError::InvalidJson)?;
        if !value.is_object() {
            return Err(ConfigError::RootIsNotObject);
        }
        let applied = apply_api(&mut value, api)?;
        Ok(Self {
            value,
            sing_box_value: applied.sing_box_value,
            core_kind: applied.core_kind,
        })
    }

    pub fn into_value(self) -> Value {
        self.value
    }

    pub fn xray_value(&self) -> &Value {
        &self.value
    }

    pub fn sing_box_value(&self) -> Option<&Value> {
        self.sing_box_value.as_ref()
    }

    pub fn to_json(&self) -> String {
        self.value.to_string()
    }

    pub fn sing_box_json(&self) -> Option<String> {
        self.sing_box_value.as_ref().map(Value::to_string)
    }

    pub fn needs_xray(&self) -> bool {
        self.core_kind == Some(CoreKind::Xray)
    }

    pub fn needs_sing_box(&self) -> bool {
        self.core_kind == Some(CoreKind::SingBox)
    }

    pub fn core_kind(&self) -> Option<CoreKind> {
        self.core_kind
    }

    pub fn configured_inbound_ports(&self) -> Vec<ConfiguredInboundPort> {
        let inbounds = self
            .sing_box_value
            .as_ref()
            .or(Some(&self.value))
            .and_then(|value| value.get("inbounds"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        inbounds
            .iter()
            .filter_map(configured_inbound_port)
            .collect()
    }
}

fn configured_inbound_port(inbound: &Value) -> Option<ConfiguredInboundPort> {
    let tag = inbound.get("tag").and_then(Value::as_str)?.to_owned();
    if tag == "API_INBOUND" {
        return None;
    }

    let protocol = inbound
        .get("protocol")
        .or_else(|| inbound.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let port = inbound
        .get("port")
        .or_else(|| inbound.get("listen_port"))
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())?;
    let listen = inbound
        .get("listen")
        .and_then(Value::as_str)
        .unwrap_or("0.0.0.0")
        .to_owned();
    let network = inbound_network(inbound, &protocol);
    let transport = if matches!(network.as_str(), "hysteria" | "udp" | "quic" | "kcp")
        || protocol == "hysteria2"
    {
        "udp"
    } else {
        "tcp"
    };

    Some(ConfiguredInboundPort {
        tag,
        protocol,
        network,
        transport: transport.to_owned(),
        listen,
        port,
    })
}

fn inbound_network(inbound: &Value, protocol: &str) -> String {
    inbound
        .get("streamSettings")
        .and_then(|settings| settings.get("network"))
        .or_else(|| inbound.get("network"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            if protocol == "hysteria2" {
                "hysteria".to_owned()
            } else {
                "tcp".to_owned()
            }
        })
}

struct AppliedConfig {
    sing_box_value: Option<Value>,
    core_kind: Option<CoreKind>,
}

fn apply_api(config: &mut Value, api: &ApiSettings) -> Result<AppliedConfig, ConfigError> {
    let allowed_inbounds: HashSet<&str> = api.allowed_inbounds.iter().map(String::as_str).collect();

    let inbounds = config
        .as_object_mut()
        .expect("root checked before apply_api")
        .entry("inbounds")
        .or_insert_with(|| json!([]));
    if !inbounds.is_array() {
        *inbounds = json!([]);
    }

    let inbound_array = inbounds.as_array_mut().expect("inbounds forced to array");
    inbound_array.retain(|inbound| {
        if inbound.get("protocol").and_then(Value::as_str) == Some("dokodemo-door")
            && inbound.get("tag").and_then(Value::as_str) == Some("API_INBOUND")
        {
            return false;
        }

        if allowed_inbounds.is_empty() {
            return true;
        }

        inbound
            .get("tag")
            .and_then(Value::as_str)
            .map(|tag| allowed_inbounds.contains(tag))
            .unwrap_or(false)
    });

    match select_core(inbound_array) {
        None => {
            remove_existing_api_rules(config);
            let object = config
                .as_object_mut()
                .expect("root checked before apply_api");
            object.remove("api");
            object.remove("stats");
            Ok(AppliedConfig {
                sing_box_value: None,
                core_kind: None,
            })
        }
        Some(CoreKind::SingBox) => {
            let sing_box_value = build_sing_box_config(inbound_array, api)?;
            inbound_array.clear();
            remove_existing_api_rules(config);
            let object = config
                .as_object_mut()
                .expect("root checked before apply_api");
            object.remove("api");
            object.remove("stats");
            Ok(AppliedConfig {
                sing_box_value: Some(sing_box_value),
                core_kind: Some(CoreKind::SingBox),
            })
        }
        Some(CoreKind::Xray) => {
            inbound_array.insert(0, api_inbound(api));
            remove_existing_api_rules(config);
            ensure_routing_rules(config).insert(0, api_routing_rule(api));

            let object = config
                .as_object_mut()
                .expect("root checked before apply_api");
            object.insert(
                "api".to_owned(),
                json!({
                    "services": ["HandlerService", "StatsService", "LoggerService"],
                    "tag": "API"
                }),
            );
            object.insert("stats".to_owned(), json!({}));
            Ok(AppliedConfig {
                sing_box_value: None,
                core_kind: Some(CoreKind::Xray),
            })
        }
    }
}

fn api_inbound(api: &ApiSettings) -> Value {
    json!({
        "listen": &api.host,
        "port": api.port,
        "protocol": "dokodemo-door",
        "settings": {
            "address": "127.0.0.1"
        },
        "streamSettings": {
            "security": "tls",
            "tlsSettings": {
                "certificates": [
                    {
                        "certificateFile": &api.ssl_cert_file,
                        "keyFile": &api.ssl_key_file
                    }
                ]
            }
        },
        "tag": "API_INBOUND"
    })
}

fn api_routing_rule(api: &ApiSettings) -> Value {
    json!({
        "inboundTag": ["API_INBOUND"],
        "source": ["127.0.0.1", &api.peer_ip],
        "outboundTag": "API",
        "type": "field"
    })
}

fn remove_existing_api_rules(config: &mut Value) {
    let api_tag = config
        .get("api")
        .and_then(|api| api.get("tag"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let Some(api_tag) = api_tag else {
        return;
    };

    if let Some(rules) = config
        .get_mut("routing")
        .and_then(|routing| routing.get_mut("rules"))
        .and_then(Value::as_array_mut)
    {
        rules.retain(|rule| {
            rule.get("outboundTag").and_then(Value::as_str) != Some(api_tag.as_str())
        });
    }
}

fn ensure_routing_rules(config: &mut Value) -> &mut Vec<Value> {
    let object = config
        .as_object_mut()
        .expect("root checked before apply_api");
    let routing = object
        .entry("routing")
        .or_insert_with(|| json!({ "rules": [] }));
    if !routing.is_object() {
        *routing = json!({ "rules": [] });
    }

    let routing_object = routing.as_object_mut().expect("routing forced to object");
    let rules = routing_object.entry("rules").or_insert_with(|| json!([]));
    if !rules.is_array() {
        *rules = json!([]);
    }
    rules.as_array_mut().expect("rules forced to array")
}

fn select_core(inbounds: &[Value]) -> Option<CoreKind> {
    if inbounds.is_empty() {
        None
    } else if inbounds.iter().any(requires_sing_box) {
        Some(CoreKind::SingBox)
    } else {
        Some(CoreKind::Xray)
    }
}

fn requires_sing_box(inbound: &Value) -> bool {
    is_hysteria2_inbound(inbound)
}

fn build_sing_box_config(inbounds: &[Value], api: &ApiSettings) -> Result<Value, ConfigError> {
    let mut sing_box_inbounds = Vec::with_capacity(inbounds.len());

    for inbound in inbounds {
        if is_hysteria2_inbound(inbound) {
            sing_box_inbounds.push(sing_box_hysteria2_inbound(inbound, api));
            continue;
        }

        match inbound.get("protocol").and_then(Value::as_str) {
            Some("vless") => sing_box_inbounds.push(sing_box_vless_inbound(inbound)),
            Some("shadowsocks") => sing_box_inbounds.push(sing_box_shadowsocks_inbound(inbound)),
            Some("trojan") => sing_box_inbounds.push(sing_box_trojan_inbound(inbound)),
            _ => {
                return Err(ConfigError::UnsupportedSingBoxInbound(inbound_tag(inbound)));
            }
        }
    }

    Ok(json!({
        "log": {
            "level": "warn"
        },
        "inbounds": sing_box_inbounds,
        "outbounds": [
            {"type": "direct", "tag": "DIRECT"},
            {"type": "block", "tag": "BLOCK"}
        ],
        "route": {
            "final": "DIRECT"
        }
    }))
}

fn inbound_tag(inbound: &Value) -> String {
    inbound
        .get("tag")
        .and_then(Value::as_str)
        .unwrap_or("<untagged>")
        .to_owned()
}

fn is_hysteria2_inbound(inbound: &Value) -> bool {
    inbound.get("protocol").and_then(Value::as_str) == Some("hysteria")
        && inbound
            .get("settings")
            .and_then(|settings| settings.get("version"))
            .and_then(Value::as_i64)
            == Some(2)
        && inbound
            .get("streamSettings")
            .and_then(|settings| settings.get("network"))
            .and_then(Value::as_str)
            == Some("hysteria")
}

fn sing_box_hysteria2_inbound(inbound: &Value, api: &ApiSettings) -> Value {
    let settings = inbound.get("settings").unwrap_or(&Value::Null);
    let stream_settings = inbound.get("streamSettings").unwrap_or(&Value::Null);
    let tls_settings = stream_settings.get("tlsSettings").unwrap_or(&Value::Null);

    let users: Vec<Value> = settings
        .get("users")
        .and_then(Value::as_array)
        .map(|users| {
            users
                .iter()
                .filter_map(|user| {
                    let password = user.get("auth").and_then(Value::as_str)?;
                    let name = user
                        .get("email")
                        .and_then(Value::as_str)
                        .unwrap_or(password);
                    Some(json!({
                        "name": name,
                        "password": password
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut tls = json!({
        "enabled": true,
        "certificate_path": api.ssl_cert_file,
        "key_path": api.ssl_key_file
    });

    if let Some(certificate) = tls_settings
        .get("certificates")
        .and_then(Value::as_array)
        .and_then(|certificates| certificates.first())
    {
        if let Some(path) = certificate.get("certificateFile").and_then(Value::as_str) {
            tls["certificate_path"] = json!(path);
        } else if let Some(lines) = certificate.get("certificate").and_then(Value::as_array) {
            tls["certificate"] = json!(lines_to_pem(lines));
            tls.as_object_mut()
                .expect("tls object")
                .remove("certificate_path");
        }

        if let Some(path) = certificate.get("keyFile").and_then(Value::as_str) {
            tls["key_path"] = json!(path);
        } else if let Some(lines) = certificate.get("key").and_then(Value::as_array) {
            tls["key"] = json!(lines_to_pem(lines));
            tls.as_object_mut().expect("tls object").remove("key_path");
        }
    }

    if let Some(alpn) = tls_settings.get("alpn").and_then(Value::as_array) {
        tls["alpn"] = json!(alpn);
    } else {
        tls["alpn"] = json!(["h3"]);
    }

    json!({
        "type": "hysteria2",
        "tag": inbound.get("tag").cloned().unwrap_or_else(|| json!("HY2")),
        "listen": inbound.get("listen").cloned().unwrap_or_else(|| json!("::")),
        "listen_port": inbound.get("port").cloned().unwrap_or_else(|| json!(443)),
        "users": users,
        "tls": tls
    })
}

fn sing_box_vless_inbound(inbound: &Value) -> Value {
    let settings = inbound.get("settings").unwrap_or(&Value::Null);
    let users: Vec<Value> = settings
        .get("clients")
        .and_then(Value::as_array)
        .map(|clients| {
            clients
                .iter()
                .filter_map(|client| {
                    let uuid = client.get("id").and_then(Value::as_str)?;
                    let mut user = json!({
                        "name": user_name(client, uuid),
                        "uuid": uuid
                    });
                    if let Some(flow) = client.get("flow").and_then(Value::as_str) {
                        if !flow.is_empty() {
                            user["flow"] = json!(flow);
                        }
                    }
                    Some(user)
                })
                .collect()
        })
        .unwrap_or_default();

    let mut value = sing_box_inbound_base(inbound, "vless");
    value["users"] = json!(users);
    if let Some(tls) = sing_box_tls(inbound) {
        value["tls"] = tls;
    }
    if let Some(transport) = sing_box_transport(inbound) {
        value["transport"] = transport;
    }
    value
}

fn sing_box_shadowsocks_inbound(inbound: &Value) -> Value {
    let settings = inbound.get("settings").unwrap_or(&Value::Null);
    let mut value = sing_box_inbound_base(inbound, "shadowsocks");
    value["method"] = settings
        .get("method")
        .cloned()
        .unwrap_or_else(|| json!("chacha20-ietf-poly1305"));
    value["password"] = settings
        .get("password")
        .cloned()
        .unwrap_or_else(|| json!(""));

    if let Some(network) = settings.get("network").and_then(Value::as_str) {
        if !network.is_empty() {
            value["network"] = json!(network);
        }
    }

    let users: Vec<Value> = settings
        .get("clients")
        .and_then(Value::as_array)
        .map(|clients| {
            clients
                .iter()
                .filter_map(|client| {
                    let password = client.get("password").and_then(Value::as_str)?;
                    Some(json!({
                        "name": user_name(client, password),
                        "password": password
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    if !users.is_empty() {
        value["users"] = json!(users);
    }
    value
}

fn sing_box_trojan_inbound(inbound: &Value) -> Value {
    let settings = inbound.get("settings").unwrap_or(&Value::Null);
    let users: Vec<Value> = settings
        .get("clients")
        .and_then(Value::as_array)
        .map(|clients| {
            clients
                .iter()
                .filter_map(|client| {
                    let password = client.get("password").and_then(Value::as_str)?;
                    Some(json!({
                        "name": user_name(client, password),
                        "password": password
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut value = sing_box_inbound_base(inbound, "trojan");
    value["users"] = json!(users);
    if let Some(tls) = sing_box_tls(inbound) {
        value["tls"] = tls;
    }
    if let Some(transport) = sing_box_transport(inbound) {
        value["transport"] = transport;
    }
    value
}

fn sing_box_inbound_base(inbound: &Value, inbound_type: &str) -> Value {
    json!({
        "type": inbound_type,
        "tag": inbound.get("tag").cloned().unwrap_or_else(|| json!(inbound_type)),
        "listen": inbound.get("listen").cloned().unwrap_or_else(|| json!("::")),
        "listen_port": inbound.get("port").cloned().unwrap_or_else(|| json!(0))
    })
}

fn user_name(user: &Value, fallback: &str) -> String {
    user.get("email")
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_owned()
}

fn sing_box_tls(inbound: &Value) -> Option<Value> {
    let stream_settings = inbound.get("streamSettings").unwrap_or(&Value::Null);
    match stream_settings.get("security").and_then(Value::as_str) {
        Some("tls") => Some(sing_box_tls_from_settings(
            stream_settings.get("tlsSettings").unwrap_or(&Value::Null),
        )),
        Some("reality") => Some(sing_box_reality_tls_from_settings(
            stream_settings
                .get("realitySettings")
                .unwrap_or(&Value::Null),
        )),
        _ => None,
    }
}

fn sing_box_tls_from_settings(tls_settings: &Value) -> Value {
    let mut tls = json!({ "enabled": true });

    if let Some(server_name) = tls_settings.get("serverName").and_then(Value::as_str) {
        if !server_name.is_empty() {
            tls["server_name"] = json!(server_name);
        }
    }

    if let Some(alpn) = tls_settings.get("alpn").and_then(Value::as_array) {
        tls["alpn"] = json!(alpn);
    }

    apply_certificate_settings(&mut tls, tls_settings);
    tls
}

fn sing_box_reality_tls_from_settings(reality_settings: &Value) -> Value {
    let mut tls = json!({
        "enabled": true,
        "reality": {
            "enabled": true
        }
    });

    if let Some(server_name) = reality_settings
        .get("serverNames")
        .and_then(Value::as_array)
        .and_then(|names| names.first())
        .and_then(Value::as_str)
    {
        tls["server_name"] = json!(server_name);
        tls["reality"]["handshake"] = json!({
            "server": server_name,
            "server_port": 443
        });
    }

    if let Some(dest) = reality_settings.get("dest").and_then(Value::as_str) {
        let (server, port) = parse_host_port(dest, 443);
        tls["reality"]["handshake"] = json!({
            "server": server,
            "server_port": port
        });
    }

    if let Some(private_key) = reality_settings.get("privateKey").and_then(Value::as_str) {
        tls["reality"]["private_key"] = json!(private_key);
    }
    if let Some(short_ids) = reality_settings.get("shortIds").and_then(Value::as_array) {
        tls["reality"]["short_id"] = json!(short_ids);
    }
    tls
}

fn apply_certificate_settings(tls: &mut Value, tls_settings: &Value) {
    if let Some(certificate) = tls_settings
        .get("certificates")
        .and_then(Value::as_array)
        .and_then(|certificates| certificates.first())
    {
        if let Some(path) = certificate.get("certificateFile").and_then(Value::as_str) {
            tls["certificate_path"] = json!(path);
        } else if let Some(lines) = certificate.get("certificate").and_then(Value::as_array) {
            tls["certificate"] = json!(lines_to_pem(lines));
        }

        if let Some(path) = certificate.get("keyFile").and_then(Value::as_str) {
            tls["key_path"] = json!(path);
        } else if let Some(lines) = certificate.get("key").and_then(Value::as_array) {
            tls["key"] = json!(lines_to_pem(lines));
        }
    }
}

fn sing_box_transport(inbound: &Value) -> Option<Value> {
    let stream_settings = inbound.get("streamSettings").unwrap_or(&Value::Null);
    match stream_settings.get("network").and_then(Value::as_str) {
        Some("ws") | Some("websocket") => {
            let ws = stream_settings.get("wsSettings").unwrap_or(&Value::Null);
            let mut transport = json!({ "type": "ws" });
            if let Some(path) = ws.get("path").and_then(Value::as_str) {
                transport["path"] = json!(path);
            }
            if let Some(headers) = ws.get("headers") {
                transport["headers"] = headers.clone();
            }
            Some(transport)
        }
        Some("grpc") => {
            let grpc = stream_settings.get("grpcSettings").unwrap_or(&Value::Null);
            let mut transport = json!({ "type": "grpc" });
            if let Some(service_name) = grpc.get("serviceName").and_then(Value::as_str) {
                transport["service_name"] = json!(service_name);
            }
            Some(transport)
        }
        Some("http") | Some("h2") => {
            let http = stream_settings.get("httpSettings").unwrap_or(&Value::Null);
            let mut transport = json!({ "type": "http" });
            if let Some(path) = http.get("path").and_then(Value::as_str) {
                transport["path"] = json!(path);
            }
            if let Some(host) = http.get("host").and_then(Value::as_array) {
                transport["host"] = json!(host);
            }
            Some(transport)
        }
        _ => None,
    }
}

fn parse_host_port(value: &str, default_port: u16) -> (String, u16) {
    if let Some((host, port)) = value.rsplit_once(':') {
        if let Ok(port) = port.parse::<u16>() {
            return (host.to_owned(), port);
        }
    }
    (value.to_owned(), default_port)
}

fn lines_to_pem(lines: &[Value]) -> String {
    let mut pem = lines
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    pem.push('\n');
    pem
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_settings() -> ApiSettings {
        ApiSettings {
            host: "0.0.0.0".to_owned(),
            port: 62051,
            ssl_cert_file: "/var/lib/marzban-node/ssl_cert.pem".to_owned(),
            ssl_key_file: "/var/lib/marzban-node/ssl_key.pem".to_owned(),
            peer_ip: "203.0.113.10".to_owned(),
            allowed_inbounds: vec![],
        }
    }

    #[test]
    fn injects_xray_api_inbound_and_route() {
        let config = XrayConfig::from_controller_json(
            r#"{"inbounds":[{"tag":"HY2","protocol":"hysteria"}],"outbounds":[]}"#,
            &api_settings(),
        )
        .unwrap()
        .into_value();

        assert_eq!(config["api"]["tag"], "API");
        assert_eq!(config["inbounds"][0]["tag"], "API_INBOUND");
        assert_eq!(config["inbounds"][0]["port"], 62051);
        assert_eq!(config["routing"]["rules"][0]["outboundTag"], "API");
        assert_eq!(config["routing"]["rules"][0]["source"][1], "203.0.113.10");
    }

    #[test]
    fn replaces_existing_api_inbound_and_route() {
        let config = XrayConfig::from_controller_json(
            r#"{
                "api": {"tag": "OLD_API"},
                "inbounds": [
                    {"tag":"API_INBOUND","protocol":"dokodemo-door"},
                    {"tag":"HY2","protocol":"hysteria"}
                ],
                "routing": {
                    "rules": [
                        {"type":"field","outboundTag":"OLD_API"},
                        {"type":"field","outboundTag":"DIRECT"}
                    ]
                }
            }"#,
            &api_settings(),
        )
        .unwrap()
        .into_value();

        let inbounds = config["inbounds"].as_array().unwrap();
        assert_eq!(inbounds.len(), 2);
        assert_eq!(inbounds[0]["tag"], "API_INBOUND");
        assert_eq!(inbounds[1]["tag"], "HY2");

        let rules = config["routing"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["outboundTag"], "API");
        assert_eq!(rules[1]["outboundTag"], "DIRECT");
    }

    #[test]
    fn filters_to_selected_inbounds_when_configured() {
        let mut api = api_settings();
        api.allowed_inbounds = vec!["KEEP".to_owned()];

        let config = XrayConfig::from_controller_json(
            r#"{
                "inbounds": [
                    {"tag":"KEEP","protocol":"hysteria"},
                    {"tag":"DROP","protocol":"vless"}
                ]
            }"#,
            &api,
        )
        .unwrap()
        .into_value();

        let inbounds = config["inbounds"].as_array().unwrap();
        assert_eq!(inbounds.len(), 2);
        assert_eq!(inbounds[0]["tag"], "API_INBOUND");
        assert_eq!(inbounds[1]["tag"], "KEEP");
    }

    #[test]
    fn converts_hysteria2_inbounds_to_sing_box() {
        let config = XrayConfig::from_controller_json(
            r#"{
                "inbounds": [
                    {
                        "tag": "HY2",
                        "listen": "0.0.0.0",
                        "port": 8443,
                        "protocol": "hysteria",
                        "settings": {
                            "version": 2,
                            "users": [
                                {"email": "1.alice", "auth": "alice-secret"},
                                {"email": "2.bob", "auth": "bob-secret"}
                            ]
                        },
                        "streamSettings": {
                            "network": "hysteria",
                            "security": "tls",
                            "tlsSettings": {
                                "alpn": ["h3"],
                                "certificates": [
                                    {
                                        "certificateFile": "/etc/cert.pem",
                                        "keyFile": "/etc/key.pem"
                                    }
                                ]
                            }
                        }
                    }
                ],
                "outbounds": [{"protocol":"freedom","tag":"DIRECT"}]
            }"#,
            &api_settings(),
        )
        .unwrap();

        let xray = config.xray_value();
        let xray_inbounds = xray["inbounds"].as_array().unwrap();
        assert!(xray_inbounds.is_empty());
        assert_eq!(config.core_kind(), Some(CoreKind::SingBox));

        let sing_box = config
            .sing_box_value()
            .expect("HY2 should create sing-box config");
        let hy2 = &sing_box["inbounds"][0];
        assert_eq!(hy2["type"], "hysteria2");
        assert_eq!(hy2["tag"], "HY2");
        assert_eq!(hy2["listen_port"], 8443);
        assert_eq!(hy2["users"][0]["name"], "1.alice");
        assert_eq!(hy2["users"][0]["password"], "alice-secret");
        assert_eq!(hy2["tls"]["enabled"], true);
        assert_eq!(hy2["tls"]["certificate_path"], "/etc/cert.pem");
        assert_eq!(hy2["tls"]["key_path"], "/etc/key.pem");
    }

    #[test]
    fn hysteria2_only_config_requires_only_sing_box() {
        let config = XrayConfig::from_controller_json(
            r#"{
                "inbounds": [
                    {
                        "tag": "HY2",
                        "port": 8443,
                        "protocol": "hysteria",
                        "settings": {"version": 2, "users": [{"auth": "secret"}]},
                        "streamSettings": {"network": "hysteria"}
                    }
                ]
            }"#,
            &api_settings(),
        )
        .unwrap();

        assert!(!config.needs_xray());
        assert!(config.needs_sing_box());
        assert!(config.xray_value()["api"].is_null());
        assert!(config.xray_value()["stats"].is_null());
        assert!(config.xray_value()["inbounds"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn xray_only_config_requires_only_xray() {
        let config = XrayConfig::from_controller_json(
            r#"{"inbounds":[{"tag":"VLESS","protocol":"vless","port":443}]}"#,
            &api_settings(),
        )
        .unwrap();

        assert!(config.needs_xray());
        assert!(!config.needs_sing_box());
    }

    #[test]
    fn unknown_inbound_in_sing_box_config_fails_explicitly() {
        let error = XrayConfig::from_controller_json(
            r#"{
                "inbounds": [
                    {
                        "tag": "HY2",
                        "port": 8443,
                        "protocol": "hysteria",
                        "settings": {"version": 2, "users": [{"auth": "secret"}]},
                        "streamSettings": {"network": "hysteria"}
                    },
                    {"tag":"VMess","protocol":"vmess","port":443}
                ]
            }"#,
            &api_settings(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ConfigError::UnsupportedSingBoxInbound(tag) if tag == "VMess"
        ));
    }

    #[test]
    fn converts_hy2_vless_shadowsocks_and_trojan_to_single_sing_box_config() {
        let config = XrayConfig::from_controller_json(
            r#"{
                "inbounds": [
                    {
                        "tag": "HY2",
                        "listen": "0.0.0.0",
                        "port": 8443,
                        "protocol": "hysteria",
                        "settings": {"version": 2, "users": [{"email": "1.hy2", "auth": "hy2-secret"}]},
                        "streamSettings": {"network": "hysteria"}
                    },
                    {
                        "tag": "VLESS",
                        "listen": "0.0.0.0",
                        "port": 443,
                        "protocol": "vless",
                        "settings": {"clients": [{"email": "2.vless", "id": "11111111-1111-1111-1111-111111111111", "flow": "xtls-rprx-vision"}]},
                        "streamSettings": {
                            "network": "tcp",
                            "security": "reality",
                            "realitySettings": {
                                "serverNames": ["example.com"],
                                "privateKey": "private-key",
                                "shortIds": ["abcd"]
                            }
                        }
                    },
                    {
                        "tag": "SS",
                        "listen": "0.0.0.0",
                        "port": 8388,
                        "protocol": "shadowsocks",
                        "settings": {
                            "method": "2022-blake3-aes-128-gcm",
                            "password": "server-secret",
                            "clients": [{"email": "3.ss", "password": "user-secret"}]
                        }
                    },
                    {
                        "tag": "Trojan",
                        "listen": "0.0.0.0",
                        "port": 8444,
                        "protocol": "trojan",
                        "settings": {"clients": [{"email": "4.trojan", "password": "trojan-secret"}]},
                        "streamSettings": {
                            "network": "ws",
                            "security": "tls",
                            "tlsSettings": {
                                "serverName": "trojan.example.com",
                                "alpn": ["h2"],
                                "certificates": [{"certificateFile": "/etc/trojan.crt", "keyFile": "/etc/trojan.key"}]
                            },
                            "wsSettings": {"path": "/trojan"}
                        }
                    }
                ]
            }"#,
            &api_settings(),
        )
        .unwrap();

        assert!(!config.needs_xray());
        assert!(config.needs_sing_box());
        assert!(config.xray_value()["inbounds"]
            .as_array()
            .unwrap()
            .is_empty());

        let sing_box = config.sing_box_value().unwrap();
        let inbounds = sing_box["inbounds"].as_array().unwrap();
        assert_eq!(inbounds.len(), 4);

        assert_eq!(inbounds[0]["type"], "hysteria2");

        let vless = &inbounds[1];
        assert_eq!(vless["type"], "vless");
        assert_eq!(
            vless["users"][0]["uuid"],
            "11111111-1111-1111-1111-111111111111"
        );
        assert_eq!(vless["users"][0]["flow"], "xtls-rprx-vision");
        assert_eq!(vless["tls"]["enabled"], true);
        assert_eq!(vless["tls"]["reality"]["enabled"], true);
        assert_eq!(
            vless["tls"]["reality"]["handshake"]["server"],
            "example.com"
        );
        assert_eq!(vless["tls"]["reality"]["private_key"], "private-key");
        assert_eq!(vless["tls"]["reality"]["short_id"][0], "abcd");

        let shadowsocks = &inbounds[2];
        assert_eq!(shadowsocks["type"], "shadowsocks");
        assert_eq!(shadowsocks["method"], "2022-blake3-aes-128-gcm");
        assert_eq!(shadowsocks["password"], "server-secret");
        assert_eq!(shadowsocks["users"][0]["name"], "3.ss");
        assert_eq!(shadowsocks["users"][0]["password"], "user-secret");

        let trojan = &inbounds[3];
        assert_eq!(trojan["type"], "trojan");
        assert_eq!(trojan["users"][0]["password"], "trojan-secret");
        assert_eq!(trojan["tls"]["enabled"], true);
        assert_eq!(trojan["tls"]["server_name"], "trojan.example.com");
        assert_eq!(trojan["tls"]["certificate_path"], "/etc/trojan.crt");
        assert_eq!(trojan["transport"]["type"], "ws");
        assert_eq!(trojan["transport"]["path"], "/trojan");
    }
}
