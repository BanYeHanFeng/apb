//! The client-side agent.
//!
//! Runs on a phone in Termux (aarch64 musl static binary is enough) or on a
//! temporary CI runner.  It owns one outbound TCP connection to the server and
//! reconnects forever with exponential backoff.  It never writes a config file
//! and never starts sshd.

use crate::delta;
use crate::proto::{
    Frame, Payload, ENTRY_DIR, ENTRY_FILE, ENTRY_SYMLINK, PLAN_DELTA, PLAN_SEND, PLAN_SKIP,
    ROLE_AGENT,
};
use crate::util::{basename, expand_tilde, mode_of, safe_rel, set_mode};
use crate::wire::Conn;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAX_AGENT_NAME: usize = 128;
const DATA_CHUNK: usize = 16 * 1024;
const PUMP_GRACE: Duration = Duration::from_secs(2);
/// How long the agent waits for the `Bye` reply to reach the socket.
const BYE_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// After `Bye` is on the wire, keep the process (and its TCP connection) alive
/// briefly so the server reads and forwards the reply instead of seeing a
/// reset connection that could discard it.
const BYE_GRACE: Duration = Duration::from_millis(400);

#[derive(Clone)]
struct WorkerSlot {
    tx: SyncSender<Vec<u8>>,
    cancel: Arc<AtomicBool>,
}

pub struct AgentOptions {
    pub server: String,
    pub key: [u8; 32],
    pub name: String,
}

pub fn default_name() -> String {
    if let Ok(n) = std::env::var("APB_NAME") {
        if !n.trim().is_empty() {
            return n;
        }
    }
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "client".into());
    if user.is_empty() {
        format!("{}-{}", std::env::consts::OS, std::process::id())
    } else {
        format!("{user}@{host}")
    }
}

pub fn run(opts: AgentOptions) -> io::Result<()> {
    let name = sanitize_name(&opts.name);
    let stop = Arc::new(AtomicBool::new(false));
    let mut backoff = 1u64;
    loop {
        eprintln!("apb agent `{name}` connecting to {}", opts.server);
        match run_once(&opts.server, &opts.key, &name, stop.clone()) {
            Ok(()) => {
                eprintln!("apb agent `{name}` connection closed");
                backoff = 1;
            }
            Err(e) => eprintln!("apb agent `{name}`: {e}"),
        }
        // A controller `Shutdown` ends the process instead of reconnecting:
        // that is what lets the CI step running `apb agent` finish with rc 0,
        // which in turn keeps the job green and its cache saveable.
        if stop.load(Ordering::SeqCst) {
            eprintln!("apb agent `{name}` ended by controller");
            return Ok(());
        }
        thread::sleep(Duration::from_secs(backoff.min(30)));
        backoff = (backoff * 2).min(30);
    }
}

fn sanitize_name(name: &str) -> String {
    let mut s: String = name
        .trim()
        .chars()
        .filter(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | ':' | '+' | '~')
        })
        .take(MAX_AGENT_NAME)
        .collect();
    if s.is_empty() {
        s = format!("agent-{}", std::process::id());
    }
    s
}

/// Annotate a connection stage with its name and elapsed time so the one-line
/// error the agent prints is enough to tell *where* a failure happened:
///
/// * `connect`    — DNS / SYN / local socket setup never completed,
/// * `handshake`  — TCP is up but the Noise PSK handshake did not finish
///                  (usually: the key differs from the server's, or something
///                  on the path is cutting the stream),
/// * `hello`      — the agent identity frame could not be sent,
/// * `hello_reply`— the server never answered with `hello_ok`.
///
/// `os error 11` (EAGAIN / "Resource temporarily unavailable") reported here is
/// a *timeout on this socket*, not an apb key or protocol error.
fn stage<T>(label: &str, start: Instant, r: io::Result<T>) -> io::Result<T> {
    r.map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "stage={label} after {}ms: {e}",
                start.elapsed().as_millis()
            ),
        )
    })
}

