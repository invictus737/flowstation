use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tungstenite::{
    Message, accept_hdr,
    handshake::server::{ErrorResponse, Request, Response},
};

use crate::net_control::commands::ControlCommand;
use crate::net_dashboard::html::DASHBOARD_HTML;
use crate::net_dashboard::state::{CallEntry, DashboardState, DashboardStateInner, MsEntry};
use crate::net_telemetry::TelemetryEvent;

type CmdSender = crossbeam_channel::Sender<ControlCommand>;

// Each WS connection registers a Sender here.
// broadcast() sends to all of them; dead connections are pruned automatically.
type WsBroadcastTx = crossbeam_channel::Sender<String>;
type WsClients = Arc<Mutex<Vec<WsBroadcastTx>>>;

pub struct DashboardServer {
    pub state: DashboardState,
    clients: WsClients,
    config_path: String,
    cmd_tx: Option<CmdSender>,
    allow_config_api: bool,
    allow_control_commands: bool,
    auth_token: Option<String>,
}

impl DashboardServer {
    pub fn new(config_path: String, allow_config_api: bool, allow_control_commands: bool, auth_token: Option<String>) -> Self {
        Self {
            state: Arc::new(RwLock::new(DashboardStateInner::new(config_path.clone()))),
            clients: Arc::new(Mutex::new(Vec::new())),
            config_path,
            cmd_tx: None,
            allow_config_api,
            allow_control_commands,
            auth_token,
        }
    }

    pub fn set_cmd_sender(&mut self, tx: CmdSender) {
        self.cmd_tx = Some(tx);
    }

