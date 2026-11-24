use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::Value;

use crate::error::SdkError;
use crate::pipe::{
    OverlappedPipe, connect_client_pipe, connect_server_pipe, create_server_pipe, discover_peers,
    pipe_path, EventHandle,
};
use crate::protocol::{EVENT_THEME_CHANGED, Message, PROTOCOL_VERSION};
use crate::security::{
    IntegrityInfo, PipeSecurityDescriptor, current_integrity_info, current_user_sid,
};
use crate::theme::ThemeConfig;

#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub module: String,
    pub version: String,
    pub provides: Vec<String>,
    pub wants: Vec<String>,
    pub allowlist: Option<Vec<String>>,
    pub initial_state: HashMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerInfo {
    pub module: String,
    pub version: String,
    pub pid: u32,
    pub elevated: bool,
    pub integrity: String,
    pub provides: Vec<String>,
    pub wants: Vec<String>,
}

#[derive(Debug)]
pub enum NipEvent {
    PeerConnected(PeerInfo),
    PeerDisconnected {
        module: String,
        graceful: bool,
    },
    StateReceived {
        from: String,
        cap: String,
        seq: u64,
        data: Value,
    },
    EventReceived {
        from: String,
        cap: Option<String>,
        name: String,
        data: Value,
    },
    Command {
        from: String,
        id: String,
        cap: String,
        name: String,
        data: Value,
        reply: ReplyHandle,
    },
    ThemeChanged(ThemeConfig),
}

/// One-shot reply handle for an incoming `NipEvent::Command`.
/// If dropped without calling `ok` or `err`, automatically sends an error reply.
#[derive(Debug)]
pub struct ReplyHandle {
    id: String,
    pipe: Arc<OverlappedPipe>,
    sent: bool,
}

impl ReplyHandle {
    pub fn ok(mut self, data: Value) -> Result<(), SdkError> {
        self.sent = true;
        let msg = Message::Reply {
            v: PROTOCOL_VERSION,
            id: self.id.clone(),
            ok: true,
            data: Some(data),
            error: None,
        };
        send_message(&self.pipe, &msg)
    }

    pub fn err(mut self, error: impl Into<String>) -> Result<(), SdkError> {
        self.sent = true;
        let msg = Message::Reply {
            v: PROTOCOL_VERSION,
            id: self.id.clone(),
            ok: false,
            data: None,
            error: Some(error.into()),
        };
        send_message(&self.pipe, &msg)
    }
}

impl Drop for ReplyHandle {
    fn drop(&mut self) {
        if !self.sent {
            let msg = Message::Reply {
                v: PROTOCOL_VERSION,
                id: self.id.clone(),
                ok: false,
                data: None,
                error: Some("command handler dropped without replying".to_string()),
            };
            let _ = send_message(&self.pipe, &msg);
        }
    }
}

struct PeerConn {
    info: PeerInfo,
    conn_id: u64,
    pipe: Arc<OverlappedPipe>,
    graceful_bye: AtomicBool,
}

#[derive(Clone)]
struct ReplyPayload {
    ok: bool,
    data: Option<Value>,
    error: Option<String>,
}

struct PendingCmd {
    target_module: String,
    tx: mpsc::Sender<ReplyPayload>,
}

struct SharedState {
    config: NodeConfig,
    user_sid: String,
    integrity: IntegrityInfo,
    pid: u32,
    stop_event: EventHandle,
    stopped: AtomicBool,
    next_id: AtomicU64,
    peers: Mutex<HashMap<String, Arc<PeerConn>>>,
    connecting: Mutex<HashSet<String>>,
    local_state: Mutex<HashMap<String, Value>>,
    remote_state: Mutex<HashMap<(String, String), Value>>,
    pending_cmds: Mutex<HashMap<String, PendingCmd>>,
    events_tx: mpsc::Sender<NipEvent>,
    state_seq: AtomicU64,
}