pub fn run_once(
    server: &str,
    key: &[u8; 32],
    name: &str,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    let started = Instant::now();
    let stage_at = Instant::now();
    let addr = stage("resolve", stage_at, crate::util::resolve_addr(server))?;
    let stage_at = Instant::now();
    let stream = stage("connect", stage_at, TcpStream::connect(addr))?;
    let stage_at = Instant::now();
    let conn = Arc::new(stage(
        "handshake",
        stage_at,
        Conn::from_stream(stream, key, true),
    )?);
    let stage_at = Instant::now();
    stage(
        "hello",
        stage_at,
        conn.send_frame(&Frame::Hello {
            role: ROLE_AGENT,
            name: name.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            pid: std::process::id(),
        }),
    )?;
    let stage_at = Instant::now();
    let reply = stage("hello_reply", stage_at, conn.recv())?;
    match Frame::decode(&reply)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
    {
        Frame::HelloOk { .. } => eprintln!(
            "apb agent `{name}` connected ({}ms)",
            started.elapsed().as_millis()
        ),
        Frame::Error { code, message } => {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("server rejected agent: {code} {message}"),
            ))
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected server hello response: {other:?}"),
            ))
        }
    }

    let workers: Arc<Mutex<HashMap<u64, WorkerSlot>>> = Arc::new(Mutex::new(HashMap::new()));
    let result = reader_loop(conn.clone(), workers.clone(), name, stop.clone());
    cancel_all(&workers);
    if stop.load(Ordering::SeqCst) {
        // The `Bye` reply has been written; hold the connection open a moment
        // longer so the server reads and forwards it.  Jobs still running were
        // cancelled above and kill their own process groups.
        thread::sleep(BYE_GRACE);
    }
    conn.close();
    result
}

fn cancel_all(workers: &Arc<Mutex<HashMap<u64, WorkerSlot>>>) {
    if let Ok(mut m) = workers.lock() {
        for (_, w) in m.drain() {
            w.cancel.store(true, Ordering::Relaxed);
        }
    }
}

