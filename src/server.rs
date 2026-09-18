//! The public-facing relay server.
//!
//! Both agents and controllers connect outbound to this process.  A controller
//! asks the server to deliver a payload to one named agent; the server assigns
//! a route id and relays opaque payloads back and forth.  The server therefore
//! never runs user commands and never persists any configuration.

use crate::proto::{Frame, Payload, PeerInfo, ROLE_AGENT, ROLE_CTRL};
use crate::util::{now_ms, resolve_addr};
use crate::wire::Conn;
use std::collections::HashMap;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread;

struct AgentEntry {
    info_name: String,
    version: String,
    os: String,
    arch: String,
    connected_at_ms: u64,
    last_seen_ms: AtomicU64,
    conn: Arc<Conn>,
}

#[derive(Clone)]
struct RouteEntry {
    ctrl_tx: SyncSender<Vec<u8>>,
    ctrl_id: u64,
    agent: String,
    req_id: u64,
}

struct State {
    agents: Mutex<HashMap<String, Arc<AgentEntry>>>,
    routes: Mutex<HashMap<u64, RouteEntry>>,
    by_req: Mutex<HashMap<(u64, u64), u64>>,
    next_route: AtomicU64,
    next_conn: AtomicU64,
}

impl State {
    fn new() -> Self {
        Self {
            agents: Mutex::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            by_req: Mutex::new(HashMap::new()),
            next_route: AtomicU64::new(first_route_id()),
            next_conn: AtomicU64::new(1),
        }
    }
}

fn first_route_id() -> u64 {
    // Avoid the reserved 0 value and make collisions with a stale client-side
    // request id improbable.
    now_ms().wrapping_mul(1_000_003).max(1)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@' | b':' | b'+' | b'~')
        })
}

/// Serve forever.  `bind` is an `IP:port` string; `key` is the 32-byte PSK.
pub fn serve(bind: &str, key: [u8; 32]) -> io::Result<()> {
    let addr = resolve_addr(bind)?;
    let listener = TcpListener::bind(addr)?;
    let actual = listener.local_addr()?;
    eprintln!("apb server listening on {actual}");
    let state = Arc::new(State::new());
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let state = state.clone();
                let cid = state.next_conn.fetch_add(1, Ordering::Relaxed);
                thread::Builder::new()
                    .name(format!("apb-conn-{cid}"))
                    .spawn(move || {
                        if let Err(e) = handle_connection(s, key, state.clone(), cid) {
                            eprintln!("apb server: connection {cid}: {e}");
                        }
                    })?;
            }
            Err(e) => eprintln!("apb server: accept: {e}"),
        }
    }
    Ok(())
}

fn send_frame(conn: &Conn, frame: Frame) -> io::Result<()> {
    conn.send(frame.encode())
}

fn send_ctrl_error(tx: &SyncSender<Vec<u8>>, id: u64, code: u16, msg: impl Into<String>) {
    let payload = Payload::Error {
        id,
        code,
        message: msg.into(),
    }
    .encode();
    let _ = tx.send(Frame::RelayResult { route: 0, payload }.encode());
}

fn choose_agent(state: &State, target: &str) -> Result<String, String> {
    let agents = state
        .agents
        .lock()
        .map_err(|_| "agents lock poisoned".to_string())?;
    if !target.is_empty() {
        if agents.contains_key(target) {
            return Ok(target.to_string());
        }
        let mut names: Vec<&str> = agents.keys().map(|s| s.as_str()).collect();
        names.sort_unstable();
        return Err(format!(
            "agent `{target}` is not connected (available: {})",
            names.join(", ")
        ));
    }
    match agents.len() {
        0 => Err("no agent is connected".into()),
        1 => Ok(agents.keys().next().unwrap().clone()),
        _ => {
            let mut names: Vec<&str> = agents.keys().map(|s| s.as_str()).collect();
            names.sort_unstable();
            Err(format!(
                "multiple agents are connected; pass a name (available: {})",
                names.join(", ")
            ))
        }
    }
}

