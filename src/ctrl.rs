//! Controller-side commands (`apb status/exec/push/pull/doctor`).

use crate::proto::{Frame, Payload, ENTRY_DIR, ENTRY_FILE, ENTRY_SYMLINK, ROLE_CTRL};
use crate::util::{
    basename, encode_base64, json_bool, json_escape, mode_of, now_ms, safe_rel, set_mode,
};
use crate::wire::Conn;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

enum Event {
    Frame(Frame),
    Closed(String),
}

pub struct CtrlSession {
    conn: Arc<Conn>,
    rx: mpsc::Receiver<Event>,
    next_id: u64,
}

impl CtrlSession {
    pub fn connect(server: &str, key: &[u8; 32]) -> io::Result<Self> {
        let addr = crate::util::resolve_addr(server)?;
        let stream = TcpStream::connect(addr)?;
        let conn = Arc::new(Conn::from_stream(stream, key, true)?);
        conn.send_frame(&Frame::Hello {
            role: ROLE_CTRL,
            name: format!("controller-{}", std::process::id()),
            version: env!("CARGO_PKG_VERSION").into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            pid: std::process::id(),
        })?;
        match Frame::decode(&conn.recv()?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
        {
            Frame::HelloOk { .. } => {}
            Frame::Error { code, message } => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("server rejected controller: {code} {message}"),
                ))
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected hello: {other:?}"),
                ))
            }
        }
        let (tx, rx) = mpsc::sync_channel(128);
        let c = conn.clone();
        let _ = thread::Builder::new()
            .name("apb-ctrl-reader".into())
            .spawn(move || loop {
                match c.recv() {
                    Ok(raw) => match Frame::decode(&raw) {
                        Ok(f) => {
                            if tx.send(Event::Frame(f)).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Event::Closed(format!("protocol error: {e}")));
                            break;
                        }
                    },
                    Err(e) => {
                        let _ = tx.send(Event::Closed(e.to_string()));
                        break;
                    }
                }
            });
        let next = now_ms().wrapping_mul(1_000_003).max(1);
        Ok(Self {
            conn,
            rx,
            next_id: next,
        })
    }

    fn new_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        self.next_id
    }

    fn send_request(&self, target: &str, payload: Payload) -> io::Result<()> {
        self.conn.send_frame(&Frame::RelayReq {
            target: target.to_string(),
            payload: payload.encode(),
        })
    }

    pub fn close(&self) {
        self.conn.close();
    }

    fn recv_event(&self, timeout: Duration) -> io::Result<Event> {
        match self.rx.recv_timeout(timeout) {
            Ok(e) => Ok(e),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(io::Error::new(io::ErrorKind::TimedOut, "timeout"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection closed",
            )),
        }
    }
}

pub struct ExecOptions {
    pub server: String,
    pub key: [u8; 32],
    pub target: String,
    pub json: bool,
    pub b64: bool,
    pub timeout_secs: u64,
    pub cwd: String,
    pub raw: bool,
    pub max_output: usize,
    pub command: String,
}