fn reader_loop(
    conn: Arc<Conn>,
    workers: Arc<Mutex<HashMap<u64, WorkerSlot>>>,
    name: &str,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    loop {
        let raw = match conn.recv() {
            Ok(v) => v,
            Err(e) => return Err(e),
        };
        let frame = Frame::decode(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        match frame {
            Frame::RelayDeliver { route, payload } => {
                let slot = workers.lock().ok().and_then(|m| m.get(&route).cloned());
                match slot {
                    Some(s) => {
                        if s.tx.send(payload).is_err() {
                            if let Ok(mut m) = workers.lock() {
                                m.remove(&route);
                            }
                        }
                    }
                    None => spawn_worker(
                        conn.clone(),
                        route,
                        payload,
                        workers.clone(),
                        stop.clone(),
                    ),
                }
            }
            Frame::RouteCancel { route } => {
                let slot = workers.lock().ok().and_then(|mut m| m.remove(&route));
                if let Some(s) = slot {
                    s.cancel.store(true, Ordering::Relaxed);
                }
            }
            Frame::Ping { nonce } => {
                let _ = conn.send_frame(&Frame::Pong { nonce });
            }
            Frame::Error { code, message } => {
                eprintln!("apb agent `{name}`: server error {code}: {message}");
            }
            other => eprintln!("apb agent `{name}`: unexpected frame {other:?}"),
        }
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
    }
}

fn spawn_worker(
    conn: Arc<Conn>,
    route: u64,
    initial: Vec<u8>,
    workers: Arc<Mutex<HashMap<u64, WorkerSlot>>>,
    stop: Arc<AtomicBool>,
) {
    let payload = match Payload::decode(&initial) {
        Ok(p) => p,
        Err(e) => {
            send_error(&conn, route, 0, 400, format!("bad payload: {e}"));
            return;
        }
    };
    if handle_shutdown(&conn, route, &payload, &stop) {
        return;
    }
    let id = payload.id();
    let is_start = matches!(
        payload,
        Payload::Exec { .. } | Payload::PushBegin { .. } | Payload::PullBegin { .. }
    );
    if !is_start {
        // The route is gone: its worker already sent the final payload or was
        // cancelled.  Answering every stray continuation frame would flood the
        // controller's queue while it is still streaming and can deadlock both
        // sides, so drop it without a word (the route's outcome is already on
        // the wire).
        return;
    }
    let (tx, rx): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::sync_channel(128);
    let cancel = Arc::new(AtomicBool::new(false));
    let slot = WorkerSlot {
        tx,
        cancel: cancel.clone(),
    };
    if let Ok(mut m) = workers.lock() {
        m.insert(route, slot);
    } else {
        send_error(&conn, route, id, 500, "agent worker map poisoned");
        return;
    }
    let workers2 = workers.clone();
    let _ = thread::Builder::new()
        .name(format!("apb-job-{route}"))
        .spawn(move || {
            if let Err(e) = dispatch(conn.clone(), route, payload, rx, cancel.clone()) {
                send_error(&conn, route, id, 500, format!("job failed: {e}"));
            }
            if let Ok(mut m) = workers2.lock() {
                m.remove(&route);
            }
        });
}

/// Handle a controller `Shutdown`: reply `Bye`, make sure that reply reached
/// the socket, then ask the connection loop to stop.  Returns true when the
/// payload was a lifecycle request and was consumed here.
fn handle_shutdown(
    conn: &Arc<Conn>,
    route: u64,
    payload: &Payload,
    stop: &Arc<AtomicBool>,
) -> bool {
    let Payload::Shutdown { id, reason } = payload else {
        return false;
    };
    let msg = if reason.trim().is_empty() {
        "agent ending".to_string()
    } else {
        format!("agent ending: {reason}")
    };
    eprintln!("apb agent: shutdown requested ({msg})");
    let _ = send_payload(
        conn,
        route,
        Payload::Bye {
            id: *id,
            code: 0,
            reason: msg,
        },
    );
    let flushed = conn.flush(BYE_FLUSH_TIMEOUT);
    eprintln!("apb agent: bye sent (flushed={flushed})");
    stop.store(true, Ordering::SeqCst);
    true
}

fn send_payload(conn: &Conn, route: u64, p: Payload) -> io::Result<()> {
    conn.send(
        Frame::RelayResult {
            route,
            payload: p.encode(),
        }
        .encode(),
    )
}

fn send_error(conn: &Conn, route: u64, id: u64, code: u16, msg: impl Into<String>) {
    let _ = send_payload(
        conn,
        route,
        Payload::Error {
            id,
            code,
            message: msg.into(),
        },
    );
}

fn dispatch(
    conn: Arc<Conn>,
    route: u64,
    initial: Payload,
    rx: Receiver<Vec<u8>>,
    cancel: Arc<AtomicBool>,
) -> io::Result<()> {
    match initial {
        Payload::Exec {
            id,
            timeout_secs,
            cwd,
            raw,
            command,
        } => run_exec(
            conn,
            route,
            ExecRequest {
                id,
                timeout_secs,
                cwd,
                raw,
                command,
            },
            cancel,
        ),
        Payload::PushBegin { .. } => run_push(conn, route, initial, rx, cancel),
        Payload::PullBegin { .. } => run_pull(conn, route, initial, rx, cancel),
        other => {
            send_error(&conn, route, other.id(), 400, "unsupported job");
            Ok(())
        }
    }
}

struct ExecRequest {
    id: u64,
    timeout_secs: u64,
    cwd: String,
    raw: bool,
    command: String,
}

fn shell_spec(raw: bool) -> (String, bool) {
    if let Ok(s) = std::env::var("APB_SHELL") {
        if !s.trim().is_empty() {
            let login = s.contains("bash") || s.contains("zsh");
            return (s, login && !raw);
        }
    }
    if let Ok(prefix) = std::env::var("PREFIX") {
        let b = format!("{prefix}/bin/bash");
        if Path::new(&b).exists() {
            return (b, !raw);
        }
    }
    if let Ok(s) = std::env::var("SHELL") {
        if !s.trim().is_empty() {
            let login = s.contains("bash") || s.contains("zsh");
            return (s, login && !raw);
        }
    }
    if Path::new("/bin/bash").exists() {
        return ("/bin/bash".into(), !raw);
    }
    ("/bin/sh".into(), false)
}

fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    status.code().unwrap_or(255)
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

fn run_exec(
    conn: Arc<Conn>,
    route: u64,
    req: ExecRequest,
    cancel: Arc<AtomicBool>,
) -> io::Result<()> {
    let id = req.id;
    let (shell, login) = shell_spec(req.raw);
    let mut cmd = Command::new(&shell);
    cmd.arg(if login { "-lc" } else { "-c" })
        .arg(&req.command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !req.cwd.is_empty() {
        let cwd = expand_tilde(&req.cwd);
        if !Path::new(&cwd).is_dir() {
            send_error(&conn, route, id, 125, format!("cwd does not exist: {cwd}"));
            return Ok(());
        }
        cmd.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            send_error(
                &conn,
                route,
                id,
                126,
                format!("cannot start shell {shell}: {e}"),
            );
            return Ok(());
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    if let Some(out) = stdout {
        spawn_pump(
            out,
            conn.clone(),
            route,
            id,
            1,
            cancel.clone(),
            done_tx.clone(),
        );
    } else {
        let _ = done_tx.send(());
    }
    if let Some(err) = stderr {
        spawn_pump(
            err,
            conn.clone(),
            route,
            id,
            2,
            cancel.clone(),
            done_tx.clone(),
        );
    } else {
        let _ = done_tx.send(());
    }
    drop(done_tx);

    let deadline = if req.timeout_secs == 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_secs(req.timeout_secs))
    };
    let mut timed_out = false;
    let mut killed = false;
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            killed = true;
            kill_group(child.id());
            let _ = child.wait();
            break None;
        }
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                send_error(&conn, route, id, 500, format!("wait failed: {e}"));
                return Ok(());
            }
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                timed_out = true;
                kill_group(child.id());
                let _ = child.wait();
                break None;
            }
        }
        thread::sleep(Duration::from_millis(30));
    };

    // Give the output pumps a short chance to drain normally.  After that,
    // stop them so a nohup-style background process cannot block the worker.
    let started = Instant::now();
    let mut done = 0;
    while done < 2 && started.elapsed() < PUMP_GRACE {
        match done_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => done += 1,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if done < 2 {
        cancel.store(true, Ordering::Relaxed);
    }

    let rc = match status {
        Some(s) => exit_code(s),
        None if timed_out => 124,
        None => 129,
    };
    let reason = if timed_out {
        "timeout"
    } else if killed {
        "cancelled"
    } else {
        "ok"
    };
    let _ = send_payload(
        &conn,
        route,
        Payload::ExecEnd {
            id,
            rc,
            timed_out,
            reason: reason.to_string(),
        },
    );
    Ok(())
}