    pub fn start(&mut self, bind: &str, port: u16) {
        let addr = format!("{}:{}", bind, port);
        let state = Arc::clone(&self.state);
        let clients = Arc::clone(&self.clients);
        let config_path = self.config_path.clone();
        let cmd_tx: Arc<Mutex<Option<CmdSender>>> = Arc::new(Mutex::new(self.cmd_tx.take()));
        let allow_config_api = self.allow_config_api;
        let allow_control_commands = self.allow_control_commands;
        let auth_token = self.auth_token.clone();

        std::thread::Builder::new()
            .name("dashboard-server".into())
            .spawn(move || {
                let listener = match TcpListener::bind(&addr) {
                    Ok(l) => {
                        tracing::info!("Dashboard listening on http://{}", addr);
                        l
                    }
                    Err(e) => {
                        tracing::error!("Dashboard failed to bind {}: {}", addr, e);
                        return;
                    }
                };
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let state = Arc::clone(&state);
                    let clients = Arc::clone(&clients);
                    let config_path = config_path.clone();
                    let cmd_tx = Arc::clone(&cmd_tx);
                    let auth_token = auth_token.clone();
                    std::thread::Builder::new()
                        .name("dashboard-conn".into())
                        .spawn(move || {
                            handle_connection(
                                stream,
                                state,
                                clients,
                                config_path,
                                cmd_tx,
                                allow_config_api,
                                allow_control_commands,
                                auth_token,
                            )
                        })
                        .ok();
                }
            })
            .expect("failed to spawn dashboard thread");
    }

    pub fn handle_telemetry(&self, event: TelemetryEvent) {
        let msg = event_to_ws_msg(&event);
        {
            let mut s = self.state.write().unwrap();
            match &event {
                TelemetryEvent::MsRegistration { issi } => {
                    s.ms_map.insert(
                        *issi,
                        MsEntry {
                            issi: *issi,
                            groups: Vec::new(),
                            rssi_dbfs: None,
                            registered_at: Instant::now(),
                            last_seen: Instant::now(),
                            energy_saving_mode: 0,
                        },
                    );
                    s.push_log("INFO", format!("MS {} registered", issi));
                }
                TelemetryEvent::MsDeregistration { issi } => {
                    s.ms_map.remove(issi);
                    s.push_log("INFO", format!("MS {} deregistered", issi));
                }
                TelemetryEvent::MsGroupAttach { issi, gssis } => {
                    if let Some(e) = s.ms_map.get_mut(issi) {
                        for g in gssis {
                            if !e.groups.contains(g) {
                                e.groups.push(*g);
                            }
                        }
                    }
                }
                TelemetryEvent::MsGroupsSnapshot { issi, gssis } => {
                    if let Some(e) = s.ms_map.get_mut(issi) {
                        e.groups = gssis.clone();
                    }
                }
                TelemetryEvent::MsGroupDetach { issi, gssis } => {
                    if let Some(e) = s.ms_map.get_mut(issi) {
                        e.groups.retain(|g| !gssis.contains(g));
                    }
                }
                TelemetryEvent::MsRssi { issi, rssi_dbfs } => {
                    if let Some(e) = s.ms_map.get_mut(issi) {
                        e.rssi_dbfs = Some(*rssi_dbfs);
                        e.last_seen = Instant::now();
                    }
                }
                TelemetryEvent::MsEnergySaving { issi, mode } => {
                    if let Some(e) = s.ms_map.get_mut(issi) {
                        e.energy_saving_mode = *mode;
                    }
                }
                TelemetryEvent::GroupCallStarted {
                    call_id,
                    gssi,
                    caller_issi,
                } => {
                    s.calls.insert(
                        *call_id,
                        CallEntry {
                            call_id: *call_id,
                            is_group: true,
                            gssi: *gssi,
                            caller_issi: *caller_issi,
                            called_issi: 0,
                            speaker_issi: Some(*caller_issi),
                            started_at: Instant::now(),
                            simplex: false,
                        },
                    );
                    s.push_last_heard(*caller_issi, "call_group", *gssi);
                    s.push_log("INFO", format!("Group call {} started: {} -> GSSI {}", call_id, caller_issi, gssi));
                }
                TelemetryEvent::GroupCallEnded { call_id, gssi: _ } => {
                    s.calls.remove(call_id);
                    s.push_log("INFO", format!("Group call {} ended", call_id));
                }
                TelemetryEvent::GroupCallSpeakerChanged {
                    call_id,
                    gssi,
                    speaker_issi,
                } => {
                    if let Some(c) = s.calls.get_mut(call_id) {
                        c.speaker_issi = Some(*speaker_issi);
                    }
                    s.push_last_heard(*speaker_issi, "call_group", *gssi);
                }
                TelemetryEvent::IndividualCallStarted {
                    call_id,
                    calling_issi,
                    called_issi,
                    simplex,
                } => {
                    s.calls.insert(
                        *call_id,
                        CallEntry {
                            call_id: *call_id,
                            is_group: false,
                            gssi: 0,
                            caller_issi: *calling_issi,
                            called_issi: *called_issi,
                            speaker_issi: None,
                            started_at: Instant::now(),
                            simplex: *simplex,
                        },
                    );
                    s.push_last_heard(*calling_issi, "call_individual", *called_issi);
                    s.push_log("INFO", format!("P2P call {} started: {} -> {}", call_id, calling_issi, called_issi));
                }
                TelemetryEvent::IndividualCallEnded { call_id } => {
                    s.calls.remove(call_id);
                    s.push_log("INFO", format!("P2P call {} ended", call_id));
                }
                TelemetryEvent::BrewConnected { connected } => {
                    s.brew_online = *connected;
                }
                TelemetryEvent::SdsActivity { source_issi, dest_issi } => {
                    s.push_last_heard(*source_issi, "sds", *dest_issi);
                }
            }
        }
        if let Some(json) = msg {
            self.broadcast(&json);
        }
    }

    pub fn push_log(&self, level: &str, msg: String) {
        let entry = {
            let mut s = self.state.write().unwrap();
            s.push_log(level, msg);
            s.log_ring.back().cloned()
        };
        if let Some(entry) = entry {
            if let Ok(json) = serde_json::to_string(&serde_json::json!({
                "type": "log", "ts": entry.ts, "level": entry.level, "msg": entry.msg
            })) {
                self.broadcast(&json);
            }
        }
    }

    fn broadcast(&self, msg: &str) {
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|tx| tx.send(msg.to_owned()).is_ok());
    }
}

