//! `apb`: one binary for server, agent and controller.

use apb::agent::{self, AgentOptions};
use apb::ctrl::{self, ExecOptions, PullOptions, PushOptions, StatusOptions, StopOptions};
use apb::util::{gen_key, hex, json_escape, parse_key};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::exit;
use std::sync::OnceLock;

static FILE_CONFIG: OnceLock<HashMap<String, String>> = OnceLock::new();

fn usage() {
    eprintln!(
        "apb {} - Agent Proxy Bridge (no sshd)\n\
\n\
Usage:\n\
  apb keygen [--json]\n\
  apb serve  [--bind IP:PORT] [--key KEY]\n\
  apb agent  [--server IP:PORT] [--key KEY] [--name NAME] [--config FILE]\n\
  apb status [--server IP:PORT] [--key KEY] [--config FILE] [--json]\n\
  apb exec   [options] -- COMMAND...\n\
  apb push   [options] LOCAL [REMOTE]\n\
  apb pull   [options] REMOTE [LOCAL]\n\
  apb stop   [--server IP:PORT] [--key KEY] --name AGENT [--json] [--reason TEXT]\n\
  apb doctor [--server IP:PORT] [--key KEY] [--config FILE] [--json]\n\
\n\
Server address and --bind require an explicit port; no default port is applied.\n\
Flags override environment variables (APB_SERVER / APB_BIND / APB_KEY / APB_NAME),\n\
and environment variables override the config file.  The config file is selected\n\
by --config, then APB_CONFIG, APB_CONFIG_DIR/agent.conf, XDG_CONFIG_HOME/apb/agent.conf\n\
or ~/.config/apb/agent.conf.  --bind may also come from APB_BIND or config.\n\
\n\
exec options:\n\
  --server A --key K --name AGENT --json --b64 --raw\n\
  --timeout SECONDS --cwd DIR --max-output BYTES\n\
push/pull options:\n\
  --server A --key K --name AGENT --json\n\
stop options:\n\
  --server A --key K --name AGENT --json --reason TEXT --timeout SECONDS\n\
  stop ends the agent process (rc 0), which is what makes a CI step that runs\n\
  `apb agent` finish green instead of being cancelled at the job timeout.",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
        exit(64);
    }
    let cmd = args[0].as_str();
    let rest = &args[1..];
    if !matches!(
        cmd,
        "keygen" | "version" | "--version" | "-V" | "help" | "--help" | "-h"
    ) {
        if let Err(err) = load_file_config(rest) {
            eprintln!("apb: {err}");
            exit(2);
        }
    }
    match cmd {
        "keygen" => cmd_keygen(rest),
        "serve" | "server" => cmd_serve(rest),
        "agent" => cmd_agent(rest),
        "status" => cmd_status(rest),
        "exec" | "run" => cmd_exec(rest),
        "push" => cmd_push(rest),
        "pull" => cmd_pull(rest),
        "stop" => cmd_stop(rest),
        "doctor" => cmd_doctor(rest),
        "version" | "--version" | "-V" => println!("apb {}", env!("CARGO_PKG_VERSION")),
        "help" | "--help" | "-h" => usage(),
        other => {
            eprintln!("apb: unknown command `{other}`");
            usage();
            exit(64);
        }
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

fn env_or(args: &[String], name: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            break;
        }
        if args[i] == name {
            if i + 1 < args.len() {
                return Some(args[i + 1].clone());
            }
            eprintln!("apb: {name} requires a value");
            exit(64);
        }
        if let Some(v) = args[i].strip_prefix(&format!("{name}=")) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
        i += 1;
    }
    None
}

fn env_str(args: &[String], flag: &str, var: &str) -> Option<String> {
    env_or(args, flag).or_else(|| env_var(var)).or_else(|| {
        FILE_CONFIG
            .get()
            .and_then(|m| m.get(var))
            .cloned()
            .filter(|s| !s.trim().is_empty())
    })
}

