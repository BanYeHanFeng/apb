//! Controller-side commands (`apb status/exec/push/pull/doctor`).

use crate::delta;
use crate::proto::{
    Frame, Payload, ENTRY_DIR, ENTRY_FILE, ENTRY_SYMLINK, PLAN_DELTA, PLAN_SEND, PLAN_SKIP, ROLE_CTRL,
};
use crate::util::{
    basename, encode_base64, json_bool, json_escape, mode_of, now_ms, safe_rel, set_mode,
};
use crate::wire::Conn;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
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

    /// Return an already-queued event without blocking.  Used by the send
    /// loops: while they stream, a failure reply must not wait until every
    /// remaining file has been pushed.
    fn try_recv_event(&self) -> Option<Event> {
        self.rx.try_recv().ok()
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

// ------------------------------------------------------------ file transfer
//
// `push` and `pull` transfer only what actually changed:
//
//   1. manifest  metadata + a SHA-256 for every file
//   2. plans     the receiving side answers per file: `skip` when its local
//                copy is already identical, `delta` with the block signatures
//                of its local copy, or `whole` when there is nothing to reuse
//   3. data      only the files that need it are streamed, and a delta file is
//                sent as `Copy` (reuse the receiver's blocks) plus `Data`
//                (literal bytes) instructions
//
// An unchanged file costs zero data frames, and a small edit in a large file
// costs only the blocks that really changed.

pub struct PushOptions {
    pub server: String,
    pub key: [u8; 32],
    pub target: String,
    pub json: bool,
    pub local: String,
    pub remote: String,
}

/// One file of a transfer manifest.
struct ManifestFile {
    seq: u64,
    rel: String,
    path: PathBuf,
    size: u64,
    mode: u32,
    sha: [u8; 32],
}

#[derive(Default)]
struct Manifest {
    dirs: Vec<(String, u32)>,
    links: Vec<(String, String, u32)>,
    files: Vec<ManifestFile>,
}

/// How one file is transferred.  Both directions use the same decision: the
/// receiver inspects its own copy and tells the sender what to do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FilePlan {
    Whole,
    Skip,
    Delta,
}

impl FilePlan {
    fn action(self) -> u8 {
        match self {
            FilePlan::Whole => PLAN_SEND,
            FilePlan::Skip => PLAN_SKIP,
            FilePlan::Delta => PLAN_DELTA,
        }
    }

    fn from_action(action: u8) -> Result<FilePlan, String> {
        match action {
            PLAN_SEND => Ok(FilePlan::Whole),
            PLAN_SKIP => Ok(FilePlan::Skip),
            PLAN_DELTA => Ok(FilePlan::Delta),
            other => Err(format!("unknown file plan action {other}")),
        }
    }
}

/// What the transfer really cost, as seen by the receiving side.
#[derive(Default, Clone, Copy)]
struct XferStats {
    /// Sum of the sizes of every file in the manifest.
    total: u64,
    /// Content bytes that crossed the wire.
    sent: u64,
    /// Bytes taken from the local copy (delta blocks + files that were already
    /// identical), i.e. bytes that did *not* cross the wire.
    reused: u64,
    files: u64,
    skipped: u64,
}

fn build_manifest(root: &Path, is_dir: bool) -> io::Result<Manifest> {
    let mut m = Manifest::default();
    if !is_dir {
        let meta = fs::symlink_metadata(root)?;
        m.files.push(ManifestFile {
            seq: 0,
            rel: String::new(),
            path: root.to_path_buf(),
            size: meta.len(),
            mode: mode_of(&meta),
            sha: hash_file(root)?,
        });
        return Ok(m);
    }
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
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
                m.links.push((child_rel, target, mode_of(&meta)));
            } else if meta.is_dir() {
                m.dirs.push((child_rel.clone(), mode_of(&meta)));
                stack.push((path, child_rel));
            } else if meta.is_file() {
                let sha = hash_file(&path)?;
                let seq = m.files.len() as u64;
                m.files.push(ManifestFile {
                    seq,
                    rel: child_rel,
                    path,
                    size: meta.len(),
                    mode: mode_of(&meta),
                    sha,
                });
            }
        }
    }
    m.dirs.sort_by(|a, b| a.0.cmp(&b.0));
    m.links.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(m)
}

