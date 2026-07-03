use crate::config::Settings;
use crate::core::XrayCore;
use crate::xray_config::{ApiSettings, XrayConfig};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error as TlsError, ServerConfig,
    ServerConnection, SignatureScheme, StreamOwned,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use uuid::Uuid;

type TlsStream = StreamOwned<ServerConnection, TcpStream>;

#[derive(Debug)]
pub enum ServerError {
    Io(std::io::Error),
    Tls(String),
    Core(crate::core::CoreError),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Tls(error) => write!(f, "{error}"),
            Self::Core(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<std::io::Error> for ServerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<crate::core::CoreError> for ServerError {
    fn from(error: crate::core::CoreError) -> Self {
        Self::Core(error)
    }
}

#[derive(Debug)]
struct State {
    connected: bool,
    client_ip: Option<String>,
    session_id: Option<Uuid>,
    core: XrayCore,
}

#[derive(Deserialize)]
struct SessionBody {
    session_id: Uuid,
    #[serde(default)]
    config: String,
    inbounds: Option<Vec<String>>,
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct RequestHead {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    content_length: usize,
}

pub fn run(settings: Settings) -> Result<(), ServerError> {
    if settings.ssl_client_cert_file.is_none() {
        return Err(ServerError::Tls(
            "SSL_CLIENT_CERT_FILE is required for the REST node service.".to_owned(),
        ));
    }

    let tls_config = Arc::new(load_tls_config(&settings)?);
    let address = format!("{}:{}", settings.service_host, settings.service_port);
    let listener = TcpListener::bind(&address)?;
    let state = Arc::new(Mutex::new(State {
        connected: false,
        client_ip: None,
        session_id: None,
        core: XrayCore::new(settings.clone())?,
    }));

    eprintln!("Marzban-node listening on {address}");

    for stream in listener.incoming() {
        let stream = stream?;
        let peer_ip = stream
            .peer_addr()
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|_| "0.0.0.0".to_owned());
        let tls_config = Arc::clone(&tls_config);
        let state = Arc::clone(&state);
        let settings = settings.clone();

        thread::spawn(move || {
            if let Err(error) = handle_connection(stream, tls_config, state, settings, peer_ip) {
                eprintln!("connection error: {error}");
            }
        });
    }

    Ok(())
}

fn handle_connection(
    stream: TcpStream,
    tls_config: Arc<ServerConfig>,
    state: Arc<Mutex<State>>,
    settings: Settings,
    peer_ip: String,
) -> Result<(), ServerError> {
    let connection =
        ServerConnection::new(tls_config).map_err(|error| ServerError::Tls(error.to_string()))?;
    let mut stream = StreamOwned::new(connection, stream);
    let request = read_request(&mut stream)?;

    if request.path.starts_with("/logs") {
        handle_logs(stream, request, state)?;
        return Ok(());
    }

    let response = handle_rest(request, state, settings, peer_ip);
    stream.write_all(&response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn handle_rest(
    request: Request,
    state: Arc<Mutex<State>>,
    settings: Settings,
    peer_ip: String,
) -> HttpResponse {
    if request.method != "POST" {
        return HttpResponse::json(405, json!({"detail": "Method not allowed"}));
    }

    match request.path.as_str() {
        "/" => {
            let mut state = lock_or_500!(state);
            response_from_state(&mut state, None)
        }
        "/connect" => {
            let mut state = lock_or_500!(state);
            if state.connected && state.core.is_started() {
                state.core.stop();
            }
            let session_id = Uuid::new_v4();
            state.connected = true;
            state.client_ip = Some(peer_ip);
            state.session_id = Some(session_id);
            response_from_state(&mut state, Some(json!({"session_id": session_id})))
        }
        "/disconnect" => {
            let mut state = lock_or_500!(state);
            state.core.stop();
            state.connected = false;
            state.client_ip = None;
            state.session_id = None;
            response_from_state(&mut state, None)
        }
        "/ping" => match parse_session_body(&request)
            .and_then(|body| match_session(&state, body.session_id))
        {
            Ok(()) => HttpResponse::json(200, json!({})),
            Err(response) => response,
        },
        "/start" => {
            let body = match parse_session_body(&request) {
                Ok(body) => body,
                Err(response) => return response,
            };
            if let Err(response) = match_session(&state, body.session_id) {
                return response;
            }
            let api = match api_settings(&state, &settings, body.inbounds.as_ref()) {
                Ok(api) => api,
                Err(response) => return response,
            };
            let config = match XrayConfig::from_controller_json(&body.config, &api) {
                Ok(config) => config,
                Err(error) => {
                    return HttpResponse::json(
                        422,
                        json!({"detail": {"config": error.to_string()}}),
                    )
                }
            };
            let mut state = lock_or_500!(state);
            match state.core.start(&config) {
                Ok(()) => response_from_state(&mut state, None),
                Err(error) => HttpResponse::json(503, json!({"detail": error.to_string()})),
            }
        }
        "/stop" => {
            let body = match parse_session_body(&request) {
                Ok(body) => body,
                Err(response) => return response,
            };
            if let Err(response) = match_session(&state, body.session_id) {
                return response;
            }
            let mut state = lock_or_500!(state);
            state.core.stop();
            response_from_state(&mut state, None)
        }
        "/restart" => {
            let body = match parse_session_body(&request) {
                Ok(body) => body,
                Err(response) => return response,
            };
            if let Err(response) = match_session(&state, body.session_id) {
                return response;
            }
            let api = match api_settings(&state, &settings, body.inbounds.as_ref()) {
                Ok(api) => api,
                Err(response) => return response,
            };
            let config = match XrayConfig::from_controller_json(&body.config, &api) {
                Ok(config) => config,
                Err(error) => {
                    return HttpResponse::json(
                        422,
                        json!({"detail": {"config": error.to_string()}}),
                    )
                }
            };
            let mut state = lock_or_500!(state);
            match state.core.restart(&config) {
                Ok(()) => response_from_state(&mut state, None),
                Err(error) => HttpResponse::json(503, json!({"detail": error.to_string()})),
            }
        }
        _ => HttpResponse::json(404, json!({"detail": "Not found"})),
    }
}

macro_rules! lock_or_500 {
    ($state:expr) => {
        match $state.lock() {
            Ok(state) => state,
            Err(_) => return HttpResponse::json(500, json!({"detail": "State lock poisoned"})),
        }
    };
}

use lock_or_500;

fn parse_session_body(request: &Request) -> Result<SessionBody, HttpResponse> {
    serde_json::from_slice(&request.body)
        .map_err(|error| HttpResponse::json(422, json!({"detail": error.to_string()})))
}

fn match_session(state: &Arc<Mutex<State>>, session_id: Uuid) -> Result<(), HttpResponse> {
    let state = state
        .lock()
        .map_err(|_| HttpResponse::json(500, json!({"detail": "State lock poisoned"})))?;
    if state.session_id != Some(session_id) {
        return Err(HttpResponse::json(
            403,
            json!({"detail": "Session ID mismatch."}),
        ));
    }
    Ok(())
}

fn api_settings(
    state: &Arc<Mutex<State>>,
    settings: &Settings,
    controller_inbounds: Option<&Vec<String>>,
) -> Result<ApiSettings, HttpResponse> {
    let state = state
        .lock()
        .map_err(|_| HttpResponse::json(500, json!({"detail": "State lock poisoned"})))?;
    let peer_ip = state
        .client_ip
        .clone()
        .unwrap_or_else(|| "127.0.0.1".to_owned());
    Ok(ApiSettings {
        host: settings.xray_api_host.clone(),
        port: settings.xray_api_port,
        ssl_cert_file: settings.ssl_cert_file.to_string_lossy().into_owned(),
        ssl_key_file: settings.ssl_key_file.to_string_lossy().into_owned(),
        peer_ip,
        allowed_inbounds: controller_inbounds
            .cloned()
            .unwrap_or_else(|| settings.inbounds.clone()),
    })
}

fn response_from_state(state: &mut State, extra: Option<Value>) -> HttpResponse {
    let started = state.core.is_started();
    let xray_api = state.core.xray_api_available();
    let mut body = json!({
        "connected": state.connected,
        "started": started,
        "xray_api": xray_api,
        "core_kind": state.core.core_kind(),
        "core_version": state.core.version(),
        "features": ["controller_inbounds", "core_kind"],
    });
    if let Some(extra) = extra {
        merge_json(&mut body, extra);
    }
    HttpResponse::json(200, body)
}

fn merge_json(base: &mut Value, extra: Value) {
    if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
}

fn handle_logs(
    mut stream: TlsStream,
    request: Request,
    state: Arc<Mutex<State>>,
) -> Result<(), ServerError> {
    let session_id =
        query_param(&request.path, "session_id").and_then(|value| Uuid::parse_str(&value).ok());
    let Some(session_id) = session_id else {
        return close_websocket(&mut stream, 4400, "session_id should be a valid UUID.");
    };
    if let Err(response) = match_session(&state, session_id) {
        stream.write_all(&response.as_bytes())?;
        return Ok(());
    }

    let key = header(&request, "sec-websocket-key")
        .ok_or_else(|| ServerError::Tls("Missing Sec-WebSocket-Key".to_owned()))?;
    let accept = websocket_accept(&key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;

    let interval = query_param(&request.path, "interval")
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.7)
        .clamp(0.1, 10.0);
    let sleep = Duration::from_secs_f64(interval);
    let mut sent = 0usize;

    loop {
        let (still_connected, logs) = {
            let state = state
                .lock()
                .map_err(|_| ServerError::Tls("State lock poisoned".to_owned()))?;
            (
                state.session_id == Some(session_id),
                state.core.logs_snapshot(),
            )
        };
        if !still_connected {
            return close_websocket(&mut stream, 1000, "Session ended.");
        }

        if logs.len() > sent {
            let payload = logs[sent..].join("\n");
            sent = logs.len();
            write_ws_text(&mut stream, &payload)?;
        }
        thread::sleep(sleep);
    }
}

fn read_request(stream: &mut TlsStream) -> Result<Request, ServerError> {
    let mut buffer = Vec::with_capacity(4096);
    let header_end = loop {
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(ServerError::Tls(
                "connection closed before request".to_owned(),
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(pos) = find_header_end(&buffer) {
            break pos;
        }
        if buffer.len() > 64 * 1024 {
            return Err(ServerError::Tls("request headers too large".to_owned()));
        }
    };

    let head = parse_request_head(&buffer, header_end)?;

    let body_start = header_end + 4;
    let mut body = buffer.get(body_start..).unwrap_or_default().to_vec();
    while body.len() < head.content_length {
        let mut chunk = vec![0u8; head.content_length - body.len()];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(head.content_length);

    Ok(Request {
        method: head.method,
        path: head.path,
        headers: head.headers,
        body,
    })
}

fn parse_request_head(buffer: &[u8], header_end: usize) -> Result<RequestHead, ServerError> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    let status = parsed
        .parse(&buffer[..header_end + 4])
        .map_err(|error| ServerError::Tls(error.to_string()))?;
    if status.is_partial() {
        return Err(ServerError::Tls(
            "request headers are incomplete".to_owned(),
        ));
    }

    let headers: Vec<(String, String)> = parsed
        .headers
        .iter()
        .map(|header| {
            (
                header.name.to_ascii_lowercase(),
                String::from_utf8_lossy(header.value).into_owned(),
            )
        })
        .collect();
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);

    Ok(RequestHead {
        method: parsed.method.unwrap_or("").to_owned(),
        path: parsed.path.unwrap_or("").to_owned(),
        headers,
        content_length,
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

fn query_param(path: &str, name: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if key == name {
            Some(value.to_owned())
        } else {
            None
        }
    })
}

fn websocket_accept(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    BASE64.encode(hasher.finalize())
}

fn write_ws_text(stream: &mut TlsStream, payload: &str) -> Result<(), ServerError> {
    let bytes = payload.as_bytes();
    let mut frame = vec![0x81];
    match bytes.len() {
        len if len < 126 => frame.push(len as u8),
        len if len <= u16::MAX as usize => {
            frame.push(126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            frame.push(127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(bytes);
    stream.write_all(&frame)?;
    stream.flush()?;
    Ok(())
}

fn close_websocket(stream: &mut TlsStream, code: u16, reason: &str) -> Result<(), ServerError> {
    let mut payload = code.to_be_bytes().to_vec();
    payload.extend_from_slice(reason.as_bytes());
    let mut frame = vec![0x88, payload.len() as u8];
    frame.extend_from_slice(&payload);
    stream.write_all(&frame)?;
    stream.flush()?;
    Ok(())
}

struct HttpResponse {
    status: u16,
    body: Value,
}

impl HttpResponse {
    fn json(status: u16, body: Value) -> Self {
        Self { status, body }
    }

    fn as_bytes(&self) -> Vec<u8> {
        let body = self.body.to_string();
        let status_text = match self.status {
            200 => "OK",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            422 => "Unprocessable Entity",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "OK",
        };
        format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.status,
            status_text,
            body.len(),
            body
        )
        .into_bytes()
    }
}

fn load_tls_config(settings: &Settings) -> Result<ServerConfig, ServerError> {
    let certs = load_certs(&settings.ssl_cert_file)?;
    let key = load_key(&settings.ssl_key_file)?;

    let builder = ServerConfig::builder();
    let config = if let Some(client_cert_file) = &settings.ssl_client_cert_file {
        let client_certs = load_certs(client_cert_file)?;
        let verifier: Arc<dyn ClientCertVerifier> =
            Arc::new(ExactClientCertVerifier::new(client_certs));
        builder
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .map_err(|error| ServerError::Tls(error.to_string()))?
    } else {
        builder
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|error| ServerError::Tls(error.to_string()))?
    };

    Ok(config)
}

struct ExactClientCertVerifier {
    allowed_der: Vec<Vec<u8>>,
}

impl ExactClientCertVerifier {
    fn new(certs: Vec<CertificateDer<'static>>) -> Self {
        Self {
            allowed_der: certs.iter().map(|cert| cert.as_ref().to_vec()).collect(),
        }
    }
}

impl fmt::Debug for ExactClientCertVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExactClientCertVerifier")
            .field("allowed_certs", &self.allowed_der.len())
            .finish()
    }
}

impl ClientCertVerifier for ExactClientCertVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        if self
            .allowed_der
            .iter()
            .any(|allowed| allowed.as_slice() == end_entity.as_ref())
        {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(TlsError::InvalidCertificate(
                CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}

fn load_certs(path: &std::path::Path) -> Result<Vec<CertificateDer<'static>>, ServerError> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(ServerError::Io)
}

fn load_key(path: &std::path::Path) -> Result<PrivateKeyDer<'static>, ServerError> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::private_key(&mut reader)?
        .ok_or_else(|| ServerError::Tls(format!("No private key found in {}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_from_complete_http_head() {
        let raw = b"POST /ping HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 53\r\n\r\n{\"session_id\":\"42d7485e-de58-4360-870c-9fb9906e713e\"}";
        let head = parse_request_head(raw, find_header_end(raw).unwrap()).unwrap();

        assert_eq!(head.content_length, 53);
    }

    #[test]
    fn computes_websocket_accept_key() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn extracts_query_param() {
        assert_eq!(
            query_param("/logs?session_id=abc&interval=0.7", "session_id"),
            Some("abc".to_owned())
        );
        assert_eq!(query_param("/logs", "session_id"), None);
    }

    #[test]
    fn session_body_accepts_controller_selected_inbounds() {
        let request = Request {
            method: "POST".to_owned(),
            path: "/restart".to_owned(),
            headers: vec![],
            body: br#"{"session_id":"42d7485e-de58-4360-870c-9fb9906e713e","config":"{}","inbounds":["VLESS"]}"#.to_vec(),
        };

        let body = match parse_session_body(&request) {
            Ok(body) => body,
            Err(response) => panic!("unexpected error response: {}", response.status),
        };

        assert_eq!(body.inbounds, Some(vec!["VLESS".to_owned()]));
    }

    #[test]
    fn controller_selected_inbounds_override_env_inbounds() {
        let settings = Settings {
            service_host: "127.0.0.1".to_owned(),
            service_port: 62050,
            xray_api_host: "127.0.0.1".to_owned(),
            xray_api_port: 62051,
            xray_executable_path: "missing-xray".into(),
            xray_assets_path: "missing-assets".into(),
            sing_box_executable_path: "missing-sing-box".into(),
            ssl_cert_file: "cert.pem".into(),
            ssl_key_file: "key.pem".into(),
            ssl_client_cert_file: None,
            debug: false,
            inbounds: vec!["HY2".to_owned()],
        };
        let state = Arc::new(Mutex::new(State {
            connected: true,
            client_ip: Some("127.0.0.1".to_owned()),
            session_id: None,
            core: XrayCore::new(settings.clone()).unwrap(),
        }));

        let api = match api_settings(&state, &settings, Some(&vec!["VLESS".to_owned()])) {
            Ok(api) => api,
            Err(response) => panic!("unexpected error response: {}", response.status),
        };

        assert_eq!(api.allowed_inbounds, vec!["VLESS".to_owned()]);
    }

    #[test]
    fn state_response_reports_xray_api_availability() {
        let settings = Settings {
            service_host: "127.0.0.1".to_owned(),
            service_port: 62050,
            xray_api_host: "127.0.0.1".to_owned(),
            xray_api_port: 62051,
            xray_executable_path: "missing-xray".into(),
            xray_assets_path: "missing-assets".into(),
            sing_box_executable_path: "missing-sing-box".into(),
            ssl_cert_file: "cert.pem".into(),
            ssl_key_file: "key.pem".into(),
            ssl_client_cert_file: None,
            debug: false,
            inbounds: vec![],
        };
        let mut state = State {
            connected: true,
            client_ip: Some("127.0.0.1".to_owned()),
            session_id: None,
            core: XrayCore::new(settings).unwrap(),
        };

        let response = response_from_state(&mut state, None);

        assert_eq!(response.body["xray_api"], false);
        assert!(response.body.as_object().unwrap().contains_key("core_kind"));
        assert_eq!(response.body["core_kind"], Value::Null);
    }
}