fn event_to_ws_msg(event: &TelemetryEvent) -> Option<String> {
    let v = match event {
        TelemetryEvent::MsRegistration { issi } => serde_json::json!({"type":"ms_registered","issi":issi}),
        TelemetryEvent::MsDeregistration { issi } => serde_json::json!({"type":"ms_deregistered","issi":issi}),
        TelemetryEvent::MsGroupAttach { issi, gssis } => serde_json::json!({"type":"ms_groups","issi":issi,"groups":gssis}),
        TelemetryEvent::MsGroupDetach { issi, gssis } => serde_json::json!({"type":"ms_groups_detach","issi":issi,"groups":gssis}),
        TelemetryEvent::MsGroupsSnapshot { issi, gssis } => serde_json::json!({"type":"ms_groups_all","issi":issi,"groups":gssis}),
        TelemetryEvent::MsRssi { issi, rssi_dbfs } => serde_json::json!({"type":"ms_rssi","issi":issi,"rssi_dbfs":rssi_dbfs}),
        TelemetryEvent::MsEnergySaving { issi, mode } => serde_json::json!({"type":"ms_energy_saving","issi":issi,"mode":mode}),
        TelemetryEvent::GroupCallStarted {
            call_id,
            gssi,
            caller_issi,
        } => {
            serde_json::json!({"type":"call_started","call_id":call_id,"call_type":"group","gssi":gssi,"caller_issi":caller_issi,"last_heard":{"issi":caller_issi,"activity":"call_group","dest":gssi}})
        }
        TelemetryEvent::GroupCallEnded { call_id, gssi: _ } => serde_json::json!({"type":"call_ended","call_id":call_id}),
        TelemetryEvent::GroupCallSpeakerChanged {
            call_id,
            gssi,
            speaker_issi,
        } => {
            serde_json::json!({"type":"speaker_changed","call_id":call_id,"speaker_issi":speaker_issi,"last_heard":{"issi":speaker_issi,"activity":"call_group","dest":gssi}})
        }
        TelemetryEvent::IndividualCallStarted {
            call_id,
            calling_issi,
            called_issi,
            simplex,
        } => {
            serde_json::json!({"type":"call_started","call_id":call_id,"call_type":"individual","caller_issi":calling_issi,"called_issi":called_issi,"simplex":simplex,"last_heard":{"issi":calling_issi,"activity":"call_individual","dest":called_issi}})
        }
        TelemetryEvent::IndividualCallEnded { call_id } => serde_json::json!({"type":"call_ended","call_id":call_id}),
        TelemetryEvent::BrewConnected { connected } => serde_json::json!({"type":"brew_status","connected":connected}),
        TelemetryEvent::SdsActivity { source_issi, dest_issi } => {
            serde_json::json!({"type":"last_heard","issi":source_issi,"activity":"sds","dest":dest_issi})
        }
    };
    serde_json::to_string(&v).ok()
}

#[derive(Default)]
struct HttpHeaders {
    content_length: usize,
    token: Option<String>,
}

fn read_http_headers<R: BufRead>(buf: &mut R) -> HttpHeaders {
    let mut headers = HttpHeaders::default();
    loop {
        let mut line = String::new();
        if buf.read_line(&mut line).is_err() {
            break;
        }
        if line == "\r\n" || line.is_empty() || line == "\n" {
            break;
        }

        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().trim_end_matches("\r\n").trim_end_matches('\n').trim();
        match name.as_str() {
            "content-length" => {
                headers.content_length = value.parse().unwrap_or(0);
            }
            "x-dashboard-token" => {
                headers.token = Some(value.to_string());
            }
            "authorization" => {
                if let Some(token) = value.strip_prefix("Bearer ") {
                    headers.token = Some(token.trim().to_string());
                }
            }
            _ => {}
        }
    }
    headers
}

fn is_authorized(provided: &Option<String>, expected: &Option<String>) -> bool {
    match expected {
        Some(expected) => provided.as_deref() == Some(expected.as_str()),
        None => true,
    }
}

fn ws_request_authorized(req: &Request, expected: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    let Some(query) = req.uri().query() else {
        return false;
    };
    query
        .split('&')
        .filter_map(|item| item.split_once('='))
        .any(|(key, value)| key == "token" && value == expected)
}