fn send_payload(sess: &CtrlSession, p: Payload) -> io::Result<()> {
    sess.send_request(&String::new(), p)
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))
}

/// While the controller streams a transfer it does not wait for replies, so a
/// failure on the other side would otherwise only be noticed after every
/// remaining byte was sent.  Drain whatever is already queued and fail fast.
fn check_send_feedback(sess: &CtrlSession) -> io::Result<()> {
    while let Some(ev) = sess.try_recv_event() {
        match ev {
            Event::Closed(e) => {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, e))
            }
            Event::Frame(Frame::Error { code, message }) => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("server error {code}: {message}"),
                ))
            }
            Event::Frame(Frame::RelayResult { payload, .. }) => match Payload::decode(&payload) {
                Ok(Payload::Error { message, .. }) => {
                    return Err(io::Error::new(io::ErrorKind::Other, message))
                }
                Ok(Payload::TransferDone { ok, error, .. }) => {
                    if !ok {
                        return Err(io::Error::new(io::ErrorKind::Other, error));
                    }
                }
                _ => {}
            },
            Event::Frame(_) => {}
        }
    }
    Ok(())
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
    let manifest = match build_manifest(&local, source_is_dir) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("apb: {e}");
            return 1;
        }
    };
    let source_files = manifest.files.len() as u64;
    let source_bytes: u64 = manifest.files.iter().map(|f| f.size).sum();

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
    if let Err(e) = wait_push_ready(&sess, id, Duration::from_secs(15)) {
        print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
        sess.close();
        return 6;
    }
    let started = Instant::now();

    // ---- phase 1: manifest ------------------------------------------------
    let manifest_frames = manifest
        .dirs
        .iter()
        .map(|(rel, mode)| Payload::Mkdir {
            id,
            rel: rel.clone(),
            mode: *mode,
        })
        .chain(manifest.links.iter().map(|(rel, target, mode)| Payload::Symlink {
            id,
            rel: rel.clone(),
            target: target.clone(),
            mode: *mode,
        }))
        .chain(manifest.files.iter().map(|f| Payload::FileBegin {
            id,
            seq: f.seq,
            rel: f.rel.clone(),
            size: f.size,
            mode: f.mode,
            sha256: f.sha,
        }))
        .chain(std::iter::once(Payload::ManifestEnd {
            id,
            files: source_files,
        }));
    for frame in manifest_frames {
        if let Err(e) = send_payload(&sess, frame).and_then(|()| check_send_feedback(&sess)) {
            print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
            sess.close();
            return 6;
        }
    }

    // ---- phase 2: plans ---------------------------------------------------
    let mut plans = vec![FilePlan::Whole; manifest.files.len()];
    let mut block_sizes = vec![0u32; manifest.files.len()];
    let mut sigs: Vec<Vec<u8>> = vec![Vec::new(); manifest.files.len()];
    if let Err(e) = read_push_plans(&sess, id, &mut plans, &mut block_sizes, &mut sigs) {
        print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
        sess.close();
        return 6;
    }

    // ---- phase 3: data ----------------------------------------------------
    let mut stats = XferStats {
        total: source_bytes,
        files: source_files,
        ..Default::default()
    };
    let result = (|| -> io::Result<()> {
        for f in manifest.files.iter() {
            check_send_feedback(&sess)?;
            match plans[f.seq as usize] {
                FilePlan::Skip => {
                    stats.skipped += 1;
                    stats.reused += f.size;
                    continue;
                }
                FilePlan::Whole => {
                    send_payload(&sess, Payload::FileStart { id, seq: f.seq })?;
                    stream_local_file(&sess, id, f, &mut stats)?;
                }
                FilePlan::Delta => {
                    send_payload(&sess, Payload::FileStart { id, seq: f.seq })?;
                    send_file_delta(
                        &sess,
                        id,
                        f,
                        block_sizes[f.seq as usize],
                        &sigs[f.seq as usize],
                        &mut stats,
                    )?;
                }
            }
            send_payload(
                &sess,
                Payload::FileEnd {
                    id,
                    sha256: f.sha,
                },
            )?;
        }
        send_payload(&sess, Payload::PushEnd { id })
    })();
    if let Err(e) = result {
        print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
        sess.close();
        return 6;
    }
    let done = match wait_transfer_done(&sess, id, Duration::from_secs(30 * 60)) {
        Ok(d) => d,
        Err(e) => {
            print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
            sess.close();
            return 6;
        }
    };
    sess.close();
    let remote_path = match done {
        TransferOutcome::Done { remote_path } => remote_path,
        TransferOutcome::Failed {
            error,
            remote_path,
            ..
        } => {
            print_xfer_error(&opts.json, "transfer_failed", &error);
            let _ = remote_path;
            return 6;
        }
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    if opts.json {
        println!(
            "{{\"ok\":true,\"direction\":\"push\",\"source\":{},\"remote_path\":{},\"method\":\"apb2\",\"bytes\":{},\"total_bytes\":{},\"reused_bytes\":{},\"files\":{},\"skipped_files\":{},\"sha256\":null,\"verified\":true,\"backup_path\":null,\"duration_ms\":{}}}",
            json_escape(&opts.local),
            json_escape(&remote_path),
            stats.sent,
            stats.total,
            stats.reused,
            stats.files,
            stats.skipped,
            duration_ms
        );
    } else {
        println!(
            "push -> {remote_path} ok ({} bytes total, {} sent, {} reused, {} unchanged)",
            stats.total, stats.sent, stats.reused, stats.skipped
        );
    }
    0
}