fn relay_to_agent(
    state: &Arc<State>,
    ctrl_id: u64,
    ctrl_tx: &SyncSender<Vec<u8>>,
    target: &str,
    payload: Vec<u8>,
) {
    let req_id = match Payload::peek_id(&payload) {
        Some(id) => id,
        None => 0,
    };

    // First, try to continue an existing logical route.  Push/pull streams send
    // many continuation frames with an empty target, so this lookup must happen
    // before default-agent selection.
    let route_and_agent: Option<(u64, String)> = state
        .by_req
        .lock()
        .ok()
        .and_then(|m| m.get(&(ctrl_id, req_id)).copied())
        .and_then(|route| {
            state
                .routes
                .lock()
                .ok()
                .and_then(|m| m.get(&route).map(|r| (route, r.agent.clone())))
        });
    if let Some((route, agent_name)) = route_and_agent {
        let agent_conn = state
            .agents
            .lock()
            .ok()
            .and_then(|m| m.get(&agent_name).map(|a| a.conn.clone()));
        match agent_conn {
            Some(conn) => {
                let frame = Frame::RelayDeliver { route, payload }.encode();
                if conn.send(frame).is_err() {
                    remove_route(state, route);
                    send_ctrl_error(
                        ctrl_tx,
                        req_id,
                        410,
                        format!("agent `{agent_name}` is unreachable"),
                    );
                }
                return;
            }
            None => {
                remove_route(state, route);
                send_ctrl_error(
                    ctrl_tx,
                    req_id,
                    410,
                    format!("agent `{agent_name}` disconnected"),
                );
                return;
            }
        }
    }

    // New logical route.
    let agent_name = match choose_agent(state, target) {
        Ok(n) => n,
        Err(e) => {
            send_ctrl_error(ctrl_tx, req_id, 404, e);
            return;
        }
    };
    let agent_conn = {
        let agents = match state.agents.lock() {
            Ok(g) => g,
            Err(_) => {
                send_ctrl_error(ctrl_tx, req_id, 500, "server state poisoned");
                return;
            }
        };
        match agents.get(&agent_name) {
            Some(a) => a.conn.clone(),
            None => {
                send_ctrl_error(
                    ctrl_tx,
                    req_id,
                    404,
                    format!("agent `{agent_name}` disconnected"),
                );
                return;
            }
        }
    };
    let route = state.next_route.fetch_add(1, Ordering::Relaxed).max(1);
    let entry = RouteEntry {
        ctrl_tx: ctrl_tx.clone(),
        ctrl_id,
        agent: agent_name.clone(),
        req_id,
    };
    if let Ok(mut routes) = state.routes.lock() {
        routes.insert(route, entry);
    }
    if let Ok(mut by_req) = state.by_req.lock() {
        by_req.insert((ctrl_id, req_id), route);
    }
    let frame = Frame::RelayDeliver { route, payload }.encode();
    if agent_conn.send(frame).is_err() {
        remove_route(state, route);
        send_ctrl_error(
            ctrl_tx,
            req_id,
            410,
            format!("agent `{agent_name}` is unreachable"),
        );
    }
}

fn remove_route(state: &Arc<State>, route: u64) -> Option<RouteEntry> {
    let entry = state.routes.lock().ok().and_then(|mut m| m.remove(&route));
    if let Some(ref e) = entry {
        if let Ok(mut by_req) = state.by_req.lock() {
            by_req.remove(&(e.ctrl_id, e.req_id));
        }
    }
    entry
}

