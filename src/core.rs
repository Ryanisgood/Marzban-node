use crate::config::Settings;
use crate::xray_config::XrayConfig;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

const LOG_BUFFER_LIMIT: usize = 100;

#[derive(Debug)]
pub enum CoreError {
    Io(std::io::Error),
    VersionNotFound(&'static str),
    AlreadyStarted,
    NoCoreRequired,
    StdinUnavailable,
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::VersionNotFound(core) => write!(f, "Failed to detect {core} version"),
            Self::AlreadyStarted => write!(f, "Core is started already"),
            Self::NoCoreRequired => write!(f, "No core is required for the selected inbounds"),
            Self::StdinUnavailable => write!(f, "Core stdin is unavailable"),
        }
    }
}

impl std::error::Error for CoreError {}

impl From<std::io::Error> for CoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
pub struct XrayCore {
    settings: Settings,
    version: String,
    child: Option<Child>,
    sing_box_child: Option<Child>,
    needs_xray: bool,
    needs_sing_box: bool,
    logs: Arc<Mutex<VecDeque<String>>>,
}

impl XrayCore {
    pub fn new(settings: Settings) -> Result<Self, CoreError> {
        Ok(Self {
            settings,
            version: "unknown".to_owned(),
            child: None,
            sing_box_child: None,
            needs_xray: false,
            needs_sing_box: false,
            logs: Arc::new(Mutex::new(VecDeque::with_capacity(LOG_BUFFER_LIMIT))),
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn is_started(&mut self) -> bool {
        let xray_started = if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(None) => true,
                Ok(Some(_)) | Err(_) => {
                    self.child = None;
                    false
                }
            }
        } else {
            false
        };

        let sing_box_started = if let Some(child) = self.sing_box_child.as_mut() {
            match child.try_wait() {
                Ok(None) => true,
                Ok(Some(_)) | Err(_) => {
                    self.sing_box_child = None;
                    false
                }
            }
        } else {
            false
        };

        let needs_any_core = self.needs_xray || self.needs_sing_box;
        needs_any_core
            && (!self.needs_xray || xray_started)
            && (!self.needs_sing_box || sing_box_started)
    }

    pub fn xray_api_available(&mut self) -> bool {
        self.needs_xray && self.is_xray_started()
    }

    pub fn core_kind(&mut self) -> Option<&'static str> {
        if self.needs_sing_box && self.is_sing_box_started() {
            Some("sing-box")
        } else if self.needs_xray && self.is_xray_started() {
            Some("xray")
        } else {
            None
        }
    }

    fn is_xray_started(&mut self) -> bool {
        is_child_started(&mut self.child)
    }

    fn is_sing_box_started(&mut self) -> bool {
        is_child_started(&mut self.sing_box_child)
    }

    fn any_core_started(&mut self) -> bool {
        self.is_xray_started() || self.is_sing_box_started()
    }

    pub fn start(&mut self, config: &XrayConfig) -> Result<(), CoreError> {
        if self.any_core_started() {
            return Err(CoreError::AlreadyStarted);
        }

        self.needs_xray = config.needs_xray();
        self.needs_sing_box = config.needs_sing_box();
        if !self.needs_xray && !self.needs_sing_box {
            return Err(CoreError::NoCoreRequired);
        }

        let mut versions = Vec::new();
        if self.needs_sing_box {
            versions.push(format!(
                "sing-box {}",
                detect_sing_box_version(&self.settings)?
            ));
            let sing_box_json = config
                .sing_box_json()
                .expect("needs_sing_box is derived from sing_box_json");
            let child = spawn_with_stdin(
                &self.settings.sing_box_executable_path,
                &["run", "-c", "/dev/stdin"],
                &[],
                &sing_box_json,
                Arc::clone(&self.logs),
            )?;
            self.sing_box_child = Some(child);
        }

        if self.needs_xray {
            versions.push(format!("xray {}", detect_xray_version(&self.settings)?));
            let mut envs = Vec::new();
            envs.push((
                "XRAY_LOCATION_ASSET",
                self.settings.xray_assets_path.to_string_lossy().to_string(),
            ));
            let child = spawn_with_stdin(
                &self.settings.xray_executable_path,
                &["run", "-config", "stdin:"],
                &envs,
                &config.to_json(),
                Arc::clone(&self.logs),
            )?;

            self.child = Some(child);
        }

        self.version = versions.join(", ");
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            self.push_log("Xray core stopped".to_owned());
        }
        if let Some(mut child) = self.sing_box_child.take() {
            let _ = child.kill();
            let _ = child.wait();
            self.push_log("sing-box core stopped".to_owned());
        }
        self.needs_xray = false;
        self.needs_sing_box = false;
        self.version = "unknown".to_owned();
    }

    pub fn restart(&mut self, config: &XrayConfig) -> Result<(), CoreError> {
        self.stop();
        self.start(config)
    }

    pub fn logs_snapshot(&self) -> Vec<String> {
        self.logs
            .lock()
            .map(|logs| logs.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn push_log(&self, line: String) {
        push_log(&self.logs, line);
    }
}

impl Drop for XrayCore {
    fn drop(&mut self) {
        self.stop();
    }
}

fn is_child_started(child_slot: &mut Option<Child>) -> bool {
    if let Some(child) = child_slot.as_mut() {
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) | Err(_) => {
                *child_slot = None;
                false
            }
        }
    } else {
        false
    }
}

