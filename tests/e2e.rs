//! End-to-end test: server + agent + controller on loopback.
//!
//! The test intentionally does not use sshd, pre-existing config files or external services.

use std::fs;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_apb")
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn wait_port(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("server did not listen on port {port}");
}

fn wait_status(server: &str, agent: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let out = Command::new(bin())
            .args(["status", "--server", server, "--key", KEY, "--json"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("\"count\":1") && text.contains(agent) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("agent {agent} did not appear in status");
}

#[test]
fn loopback_exec_and_files() {
    let port = free_port();
    let server = format!("127.0.0.1:{port}");
    let agent = format!("apb-test-{}", std::process::id());

    let server_child = Command::new(bin())
        .args(["serve", "--bind", &server, "--key", KEY])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = Kill(server_child);
    wait_port(port);

    let agent_child = Command::new(bin())
        .args(["agent", "--server", &server, "--key", KEY, "--name", &agent])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _agent_guard = Kill(agent_child);
    wait_status(&server, &agent);

    // exec --json: rc and separated streams.
    let out = Command::new(bin())
        .args([
            "exec",
            "--server",
            &server,
            "--key",
            KEY,
            "--name",
            &agent,
            "--json",
            "--",
            "echo hello; echo oops >&2; exit 3",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() || out.status.code() == Some(3),
        "status={:?} text={text}",
        out.status
    );
    assert!(text.contains("\"rc\":3"), "{text}");
    assert!(text.contains("hello"), "{text}");
    assert!(text.contains("oops"), "{text}");

    // push + pull a regular file.
    let root = std::env::temp_dir().join(format!("apb-e2e-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let local = root.join("local.txt");
    fs::write(&local, b"payload-from-host").unwrap();
    let remote = root.join("remote.txt");
    let pulled_dir = root.join("pulled");

    let out = Command::new(bin())
        .args([
            "push",
            "--server",
            &server,
            "--key",
            KEY,
            "--name",
            &agent,
            "--json",
            local.to_str().unwrap(),
            remote.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "push status={:?} text={text} err={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("\"ok\":true"), "{text}");

    let out = Command::new(bin())
        .args([
            "pull",
            "--server",
            &server,
            "--key",
            KEY,
            "--name",
            &agent,
            "--json",
            remote.to_str().unwrap(),
            pulled_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "pull status={:?} text={text} err={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let pulled = pulled_dir.join("remote.txt");
    assert_eq!(fs::read(&pulled).unwrap(), b"payload-from-host");

    let mut f = fs::File::create(&local).unwrap();
    f.write_all(b"x").unwrap();
    drop(f);

    let _ = fs::remove_dir_all(&root);
}

/// Deterministic pseudo-random bytes: identical on every run, but without long
/// runs of equal bytes that would make a delta look better than it is.
fn rand_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    let mut s = seed | 1;
    while out.len() < len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.extend_from_slice(&s.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn json_num(text: &str, key: &str) -> u64 {
    let pat = format!("\"{key}\":");
    let at = text
        .find(&pat)
        .unwrap_or_else(|| panic!("no `{key}` in {text}"))
        + pat.len();
    let rest = &text[at..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end]
        .parse()
        .unwrap_or_else(|_| panic!("`{key}` is not a number in {text}"))
}

/// Start a loopback server + agent pair for the transfer tests.
fn start_pair(tag: &str) -> (String, String, Kill, Kill) {
    let port = free_port();
    let server = format!("127.0.0.1:{port}");
    let agent = format!("apb-{tag}-{}", std::process::id());
    let server_child = Command::new(bin())
        .args(["serve", "--bind", &server, "--key", KEY])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server_guard = Kill(server_child);
    wait_port(port);
    let agent_child = Command::new(bin())
        .args(["agent", "--server", &server, "--key", KEY, "--name", &agent])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let agent_guard = Kill(agent_child);
    wait_status(&server, &agent);
    (server, agent, server_guard, agent_guard)
}

fn run_push(server: &str, agent: &str, local: &std::path::Path, remote: &std::path::Path) -> String {
    let out = Command::new(bin())
        .args([
            "push",
            "--server",
            server,
            "--key",
            KEY,
            "--name",
            agent,
            "--json",
            local.to_str().unwrap(),
            remote.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "push status={:?} text={text} err={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    text
}

fn run_pull(server: &str, agent: &str, remote: &std::path::Path, local: &std::path::Path) -> String {
    let out = Command::new(bin())
        .args([
            "pull",
            "--server",
            server,
            "--key",
            KEY,
            "--name",
            agent,
            "--json",
            remote.to_str().unwrap(),
            local.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "pull status={:?} text={text} err={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    text
}

/// The point of the whole feature: a repeated push/pull of a large file must
/// only move the bytes that actually changed.
#[test]
fn delta_transfer_moves_only_changed_bytes() {
    let (server, agent, _sg, _ag) = start_pair("delta");
    let root = std::env::temp_dir().join(format!("apb-delta-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    const SIZE: usize = 3 * 1024 * 1024;
    let local = root.join("blob.bin");
    let remote = root.join("remote-blob.bin");
    let original = rand_bytes(SIZE, 0x1234_5678);
    fs::write(&local, &original).unwrap();

    // 1) First push: there is nothing on the other side to reuse.
    let text = run_push(&server, &agent, &local, &remote);
    assert_eq!(json_num(&text, "bytes"), SIZE as u64, "{text}");
    assert_eq!(json_num(&text, "total_bytes"), SIZE as u64, "{text}");
    assert_eq!(json_num(&text, "reused_bytes"), 0, "{text}");
    assert_eq!(json_num(&text, "skipped_files"), 0, "{text}");
    assert_eq!(fs::read(&remote).unwrap(), original);

    // 2) Same content again: zero data frames.
    let text = run_push(&server, &agent, &local, &remote);
    assert_eq!(json_num(&text, "bytes"), 0, "{text}");
    assert_eq!(json_num(&text, "skipped_files"), 1, "{text}");
    assert_eq!(json_num(&text, "reused_bytes"), SIZE as u64, "{text}");
    assert_eq!(fs::read(&remote).unwrap(), original);

    // 3) A small edit in the middle: only the touched blocks travel.
    let mut edited = original.clone();
    for b in edited[SIZE / 2..SIZE / 2 + 37].iter_mut() {
        *b ^= 0xa5;
    }
    fs::write(&local, &edited).unwrap();
    let text = run_push(&server, &agent, &local, &remote);
    let sent = json_num(&text, "bytes");
    assert!(sent < 64 * 1024, "sent {sent} bytes for a 37-byte edit: {text}");
    assert!(json_num(&text, "reused_bytes") > SIZE as u64 - 64 * 1024, "{text}");
    assert_eq!(fs::read(&remote).unwrap(), edited);

    // 4) Append: only the tail travels.
    let mut appended = edited.clone();
    appended.extend_from_slice(&rand_bytes(4096, 7));
    fs::write(&local, &appended).unwrap();
    let text = run_push(&server, &agent, &local, &remote);
    let sent = json_num(&text, "bytes");
    assert!(sent < 32 * 1024, "sent {sent} bytes for an append: {text}");
    assert_eq!(fs::read(&remote).unwrap(), appended);

    // 5) Insert at the front: every block shifts, the rolling checksum absorbs it.
    let mut inserted = rand_bytes(1000, 99);
    inserted.extend_from_slice(&appended);
    fs::write(&local, &inserted).unwrap();
    let text = run_push(&server, &agent, &local, &remote);
    let sent = json_num(&text, "bytes");
    assert!(
        sent < 64 * 1024,
        "sent {sent} bytes for a 1000-byte insertion: {text}"
    );
    assert_eq!(fs::read(&remote).unwrap(), inserted);

    // 6) Pull over an existing local copy that differs in a few bytes.
    let pulled = root.join("pulled");
    fs::create_dir_all(&pulled).unwrap();
    let mut local_copy = inserted.clone();
    local_copy[1234] ^= 0xff;
    fs::write(pulled.join("remote-blob.bin"), &local_copy).unwrap();
    let text = run_pull(&server, &agent, &remote, &pulled);
    let got = json_num(&text, "bytes");
    assert!(got < 64 * 1024, "received {got} bytes for a 1-byte edit: {text}");
    assert_eq!(fs::read(pulled.join("remote-blob.bin")).unwrap(), inserted);

    // 7) Pull again: identical, nothing transferred.
    let text = run_pull(&server, &agent, &remote, &pulled);
    assert_eq!(json_num(&text, "bytes"), 0, "{text}");
    assert_eq!(json_num(&text, "skipped_files"), 1, "{text}");

    let _ = fs::remove_dir_all(&root);
}

/// Directory pushes skip every unchanged file and still fix what changed.
#[test]
fn delta_directory_push_skips_unchanged_files() {
    let (server, agent, _sg, _ag) = start_pair("dirdelta");
    let root = std::env::temp_dir().join(format!("apb-dir-delta-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let dst = root.join("dst");
    fs::create_dir_all(src.join("sub")).unwrap();
    // An existing destination directory means the tree lands in `dst/src`,
    // which keeps both pushes of this test aimed at the same remote path.
    fs::create_dir_all(&dst).unwrap();

    let big = rand_bytes(1024 * 1024, 11);
    let small_a = rand_bytes(20_000, 12);
    let small_b = rand_bytes(30_000, 13);
    fs::write(src.join("big.bin"), &big).unwrap();
    fs::write(src.join("sub/a.txt"), &small_a).unwrap();
    fs::write(src.join("sub/b.txt"), &small_b).unwrap();

    let text = run_push(&server, &agent, &src, &dst);
    assert_eq!(json_num(&text, "files"), 3, "{text}");
    assert_eq!(json_num(&text, "skipped_files"), 0, "{text}");
    assert_eq!(json_num(&text, "bytes"), json_num(&text, "total_bytes"), "{text}");
    let remote_root = dst.join("src");
    assert_eq!(fs::read(remote_root.join("big.bin")).unwrap(), big);

    // Only `small_a` changes: the other two files must cost nothing.
    let mut changed = small_a.clone();
    changed.extend_from_slice(b"tail");
    fs::write(src.join("sub/a.txt"), &changed).unwrap();
    let text = run_push(&server, &agent, &src, &dst);
    assert_eq!(json_num(&text, "skipped_files"), 2, "{text}");
    let sent = json_num(&text, "bytes");
    assert!(sent < 16 * 1024, "sent {sent} bytes for one appended file: {text}");
    assert_eq!(fs::read(remote_root.join("big.bin")).unwrap(), big);
    assert_eq!(fs::read(remote_root.join("sub/a.txt")).unwrap(), changed);
    assert_eq!(fs::read(remote_root.join("sub/b.txt")).unwrap(), small_b);

    let _ = fs::remove_dir_all(&root);
}

/// The same push command must land in the same place on every run, otherwise a
/// repeated push can never reuse anything.
#[test]
fn directory_push_lands_in_the_same_place_every_time() {
    let (server, agent, _sg, _ag) = start_pair("repeat");
    let root = std::env::temp_dir().join(format!("apb-repeat-dir-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let dst = root.join("dst"); // intentionally does not exist yet
    fs::create_dir_all(src.join("sub")).unwrap();

    let a = rand_bytes(300_000, 31);
    let b = rand_bytes(120_000, 32);
    fs::write(src.join("a.bin"), &a).unwrap();
    fs::write(src.join("sub/b.bin"), &b).unwrap();

    let text = run_push(&server, &agent, &src, &dst);
    assert_eq!(json_num(&text, "files"), 2, "{text}");
    assert_eq!(json_num(&text, "skipped_files"), 0, "{text}");
    let remote_root = dst.join("src");
    assert_eq!(fs::read(remote_root.join("a.bin")).unwrap(), a);
    assert_eq!(fs::read(remote_root.join("sub/b.bin")).unwrap(), b);

    let text = run_push(&server, &agent, &src, &dst);
    assert_eq!(json_num(&text, "bytes"), 0, "{text}");
    assert_eq!(json_num(&text, "skipped_files"), 2, "{text}");
    assert!(
        !remote_root.join("src").exists(),
        "the tree was nested a second time: {text}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// A skipped file still costs zero data, but its permissions must follow the
/// source: metadata is not content, and a chmod-only change used to be applied
/// by the old always-rewrite flow.
#[cfg(unix)]
#[test]
fn skip_still_syncs_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let (server, agent, _sg, _ag) = start_pair("perm");
    let root = std::env::temp_dir().join(format!("apb-perm-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let local = root.join("perm.txt");
    let remote = root.join("remote-perm.txt");
    fs::write(&local, b"same content").unwrap();
    fs::set_permissions(&local, fs::Permissions::from_mode(0o644)).unwrap();

    let text = run_push(&server, &agent, &local, &remote);
    assert_eq!(json_num(&text, "skipped_files"), 0, "{text}");
    assert_eq!(
        fs::metadata(&remote).unwrap().permissions().mode() & 0o777,
        0o644
    );

    fs::set_permissions(&local, fs::Permissions::from_mode(0o600)).unwrap();
    let text = run_push(&server, &agent, &local, &remote);
    assert_eq!(json_num(&text, "bytes"), 0, "{text}");
    assert_eq!(json_num(&text, "skipped_files"), 1, "{text}");
    assert_eq!(
        fs::metadata(&remote).unwrap().permissions().mode() & 0o777,
        0o600,
        "push must sync permissions even when the content is skipped"
    );

    let pulled = root.join("pulled");
    fs::create_dir_all(&pulled).unwrap();
    let text = run_pull(&server, &agent, &remote, &pulled);
    assert_eq!(json_num(&text, "skipped_files"), 0, "{text}");
    let pulled_file = pulled.join("remote-perm.txt");
    assert_eq!(
        fs::metadata(&pulled_file).unwrap().permissions().mode() & 0o777,
        0o600
    );

    fs::set_permissions(&pulled_file, fs::Permissions::from_mode(0o644)).unwrap();
    let text = run_pull(&server, &agent, &remote, &pulled);
    assert_eq!(json_num(&text, "bytes"), 0, "{text}");
    assert_eq!(json_num(&text, "skipped_files"), 1, "{text}");
    assert_eq!(
        fs::metadata(&pulled_file).unwrap().permissions().mode() & 0o777,
        0o600,
        "pull must sync permissions even when the content is skipped"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn stop_ends_agent_process_with_zero_exit() {
    let port = free_port();
    let server = format!("127.0.0.1:{port}");
    let agent = format!("apb-stop-{}", std::process::id());

    let server_child = Command::new(bin())
        .args(["serve", "--bind", &server, "--key", KEY])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _server_guard = Kill(server_child);
    wait_port(port);

    let mut agent_child = Command::new(bin())
        .args(["agent", "--server", &server, "--key", KEY, "--name", &agent])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_status(&server, &agent);

    let out = Command::new(bin())
        .args([
            "stop", "--server", &server, "--key", KEY, "--name", &agent, "--json", "--reason", "e2e",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stop status={:?} text={text} err={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("\"ended\":true"), "{text}");

    // The point of the whole command: the agent process ends with rc 0, so the
    // CI step running `apb agent` succeeds and the job's post steps (cache save)
    // still run instead of being cancelled at the job timeout.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match agent_child.try_wait().unwrap() {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = agent_child.kill();
                panic!("agent did not exit after stop");
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };
    assert_eq!(status.code(), Some(0), "agent exit status: {status:?}");

    // And the server must eventually drop it from the agent list.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = Command::new(bin())
            .args(["status", "--server", &server, "--key", KEY, "--json"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("\"count\":0") {
            break;
        }
        assert!(Instant::now() < deadline, "agent still listed: {text}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn stop_refuses_to_choose_an_agent_on_its_own() {
    let out = Command::new(bin())
        .args(["stop", "--server", "127.0.0.1:1", "--key", KEY])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(64));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--name"), "{stderr}");
}

#[test]
fn config_file_supplies_server_key_and_name() {
    let port = free_port();
    let server = format!("127.0.0.1:{port}");
    let agent = format!("apb-config-{}", std::process::id());

    let server_child = Command::new(bin())
        .args(["serve", "--bind", &server, "--key", KEY])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _server_guard = Kill(server_child);
    wait_port(port);

    let root = std::env::temp_dir().join(format!("apb-e2e-config-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let config = root.join("agent.conf");
    fs::write(
        &config,
        format!("# apb test config\nAPB_SERVER={server}\nAPB_KEY={KEY}\nAPB_NAME={agent}\n"),
    )
    .unwrap();

    let agent_child = Command::new(bin())
        .args(["agent", "--config", config.to_str().unwrap()])
        .env_remove("APB_SERVER")
        .env_remove("APB_KEY")
        .env_remove("APB_NAME")
        .env_remove("APB_CONFIG")
        .env_remove("APB_CONFIG_DIR")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _agent_guard = Kill(agent_child);
    wait_status(&server, &agent);

    // Controller uses only the config file: both server and key come from it.
    let out = Command::new(bin())
        .args(["status", "--config", config.to_str().unwrap(), "--json"])
        .env_remove("APB_SERVER")
        .env_remove("APB_KEY")
        .env_remove("APB_NAME")
        .env_remove("APB_CONFIG")
        .env_remove("APB_CONFIG_DIR")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "status={:?} text={text} err={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains(&agent), "{text}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn missing_server_and_bind_have_no_default_port() {
    let out = Command::new(bin())
        .args(["status"])
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing server"), "{stderr}");

    let out = Command::new(bin())
        .args(["serve", "--key", KEY])
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing bind"), "{stderr}");
}
