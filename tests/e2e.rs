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