fn read_push_plans(
    sess: &CtrlSession,
    id: u64,
    plans: &mut [FilePlan],
    block_sizes: &mut [u32],
    sigs: &mut [Vec<u8>],
) -> io::Result<()> {
    // The agent may have to hash and sign a large tree before it can answer.
    let deadline = Instant::now() + Duration::from_secs(60 * 60);
    loop {
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timeout waiting for file plans",
            ));
        }
        match sess.recv_event(rem) {
            Ok(Event::Frame(Frame::RelayResult { payload, .. })) => match Payload::decode(&payload) {
                Ok(p) if p.id() == id => {
                    let step = match p {
                        Payload::FilePlan {
                            seq,
                            action,
                            block_size,
                            block_count,
                            ..
                        } => match usize::try_from(seq).ok().filter(|i| *i < plans.len()) {
                            Some(idx) => match FilePlan::from_action(action) {
                                Ok(plan) => {
                                    plans[idx] = plan;
                                    block_sizes[idx] = block_size;
                                    if plan == FilePlan::Delta {
                                        let cap = usize::try_from(block_count)
                                            .ok()
                                            .and_then(|n| n.checked_mul(delta::SIG_LEN))
                                            .unwrap_or(0)
                                            .min(1 << 24);
                                        sigs[idx] = Vec::with_capacity(cap);
                                    }
                                    Ok(())
                                }
                                Err(e) => Err(e),
                            },
                            None => Err(format!("plan for unknown file seq {seq}")),
                        },
                        Payload::FileSigs {
                            seq, first, sigs: blob, ..
                        } => match usize::try_from(seq).ok().and_then(|i| sigs.get_mut(i)) {
                            Some(buf) => {
                                let at = usize::try_from(first)
                                    .ok()
                                    .and_then(|n| n.checked_mul(delta::SIG_LEN));
                                match at {
                                    Some(at) if buf.len() == at => {
                                        buf.extend_from_slice(&blob);
                                        Ok(())
                                    }
                                    _ => Err(format!(
                                        "signature frame out of order for seq {seq}"
                                    )),
                                }
                            }
                            None => Err(format!("signatures for unknown file seq {seq}")),
                        },
                        Payload::PlanEnd { .. } => return Ok(()),
                        Payload::Error { message, .. } => {
                            return Err(io::Error::new(io::ErrorKind::Other, message))
                        }
                        _ => Ok(()),
                    };
                    if let Err(e) = step {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, e));
                    }
                }
                Ok(_) => {}
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            },
            Ok(Event::Frame(Frame::Error { code, message })) => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("server error {code}: {message}"),
                ))
            }
            Ok(Event::Closed(e)) => return Err(io::Error::new(io::ErrorKind::BrokenPipe, e)),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
}