fn spawn_pump<R: Read + Send + 'static>(
    mut reader: R,
    conn: Arc<Conn>,
    route: u64,
    id: u64,
    stream: u8,
    cancel: Arc<AtomicBool>,
    done: mpsc::Sender<()>,
) {
    let _ = thread::Builder::new()
        .name(format!("apb-pump-{route}-{stream}"))
        .spawn(move || {
            let mut buf = vec![0u8; DATA_CHUNK];
            loop {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let p = Payload::ExecOut {
                            id,
                            stream,
                            data: buf[..n].to_vec(),
                        };
                        if send_payload(&conn, route, p).is_err() {
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = done.send(());
        });
}
// --------------------------------------------------------------- transfers
//
// `push` (controller → agent) and `pull` (agent → controller) run in the same
// three phases, and each phase is strictly one-directional so a bulk payload
// can never deadlock against a reply that travels the other way:
//
//   1. manifest  the sender lists every entry (metadata + content hash)
//   2. plans     the receiver answers per file: skip, whole file, or delta
//                together with the block signatures of its local copy
//   3. data      the sender emits FileStart + Copy/Data + FileEnd
//
// An unchanged file therefore costs zero data frames, and a file with a small
// change only costs the blocks that really differ.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    Send,
    Skip,
    Delta,
}

impl Plan {
    fn action(self) -> u8 {
        match self {
            Plan::Send => PLAN_SEND,
            Plan::Skip => PLAN_SKIP,
            Plan::Delta => PLAN_DELTA,
        }
    }

    fn from_action(action: u8) -> Result<Plan, String> {
        match action {
            PLAN_SEND => Ok(Plan::Send),
            PLAN_SKIP => Ok(Plan::Skip),
            PLAN_DELTA => Ok(Plan::Delta),
            other => Err(format!("unknown file plan action {other}")),
        }
    }
}

/// One file the controller wants on this node (`push`).
struct PushTarget {
    seq: u64,
    dest: PathBuf,
    size: u64,
    mode: u32,
    sha: [u8; 32],
    plan: Plan,
    block_size: u32,
    block_count: u64,
}

/// One file this node sends to the controller (`pull`).
struct PullSource {
    seq: u64,
    path: PathBuf,
    size: u64,
    sha: [u8; 32],
    plan: Plan,
    block_size: u32,
    block_count: u64,
    sigs: Vec<u8>,
}

/// Wait for the next payload of this job.  `Ok(None)` means the job was
/// cancelled or the route is gone, and the worker should stop quietly.
fn next_payload(rx: &Receiver<Vec<u8>>, cancel: &AtomicBool) -> io::Result<Option<Payload>> {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(raw) => {
                return Payload::decode(&raw).map(Some).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("bad payload: {e}"))
                })
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
}

fn tmp_path(dest: &Path, id: u64, seq: u64) -> PathBuf {
    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    dest.with_file_name(format!(".{name}.apb-{id}-{seq}.tmp"))
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
    let mut out = [0u8; 32];
    out.copy_from_slice(&hasher.finalize());
    Ok(out)
}