fn spawn_with_stdin(
    executable: &std::path::Path,
    args: &[&str],
    envs: &[(&str, String)],
    config: &str,
    logs: Arc<Mutex<VecDeque<String>>>,
) -> Result<Child, CoreError> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }

    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().ok_or(CoreError::StdinUnavailable)?;
    stdin.write_all(config.as_bytes())?;
    drop(stdin);

    if let Some(stdout) = child.stdout.take() {
        spawn_log_reader(stdout, Arc::clone(&logs));
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_log_reader(stderr, logs);
    }

    Ok(child)
}

fn detect_xray_version(settings: &Settings) -> Result<String, CoreError> {
    let output = Command::new(&settings.xray_executable_path)
        .arg("version")
        .output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text
        .lines()
        .find_map(|line| line.strip_prefix("Xray "))
        .and_then(|rest| rest.split_whitespace().next())
        .ok_or(CoreError::VersionNotFound("Xray"))?;
    Ok(version.to_owned())
}

fn detect_sing_box_version(settings: &Settings) -> Result<String, CoreError> {
    let output = Command::new(&settings.sing_box_executable_path)
        .arg("version")
        .output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text
        .lines()
        .find_map(|line| {
            line.strip_prefix("sing-box version ")
                .or_else(|| line.strip_prefix("sing-box "))
        })
        .and_then(|rest| rest.split_whitespace().next())
        .ok_or(CoreError::VersionNotFound("sing-box"))?;
    Ok(version.to_owned())
}

fn spawn_log_reader<R>(reader: R, logs: Arc<Mutex<VecDeque<String>>>)
where
    R: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines().map_while(Result::ok) {
            push_log(&logs, line);
        }
    });
}