/// Peer-to-peer NIP endpoint managing discovery, handshake, capability registry,
/// overlapped named-pipe transport, and automatic reconnection.
pub struct NipNode {
    shared: Arc<SharedState>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl NipNode {
    pub fn start(config: NodeConfig) -> Result<(Self, mpsc::Receiver<NipEvent>), SdkError> {
        let user_sid = current_user_sid()?;
        let integrity = current_integrity_info();
        let sd = PipeSecurityDescriptor::same_user_medium_ml(&user_sid)?;

        let my_pipe_path = pipe_path(&config.module, &user_sid);
        let initial_server_pipe = create_server_pipe(&my_pipe_path, &sd)?;
        let stop_event = EventHandle::new_manual(false)?;
        let (events_tx, events_rx) = mpsc::channel();
        let initial_state = config.initial_state.clone();

        let shared = Arc::new(SharedState {
            config,
            user_sid,
            integrity,
            pid: std::process::id(),
            stop_event,
            stopped: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            peers: Mutex::new(HashMap::new()),
            connecting: Mutex::new(HashSet::new()),
            local_state: Mutex::new(initial_state),
            remote_state: Mutex::new(HashMap::new()),
            pending_cmds: Mutex::new(HashMap::new()),
            events_tx,
            state_seq: AtomicU64::new(1),
        });

        let mut threads = Vec::new();

        // 1. Server accept thread.
        let accept_shared = Arc::clone(&shared);
        struct SendHandle(windows::Win32::Foundation::HANDLE);
        unsafe impl Send for SendHandle {}
        let initial_handle = SendHandle(initial_server_pipe);
        threads.push(thread::spawn(move || {
            let initial_handle = initial_handle;
            let mut current_pipe = Some(initial_handle.0);
            while !accept_shared.stopped.load(Ordering::Acquire) {
                let pipe_handle = match current_pipe.take() {
                    Some(h) => h,
                    None => match create_server_pipe(&my_pipe_path, &sd) {
                        Ok(h) => h,
                        Err(_) => break,
                    },
                };

                match connect_server_pipe(pipe_handle, accept_shared.stop_event.0) {
                    Ok(true) => {
                        current_pipe = create_server_pipe(&my_pipe_path, &sd).ok();
                        if let Ok(pipe) = OverlappedPipe::new(pipe_handle) {
                            let pipe = Arc::new(pipe);
                            let s = Arc::clone(&accept_shared);
                            thread::spawn(move || {
                                handle_incoming_connection(s, pipe);
                            });
                        }
                    }
                    Ok(false) => {
                        let _ = OverlappedPipe::new(pipe_handle);
                        break;
                    }
                    Err(_) => {
                        let _ = OverlappedPipe::new(pipe_handle);
                        if !accept_shared.stopped.load(Ordering::Acquire) {
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            }
        }));

        // 2. Discovery + exponential-backoff reconnect loop.
        let disc_shared = Arc::clone(&shared);
        threads.push(thread::spawn(move || {
            let mut backoff_ms = 120u64;
            while !disc_shared.stopped.load(Ordering::Acquire) {
                if let Ok(found) = discover_peers(&disc_shared.config.module, &disc_shared.user_sid) {
                    let mut tried_new = false;
                    for peer in found {
                        let already = {
                            let peers = disc_shared.peers.lock().unwrap();
                            let connecting = disc_shared.connecting.lock().unwrap();
                            peers.contains_key(&peer.module) || connecting.contains(&peer.module)
                        };
                        if !already {
                            tried_new = true;
                            let s = Arc::clone(&disc_shared);
                            thread::spawn(move || {
                                connect_outbound(s, &peer.module, &peer.path);
                            });
                        }
                    }
                    backoff_ms = if tried_new {
                        120
                    } else {
                        (backoff_ms * 2).min(1200)
                    };
                }
                let mut waited = 0u64;
                while waited < backoff_ms && !disc_shared.stopped.load(Ordering::Acquire) {
                    let step = 40.min(backoff_ms - waited);
                    thread::sleep(Duration::from_millis(step));
                    waited += step;
                }
            }
        }));

        Ok((
            Self {
                shared,
                threads: Mutex::new(threads),
            },
            events_rx,
        ))
    }

    pub fn module_name(&self) -> &str {
        &self.shared.config.module
    }

    pub fn integrity_info(&self) -> &IntegrityInfo {
        &self.shared.integrity
    }

    pub fn peers(&self) -> Vec<PeerInfo> {
        self.shared
            .peers
            .lock()
            .unwrap()
            .values()
            .map(|p| p.info.clone())
            .collect()
    }

    pub fn is_peer_connected(&self, module: &str) -> bool {
        self.shared.peers.lock().unwrap().contains_key(module)
    }

    pub fn providers_of(&self, capability: &str) -> Vec<String> {
        self.shared
            .peers
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.info.provides.iter().any(|c| c == capability))
            .map(|p| p.info.module.clone())
            .collect()
    }

    pub fn has_capability(&self, capability: &str) -> bool {
        self.shared
            .peers
            .lock()
            .unwrap()
            .values()
            .any(|p| p.info.provides.iter().any(|c| c == capability))
    }

    pub fn latest_state(&self, peer_module: &str, cap: &str) -> Option<Value> {
        self.shared
            .remote_state
            .lock()
            .unwrap()
            .get(&(peer_module.to_string(), cap.to_string()))
            .cloned()
    }

    /// Updates local capability snapshot and pushes `State` to all connected peers that want `cap`.
    pub fn publish_state(&self, cap: &str, data: Value) -> Result<(), SdkError> {
        self.shared
            .local_state
            .lock()
            .unwrap()
            .insert(cap.to_string(), data.clone());

        let seq = self.shared.state_seq.fetch_add(1, Ordering::Relaxed);
        let msg = Message::State {
            v: PROTOCOL_VERSION,
            cap: cap.to_string(),
            seq,
            data,
        };
        let targets: Vec<Arc<PeerConn>> = self
            .shared
            .peers
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.info.wants.iter().any(|w| w == cap))
            .cloned()
            .collect();

        for conn in targets {
            let _ = send_message(&conn.pipe, &msg);
        }
        Ok(())
    }

    /// Sends an `Event` to a specific peer (`Some("wm")`) or broadcasts to all connected peers (`None`).
    pub fn emit_event(
        &self,
        target_module: Option<&str>,
        cap: Option<&str>,
        name: &str,
        data: Value,
    ) -> Result<(), SdkError> {
        let msg = Message::Event {
            v: PROTOCOL_VERSION,
            cap: cap.map(ToString::to_string),
            name: name.to_string(),
            data,
        };
        match target_module {
            Some(target) => {
                let conn = self
                    .shared
                    .peers
                    .lock()
                    .unwrap()
                    .get(target)
                    .cloned()
                    .ok_or_else(|| SdkError::PeerNotConnected(target.to_string()))?;
                send_message(&conn.pipe, &msg)
            }
            None => {
                let peers: Vec<Arc<PeerConn>> =
                    self.shared.peers.lock().unwrap().values().cloned().collect();
                for conn in peers {
                    let _ = send_message(&conn.pipe, &msg);
                }
                Ok(())
            }
        }
    }

    /// Broadcasts `theme.changed` with the given `ThemeConfig` to all connected peers.
    pub fn broadcast_theme_changed(&self, theme: &ThemeConfig) -> Result<(), SdkError> {
        let payload = serde_json::to_value(theme)?;
        self.emit_event(None, None, EVENT_THEME_CHANGED, payload)
    }

    /// Sends a command and awaits its `Reply` up to `timeout`, failing fast if the peer disconnects.
    pub fn send_cmd(
        &self,
        target_module: &str,
        cap: &str,
        name: &str,
        data: Value,
        timeout: Duration,
    ) -> Result<Value, SdkError> {
        let conn = self
            .shared
            .peers
            .lock()
            .unwrap()
            .get(target_module)
            .cloned()
            .ok_or_else(|| SdkError::PeerNotConnected(target_module.to_string()))?;

        let id = format!(
            "cmd-{}",
            self.shared.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = mpsc::channel();
        self.shared.pending_cmds.lock().unwrap().insert(
            id.clone(),
            PendingCmd {
                target_module: target_module.to_string(),
                tx,
            },
        );

        let msg = Message::Cmd {
            v: PROTOCOL_VERSION,
            id: id.clone(),
            cap: cap.to_string(),
            name: name.to_string(),
            data,
        };
        if let Err(e) = send_message(&conn.pipe, &msg) {
            self.shared.pending_cmds.lock().unwrap().remove(&id);
            return Err(e);
        }

        let res = rx.recv_timeout(timeout);
        self.shared.pending_cmds.lock().unwrap().remove(&id);
        match res {
            Ok(reply) if reply.ok => Ok(reply.data.unwrap_or(Value::Null)),
            Ok(reply) => Err(SdkError::CommandRejected(
                reply
                    .error
                    .unwrap_or_else(|| "command returned error".to_string()),
            )),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(SdkError::CommandTimeout {
                target: target_module.to_string(),
                name: name.to_string(),
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(SdkError::PeerNotConnected(target_module.to_string()))
            }
        }
    }

    pub fn shutdown(&self) {
        if self.shared.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        let peers: Vec<Arc<PeerConn>> = {
            let mut guard = self.shared.peers.lock().unwrap();
            guard.drain().map(|(_, v)| v).collect()
        };
        let bye = Message::Bye {
            v: PROTOCOL_VERSION,
            module: self.shared.config.module.clone(),
            reason: "shutdown".to_string(),
        };
        for conn in &peers {
            let _ = send_message(&conn.pipe, &bye);
            conn.pipe.close_handle();
        }
        self.shared.stop_event.set();
        self.shared.pending_cmds.lock().unwrap().clear();

        let threads: Vec<JoinHandle<()>> = self.threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
    }
}

impl Drop for NipNode {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn send_message(pipe: &OverlappedPipe, msg: &Message) -> Result<(), SdkError> {
    let bytes = msg.to_ndjson_line()?;
    pipe.write_all(&bytes)?;
    Ok(())
}

fn read_one_line(
    pipe: &OverlappedPipe,
    pending_buf: &mut Vec<u8>,
    stop_event: windows::Win32::Foundation::HANDLE,
) -> Option<String> {
    loop {
        if let Some(pos) = pending_buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = pending_buf.drain(..=pos).collect();
            let trimmed = String::from_utf8_lossy(&line_bytes)
                .trim_end_matches(&['\r', '\n'][..])
                .to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
            continue;
        }
        let mut chunk = [0u8; 4096];
        match pipe.read_chunk(&mut chunk, stop_event) {
            Ok(0) => return None,
            Ok(n) => pending_buf.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }
}

fn handle_incoming_connection(shared: Arc<SharedState>, pipe: Arc<OverlappedPipe>) {
    let mut read_buf = Vec::new();
    let Some(first_line) = read_one_line(&pipe, &mut read_buf, shared.stop_event.0) else {
        return;
    };
    let Ok(Message::Hello {
        v,
        module,
        version,
        proto,
        pid,
        elevated,
        integrity,
        provides,
        wants,
    }) = serde_json::from_str::<Message>(&first_line)
    else {
        return;
    };

    if v != PROTOCOL_VERSION || proto[0] > PROTOCOL_VERSION || proto[1] < PROTOCOL_VERSION {
        return;
    }

    let welcome = Message::Welcome {
        v: PROTOCOL_VERSION,
        module: shared.config.module.clone(),
        version: shared.config.version.clone(),
        proto: PROTOCOL_VERSION,
        pid: shared.pid,
        elevated: shared.integrity.elevated,
        integrity: shared.integrity.level.clone(),
        provides: shared.config.provides.clone(),
        wants: shared.config.wants.clone(),
    };
    if send_message(&pipe, &welcome).is_err() {
        return;
    }

    let info = PeerInfo {
        module,
        version,
        pid,
        elevated,
        integrity,
        provides,
        wants,
    };
    register_and_run_peer(shared, pipe, read_buf, info);
}

fn connect_outbound(shared: Arc<SharedState>, target_module: &str, path: &str) {
    {
        let mut connecting = shared.connecting.lock().unwrap();
        if !connecting.insert(target_module.to_string()) {
            return;
        }
    }
    struct ConnectingGuard {
        shared: Arc<SharedState>,
        module: String,
    }
    impl Drop for ConnectingGuard {
        fn drop(&mut self) {
            self.shared.connecting.lock().unwrap().remove(&self.module);
        }
    }
    let _guard = ConnectingGuard {
        shared: Arc::clone(&shared),
        module: target_module.to_string(),
    };

    if shared.peers.lock().unwrap().contains_key(target_module) {
        return;
    }

    let Ok(handle) = connect_client_pipe(path) else {
        return;
    };
    let Ok(pipe) = OverlappedPipe::new(handle) else {
        return;
    };
    let pipe = Arc::new(pipe);

    let hello = Message::Hello {
        v: PROTOCOL_VERSION,
        module: shared.config.module.clone(),
        version: shared.config.version.clone(),
        proto: [PROTOCOL_VERSION, PROTOCOL_VERSION],
        pid: shared.pid,
        elevated: shared.integrity.elevated,
        integrity: shared.integrity.level.clone(),
        provides: shared.config.provides.clone(),
        wants: shared.config.wants.clone(),
    };
    if send_message(&pipe, &hello).is_err() {
        return;
    }

    let mut read_buf = Vec::new();
    let Some(first_line) = read_one_line(&pipe, &mut read_buf, shared.stop_event.0) else {
        return;
    };
    let Ok(Message::Welcome {
        v,
        module,
        version,
        proto,
        pid,
        elevated,
        integrity,
        provides,
        wants,
    }) = serde_json::from_str::<Message>(&first_line)
    else {
        return;
    };
    if v != PROTOCOL_VERSION || proto != PROTOCOL_VERSION {
        return;
    }

    let info = PeerInfo {
        module,
        version,
        pid,
        elevated,
        integrity,
        provides,
        wants,
    };
    register_and_run_peer(shared, pipe, read_buf, info);
}

fn register_and_run_peer(
    shared: Arc<SharedState>,
    pipe: Arc<OverlappedPipe>,
    mut read_buf: Vec<u8>,
    info: PeerInfo,
) {
    let peer_module = info.module.clone();
    let conn_id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let conn = Arc::new(PeerConn {
        info: info.clone(),
        conn_id,
        pipe: Arc::clone(&pipe),
        graceful_bye: AtomicBool::new(false),
    });

    {
        let mut peers = shared.peers.lock().unwrap();
        if let Some(existing) = peers.get(&peer_module) {
            if existing.conn_id < conn_id {
                return;
            }
        }
        peers.insert(peer_module.clone(), Arc::clone(&conn));
    }

    let _ = shared.events_tx.send(NipEvent::PeerConnected(info.clone()));

    // Push initial state snapshots for every capability the peer wants that we provide.
    {
        let local = shared.local_state.lock().unwrap();
        for cap in &info.wants {
            if shared.config.provides.iter().any(|p| p == cap) {
                if let Some(snapshot) = local.get(cap) {
                    let seq = shared.state_seq.fetch_add(1, Ordering::Relaxed);
                    let msg = Message::State {
                        v: PROTOCOL_VERSION,
                        cap: cap.clone(),
                        seq,
                        data: snapshot.clone(),
                    };
                    let _ = send_message(&pipe, &msg);
                }
            }
        }
    }

    while !shared.stopped.load(Ordering::Acquire) {
        let Some(line) = read_one_line(&pipe, &mut read_buf, shared.stop_event.0) else {
            break;
        };
        let Ok(msg) = serde_json::from_str::<Message>(&line) else {
            continue;
        };
        match msg {
            Message::Event { cap, name, data, .. } => {
                if name == EVENT_THEME_CHANGED {
                    if let Ok(theme) = serde_json::from_value::<ThemeConfig>(data.clone()) {
                        let _ = shared.events_tx.send(NipEvent::ThemeChanged(theme));
                        continue;
                    }
                }
                let _ = shared.events_tx.send(NipEvent::EventReceived {
                    from: peer_module.clone(),
                    cap,
                    name,
                    data,
                });
            }
            Message::State { cap, seq, data, .. } => {
                shared
                    .remote_state
                    .lock()
                    .unwrap()
                    .insert((peer_module.clone(), cap.clone()), data.clone());
                let _ = shared.events_tx.send(NipEvent::StateReceived {
                    from: peer_module.clone(),
                    cap,
                    seq,
                    data,
                });
            }
            Message::Cmd {
                id,
                cap,
                name,
                data,
                ..
            } => {
                if let Some(allowlist) = &shared.config.allowlist {
                    if !allowlist.iter().any(|allowed| allowed == &name) {
                        let reply = Message::Reply {
                            v: PROTOCOL_VERSION,
                            id,
                            ok: false,
                            data: None,
                            error: Some(format!("command '{name}' rejected by allowlist")),
                        };
                        let _ = send_message(&pipe, &reply);
                        continue;
                    }
                }
                let _ = shared.events_tx.send(NipEvent::Command {
                    from: peer_module.clone(),
                    id: id.clone(),
                    cap,
                    name,
                    data,
                    reply: ReplyHandle {
                        id,
                        pipe: Arc::clone(&pipe),
                        sent: false,
                    },
                });
            }
            Message::Reply {
                id, ok, data, error, ..
            } => {
                if let Some(pending) = shared.pending_cmds.lock().unwrap().remove(&id) {
                    let _ = pending.tx.send(ReplyPayload { ok, data, error });
                }
            }
            Message::Bye { .. } => {
                conn.graceful_bye.store(true, Ordering::Release);
                break;
            }
            Message::Hello { .. } | Message::Welcome { .. } => {}
        }
    }

    let removed_current = {
        let mut peers = shared.peers.lock().unwrap();
        if peers
            .get(&peer_module)
            .is_some_and(|cur| cur.conn_id == conn_id)
        {
            peers.remove(&peer_module);
            true
        } else {
            false
        }
    };

    if removed_current {
        // Fail fast any in-flight commands targeting this peer.
        shared
            .pending_cmds
            .lock()
            .unwrap()
            .retain(|_, p| p.target_module != peer_module);

        if !shared.stopped.load(Ordering::Acquire) {
            let graceful = conn.graceful_bye.load(Ordering::Acquire);
            let _ = shared.events_tx.send(NipEvent::PeerDisconnected {
                module: peer_module,
                graceful,
            });
        }
    }
}