fn stream_local_file(
    sess: &CtrlSession,
    id: u64,
    f: &ManifestFile,
    stats: &mut XferStats,
) -> io::Result<()> {
    let mut file = File::open(&f.path)?;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        stats.sent += n as u64;
        send_payload(
            sess,
            Payload::Data {
                id,
                data: buf[..n].to_vec(),
            },
        )?;
    }
    Ok(())
}

/// Stream `f` as `Copy` (blocks the receiver already has) plus `Data` (the
/// bytes that really differ).
fn send_file_delta(
    sess: &CtrlSession,
    id: u64,
    f: &ManifestFile,
    block_size: u32,
    sigs: &[u8],
    stats: &mut XferStats,
) -> io::Result<()> {
    let block_count = (sigs.len() / delta::SIG_LEN) as u64;
    // The block size is the receiver's choice: it must match the signatures.
    let index = delta::SigIndex::new(block_size, block_count, sigs)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut file = File::open(&f.path)?;
    let mut emit = |op: delta::Op| -> io::Result<()> {
        match op {
            delta::Op::Copy { start, len } => {
                stats.reused += len;
                send_payload(sess, Payload::Copy { id, start, len })
            }
            delta::Op::Data(data) => {
                stats.sent += data.len() as u64;
                send_payload(sess, Payload::Data { id, data })
            }
        }
    };
    delta::produce_delta(&mut file, &index, &mut emit)?;
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
    },
    Failed {
        error: String,
        remote_path: String,
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
                    ..
                }) if rid == id => {
                    if ok {
                        return Ok(TransferOutcome::Done { remote_path });
                    }
                    return Ok(TransferOutcome::Failed {
                        error,
                        remote_path,
                    });
                }
                Ok(Payload::Error {
                    id: rid, message, ..
                }) if rid == id => {
                    return Ok(TransferOutcome::Failed {
                        error: message,
                        remote_path: String::new(),
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

/// One file the controller is about to receive.
struct PullTarget {
    seq: u64,
    dest: PathBuf,
    size: u64,
    mode: u32,
    sha: [u8; 32],
    plan: FilePlan,
    block_size: u32,
    block_count: u64,
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
    // Walking and hashing a large remote tree can take a while; the data phase
    // below gets its own, much longer deadline.
    let manifest_deadline = Instant::now() + Duration::from_secs(60 * 60);
    let deadline = Instant::now() + Duration::from_secs(24 * 60 * 60);
    let mut remote_path = String::new();
    let mut final_path: Option<PathBuf> = None;


    // ---- phase 1: manifest ------------------------------------------------
    let mut files: Vec<PullTarget> = Vec::new();
    let mut total: u64 = 0;
    loop {
        let ev = match recv_before(&sess, manifest_deadline) {
            Ok(e) => e,
            Err(e) => {
                print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
                sess.close();
                return 6;
            }
        };
        let p = match ev {
            Incoming::Payload(p) => p,
            Incoming::Closed(e) => {
                print_xfer_error(&opts.json, "transfer_failed", &e);
                sess.close();
                return 6;
            }
            Incoming::Other => continue,
        };
        if p.id() != id {
            continue;
        }
        match p {
            Payload::Entry {
                seq,
                rel,
                kind,
                size,
                mode,
                link_target,
                sha256,
                ..
            } => {
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
                    ENTRY_SYMLINK => {
                        #[cfg(unix)]
                        {
                            let _ = fs::remove_file(&dest);
                            if let Err(e) = std::os::unix::fs::symlink(&link_target, &dest) {
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
                    ENTRY_FILE => {
                        if seq != files.len() as u64 {
                            print_xfer_error(
                                &opts.json,
                                "transfer_failed",
                                &format!("file seq {seq} is out of order"),
                            );
                            sess.close();
                            return 6;
                        }
                        total += size;
                        files.push(PullTarget {
                            seq,
                            dest,
                            size,
                            mode,
                            sha: sha256,
                            plan: FilePlan::Whole,
                            block_size: 0,
                            block_count: 0,
                        });
                    }
                    other => {
                        print_xfer_error(
                            &opts.json,
                            "transfer_failed",
                            &format!("unknown entry kind {other}"),
                        );
                        sess.close();
                        return 6;
                    }
                }
            }
            Payload::ManifestEnd { .. } => break,
            Payload::TransferDone { error, .. } => {
                // The agent failed while walking the tree; without this arm the
                // controller would wait for a manifest that never comes.
                let detail = if error.is_empty() {
                    "agent finished before the manifest ended".to_string()
                } else {
                    error
                };
                print_xfer_error(&opts.json, "transfer_failed", &detail);
                sess.close();
                return 6;
            }
            Payload::Error { message, .. } => {
                print_xfer_error(&opts.json, "transfer_failed", &message);
                sess.close();
                return 6;
            }
            _ => {}
        }
    }

    // ---- phase 2: plans ---------------------------------------------------
    let mut stats = XferStats {
        total,
        files: files.len() as u64,
        ..Default::default()
    };
    if let Err(e) = plan_pull_files(&sess, id, &mut files, &mut stats) {
        print_xfer_error(&opts.json, "transfer_failed", &e.to_string());
        sess.close();
        return 6;
    }

    // ---- phase 3: data ----------------------------------------------------
    let mut current: Option<(usize, delta::FileWriter)> = None;
    let result = (|| -> Result<(), String> {
        loop {
            let ev = recv_before(&sess, deadline).map_err(|e| e.to_string())?;
            let p = match ev {
                Incoming::Payload(p) => p,
                Incoming::Closed(e) => return Err(e),
                Incoming::Other => continue,
            };
            if p.id() != id {
                continue;
            }
            match p {
                Payload::FileStart { seq, .. } => {
                    if current.is_some() {
                        return Err("FileStart before FileEnd".into());
                    }
                    let idx = match usize::try_from(seq).ok().filter(|i| *i < files.len()) {
                        Some(i) if files[i].seq == seq => i,
                        _ => return Err(format!("FileStart for unknown seq {seq}")),
                    };
                    let t = &files[idx];
                    let base = if t.plan == FilePlan::Delta {
                        Some(t.dest.as_path())
                    } else {
                        None
                    };
                    let name = t
                        .dest
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "file".into());
                    let tmp = t.dest.with_file_name(format!(".{name}.apb-pull-{id}-{seq}.tmp"));
                    match delta::FileWriter::create(&t.dest, base, tmp, t.size) {
                        Ok(w) => current = Some((idx, w)),
                        Err(e) => return Err(format!("create {}: {e}", t.dest.display())),
                    }
                }
                Payload::Data { data, .. } => {
                    let Some((_, w)) = current.as_mut() else {
                        return Err("Data without FileStart".into());
                    };
                    stats.sent += data.len() as u64;
                    if let Err(e) = w.write_literal(&data) {
                        current.take().unwrap().1.abort();
                        return Err(e.to_string());
                    }
                }
                Payload::Copy { start, len, .. } => {
                    let Some((_, w)) = current.as_mut() else {
                        return Err("Copy without FileStart".into());
                    };
                    stats.reused += len;
                    if let Err(e) = w.copy_from_base(start, len) {
                        current.take().unwrap().1.abort();
                        return Err(e.to_string());
                    }
                }
                Payload::FileEnd { sha256, .. } => {
                    let Some((idx, w)) = current.take() else {
                        return Err("FileEnd without FileStart".into());
                    };
                    if sha256 == [0u8; 32] {
                        w.abort();
                        return Err("FileEnd without a content hash".into());
                    }
                    let mode = files[idx].mode;
                    w.finish(&sha256, mode)?;
                }
                Payload::TransferDone {
                    ok,
                    error,
                    remote_path: reported,
                    ..
                } => {
                    if !ok {
                        return Err(error);
                    }
                    remote_path = reported;
                    break;
                }
                Payload::Error { message, .. } => return Err(message),
                _ => {}
            }
        }
        Ok(())
    })();
    if let Some((_, w)) = current.take() {
        w.abort();
    }
    sess.close();
    if let Err(e) = result {
        print_xfer_error(&opts.json, "transfer_failed", &e);
        return 6;
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    let final_path = final_path
        .unwrap_or_else(|| dst_root.join(basename(&remote_path)))
        .to_string_lossy()
        .to_string();
    if opts.json {
        println!(
            "{{\"ok\":true,\"direction\":\"pull\",\"source\":{},\"local_path\":{},\"remote_path\":{},\"method\":\"apb2\",\"bytes\":{},\"total_bytes\":{},\"reused_bytes\":{},\"files\":{},\"skipped_files\":{},\"sha256\":null,\"duration_ms\":{}}}",
            json_escape(&opts.remote),
            json_escape(&final_path),
            json_escape(&remote_path),
            stats.sent,
            stats.total,
            stats.reused,
            stats.files,
            stats.skipped,
            duration_ms
        );
    } else {
        println!(
            "pull -> {final_path} ok ({} bytes total, {} received, {} reused, {} unchanged)",
            stats.total, stats.sent, stats.reused, stats.skipped
        );
    }
    0
}

/// Decide per file what the agent has to send, and send the plans (plus the
/// block signatures of the local copy for delta files).
fn plan_pull_files(
    sess: &CtrlSession,
    id: u64,
    files: &mut [PullTarget],
    stats: &mut XferStats,
) -> io::Result<()> {
    for t in files.iter_mut() {
        let mut set = None;
        let meta = fs::symlink_metadata(&t.dest);
        if let Ok(meta) = meta {
            if meta.is_file() {
                let block_size = delta::choose_block_size(meta.len());
                let scanned = delta::SigSet::scan(&t.dest, block_size)?;
                if t.sha != [0u8; 32] && scanned.file_sha == t.sha {
                    // Identical content: no data, but keep the permissions in
                    // sync with the remote file.
                    let _ = set_mode(&t.dest, t.mode);
                    t.plan = FilePlan::Skip;
                } else if delta::worth_delta(scanned.size, t.size, scanned.block_count) {
                    t.plan = FilePlan::Delta;
                    t.block_size = scanned.block_size;
                    t.block_count = scanned.block_count;
                    set = Some(scanned);
                } else {
                    t.plan = FilePlan::Whole;
                }
            }
        }
        if t.plan == FilePlan::Skip {
            // Nothing will cross the wire for this file; count it here.
            stats.skipped += 1;
            stats.reused += t.size;
        }
        send_payload(
            sess,
            Payload::FilePlan {
                id,
                seq: t.seq,
                action: t.plan.action(),
                block_size: t.block_size,
                block_count: t.block_count,
            },
        )?;
        if let Some(set) = set {
            let blob = set.encode();
            for (n, chunk) in blob.chunks(delta::SIG_BATCH).enumerate() {
                send_payload(
                    sess,
                    Payload::FileSigs {
                        id,
                        seq: t.seq,
                        first: (n * delta::SIGS_PER_FRAME) as u64,
                        sigs: chunk.to_vec(),
                    },
                )?;
            }
        }
    }
    send_payload(sess, Payload::PlanEnd { id })
}

enum Incoming {
    Payload(Payload),
    Closed(String),
    Other,
}

fn recv_before(sess: &CtrlSession, deadline: Instant) -> io::Result<Incoming> {
    let rem = deadline.saturating_duration_since(Instant::now());
    if rem.is_zero() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "transfer timeout"));
    }
    match sess.recv_event(rem) {
        Ok(Event::Frame(Frame::RelayResult { payload, .. })) => match Payload::decode(&payload) {
            Ok(p) => Ok(Incoming::Payload(p)),
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        },
        Ok(Event::Frame(Frame::Error { code, message })) => Err(io::Error::new(
            io::ErrorKind::Other,
            format!("server error {code}: {message}"),
        )),
        Ok(Event::Frame(_)) => Ok(Incoming::Other),
        Ok(Event::Closed(e)) => Ok(Incoming::Closed(e)),
        Err(e) => Err(e),
    }
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