fn config_path(args: &[String]) -> (Option<PathBuf>, bool) {
    if let Some(path) = env_or(args, "--config") {
        return (Some(PathBuf::from(path)), true);
    }
    if let Some(path) = env_var("APB_CONFIG") {
        return (Some(PathBuf::from(path)), true);
    }
    if let Some(dir) = env_var("APB_CONFIG_DIR") {
        return (Some(PathBuf::from(dir).join("agent.conf")), false);
    }
    let config_home = env_var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env_var("HOME").map(|home| PathBuf::from(home).join(".config")));
    match config_home {
        Some(base) => (Some(base.join("apb").join("agent.conf")), false),
        None => (None, false),
    }
}

/// Read simple `KEY=VALUE` lines from the agent config file.  Missing default
/// files are ignored; an explicitly requested file must exist and be readable.
/// Values are used only as fallbacks for CLI flags / environment variables.
fn load_file_config(args: &[String]) -> Result<(), String> {
    let (path, required) = config_path(args);
    let Some(path) = path else {
        return Ok(());
    };
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if required => {
            return Err(format!("cannot read config file {}: {err}", path.display()));
        }
        Err(_) => return Ok(()),
    };

    let mut values = HashMap::new();
    for (index, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            if required {
                return Err(format!(
                    "invalid config line {} in {}: missing `=`",
                    index + 1,
                    path.display()
                ));
            }
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.starts_with("APB_") {
            continue;
        }
        values.insert(key.to_string(), value.trim().to_string());
    }

    // Only the first loaded file is used; load_file_config runs once per process.
    let _ = FILE_CONFIG.set(values);
    Ok(())
}

fn required_server(args: &[String]) -> String {
    env_str(args, "--server", "APB_SERVER").unwrap_or_else(|| {
        eprintln!(
            "apb: missing server: pass --server, set APB_SERVER, or add APB_SERVER to the config file"
        );
        exit(2);
    })
}

fn required_bind(args: &[String]) -> String {
    env_str(args, "--bind", "APB_BIND").unwrap_or_else(|| {
        eprintln!(
            "apb: missing bind address: pass --bind IP:PORT, set APB_BIND, or add APB_BIND to the config file"
        );
        exit(2);
    })
}

fn has_flag(args: &[String], flag: &str) -> bool {
    for a in args {
        if a == "--" {
            break;
        }
        if a == flag {
            return true;
        }
    }
    false
}

fn required_key(args: &[String]) -> [u8; 32] {
    let raw = env_str(args, "--key", "APB_KEY").unwrap_or_else(|| {
        eprintln!("apb: missing key: pass --key, set APB_KEY, or add APB_KEY to the config file");
        exit(2);
    });
    parse_key(&raw).unwrap_or_else(|e| {
        eprintln!("apb: invalid key: {e}");
        exit(2);
    })
}

