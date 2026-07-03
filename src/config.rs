use std::env;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub service_host: String,
    pub service_port: u16,
    pub xray_api_host: String,
    pub xray_api_port: u16,
    pub xray_executable_path: PathBuf,
    pub xray_assets_path: PathBuf,
    pub sing_box_executable_path: PathBuf,
    pub ssl_cert_file: PathBuf,
    pub ssl_key_file: PathBuf,
    pub ssl_client_cert_file: Option<PathBuf>,
    pub debug: bool,
    pub inbounds: Vec<String>,
}

impl Settings {
    pub fn from_env() -> Self {
        Self {
            service_host: env_string("SERVICE_HOST", "0.0.0.0"),
            service_port: env_u16("SERVICE_PORT", 62050),
            xray_api_host: env_string("XRAY_API_HOST", "0.0.0.0"),
            xray_api_port: env_u16("XRAY_API_PORT", 62051),
            xray_executable_path: PathBuf::from(env_string(
                "XRAY_EXECUTABLE_PATH",
                "/usr/local/bin/xray",
            )),
            xray_assets_path: PathBuf::from(env_string(
                "XRAY_ASSETS_PATH",
                "/usr/local/share/xray",
            )),
            sing_box_executable_path: PathBuf::from(env_string(
                "SING_BOX_EXECUTABLE_PATH",
                "/usr/local/bin/sing-box",
            )),
            ssl_cert_file: PathBuf::from(env_string(
                "SSL_CERT_FILE",
                "/var/lib/marzban-node/ssl_cert.pem",
            )),
            ssl_key_file: PathBuf::from(env_string(
                "SSL_KEY_FILE",
                "/var/lib/marzban-node/ssl_key.pem",
            )),
            ssl_client_cert_file: env::var("SSL_CLIENT_CERT_FILE")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from),
            debug: env_bool("DEBUG", false),
            inbounds: env::var("INBOUNDS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
        }
    }
}

fn env_string(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn env_u16(key: &str, default: u16) -> u16 {
    env::var(key)
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(default)
}

fn env_bool(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .and_then(|value| match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}