fn run_push(
    conn: Arc<Conn>,
    route: u64,
    initial: Payload,
    rx: Receiver<Vec<u8>>,
    cancel: Arc<AtomicBool>,
) -> io::Result<()> {
    let (id, remote_path, source_name, source_is_dir) = match initial {
        Payload::PushBegin {
            id,
            remote_path,
            source_name,
            source_is_dir,
            ..
        } => (id, remote_path, source_name, source_is_dir),
        _ => return Ok(()),
    };
    let root = match resolve_push_root(&remote_path, &source_name, source_is_dir) {
        Ok(p) => p,
        Err(e) => {
            send_error(&conn, route, id, 400, e);
            return Ok(());
        }
    };
    let _ = send_payload(
        &conn,
        route,
        Payload::PushReady {
            id,
            remote_root: root.to_string_lossy().to_string(),
        },
    );

    // ---- phase 1: manifest ------------------------------------------------
    let mut targets: Vec<PushTarget> = Vec::new();
    let declared: u64 = loop {
        let p = match next_payload(&rx, &cancel)? {
            Some(p) => p,
            None => return Ok(()),
        };
        if p.id() != id {
            continue;
        }
        match p {
            Payload::Mkdir { rel, mode, .. } => {
                let dir = match join_rel(&root, &rel) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error(&conn, route, id, 400, e);
                        return Ok(());
                    }
                };
                if let Err(e) = fs::create_dir_all(&dir) {
                    send_error(&conn, route, id, 500, format!("mkdir {}: {e}", dir.display()));
                    return Ok(());
                }
                let _ = set_mode(&dir, mode);
            }
            Payload::Symlink { rel, target, .. } => {
                let path = match join_rel(&root, &rel) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error(&conn, route, id, 400, e);
                        return Ok(());
                    }
                };
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::remove_file(&path);
                #[cfg(unix)]
                {
                    if let Err(e) = std::os::unix::fs::symlink(&target, &path) {
                        send_error(
                            &conn,
                            route,
                            id,
                            500,
                            format!("symlink {}: {e}", path.display()),
                        );
                        return Ok(());
                    }
                }
            }
            Payload::FileBegin {
                seq,
                rel,
                size,
                mode,
                sha256,
                ..
            } => {
                let dest = match join_rel(&root, &rel) {
                    Ok(p) => p,
                    Err(e) => {
                        send_error(&conn, route, id, 400, e);
                        return Ok(());
                    }
                };
                if dest.is_dir() {
                    send_error(
                        &conn,
                        route,
                        id,
                        400,
                        format!("target is a directory: {}", dest.display()),
                    );
                    return Ok(());
                }
                if seq != targets.len() as u64 {
                    send_error(
                        &conn,
                        route,
                        id,
                        400,
                        format!("file seq {seq} is out of order"),
                    );
                    return Ok(());
                }
                targets.push(PushTarget {
                    seq,
                    dest,
                    size,
                    mode,
                    sha: sha256,
                    plan: Plan::Send,
                    block_size: 0,
                    block_count: 0,
                });
            }
            Payload::ManifestEnd { files, .. } => break files,
            Payload::Error { .. } => return Ok(()),
            _ => {}
        }
    };
    if declared != targets.len() as u64 {
        send_error(
            &conn,
            route,
            id,
            400,
            format!(
                "manifest declared {declared} files but listed {}",
                targets.len()
            ),
        );
        return Ok(());
    }

    // ---- phase 2: plans ---------------------------------------------------
    for t in targets.iter_mut() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let sigs = match plan_push_target(t) {
            Ok(s) => s,
            Err(e) => {
                send_error(&conn, route, id, 500, e.to_string());
                return Ok(());
            }
        };
        let plan = Payload::FilePlan {
            id,
            seq: t.seq,
            action: t.plan.action(),
            block_size: t.block_size,
            block_count: t.block_count,
        };
        if send_payload(&conn, route, plan).is_err() {
            return Ok(());
        }
        if let Some(set) = sigs {
            let blob = set.encode();
            for (i, chunk) in blob.chunks(delta::SIG_BATCH).enumerate() {
                let frame = Payload::FileSigs {
                    id,
                    seq: t.seq,
                    first: (i * delta::SIGS_PER_FRAME) as u64,
                    sigs: chunk.to_vec(),
                };
                if send_payload(&conn, route, frame).is_err() {
                    return Ok(());
                }
            }
        }
    }
    let _ = send_payload(&conn, route, Payload::PlanEnd { id });

    // ---- phase 3: data ----------------------------------------------------
    let total: u64 = targets.iter().map(|t| t.size).sum();
    let mut current: Option<(usize, delta::FileWriter)> = None;
    loop {
        let p = match next_payload(&rx, &cancel) {
            Ok(Some(p)) => p,
            Ok(None) => {
                if let Some((_, w)) = current.take() {
                    w.abort();
                }
                return Ok(());
            }
            Err(e) => {
                if let Some((_, w)) = current.take() {
                    w.abort();
                }
                return Err(e);
            }
        };
        if p.id() != id {
            continue;
        }
        match p {
            Payload::FileStart { seq, .. } => {
                let idx = match usize::try_from(seq).ok().filter(|i| *i < targets.len()) {
                    Some(i) if targets[i].seq == seq => i,
                    _ => {
                        if let Some((_, w)) = current.take() {
                            w.abort();
                        }
                        send_error(
                            &conn,
                            route,
                            id,
                            400,
                            format!("FileStart for unknown seq {seq}"),
                        );
                        return Ok(());
                    }
                };
                if current.is_some() {
                    if let Some((_, w)) = current.take() {
                        w.abort();
                    }
                    send_error(&conn, route, id, 400, "FileStart before FileEnd");
                    return Ok(());
                }
                let t = &targets[idx];
                let base = if t.plan == Plan::Delta {
                    Some(t.dest.as_path())
                } else {
                    None
                };
                match delta::FileWriter::create(&t.dest, base, tmp_path(&t.dest, id, seq), t.size) {
                    Ok(w) => current = Some((idx, w)),
                    Err(e) => {
                        send_error(
                            &conn,
                            route,
                            id,
                            500,
                            format!("create {}: {e}", t.dest.display()),
                        );
                        return Ok(());
                    }
                }
            }
            Payload::Data { data, .. } => {
                let Some((_, w)) = current.as_mut() else {
                    send_error(&conn, route, id, 400, "Data without FileStart");
                    return Ok(());
                };
                if let Err(e) = w.write_literal(&data) {
                    let msg = e.to_string();
                    current.take().unwrap().1.abort();
                    send_error(&conn, route, id, 500, msg);
                    return Ok(());
                }
            }
            Payload::Copy { start, len, .. } => {
                let Some((_, w)) = current.as_mut() else {
                    send_error(&conn, route, id, 400, "Copy without FileStart");
                    return Ok(());
                };
                if let Err(e) = w.copy_from_base(start, len) {
                    let msg = e.to_string();
                    current.take().unwrap().1.abort();
                    send_error(&conn, route, id, 500, msg);
                    return Ok(());
                }
            }
            Payload::FileEnd { sha256, .. } => {
                let Some((idx, w)) = current.take() else {
                    send_error(&conn, route, id, 400, "FileEnd without FileStart");
                    return Ok(());
                };
                if sha256 == [0u8; 32] {
                    w.abort();
                    send_error(&conn, route, id, 400, "FileEnd without a content hash");
                    return Ok(());
                }
                let mode = targets[idx].mode;
                if let Err(e) = w.finish(&sha256, mode) {
                    send_error(&conn, route, id, 500, e);
                    return Ok(());
                }
            }
            Payload::PushEnd { .. } => {
                if let Some((_, w)) = current.take() {
                    w.abort();
                    send_error(&conn, route, id, 400, "PushEnd before FileEnd");
                    return Ok(());
                }
                let _ = send_payload(
                    &conn,
                    route,
                    Payload::TransferDone {
                        id,
                        ok: true,
                        error: String::new(),
                        remote_path: root.to_string_lossy().to_string(),
                        bytes: total,
                        sha256: [0u8; 32],
                    },
                );
                return Ok(());
            }
            Payload::Error { .. } => {
                if let Some((_, w)) = current.take() {
                    w.abort();
                }
                return Ok(());
            }
            _ => {}
        }
    }
}