fn positional(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut after_dashdash = false;
    while i < args.len() {
        let a = &args[i];
        if after_dashdash {
            out.push(a.clone());
            i += 1;
            continue;
        }
        if a == "--" {
            after_dashdash = true;
            i += 1;
            continue;
        }
        if a.starts_with("--") {
            let takes_value = matches!(
                a.as_str(),
                "--server"
                    | "--key"
                    | "--name"
                    | "--timeout"
                    | "--cwd"
                    | "--max-output"
                    | "--bind"
                    | "--config"
            );
            i += if takes_value { 2 } else { 1 };
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

fn parse_u64(args: &[String], flag: &str, var: &str, default: u64) -> u64 {
    if let Some(v) = env_str(args, flag, var) {
        return v.parse().unwrap_or_else(|_| {
            eprintln!("apb: expected an integer for {flag}: {v}");
            exit(64);
        });
    }
    default
}

fn cmd_keygen(args: &[String]) -> ! {
    let key = gen_key().unwrap_or_else(|e| {
        eprintln!("apb: {e}");
        exit(1);
    });
    if has_flag(args, "--json") {
        println!("{{\"key\":\"{}\"}}", hex(&key));
    } else {
        println!("{}", hex(&key));
    }
    exit(0)
}

fn cmd_serve(args: &[String]) -> ! {
    let key = required_key(args);
    let bind = required_bind(args);
    if let Err(e) = apb::server::serve(&bind, key) {
        eprintln!("apb: serve: {e}");
        exit(1);
    }
    exit(0)
}

fn cmd_agent(args: &[String]) -> ! {
    let server = required_server(args);
    let key = required_key(args);
    let name = env_str(args, "--name", "APB_NAME").unwrap_or_else(agent::default_name);
    if let Err(e) = agent::run(AgentOptions { server, key, name }) {
        eprintln!("apb: agent: {e}");
        exit(1);
    }
    exit(0)
}

fn cmd_status(args: &[String]) -> ! {
    let server = required_server(args);
    let key = required_key(args);
    let json = has_flag(args, "--json");
    exit(ctrl::status(StatusOptions { server, key, json }))
}

fn cmd_doctor(args: &[String]) -> ! {
    let server = required_server(args);
    let key = required_key(args);
    let json = has_flag(args, "--json");
    exit(ctrl::doctor(&server, &key, json))
}

fn cmd_exec(args: &[String]) -> ! {
    let server = required_server(args);
    let key = required_key(args);
    let target = env_str(args, "--name", "APB_NAME").unwrap_or_default();
    let json = has_flag(args, "--json");
    let b64 = has_flag(args, "--b64");
    let raw = has_flag(args, "--raw");
    let timeout_secs = parse_u64(args, "--timeout", "APB_TIMEOUT", 300);
    let cwd = env_str(args, "--cwd", "APB_CWD").unwrap_or_default();
    let max_output = parse_u64(args, "--max-output", "APB_MAX_OUTPUT", 1024 * 1024) as usize;
    let command = positional(args);
    if command.is_empty() {
        eprintln!("apb: exec requires a command; use `apb exec -- 'cmd'`");
        exit(64);
    }
    let command = command.join(" ");
    exit(ctrl::exec(ExecOptions {
        server,
        key,
        target,
        json,
        b64,
        timeout_secs,
        cwd,
        raw,
        max_output,
        command,
    }))
}

fn cmd_push(args: &[String]) -> ! {
    let server = required_server(args);
    let key = required_key(args);
    let target = env_str(args, "--name", "APB_NAME").unwrap_or_default();
    let json = has_flag(args, "--json");
    let pos = positional(args);
    if pos.is_empty() {
        eprintln!("apb: push requires LOCAL [REMOTE]");
        exit(64);
    }
    let local = pos[0].clone();
    let remote = pos.get(1).cloned().unwrap_or_else(|| {
        let name = local.rsplit('/').next().unwrap_or("item").to_string();
        format!("~/{name}")
    });
    exit(ctrl::push(PushOptions {
        server,
        key,
        target,
        json,
        local,
        remote,
    }))
}

fn cmd_pull(args: &[String]) -> ! {
    let server = required_server(args);
    let key = required_key(args);
    let target = env_str(args, "--name", "APB_NAME").unwrap_or_default();
    let json = has_flag(args, "--json");
    let pos = positional(args);
    if pos.is_empty() {
        eprintln!("apb: pull requires REMOTE [LOCAL_DIR]");
        exit(64);
    }
    let remote = pos[0].clone();
    let local = pos.get(1).cloned().unwrap_or_else(|| ".".into());
    exit(ctrl::pull(PullOptions {
        server,
        key,
        target,
        json,
        remote,
        local,
    }))
}

fn cmd_stop(args: &[String]) -> ! {
    let server = required_server(args);
    let key = required_key(args);
    let target = env_str(args, "--name", "APB_NAME").unwrap_or_default();
    if target.is_empty() {
        // Never guess here: ending the wrong node is not recoverable by retry.
        eprintln!(
            "apb: stop requires --name AGENT (or APB_NAME): refusing to choose an agent to end"
        );
        exit(64);
    }
    let json = has_flag(args, "--json");
    let timeout_secs = parse_u64(args, "--timeout", "APB_TIMEOUT", 15);
    let reason = env_or(args, "--reason").unwrap_or_default();
    exit(ctrl::stop(StopOptions {
        server,
        key,
        target,
        json,
        timeout_secs,
        reason,
    }))
}

#[allow(dead_code)]
fn _shown(_: &str) -> String {
    json_escape("")
}