fn cleanup_agent(state: &Arc<State>, name: &str, conn: &Arc<Conn>) {
    {
        let mut agents = match state.agents.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(cur) = agents.get(name) {
            if Arc::ptr_eq(&cur.conn, conn) {
                agents.remove(name);
            }
        }
    }
    let mut doomed = Vec::new();
    if let Ok(mut routes) = state.routes.lock() {
        let keys: Vec<u64> = routes
            .iter()
            .filter(|(_, r)| r.agent == name)
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            if let Some(r) = routes.remove(&k) {
                if let Ok(mut by_req) = state.by_req.lock() {
                    by_req.remove(&(r.ctrl_id, r.req_id));
                }
                doomed.push(r);
            }
        }
    }
    for r in doomed {
        send_ctrl_error(
            &r.ctrl_tx,
            r.req_id,
            410,
            format!("agent `{name}` disconnected"),
        );
    }
}

fn cleanup_ctrl(state: &Arc<State>, conn: &Arc<Conn>, ctrl_id: u64) {
    let mut doomed: Vec<(u64, RouteEntry)> = Vec::new();
    if let Ok(mut routes) = state.routes.lock() {
        let keys: Vec<u64> = routes
            .iter()
            .filter(|(_, r)| r.ctrl_id == ctrl_id)
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            if let Some(r) = routes.remove(&k) {
                if let Ok(mut by_req) = state.by_req.lock() {
                    by_req.remove(&(r.ctrl_id, r.req_id));
                }
                doomed.push((k, r));
            }
        }
    }
    for (route, r) in doomed {
        if let Ok(agents) = state.agents.lock() {
            if let Some(a) = agents.get(&r.agent) {
                let _ = a.conn.send(Frame::RouteCancel { route }.encode());
            }
        }
    }
    conn.close();
}