/// Decide how the controller has to deliver one file and, for a delta, return
/// the block signatures of the local copy (computed in the same pass that
/// hashes it).
fn plan_push_target(t: &mut PushTarget) -> io::Result<Option<delta::SigSet>> {
    t.plan = Plan::Send;
    t.block_size = 0;
    t.block_count = 0;
    let meta = match fs::symlink_metadata(&t.dest) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if !meta.is_file() {
        return Ok(None);
    }
    let base_size = meta.len();
    let set = delta::SigSet::scan(&t.dest, delta::choose_block_size(base_size))?;
    if t.sha != [0u8; 32] && set.file_sha == t.sha {
        // Content is identical: no data crosses the wire, but permissions are
        // metadata the source owns and must still be applied.
        let _ = set_mode(&t.dest, t.mode);
        t.plan = Plan::Skip;
        return Ok(None);
    }
    if !delta::worth_delta(base_size, t.size, set.block_count) {
        return Ok(None);
    }
    t.plan = Plan::Delta;
    t.block_size = set.block_size;
    t.block_count = set.block_count;
    Ok(Some(set))
}

fn resolve_push_root(
    remote_path: &str,
    source_name: &str,
    source_is_dir: bool,
) -> Result<PathBuf, String> {
    let expanded = expand_tilde(remote_path);
    let trailing = remote_path.ends_with('/');
    let p = Path::new(&expanded);
    let exists = p.exists();
    let is_dir = exists && p.is_dir();
    if source_is_dir {
        if exists && !is_dir {
            return Err(format!(
                "cannot push directory onto existing file: {}",
                p.display()
            ));
        }
        // A directory always keeps its own name inside the remote path, whether
        // or not that path already exists (rsync semantics).  Resolving it any
        // other way would move the destination between two identical pushes and
        // throw away everything the delta transfer could reuse.
        let root = p.join(source_name);
        fs::create_dir_all(&root).map_err(|e| format!("mkdir {}: {e}", root.display()))?;
        return Ok(root);
    }
    let root = if is_dir || trailing {
        p.join(source_name)
    } else {
        p.to_path_buf()
    };
    if root.is_dir() {
        return Err(format!("target is a directory: {}", root.display()));
    }
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    Ok(root)
}