fn atomic_write_config(config_path: &str, body: &[u8]) -> std::io::Result<()> {
    let path = Path::new(config_path);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("config.toml");
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp_path = dir.join(format!(".{}.tmp.{}.{}", file_name, std::process::id(), nonce));
    let bak_path = dir.join(format!("{}.bak", file_name));

    {
        let mut tmp = File::create(&tmp_path)?;
        tmp.write_all(body)?;
        tmp.sync_all()?;
    }

    if path.exists() {
        let _ = fs::copy(path, &bak_path);
    }
    fs::rename(&tmp_path, path)?;
    if let Ok(dir_file) = File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

fn handle_connection(
    stream: TcpStream,
    state: DashboardState,
    clients: WsClients,
    config_path: String,
    cmd_tx: Arc<Mutex<Option<CmdSender>>>,
    allow_config_api: bool,
    allow_control_commands: bool,
    auth_token: Option<String>,
) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));

    let mut peek_buf = [0u8; 256];
    let n = match stream.peek(&mut peek_buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    let peek_str = String::from_utf8_lossy(&peek_buf[..n]);
    let req_line = peek_str.lines().next().unwrap_or("").to_string();

    if req_line.contains("/ws") {
        handle_ws(stream, state, clients, cmd_tx, allow_control_commands, auth_token);
    } else if req_line.contains("GET /api/config") {
        let mut buf = BufReader::new(stream);
        let headers = read_http_headers(&mut buf);
        if allow_config_api {
            if is_authorized(&headers.token, &auth_token) {
                serve_config_get(buf.into_inner(), &config_path);
            } else {
                http_response(buf.into_inner(), 401, "dashboard auth required");
            }
        } else {
            http_response(buf.into_inner(), 403, "dashboard config API disabled");
        }
    } else if req_line.contains("POST /api/config") {
        let mut buf = BufReader::new(stream);
        let headers = read_http_headers(&mut buf);
        if !allow_config_api {
            http_response(buf.into_inner(), 403, "dashboard config API disabled");
            return;
        }
        if !is_authorized(&headers.token, &auth_token) {
            http_response(buf.into_inner(), 401, "dashboard auth required");
            return;
        }
        const MAX_CONFIG_BODY_BYTES: usize = 256 * 1024;
        let content_length = headers.content_length;
        if content_length == 0 || content_length > MAX_CONFIG_BODY_BYTES {
            http_response(buf.into_inner(), 413, "invalid config body size");
            return;
        }
        let mut body = vec![0u8; content_length];
        if let Err(e) = buf.read_exact(&mut body) {
            http_response(buf.into_inner(), 400, &format!("failed reading request body: {}", e));
            return;
        }
        let body_str = match String::from_utf8(body) {
            Ok(s) => s,
            Err(e) => {
                http_response(buf.into_inner(), 400, &format!("config must be UTF-8: {}", e));
                return;
            }
        };
        if let Err(e) = tetra_config::bluestation::from_toml_str(&body_str) {
            http_response(buf.into_inner(), 400, &format!("invalid config: {}", e));
            return;
        }
        match atomic_write_config(&config_path, body_str.as_bytes()) {
            Ok(_) => http_response(buf.into_inner(), 200, "OK"),
            Err(e) => http_response(buf.into_inner(), 500, &e.to_string()),
        }
    } else {
        let mut buf = BufReader::new(stream);
        loop {
            let mut line = String::new();
            let _ = buf.read_line(&mut line);
            if line == "\r\n" || line.is_empty() || line == "\n" {
                break;
            }
        }
        serve_html(buf.into_inner());
    }
}

