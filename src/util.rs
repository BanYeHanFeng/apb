//! Small helpers shared by all roles.

use crate::proto::{PResult, ProtoError};
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn err<T>(msg: impl Into<String>) -> PResult<T> {
    Err(ProtoError(msg.into()))
}

pub fn io_err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn now_iso() -> String {
    // Not a real calendar implementation; status output only needs a stable,
    // machine-readable timestamp.
    let ms = now_ms();
    format!("{}", ms)
}

pub fn parse_key(s: &str) -> io::Result<[u8; 32]> {
    let s = s.trim();
    let s = s
        .strip_prefix("hex:")
        .or_else(|| s.strip_prefix("HEX:"))
        .unwrap_or(s);
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        let mut out = [0u8; 32];
        for i in 0..32 {
            let hi = hex_val(s.as_bytes()[i * 2])?;
            let lo = hex_val(s.as_bytes()[i * 2 + 1])?;
            out[i] = (hi << 4) | lo;
        }
        return Ok(out);
    }
    if let Some(b) = s
        .strip_prefix("base64:")
        .or_else(|| s.strip_prefix("BASE64:"))
    {
        if let Some(k) = decode_base64(b.trim()) {
            if k.len() == 32 {
                let mut out = [0u8; 32];
                out.copy_from_slice(&k);
                return Ok(out);
            }
        }
    }
    Err(io_err(
        "key must be 64 hex characters (or base64:... with 32 bytes)",
    ))
}

fn hex_val(c: u8) -> io::Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(io_err("invalid hex digit in key")),
    }
}

pub fn gen_key() -> io::Result<[u8; 32]> {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| io_err(format!("getrandom failed: {e}")))?;
    Ok(key)
}

pub fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 0xf) as usize] as char);
    }
    s
}

pub fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

pub fn encode_base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for c in data.chunks(3) {
        let b0 = c[0] as u32;
        let b1 = if c.len() > 1 { c[1] as u32 } else { 0 };
        let b2 = if c.len() > 2 { c[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        if c.len() > 1 {
            out.push(T[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if c.len() > 2 {
            out.push(T[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn json_bool(v: bool) -> &'static str {
    if v {
        "true"
    } else {
        "false"
    }
}

/// Expand a leading `~` using HOME without touching the shell.
pub fn expand_tilde(p: &str) -> String {
    if p == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| "/".into());
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}

/// Reject relative paths that could escape the destination root.
pub fn safe_rel(rel: &str) -> PResult<PathBuf> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return err(format!("absolute path not allowed in archive entry: {rel}"));
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(x) => out.push(x),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return err(format!("unsafe path in archive entry: {rel}"));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return err("empty archive entry path");
    }
    Ok(out)
}

pub fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "item".into())
}

pub fn resolve_addr(s: &str) -> io::Result<SocketAddr> {
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Ok(a);
    }
    let mut addrs = s.to_socket_addrs()?;
    addrs
        .next()
        .ok_or_else(|| io_err(format!("cannot resolve address: {s}")))
}

pub fn split_host_port(s: &str, default_port: u16) -> io::Result<String> {
    let s = s.trim();
    if s.is_empty() {
        return Err(io_err("empty server address"));
    }
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Ok(a.to_string());
    }
    if s.starts_with('[') {
        // [ipv6] or [ipv6]:port
        if s.contains("]:") {
            return Ok(s.to_string());
        }
        return Ok(format!("{}:{}", s, default_port));
    }
    let colons = s.matches(':').count();
    if colons == 1 {
        // host:port or ipv4:port
        return Ok(s.to_string());
    }
    if colons > 1 {
        // bare IPv6
        return Ok(format!("[{s}]:{default_port}"));
    }
    Ok(format!("{s}:{default_port}"))
}

#[cfg(unix)]
pub fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
pub fn mode_of(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if mode != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

pub fn is_dir(path: &str) -> bool {
    std::fs::metadata(expand_tilde(path))
        .map(|m| m.is_dir())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrip() {
        let k = [0xabu8; 32];
        let s = hex(&k);
        assert_eq!(parse_key(&s).unwrap(), k);
        assert_eq!(
            parse_key(&format!("base64:{}", encode_base64(&k))).unwrap(),
            k
        );
    }

    #[test]
    fn unsafe_rels_rejected() {
        assert!(safe_rel("/etc/passwd").is_err());
        assert!(safe_rel("../x").is_err());
        assert!(safe_rel("a/b").is_ok());
    }

    #[test]
    fn json_escape_basic() {
        assert_eq!(json_escape("a\"b\n"), "\"a\\\"b\\n\"");
    }
}