fn handle_connection(
    stream: TcpStream,
    key: [u8; 32],
    state: Arc<State>,
    ctrl_id: u64,
) -> io::Result<()> {
    let conn = Arc::new(Conn::from_stream(stream, &key, false)?);
    let hello = match conn.recv() {
        Ok(raw) => Frame::decode(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?,
        Err(e) => {
            conn.close();
            return Err(e);
        }
    };
    match hello {
        Frame::Hello {
            role,
            name,
            version,
            os,
            arch,
            pid: _,
        } => {
            if role == ROLE_AGENT {
                if !valid_name(&name) {
                    let _ = send_frame(
                        &conn,
                        Frame::Error {
                            code: 400,
                            message: "invalid agent name".into(),
                        },
                    );
                    conn.close();
                    return Ok(());
                }
                let entry = Arc::new(AgentEntry {
                    info_name: name.clone(),
                    version,
                    os,
                    arch,
                    connected_at_ms: now_ms(),
                    last_seen_ms: AtomicU64::new(now_ms()),
                    conn: conn.clone(),
                });
                {
                    let mut agents = state
                        .agents
                        .lock()
                        .map_err(|_| io::Error::new(io::ErrorKind::Other, "agents poisoned"))?;
                    if agents.contains_key(&name) {
                        drop(agents);
                        let _ = send_frame(
                            &conn,
                            Frame::Error {
                                code: 409,
                                message: format!("agent `{name}` is already connected"),
                            },
                        );
                        conn.close();
                        return Ok(());
                    }
                    agents.insert(name.clone(), entry);
                }
                let _ = send_frame(
                    &conn,
                    Frame::HelloOk {
                        version: env!("CARGO_PKG_VERSION").into(),
                    },
                );
                eprintln!(
                    "apb server: agent `{name}` connected from {}",
                    conn_peer(&conn)
                );
                let result = agent_loop(&conn, &state, &name);
                cleanup_agent(&state, &name, &conn);
                conn.close();
                eprintln!("apb server: agent `{name}` disconnected");
                result
            } else if role == ROLE_CTRL {
                let cid = if ctrl_id == 0 { 1 } else { ctrl_id };
                let _ = send_frame(
                    &conn,
                    Frame::HelloOk {
                        version: env!("CARGO_PKG_VERSION").into(),
                    },
                );
                let result = ctrl_loop(&conn, &state, cid);
                cleanup_ctrl(&state, &conn, cid);
                conn.close();
                result
            } else {
                let _ = send_frame(
                    &conn,
                    Frame::Error {
                        code: 400,
                        message: "unknown role".into(),
                    },
                );
                conn.close();
                Ok(())
            }
        }
        other => {
            let _ = send_frame(
                &conn,
                Frame::Error {
                    code: 400,
                    message: format!("expected hello, got {other:?}"),
                },
            );
            conn.close();
            Ok(())
        }
    }
}

fn conn_peer(conn: &Conn) -> String {
    // A tiny helper so the log line never tries to read private key material.
    // TcpStream's peer_addr is not exposed through Conn; the address is already
    // represented by the OS log if needed.
    let _ = conn;
    "remote".into()
}

fn touch_agent(state: &State, name: &str) {
    if let Ok(agents) = state.agents.lock() {
        if let Some(a) = agents.get(name) {
            a.last_seen_ms.store(now_ms(), Ordering::Relaxed);
        }
    }
}

fn agent_loop(conn: &Arc<Conn>, state: &Arc<State>, name: &str) -> io::Result<()> {
    loop {
        let raw = match conn.recv() {
            Ok(v) => v,
            Err(e) => return Err(e),
        };
        touch_agent(state, name);
        let frame = Frame::decode(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        match frame {
            Frame::RelayResult { route, payload } => {
                let entry = state
                    .routes
                    .lock()
                    .ok()
                    .and_then(|m| m.get(&route).cloned());
                if let Some(e) = entry {
                    let is_final = Payload::decode(&payload)
                        .map(|p| p.is_final())
                        .unwrap_or(false);
                    let out = Frame::RelayResult { route, payload };
                    let _ = e.ctrl_tx.send(out.encode());
                    if is_final {
                        remove_route(state, route);
                    }
                }
            }
            Frame::Ping { nonce } => {
                let _ = send_frame(conn, Frame::Pong { nonce });
            }
            Frame::RouteCancel { route } => {
                let _ = send_frame(conn, Frame::RouteCancel { route });
            }
            Frame::Error { code, message } => {
                eprintln!("apb server: agent `{name}` reported error {code}: {message}");
            }
            other => {
                let _ = send_frame(
                    conn,
                    Frame::Error {
                        code: 400,
                        message: format!("agent sent unexpected frame: {other:?}"),
                    },
                );
            }
        }
    }
}

fn ctrl_loop(conn: &Arc<Conn>, state: &Arc<State>, ctrl_id: u64) -> io::Result<()> {
    loop {
        let raw = match conn.recv() {
            Ok(v) => v,
            Err(e) => return Err(e),
        };
        let frame = Frame::decode(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        match frame {
            Frame::RelayReq { target, payload } => {
                relay_to_agent(state, ctrl_id, &conn.sender_clone(), &target, payload);
            }
            Frame::ListReq => {
                let peers = list_peers(state);
                if send_frame(conn, Frame::ListRes { peers }).is_err() {
                    return Ok(());
                }
            }
            Frame::Ping { nonce } => {
                let _ = send_frame(conn, Frame::Pong { nonce });
            }
            Frame::Error { code, message } => {
                eprintln!("apb server: controller {ctrl_id} reported error {code}: {message}");
            }
            other => {
                let _ = send_frame(
                    conn,
                    Frame::Error {
                        code: 400,
                        message: format!("controller sent unexpected frame: {other:?}"),
                    },
                );
            }
        }
    }
}

fn list_peers(state: &State) -> Vec<PeerInfo> {
    let mut out = Vec::new();
    if let Ok(agents) = state.agents.lock() {
        for (_, a) in agents.iter() {
            out.push(PeerInfo {
                role: ROLE_AGENT,
                name: a.info_name.clone(),
                version: a.version.clone(),
                os: a.os.clone(),
                arch: a.arch.clone(),
                connected_at_ms: a.connected_at_ms,
                last_seen_ms: a.last_seen_ms.load(Ordering::Relaxed),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}