pub fn exec(opts: ExecOptions) -> i32 {
    let mut sess = match CtrlSession::connect(&opts.server, &opts.key) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("apb: {e}");
            return 3;
        }
    };
    let id = sess.new_id();
    let payload = Payload::Exec {
        id,
        timeout_secs: opts.timeout_secs,
        cwd: opts.cwd.clone(),
        raw: opts.raw,
        command: opts.command.clone(),
    };
    if let Err(e) = sess.send_request(&opts.target, payload) {
        eprintln!("apb: {e}");
        sess.close();
        return 3;
    }
    let started = Instant::now();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_total = 0usize;
    let mut err_total = 0usize;
    let mut out_trunc = false;
    let mut err_trunc = false;
    let rc: i32;
    let timed_out: bool;
    let ended_reason: String;
    let total_timeout = if opts.timeout_secs == 0 {
        Duration::from_secs(24 * 60 * 60)
    } else {
        Duration::from_secs(opts.timeout_secs.saturating_add(30))
    };
    let deadline = Instant::now() + total_timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            eprintln!("apb: timeout waiting for agent");
            sess.close();
            return 5;
        }
        let ev = match sess.recv_event(remaining) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("apb: {e}");
                sess.close();
                return 3;
            }
        };
        match ev {
            Event::Closed(e) => {
                eprintln!("apb: {e}");
                sess.close();
                return 3;
            }
            Event::Frame(Frame::RelayResult { payload, .. }) => {
                let p = match Payload::decode(&payload) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("apb: bad payload: {e}");
                        sess.close();
                        return 3;
                    }
                };
                if p.id() != id {
                    continue;
                }
                match p {
                    Payload::ExecOut { stream, data, .. } => {
                        if stream == 1 {
                            out_total += data.len();
                            if out.len() < opts.max_output {
                                let room = opts.max_output - out.len();
                                out.extend_from_slice(&data[..data.len().min(room)]);
                            }
                            if out_total > opts.max_output {
                                out_trunc = true;
                            }
                            if !opts.json {
                                let _ = io::stdout().write_all(&data);
                            }
                        } else {
                            err_total += data.len();
                            if err.len() < opts.max_output {
                                let room = opts.max_output - err.len();
                                err.extend_from_slice(&data[..data.len().min(room)]);
                            }
                            if err_total > opts.max_output {
                                err_trunc = true;
                            }
                            if !opts.json {
                                let _ = io::stderr().write_all(&data);
                            }
                        }
                    }
                    Payload::ExecEnd {
                        rc: r,
                        timed_out: t,
                        reason,
                        ..
                    } => {
                        rc = r;
                        timed_out = t;
                        ended_reason = reason;
                        break;
                    }
                    Payload::Error { code, message, .. } => {
                        if code == 125 {
                            print_exec_error(&opts, code, &message);
                            sess.close();
                            return 125;
                        }
                        print_exec_error(&opts, code, &message);
                        sess.close();
                        return 1;
                    }
                    _ => {}
                }
            }
            Event::Frame(Frame::Error { code, message }) => {
                eprintln!("apb: server error {code}: {message}");
                sess.close();
                return 3;
            }
            Event::Frame(Frame::Ping { nonce }) => {
                let _ = sess.conn.send_frame(&Frame::Pong { nonce });
            }
            Event::Frame(_) => {}
        }
    }
    sess.close();
    let duration_ms = started.elapsed().as_millis() as u64;
    if opts.json {
        let stdout = if opts.b64 {
            format!("{}", json_escape(&encode_base64(&out)))
        } else {
            json_escape(&String::from_utf8_lossy(&out))
        };
        let stderr = if opts.b64 {
            format!("{}", json_escape(&encode_base64(&err)))
        } else {
            json_escape(&String::from_utf8_lossy(&err))
        };
        let out_field = if opts.b64 { "stdout_b64" } else { "stdout" };
        let err_field = if opts.b64 { "stderr_b64" } else { "stderr" };
        println!(
            "{{\"ok\":true,\"rc\":{rc},\"duration_ms\":{duration_ms},\"timeout_s\":{},\"cwd\":{},\"command\":{},\
             \"{out_field}\":{stdout},\"{err_field}\":{stderr},\
             \"stdout_bytes\":{out_total},\"stderr_bytes\":{err_total},\
             \"stdout_truncated\":{},\"stderr_truncated\":{},\
             \"timed_out\":{},\"ended_reason\":{}}}",
            opts.timeout_secs,
            json_escape(&opts.cwd),
            json_escape(&opts.command),
            if out_trunc { 1 } else { 0 },
            if err_trunc { 1 } else { 0 },
            json_bool(timed_out),
            json_escape(&ended_reason)
        );
    }
    if timed_out {
        5
    } else {
        rc
    }
}