fn join_rel(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Ok(root.to_path_buf());
    }
    let r = safe_rel(rel).map_err(|e| e.to_string())?;
    Ok(root.join(r))
}

fn run_pull(
    conn: Arc<Conn>,
    route: u64,
    initial: Payload,
    rx: Receiver<Vec<u8>>,
    cancel: Arc<AtomicBool>,
) -> io::Result<()> {
    let (id, remote_path) = match initial {
        Payload::PullBegin { id, remote_path } => (id, remote_path),
        _ => return Ok(()),
    };
    let expanded = expand_tilde(&remote_path);
    let root = Path::new(&expanded);
    let meta = match fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) => {
            send_error(
                &conn,
                route,
                id,
                404,
                format!("remote path not found: {} ({e})", root.display()),
            );
            return Ok(());
        }
    };
    let root_name = basename(&expanded);

    // ---- phase 1: manifest ------------------------------------------------
    let mut sources: Vec<PullSource> = Vec::new();
    let result = if meta.file_type().is_symlink() {
        let target = fs::read_link(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        send_entry(
            &conn,
            route,
            id,
            0,
            &root_name,
            ENTRY_SYMLINK,
            0,
            mode_of(&meta),
            &target,
            [0u8; 32],
        )
    } else if meta.is_file() {
        let sha = hash_file(root)?;
        sources.push(PullSource {
            seq: 0,
            path: root.to_path_buf(),
            size: meta.len(),
            sha,
            plan: Plan::Send,
            block_size: 0,
            block_count: 0,
            sigs: Vec::new(),
        });
        send_entry(
            &conn,
            route,
            id,
            0,
            &root_name,
            ENTRY_FILE,
            meta.len(),
            mode_of(&meta),
            "",
            sha,
        )
    } else if meta.is_dir() {
        send_entry(
            &conn,
            route,
            id,
            0,
            &root_name,
            ENTRY_DIR,
            0,
            mode_of(&meta),
            "",
            [0u8; 32],
        )
        .and_then(|()| walk_pull(&conn, route, id, root, &root_name, &mut sources, &cancel))
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "unsupported special file",
        ))
    };
    if let Err(e) = result {
        let _ = send_payload(
            &conn,
            route,
            Payload::TransferDone {
                id,
                ok: false,
                error: e.to_string(),
                remote_path: root.to_string_lossy().to_string(),
                bytes: 0,
                sha256: [0u8; 32],
            },
        );
        return Ok(());
    }
    let total: u64 = sources.iter().map(|s| s.size).sum();
    let _ = send_payload(
        &conn,
        route,
        Payload::ManifestEnd {
            id,
            files: sources.len() as u64,
        },
    );

    // ---- phase 2: plans ---------------------------------------------------
    let mut planned = false;
    while !planned {
        let p = match next_payload(&rx, &cancel)? {
            Some(p) => p,
            None => return Ok(()),
        };
        if p.id() != id {
            continue;
        }
        let step = match p {
            Payload::FilePlan {
                seq,
                action,
                block_size,
                block_count,
                ..
            } => match sources.get_mut(seq as usize) {
                Some(s) if s.seq == seq => match Plan::from_action(action) {
                    Ok(plan) => {
                        s.plan = plan;
                        s.block_size = block_size;
                        s.block_count = block_count;
                        s.sigs.clear();
                        Ok(())
                    }
                    Err(e) => Err(e),
                },
                _ => Err(format!("plan for unknown file seq {seq}")),
            },
            Payload::FileSigs {
                seq, first, sigs, ..
            } => match sources.get_mut(seq as usize) {
                Some(s) if s.seq == seq => {
                    let at = usize::try_from(first)
                        .ok()
                        .and_then(|n| n.checked_mul(delta::SIG_LEN));
                    match at {
                        Some(at) if s.sigs.len() == at => {
                            s.sigs.extend_from_slice(&sigs);
                            Ok(())
                        }
                        _ => Err(format!("signature frame out of order for file seq {seq}")),
                    }
                }
                _ => Err(format!("signatures for unknown file seq {seq}")),
            },
            Payload::PlanEnd { .. } => {
                planned = true;
                Ok(())
            }
            Payload::Error { .. } => return Ok(()),
            _ => Ok(()),
        };
        if let Err(e) = step {
            send_error(&conn, route, id, 400, e);
            return Ok(());
        }
    }

    // ---- phase 3: data ----------------------------------------------------
    for s in sources.iter() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if s.plan == Plan::Skip {
            continue;
        }
        if send_payload(&conn, route, Payload::FileStart { id, seq: s.seq }).is_err() {
            return Ok(());
        }
        match s.plan {
            Plan::Send => {
                if let Err(e) = stream_file(&conn, route, id, &s.path, &cancel) {
                    return finish_pull(&conn, route, id, root, total, Err(e));
                }
            }
            Plan::Delta => {
                let idx = match delta::SigIndex::new(s.block_size, s.block_count, &s.sigs) {
                    Ok(i) => i,
                    Err(e) => {
                        send_error(&conn, route, id, 400, e);
                        return Ok(());
                    }
                };
                let mut f = File::open(&s.path)?;
                let mut emit = |op: delta::Op| -> io::Result<()> {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                    }
                    let p = match op {
                        delta::Op::Copy { start, len } => Payload::Copy { id, start, len },
                        delta::Op::Data(data) => Payload::Data { id, data },
                    };
                    send_payload(&conn, route, p)
                        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))
                };
                if let Err(e) = delta::produce_delta(&mut f, &idx, &mut emit) {
                    if e.kind() == io::ErrorKind::Interrupted {
                        return Ok(());
                    }
                    return finish_pull(&conn, route, id, root, total, Err(e));
                }
            }
            Plan::Skip => {}
        }
        let frame = Payload::FileEnd { id, sha256: s.sha };
        if send_payload(&conn, route, frame).is_err() {
            return Ok(());
        }
    }
    finish_pull(&conn, route, id, root, total, Ok(()))
}