fn push_log(logs: &Arc<Mutex<VecDeque<String>>>, line: String) {
    if let Ok(mut logs) = logs.lock() {
        if logs.len() >= LOG_BUFFER_LIMIT {
            logs.pop_front();
        }
        logs.push_back(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xray_config::ApiSettings;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn api_settings() -> ApiSettings {
        ApiSettings {
            host: "127.0.0.1".to_owned(),
            port: 62051,
            ssl_cert_file: "/tmp/cert.pem".to_owned(),
            ssl_key_file: "/tmp/key.pem".to_owned(),
            peer_ip: "127.0.0.1".to_owned(),
            allowed_inbounds: vec![],
        }
    }

    fn settings(root: &Path, xray: PathBuf, sing_box: PathBuf) -> Settings {
        Settings {
            service_host: "127.0.0.1".to_owned(),
            service_port: 62050,
            xray_api_host: "127.0.0.1".to_owned(),
            xray_api_port: 62051,
            xray_executable_path: xray,
            xray_assets_path: root.join("assets"),
            sing_box_executable_path: sing_box,
            ssl_cert_file: root.join("cert.pem"),
            ssl_key_file: root.join("key.pem"),
            ssl_client_cert_file: None,
            debug: false,
            inbounds: vec![],
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("marzban-node-{name}-{nanos}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn new_does_not_require_xray_executable() {
        let root = temp_root("lazy-new");
        let core = XrayCore::new(settings(
            &root,
            root.join("missing-xray"),
            root.join("missing-sing-box"),
        ));

        assert!(core.is_ok());
    }

    #[test]
    fn hysteria2_only_starts_sing_box_without_xray() {
        let root = temp_root("hy2-only");
        let sing_box = root.join("sing-box");
        let sing_marker = root.join("sing-box-started");
        write_executable(
            &sing_box,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"version\" ]; then echo 'sing-box version 1.13.0'; exit 0; fi\ntouch '{}'\nsleep 60\n",
                sing_marker.display()
            ),
        );

        let config = XrayConfig::from_controller_json(
            r#"{"inbounds":[{"tag":"HY2","port":8443,"protocol":"hysteria","settings":{"version":2,"users":[{"auth":"secret"}]},"streamSettings":{"network":"hysteria"}}]}"#,
            &api_settings(),
        )
        .unwrap();
        let mut core = XrayCore::new(settings(&root, root.join("missing-xray"), sing_box)).unwrap();

        core.start(&config).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        assert!(sing_marker.exists());
        assert!(core.is_started());
        assert_eq!(core.core_kind(), Some("sing-box"));
        core.stop();
    }

    #[test]
    fn xray_only_starts_xray_without_sing_box() {
        let root = temp_root("xray-only");
        let xray = root.join("xray");
        let xray_marker = root.join("xray-started");
        write_executable(
            &xray,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"version\" ]; then echo 'Xray 26.6.27'; exit 0; fi\ntouch '{}'\nsleep 60\n",
                xray_marker.display()
            ),
        );

        let config = XrayConfig::from_controller_json(
            r#"{"inbounds":[{"tag":"VLESS","protocol":"vless","port":443}]}"#,
            &api_settings(),
        )
        .unwrap();
        let mut core = XrayCore::new(settings(&root, xray, root.join("missing-sing-box"))).unwrap();

        core.start(&config).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        assert!(xray_marker.exists());
        assert!(core.is_started());
        assert_eq!(core.core_kind(), Some("xray"));
        core.stop();
    }

    #[test]
    fn hy2_with_xray_protocols_starts_only_sing_box() {
        let root = temp_root("hy2-vless");
        let sing_box = root.join("sing-box");
        let sing_marker = root.join("sing-box-started");
        write_executable(
            &sing_box,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"version\" ]; then echo 'sing-box version 1.13.0'; exit 0; fi\ntouch '{}'\nsleep 60\n",
                sing_marker.display()
            ),
        );

        let config = XrayConfig::from_controller_json(
            r#"{"inbounds":[{"tag":"HY2","port":8443,"protocol":"hysteria","settings":{"version":2,"users":[{"auth":"secret"}]},"streamSettings":{"network":"hysteria"}},{"tag":"VLESS","protocol":"vless","port":443,"settings":{"clients":[{"id":"11111111-1111-1111-1111-111111111111"}]}}]}"#,
            &api_settings(),
        )
        .unwrap();
        let mut core = XrayCore::new(settings(&root, root.join("missing-xray"), sing_box)).unwrap();

        core.start(&config).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        assert!(sing_marker.exists());
        assert!(core.is_started());
        assert!(!core.xray_api_available());
        assert_eq!(core.core_kind(), Some("sing-box"));
        core.stop();
    }

    #[test]
    fn exited_core_reports_no_current_core() {
        let root = temp_root("exited-core");
        let xray = root.join("xray");
        write_executable(
            &xray,
            "#!/bin/sh\nif [ \"$1\" = \"version\" ]; then echo 'Xray 26.6.27'; exit 0; fi\nexit 0\n",
        );

        let config = XrayConfig::from_controller_json(
            r#"{"inbounds":[{"tag":"VLESS","protocol":"vless","port":443}]}"#,
            &api_settings(),
        )
        .unwrap();
        let mut core = XrayCore::new(settings(&root, xray, root.join("missing-sing-box"))).unwrap();

        core.start(&config).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        assert!(!core.is_started());
        assert_eq!(core.core_kind(), None);
        core.stop();
    }
}