fn print_exec_error(opts: &ExecOptions, code: u16, message: &str) {
    if opts.json {
        println!(
            "{{\"ok\":false,\"error\":{},\"rc\":{},\"detail\":{}}}",
            json_escape(if code == 125 {
                "bad_cwd"
            } else {
                "exec_failed"
            }),
            code,
            json_escape(message)
        );
    } else {
        eprintln!("apb: {message}");
    }
}

pub struct StopOptions {
    pub server: String,
    pub key: [u8; 32],
    pub target: String,
    pub json: bool,
    pub timeout_secs: u64,
    pub reason: String,
}

/// Ask one agent to end itself.  The agent answers with a final `Bye` payload
/// and then exits its process with `Bye.code`, so a CI step that runs
/// `apb agent` finishes successfully instead of being killed at the job
/// timeout -- which is what lets the steps after it (a cache save, for
/// example) still run.
pub fn stop(opts: StopOptions) -> i32 {
    let mut sess = match CtrlSession::connect(&opts.server, &opts.key) {
        Ok(s) => s,
        Err(e) => {
            if opts.json {
                println!(
                    "{{\"ok\":false,\"error\":\"connect_failed\",\"detail\":{}}}",
                    json_escape(&e.to_string())
                );
            } else {
                eprintln!("apb: {e}");
            }
            return 3;
        }
    };
    let id = sess.new_id();
    let payload = Payload::Shutdown {
        id,
        reason: opts.reason.clone(),
    };
    if let Err(e) = sess.send_request(&opts.target, payload) {
        eprintln!("apb: {e}");
        sess.close();
        return 3;
    }
    let deadline = Instant::now() + Duration::from_secs(opts.timeout_secs.max(1));
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            if opts.json {
                println!(
                    "{{\"ok\":false,\"error\":\"timeout\",\"detail\":{}}}",
                    json_escape("no reply from the agent")
                );
            } else {
                eprintln!("apb: timeout waiting for the agent to end");
            }
            sess.close();
            return 5;
        }
        match sess.recv_event(remaining) {
            Ok(Event::Frame(Frame::RelayResult { payload, .. })) => {
                let p = match Payload::decode(&payload) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("apb: bad payload: {e}");
                        sess.close();
                        return 3;
                    }
                };
                // A peer that cannot even decode the request (an older agent,
                // for example) answers with id 0, so route-level errors must be
                // matched before the request-id filter.
                if let Payload::Error { code, message, .. } = &p {
                    if opts.json {
                        println!(
                            "{{\"ok\":false,\"error\":\"refused\",\"code\":{code},\"detail\":{}}}",
                            json_escape(message)
                        );
                    } else {
                        eprintln!("apb: {message}");
                    }
                    sess.close();
                    return 1;
                }
                if p.id() != id {
                    continue;
                }
                match p {
                    Payload::Bye { code, reason, .. } => {
                        if opts.json {
                            println!(
                                "{{\"ok\":true,\"ended\":true,\"code\":{code},\"reason\":{}}}",
                                json_escape(&reason)
                            );
                        } else {
                            println!("agent ended: {reason}");
                        }
                        sess.close();
                        return 0;
                    }
                    other => {
                        eprintln!("apb: unexpected reply {other:?}");
                        sess.close();
                        return 3;
                    }
                }
            }
            Ok(Event::Frame(Frame::Error { code, message })) => {
                eprintln!("apb: server error {code}: {message}");
                sess.close();
                return 3;
            }
            Ok(Event::Frame(Frame::Ping { nonce })) => {
                let _ = sess.conn.send_frame(&Frame::Pong { nonce });
            }
            Ok(Event::Frame(_)) => {}
            Ok(Event::Closed(e)) => {
                eprintln!("apb: {e}");
                sess.close();
                return 3;
            }
            Err(e) => {
                eprintln!("apb: {e}");
                sess.close();
                return 3;
            }
        }
    }
}

pub struct StatusOptions {
    pub server: String,
    pub key: [u8; 32],
    pub json: bool,
}