fn finish_pull(
    conn: &Conn,
    route: u64,
    id: u64,
    root: &Path,
    total: u64,
    result: io::Result<()>,
) -> io::Result<()> {
    let (ok, error) = match result {
        Ok(()) => (true, String::new()),
        Err(e) => (false, e.to_string()),
    };
    let _ = send_payload(
        conn,
        route,
        Payload::TransferDone {
            id,
            ok,
            error,
            remote_path: root.to_string_lossy().to_string(),
            bytes: total,
            sha256: [0u8; 32],
        },
    );
    Ok(())
}

fn walk_pull(
    conn: &Conn,
    route: u64,
    id: u64,
    dir: &Path,
    rel_prefix: &str,
    sources: &mut Vec<PullSource>,
    cancel: &AtomicBool,
) -> io::Result<()> {
    let mut entries: Vec<PathBuf> = Vec::new();
    for e in fs::read_dir(dir)? {
        entries.push(e?.path());
    }
    entries.sort();
    for p in entries {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "x".into());
        let rel = format!("{rel_prefix}/{name}");
        let meta = fs::symlink_metadata(&p)?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&p)
                .map(|t| t.to_string_lossy().to_string())
                .unwrap_or_default();
            send_entry(
                conn,
                route,
                id,
                0,
                &rel,
                ENTRY_SYMLINK,
                0,
                mode_of(&meta),
                &target,
                [0u8; 32],
            )?;
        } else if meta.is_dir() {
            send_entry(
                conn,
                route,
                id,
                0,
                &rel,
                ENTRY_DIR,
                0,
                mode_of(&meta),
                "",
                [0u8; 32],
            )?;
            walk_pull(conn, route, id, &p, &rel, sources, cancel)?;
        } else if meta.is_file() {
            // The hash is what lets the controller skip an identical local file
            // and what verifies the rebuilt file afterwards.
            let sha = hash_file(&p)?;
            let seq = sources.len() as u64;
            sources.push(PullSource {
                seq,
                path: p.clone(),
                size: meta.len(),
                sha,
                plan: Plan::Send,
                block_size: 0,
                block_count: 0,
                sigs: Vec::new(),
            });
            send_entry(
                conn,
                route,
                id,
                seq,
                &rel,
                ENTRY_FILE,
                meta.len(),
                mode_of(&meta),
                "",
                sha,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn send_entry(
    conn: &Conn,
    route: u64,
    id: u64,
    seq: u64,
    rel: &str,
    kind: u8,
    size: u64,
    mode: u32,
    link_target: &str,
    sha: [u8; 32],
) -> io::Result<()> {
    send_payload(
        conn,
        route,
        Payload::Entry {
            id,
            seq,
            rel: rel.to_string(),
            kind,
            size,
            mode,
            link_target: link_target.to_string(),
            sha256: sha,
        },
    )
}

fn stream_file(
    conn: &Conn,
    route: u64,
    id: u64,
    path: &Path,
    cancel: &AtomicBool,
) -> io::Result<u64> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; DATA_CHUNK];
    let mut total = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        send_payload(
            conn,
            route,
            Payload::Data {
                id,
                data: buf[..n].to_vec(),
            },
        )?;
        total += n as u64;
    }
    Ok(total)
}