fn handle_ws(
    stream: TcpStream,
    state: DashboardState,
    clients: WsClients,
    cmd_tx: Arc<Mutex<Option<CmdSender>>>,
    allow_control_commands: bool,
    auth_token: Option<String>,
) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(50)));

    let callback = |req: &Request, res: Response| -> Result<Response, ErrorResponse> {
        if auth_token.is_some() && !ws_request_authorized(req, auth_token.as_deref()) {
            return Err(ErrorResponse::new(Some("401 Unauthorized".to_string())));
        }
        Ok(res)
    };
    let mut ws = match accept_hdr(stream, callback) {
        Ok(w) => w,
        Err(e) => {
            tracing::debug!("WS handshake failed: {}", e);
            return;
        }
    };

    // Register this connection for broadcasts
    let (broadcast_tx, broadcast_rx) = crossbeam_channel::unbounded::<String>();
    {
        let mut c = clients.lock().unwrap();
        c.push(broadcast_tx);
    }

    // Send initial snapshot
    {
        let s = state.read().unwrap();
        let ms = s.snapshot_ms();
        let calls = s.snapshot_calls();
        let logs: Vec<_> = s.log_ring.iter().cloned().collect();
        let last_heard: Vec<_> = s.last_heard.iter().cloned().collect();
        drop(s);
        let brew_online = state.read().unwrap().brew_online;
        if let Ok(json) = serde_json::to_string(&serde_json::json!({
            "type": "snapshot", "ms": ms, "calls": calls, "log": logs,
            "brew_online": brew_online, "last_heard": last_heard
        })) {
            let _ = ws.send(Message::Text(json));
        }
    }

    let _ = ws.get_ref().set_read_timeout(Some(std::time::Duration::from_millis(20)));

    loop {
        // Drain outbound broadcast messages first
        while let Ok(msg) = broadcast_rx.try_recv() {
            if ws.send(Message::Text(msg)).is_err() {
                return;
            }
        }

        // Then check for inbound messages from browser
        match ws.read() {
            Ok(Message::Text(text)) => {
                if allow_control_commands {
                    handle_ws_command(&text, &state, &cmd_tx);
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(data)) => {
                let _ = ws.send(Message::Pong(data));
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
}

fn handle_ws_command(text: &str, state: &DashboardState, cmd_tx: &Arc<Mutex<Option<CmdSender>>>) {
    const MAX_WS_COMMAND_BYTES: usize = 4096;
    const MAX_SDS_BYTES: usize = 140;
    const MAX_TETRA_SSI: u64 = 0x00FF_FFFF;

    if text.len() > MAX_WS_COMMAND_BYTES {
        tracing::warn!("Dashboard: dropping oversized WS command ({} bytes)", text.len());
        return;
    }

    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };

    let send_cmd = |cmd: ControlCommand| -> bool {
        if let Ok(guard) = cmd_tx.lock() {
            if let Some(ref tx) = *guard {
                return tx.send(cmd).is_ok();
            }
        }
        false
    };

    match v.get("type").and_then(|t| t.as_str()) {
        Some("kick") => {
            let Some(issi_raw) = v.get("issi").and_then(|i| i.as_u64()) else {
                return;
            };
            if issi_raw == 0 || issi_raw > MAX_TETRA_SSI {
                return;
            }
            let issi = issi_raw as u32;
            tracing::info!("Dashboard: kick ISSI {}", issi);
            if !send_cmd(ControlCommand::KickMs { issi }) {
                tracing::warn!("Dashboard: no control dispatcher for kick");
            }
            let mut s = state.write().unwrap();
            s.push_log("INFO", format!("Kick requested for ISSI {}", issi));
        }
        Some("restart") => {
            tracing::warn!("Dashboard: restart command ignored; service control is disabled in the embedded dashboard");
        }
        Some("shutdown") => {
            tracing::warn!("Dashboard: shutdown command ignored; service control is disabled in the embedded dashboard");
        }
        Some("sds") => {
            let Some(dest_raw) = v.get("dest_issi").and_then(|i| i.as_u64()) else {
                return;
            };
            let msg_text = v.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
            if dest_raw == 0 || dest_raw > MAX_TETRA_SSI || msg_text.is_empty() {
                return;
            }
            let payload = msg_text.as_bytes().to_vec();
            if payload.len() > MAX_SDS_BYTES {
                tracing::warn!("Dashboard: dropping oversized SDS payload ({} bytes)", payload.len());
                return;
            }
            let Some(len_bits) = payload.len().checked_mul(8).and_then(|len| u16::try_from(len).ok()) else {
                return;
            };
            let dest = dest_raw as u32;
            tracing::info!("Dashboard: SDS to {} = {}", dest, msg_text);
            send_cmd(ControlCommand::SendSds {
                handle: 0,
                source_ssi: 9999, // BS dispatcher ISSI
                dest_ssi: dest,
                dest_is_group: false,
                len_bits,
                payload,
            });
            let mut s = state.write().unwrap();
            s.push_log("INFO", format!("SDS sent to {}: {}", dest, msg_text));
        }
        _ => {}
    }
}

fn serve_html(mut stream: TcpStream) {
    let body = DASHBOARD_HTML.replace("{{STACK_VERSION}}", tetra_core::STACK_VERSION);
    let body = body.as_bytes();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

fn serve_config_get(mut stream: TcpStream, config_path: &str) {
    match std::fs::read_to_string(config_path) {
        Ok(content) => {
            let body = content.as_bytes();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(body);
        }
        Err(e) => http_response(stream, 500, &e.to_string()),
    }
}

fn http_response(mut stream: TcpStream, code: u16, body: &str) {
    let status = if code == 200 { "OK" } else { "Error" };
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        code,
        status,
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
}