pub fn status(opts: StatusOptions) -> i32 {
    let sess = match CtrlSession::connect(&opts.server, &opts.key) {
        Ok(s) => s,
        Err(e) => {
            if opts.json {
                println!(
                    "{{\"ok\":false,\"error\":\"connect_failed\",\"detail\":{}}}",
                    json_escape(&e.to_string())
                );
            } else {
                eprintln!("apb: {e}");
            }
            return 3;
        }
    };
    if let Err(e) = sess.conn.send_frame(&Frame::ListReq) {
        if opts.json {
            println!(
                "{{\"ok\":false,\"error\":\"list_failed\",\"detail\":{}}}",
                json_escape(&e.to_string())
            );
        } else {
            eprintln!("apb: {e}");
        }
        sess.close();
        return 3;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match sess.recv_event(remaining) {
            Ok(Event::Frame(Frame::ListRes { peers })) => {
                if opts.json {
                    let items: Vec<String> = peers
                        .iter()
                        .map(|p| {
                            format!(
                                "{{\"name\":{},\"role\":{},\"version\":{},\"os\":{},\"arch\":{},\"connected_at_ms\":{},\"last_seen_ms\":{}}}",
                                json_escape(&p.name),
                                p.role,
                                json_escape(&p.version),
                                json_escape(&p.os),
                                json_escape(&p.arch),
                                p.connected_at_ms,
                                p.last_seen_ms
                            )
                        })
                        .collect();
                    println!(
                        "{{\"ok\":true,\"server\":{},\"count\":{},\"agents\":[{}]}}",
                        json_escape(&opts.server),
                        peers.len(),
                        items.join(",")
                    );
                } else {
                    println!("server={} agents={}", opts.server, peers.len());
                    for p in &peers {
                        println!(
                            "  {} {} {} {} connected_at={}",
                            p.name, p.version, p.os, p.arch, p.connected_at_ms
                        );
                    }
                }
                sess.close();
                return 0;
            }
            Ok(Event::Frame(Frame::Error { code, message })) => {
                if opts.json {
                    println!(
                        "{{\"ok\":false,\"error\":\"server_error\",\"rc\":{code},\"detail\":{}}}",
                        json_escape(&message)
                    );
                } else {
                    eprintln!("apb: server error {code}: {message}");
                }
                sess.close();
                return 3;
            }
            Ok(Event::Closed(e)) => {
                if opts.json {
                    println!(
                        "{{\"ok\":false,\"error\":\"connection_closed\",\"detail\":{}}}",
                        json_escape(&e)
                    );
                } else {
                    eprintln!("apb: {e}");
                }
                sess.close();
                return 3;
            }
            Ok(_) => {}
            Err(_) => {
                if opts.json {
                    println!("{{\"ok\":false,\"error\":\"timeout\"}}");
                } else {
                    eprintln!("apb: timeout waiting for server");
                }
                sess.close();
                return 5;
            }
        }
    }
}

pub struct PushOptions {
    pub server: String,
    pub key: [u8; 32],
    pub target: String,
    pub json: bool,
    pub local: String,
    pub remote: String,
}

pub fn push(opts: PushOptions) -> i32 {
    let local = PathBuf::from(&opts.local);
    let meta = match fs::symlink_metadata(&local) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("apb: local {}: {e}", local.display());
            return 1;
        }
    };
    if meta.file_type().is_symlink() {
        eprintln!("apb: source symlink is not followed by push; pass the target path");
        return 1;
    }
    let source_is_dir = meta.is_dir();
    let source_name = basename(&opts.local);
    let (source_files, source_bytes) = match count_tree(&local) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("apb: {e}");
            return 1;
        }
    };
    let mut sess = match CtrlSession::connect(&opts.server, &opts.key) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("apb: {e}");
            return 3;
        }
    };
    let id = sess.new_id();
    let begin = Payload::PushBegin {
        id,
        remote_path: opts.remote.clone(),
        source_name: source_name.clone(),
        source_is_dir,
        source_files,
        source_bytes,
    };
    if let Err(e) = sess.send_request(&opts.target, begin) {
        eprintln!("apb: {e}");
        sess.close();
        return 3;
    }
    let _root = match wait_push_ready(&sess, id, Duration::from_secs(15)) {
        Ok(r) => r,
        Err(e) => {
            print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
            sess.close();
            return 6;
        }
    };
    let started = Instant::now();
    let mut bytes: u64 = 0;
    let result = if source_is_dir {
        push_dir(&sess, id, &local, &mut bytes)
    } else {
        push_file(&sess, id, "", &local, &meta, &mut bytes)
    };
    if let Err(e) = result {
        print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
        sess.close();
        return 6;
    }
    if let Err(e) = sess.send_request(&opts.target, Payload::PushEnd { id }) {
        print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
        sess.close();
        return 6;
    }
    let done = match wait_transfer_done(&sess, id, Duration::from_secs(30)) {
        Ok(d) => d,
        Err(e) => {
            print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
            sess.close();
            return 6;
        }
    };
    sess.close();
    let (ok, remote_path, done_bytes, err) = match done {
        TransferOutcome::Done { remote_path, bytes } => (true, remote_path, bytes, String::new()),
        TransferOutcome::Failed {
            error,
            remote_path,
            bytes,
        } => (false, remote_path, bytes, error),
    };
    if !ok {
        print_xfer_error(&opts.json, "transfer_failed", &err);
        return 6;
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    let _ = done_bytes;
    if opts.json {
        println!(
            "{{\"ok\":true,\"direction\":\"push\",\"source\":{},\"remote_path\":{},\"method\":\"apb2\",\"bytes\":{},\"sha256\":null,\"verified\":true,\"backup_path\":null,\"duration_ms\":{}}}",
            json_escape(&opts.local),
            json_escape(&remote_path),
            source_bytes,
            duration_ms
        );
    } else {
        println!("push -> {remote_path} ok ({source_bytes} bytes)");
    }
    0
}

fn push_dir(sess: &CtrlSession, id: u64, root: &Path, bytes: &mut u64) -> io::Result<()> {
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    let mut dirs: Vec<(PathBuf, String)> = Vec::new();
    while let Some((dir, rel)) = stack.pop() {
        let mut children: Vec<(PathBuf, String)> = Vec::new();
        for e in fs::read_dir(&dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            children.push((e.path(), child_rel));
        }
        children.sort_by(|a, b| a.1.cmp(&b.1));
        for (path, child_rel) in children {
            let meta = fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&path)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                sess.send_request(
                    &String::new(),
                    Payload::Symlink {
                        id,
                        rel: child_rel,
                        target,
                        mode: mode_of(&meta),
                    },
                )
                .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
            } else if meta.is_dir() {
                dirs.push((path.clone(), child_rel.clone()));
                stack.push((path, child_rel));
            } else if meta.is_file() {
                push_file(sess, id, &child_rel, &path, &meta, bytes)?;
            }
        }
    }
    // Directories were sent in reverse traversal order above; it is harmless to
    // create parent directories on demand in the receiver, but explicitly send
    // them for deterministic permissions too.
    dirs.sort_by(|a, b| a.1.cmp(&b.1));
    for (path, rel) in dirs {
        let meta = fs::symlink_metadata(&path)?;
        sess.send_request(
            &String::new(),
            Payload::Mkdir {
                id,
                rel,
                mode: mode_of(&meta),
            },
        )
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    }
    Ok(())
}

fn push_file(
    sess: &CtrlSession,
    id: u64,
    rel: &str,
    path: &Path,
    meta: &fs::Metadata,
    bytes: &mut u64,
) -> io::Result<()> {
    let size = meta.len();
    let sha = hash_file(path)?;
    sess.send_request(
        &String::new(),
        Payload::FileBegin {
            id,
            rel: rel.to_string(),
            size,
            mode: mode_of(meta),
            sha256: sha,
        },
    )
    .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sess.send_request(
            &String::new(),
            Payload::Data {
                id,
                data: buf[..n].to_vec(),
            },
        )
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
        *bytes += n as u64;
    }
    sess.send_request(&String::new(), Payload::FileEnd { id })
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    Ok(())
}

fn hash_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let out = hasher.finalize();
    let mut a = [0u8; 32];
    a.copy_from_slice(&out);
    Ok(a)
}

fn count_tree(root: &Path) -> io::Result<(u64, u64)> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let meta = fs::symlink_metadata(&p)?;
        if meta.is_dir() {
            for e in fs::read_dir(&p)? {
                stack.push(e?.path());
            }
        } else if meta.is_file() {
            files += 1;
            bytes += meta.len();
        }
    }
    Ok((files, bytes))
}

fn wait_push_ready(sess: &CtrlSession, id: u64, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return Err("timeout waiting for push ready".into());
        }
        match sess.recv_event(rem) {
            Ok(Event::Frame(Frame::RelayResult { payload, .. })) => match Payload::decode(&payload)
            {
                Ok(Payload::PushReady {
                    id: rid,
                    remote_root,
                }) if rid == id => return Ok(remote_root),
                Ok(Payload::Error {
                    id: rid, message, ..
                }) if rid == id => return Err(message),
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            },
            Ok(Event::Frame(Frame::Error { code, message })) => {
                return Err(format!("server error {code}: {message}"))
            }
            Ok(Event::Closed(e)) => return Err(e),
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

enum TransferOutcome {
    Done {
        remote_path: String,
        bytes: u64,
    },
    Failed {
        error: String,
        remote_path: String,
        bytes: u64,
    },
}

fn wait_transfer_done(
    sess: &CtrlSession,
    id: u64,
    timeout: Duration,
) -> Result<TransferOutcome, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return Err("timeout waiting for transfer result".into());
        }
        match sess.recv_event(rem) {
            Ok(Event::Frame(Frame::RelayResult { payload, .. })) => match Payload::decode(&payload)
            {
                Ok(Payload::TransferDone {
                    id: rid,
                    ok,
                    error,
                    remote_path,
                    bytes,
                    ..
                }) if rid == id => {
                    if ok {
                        return Ok(TransferOutcome::Done { remote_path, bytes });
                    }
                    return Ok(TransferOutcome::Failed {
                        error,
                        remote_path,
                        bytes,
                    });
                }
                Ok(Payload::Error {
                    id: rid, message, ..
                }) if rid == id => {
                    return Ok(TransferOutcome::Failed {
                        error: message,
                        remote_path: String::new(),
                        bytes: 0,
                    })
                }
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            },
            Ok(Event::Frame(Frame::Error { code, message })) => {
                return Err(format!("server error {code}: {message}"))
            }
            Ok(Event::Closed(e)) => return Err(e),
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn print_xfer_error(json: &bool, code: &str, detail: &str) {
    if *json {
        println!(
            "{{\"ok\":false,\"error\":{},\"detail\":{}}}",
            json_escape(code),
            json_escape(detail)
        );
    } else {
        eprintln!("apb: {detail}");
    }
}

pub struct PullOptions {
    pub server: String,
    pub key: [u8; 32],
    pub target: String,
    pub json: bool,
    pub remote: String,
    pub local: String,
}

pub fn pull(opts: PullOptions) -> i32 {
    let mut sess = match CtrlSession::connect(&opts.server, &opts.key) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("apb: {e}");
            return 3;
        }
    };
    let id = sess.new_id();
    if let Err(e) = sess.send_request(
        &opts.target,
        Payload::PullBegin {
            id,
            remote_path: opts.remote.clone(),
        },
    ) {
        eprintln!("apb: {e}");
        sess.close();
        return 3;
    }
    let dst_root = PathBuf::from(&opts.local);
    if let Err(e) = fs::create_dir_all(&dst_root) {
        eprintln!("apb: {}: {e}", dst_root.display());
        sess.close();
        return 1;
    }
    let started = Instant::now();
    let mut current: Option<PullFile> = None;
    let mut bytes = 0u64;
    let mut remote_path = String::new();
    let mut final_path: Option<PathBuf> = None;
    let deadline = Instant::now() + Duration::from_secs(24 * 60 * 60);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let ev = match sess.recv_event(remaining) {
            Ok(e) => e,
            Err(e) => {
                cleanup_pull(&dst_root, &mut current);
                print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
                sess.close();
                return 6;
            }
        };
        match ev {
            Event::Closed(e) => {
                cleanup_pull(&dst_root, &mut current);
                print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
                sess.close();
                return 6;
            }
            Event::Frame(Frame::RelayResult { payload, .. }) => {
                let p = match Payload::decode(&payload) {
                    Ok(p) => p,
                    Err(e) => {
                        cleanup_pull(&dst_root, &mut current);
                        print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
                        sess.close();
                        return 6;
                    }
                };
                if p.id() != id {
                    continue;
                }
                match p {
                    Payload::Entry {
                        rel,
                        kind,
                        size,
                        mode,
                        link_target,
                        ..
                    } => {
                        if let Err(e) = finalize_pull(&mut current, &dst_root) {
                            print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
                            sess.close();
                            return 6;
                        }
                        let dest = match safe_rel(&rel) {
                            Ok(r) => dst_root.join(r),
                            Err(e) => {
                                print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
                                sess.close();
                                return 6;
                            }
                        };
                        if final_path.is_none() {
                            final_path = Some(dest.clone());
                        }
                        if let Some(parent) = dest.parent() {
                            if let Err(e) = fs::create_dir_all(parent) {
                                print_xfer_error(
                                    &opts.json,
                                    "transfer_failed",
                                    &format!("mkdir {}: {e}", parent.display()),
                                );
                                sess.close();
                                return 6;
                            }
                        }
                        match kind {
                            ENTRY_DIR => {
                                if let Err(e) = fs::create_dir_all(&dest) {
                                    print_xfer_error(
                                        &opts.json,
                                        "transfer_failed",
                                        &format!("mkdir {}: {e}", dest.display()),
                                    );
                                    sess.close();
                                    return 6;
                                }
                                let _ = set_mode(&dest, mode);
                            }
                            ENTRY_FILE => {
                                if dest.exists() {
                                    print_xfer_error(
                                        &opts.json,
                                        "transfer_failed",
                                        &format!("destination exists: {}", dest.display()),
                                    );
                                    sess.close();
                                    return 6;
                                }
                                let file_name = dest
                                    .file_name()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_else(|| "file".into());
                                let tmp =
                                    dest.with_file_name(format!(".{file_name}.apb-pull-{id}.tmp"));
                                match OpenOptions::new().write(true).create_new(true).open(&tmp) {
                                    Ok(f) => {
                                        current = Some(PullFile {
                                            tmp,
                                            dest,
                                            file: f,
                                            received: 0,
                                            expected: size,
                                            mode,
                                        })
                                    }
                                    Err(e) => {
                                        print_xfer_error(
                                            &opts.json,
                                            "transfer_failed",
                                            &format!("create {}: {e}", tmp.display()),
                                        );
                                        sess.close();
                                        return 6;
                                    }
                                }
                            }
                            ENTRY_SYMLINK => {
                                #[cfg(unix)]
                                {
                                    let _ = fs::remove_file(&dest);
                                    if let Err(e) = std::os::unix::fs::symlink(&link_target, &dest)
                                    {
                                        print_xfer_error(
                                            &opts.json,
                                            "transfer_failed",
                                            &format!("symlink {}: {e}", dest.display()),
                                        );
                                        sess.close();
                                        return 6;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Payload::Data { data, .. } => {
                        if let Some(cur) = current.as_mut() {
                            if let Err(e) = cur.file.write_all(&data) {
                                print_xfer_error(
                                    &opts.json,
                                    "transfer_failed",
                                    &format!("write {}: {e}", cur.tmp.display()),
                                );
                                sess.close();
                                return 6;
                            }
                            cur.received += data.len() as u64;
                            bytes += data.len() as u64;
                        }
                    }
                    Payload::TransferDone {
                        ok,
                        error,
                        remote_path: rp,
                        bytes: b,
                        ..
                    } => {
                        if remote_path.is_empty() {
                            remote_path = rp;
                        }
                        if !ok {
                            cleanup_pull(&dst_root, &mut current);
                            print_xfer_error(&opts.json, "transfer_failed", &error);
                            sess.close();
                            return 6;
                        }
                        if let Err(e) = finalize_pull(&mut current, &dst_root) {
                            print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
                            sess.close();
                            return 6;
                        }
                        bytes = bytes.max(b);
                        break;
                    }
                    Payload::Error { message, .. } => {
                        cleanup_pull(&dst_root, &mut current);
                        print_xfer_error(&opts.json, "transfer_failed", &message);
                        sess.close();
                        return 6;
                    }
                    _ => {}
                }
            }
            Event::Frame(Frame::Error { code, message }) => {
                cleanup_pull(&dst_root, &mut current);
                print_xfer_error(
                    &opts.json,
                    "transfer_failed",
                    &format!("server error {code}: {message}"),
                );
                sess.close();
                return 6;
            }
            Event::Frame(_) => {}
        }
    }
    sess.close();
    let duration_ms = started.elapsed().as_millis() as u64;
    let final_path = final_path
        .unwrap_or_else(|| dst_root.join(basename(&remote_path)))
        .to_string_lossy()
        .to_string();
    if opts.json {
        println!(
            "{{\"ok\":true,\"direction\":\"pull\",\"source\":{},\"local_path\":{},\"remote_path\":{},\"method\":\"apb2\",\"bytes\":{},\"sha256\":null,\"duration_ms\":{}}}",
            json_escape(&opts.remote),
            json_escape(&final_path),
            json_escape(&remote_path),
            bytes,
            duration_ms
        );
    } else {
        println!("pull -> {final_path} ok ({bytes} bytes)");
    }
    0
}

#[derive(Debug)]
struct PullFile {
    tmp: PathBuf,
    dest: PathBuf,
    file: File,
    received: u64,
    expected: u64,
    mode: u32,
}

fn finalize_pull(current: &mut Option<PullFile>, _dst_root: &Path) -> Result<(), String> {
    if let Some(mut cur) = current.take() {
        if cur.expected != 0 && cur.received != cur.expected {
            let _ = fs::remove_file(&cur.tmp);
            return Err(format!(
                "size mismatch for {}: expected {}, got {}",
                cur.dest.display(),
                cur.expected,
                cur.received
            ));
        }
        if let Err(e) = cur.file.flush() {
            let _ = fs::remove_file(&cur.tmp);
            return Err(format!("flush {}: {e}", cur.tmp.display()));
        }
        if let Err(e) = cur.file.sync_all() {
            let _ = fs::remove_file(&cur.tmp);
            return Err(format!("sync {}: {e}", cur.tmp.display()));
        }
        drop(cur.file);
        fs::rename(&cur.tmp, &cur.dest)
            .map_err(|e| format!("rename {}: {e}", cur.dest.display()))?;
        let _ = set_mode(&cur.dest, cur.mode);
    }
    Ok(())
}

fn cleanup_pull(dst_root: &Path, current: &mut Option<PullFile>) {
    if let Some(cur) = current.take() {
        let _ = fs::remove_file(&cur.tmp);
    }
    let _ = dst_root;
}

pub fn doctor(server: &str, key: &[u8; 32], json: bool) -> i32 {
    match CtrlSession::connect(server, key) {
        Ok(s) => {
            s.close();
            if json {
                println!(
                    "{{\"ok\":true,\"server\":{},\"noise\":\"ok\"}}",
                    json_escape(server)
                );
            } else {
                println!("ok server={} noise=ok", server);
            }
            0
        }
        Err(e) => {
            if json {
                println!(
                    "{{\"ok\":false,\"server\":{},\"detail\":{}}}",
                    json_escape(server),
                    json_escape(&e.to_string())
                );
            } else {
                eprintln!("apb: {e}");
            }
            3
        }
    }
}
