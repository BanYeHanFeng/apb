//! Wire messages for the APB0 relay protocol (apb 0.1.0).
//!
//! Every TCP connection runs a Noise handshake first.  After that, each
//! encrypted Noise message carries exactly one `Frame` (control-plane) or one
//! relay envelope whose body is a `Payload` (data-plane).  Frames are laid out
//! with explicit big-endian integers and length-prefixed strings so the format
//! is independent of any serde/JSON implementation.

use std::fmt;

pub const MAGIC: &[u8; 4] = b"APB0";

// Control-plane frame tags.
pub const F_HELLO: u8 = 1;
pub const F_HELLO_OK: u8 = 2;
pub const F_ERROR: u8 = 3;
pub const F_PING: u8 = 4;
pub const F_PONG: u8 = 5;
pub const F_RELAY_REQ: u8 = 6;
pub const F_RELAY_DELIVER: u8 = 7;
pub const F_RELAY_RESULT: u8 = 8;
pub const F_ROUTE_CANCEL: u8 = 9;
pub const F_LIST_REQ: u8 = 10;
pub const F_LIST_RES: u8 = 11;

pub const ROLE_AGENT: u8 = 1;
pub const ROLE_CTRL: u8 = 2;

// Data-plane payload tags.  Every payload starts with `op` followed by the
// request id (u64, big-endian) so the server can route/answer continuation
// frames without understanding the operation.
pub const OP_EXEC: u8 = 1;
pub const OP_EXEC_OUT: u8 = 2;
pub const OP_EXEC_END: u8 = 3;
pub const OP_ERROR: u8 = 4;
pub const OP_PUSH_BEGIN: u8 = 10;
pub const OP_PUSH_READY: u8 = 11;
pub const OP_MKDIR: u8 = 12;
pub const OP_FILE_BEGIN: u8 = 13;
pub const OP_DATA: u8 = 14;
pub const OP_FILE_END: u8 = 15;
pub const OP_SYMLINK: u8 = 16;
pub const OP_PUSH_END: u8 = 17;
pub const OP_TRANSFER_DONE: u8 = 18;
pub const OP_PULL_BEGIN: u8 = 20;
pub const OP_ENTRY: u8 = 21;

pub const ENTRY_DIR: u8 = 0;
pub const ENTRY_FILE: u8 = 1;
pub const ENTRY_SYMLINK: u8 = 2;

pub const MAX_STR: usize = 1 << 20;
pub const MAX_BLOB: usize = 16 << 20;

#[derive(Debug, Clone)]
pub struct ProtoError(pub String);

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ProtoError {}

pub type PResult<T> = Result<T, ProtoError>;

fn err<T>(msg: impl Into<String>) -> PResult<T> {
    Err(ProtoError(msg.into()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub role: u8,
    pub name: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub connected_at_ms: u64,
    pub last_seen_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Hello {
        role: u8,
        name: String,
        version: String,
        os: String,
        arch: String,
        pid: u32,
    },
    HelloOk {
        version: String,
    },
    Error {
        code: u16,
        message: String,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    RelayReq {
        target: String,
        payload: Vec<u8>,
    },
    RelayDeliver {
        route: u64,
        payload: Vec<u8>,
    },
    RelayResult {
        route: u64,
        payload: Vec<u8>,
    },
    RouteCancel {
        route: u64,
    },
    ListReq,
    ListRes {
        peers: Vec<PeerInfo>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Exec {
        id: u64,
        timeout_secs: u64,
        cwd: String,
        raw: bool,
        command: String,
    },
    ExecOut {
        id: u64,
        stream: u8,
        data: Vec<u8>,
    },
    ExecEnd {
        id: u64,
        rc: i32,
        timed_out: bool,
        reason: String,
    },
    Error {
        id: u64,
        code: u16,
        message: String,
    },
    PushBegin {
        id: u64,
        remote_path: String,
        source_name: String,
        source_is_dir: bool,
        source_files: u64,
        source_bytes: u64,
    },
    PushReady {
        id: u64,
        remote_root: String,
    },
    Mkdir {
        id: u64,
        rel: String,
        mode: u32,
    },
    FileBegin {
        id: u64,
        rel: String,
        size: u64,
        mode: u32,
        sha256: [u8; 32],
    },
    Data {
        id: u64,
        data: Vec<u8>,
    },
    FileEnd {
        id: u64,
    },
    Symlink {
        id: u64,
        rel: String,
        target: String,
        mode: u32,
    },
    PushEnd {
        id: u64,
    },
    TransferDone {
        id: u64,
        ok: bool,
        error: String,
        remote_path: String,
        bytes: u64,
        sha256: [u8; 32],
    },
    PullBegin {
        id: u64,
        remote_path: String,
    },
    Entry {
        id: u64,
        rel: String,
        kind: u8,
        size: u64,
        mode: u32,
        link_target: String,
        sha256: [u8; 32],
    },
}

pub struct Encoder {
    pub buf: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(128),
        }
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(if v { 1 } else { 0 })
    }
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
        self
    }
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
    pub fn fixed32(&mut self, v: &[u8; 32]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn need(&self, n: usize) -> PResult<()> {
        if self.pos + n > self.buf.len() {
            err(format!(
                "truncated message at {} (need {} bytes, have {})",
                self.pos,
                n,
                self.buf.len() - self.pos
            ))
        } else {
            Ok(())
        }
    }
    pub fn u8(&mut self) -> PResult<u8> {
        self.need(1)?;
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }
    pub fn u16(&mut self) -> PResult<u16> {
        self.need(2)?;
        let v = u16::from_be_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }
    pub fn u32(&mut self) -> PResult<u32> {
        self.need(4)?;
        let mut b = [0u8; 4];
        b.copy_from_slice(&self.buf[self.pos..self.pos + 4]);
        self.pos += 4;
        Ok(u32::from_be_bytes(b))
    }
    pub fn u64(&mut self) -> PResult<u64> {
        self.need(8)?;
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.buf[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(u64::from_be_bytes(b))
    }
    pub fn i32(&mut self) -> PResult<i32> {
        self.need(4)?;
        let mut b = [0u8; 4];
        b.copy_from_slice(&self.buf[self.pos..self.pos + 4]);
        self.pos += 4;
        Ok(i32::from_be_bytes(b))
    }
    pub fn bool(&mut self) -> PResult<bool> {
        Ok(self.u8()? != 0)
    }
    pub fn bytes(&mut self) -> PResult<Vec<u8>> {
        let n = self.u32()? as usize;
        if n > MAX_BLOB {
            return err(format!("blob too large: {} bytes", n));
        }
        self.need(n)?;
        let v = self.buf[self.pos..self.pos + n].to_vec();
        self.pos += n;
        Ok(v)
    }
    pub fn str(&mut self) -> PResult<String> {
        let n = self.u32()? as usize;
        if n > MAX_STR {
            return err(format!("string too large: {} bytes", n));
        }
        self.need(n)?;
        let v = std::str::from_utf8(&self.buf[self.pos..self.pos + n])
            .map_err(|_| ProtoError("invalid UTF-8 string".into()))?
            .to_string();
        self.pos += n;
        Ok(v)
    }
    pub fn fixed32(&mut self) -> PResult<[u8; 32]> {
        self.need(32)?;
        let mut v = [0u8; 32];
        v.copy_from_slice(&self.buf[self.pos..self.pos + 32]);
        self.pos += 32;
        Ok(v)
    }
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
    pub fn finish(&self) -> PResult<()> {
        if self.pos != self.buf.len() {
            err(format!(
                "trailing bytes after message: {}",
                self.buf.len() - self.pos
            ))
        } else {
            Ok(())
        }
    }
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        match self {
            Frame::Hello {
                role,
                name,
                version,
                os,
                arch,
                pid,
            } => {
                e.u8(F_HELLO)
                    .u8(*role)
                    .str(name)
                    .str(version)
                    .str(os)
                    .str(arch)
                    .u32(*pid);
            }
            Frame::HelloOk { version } => {
                e.u8(F_HELLO_OK).str(version);
            }
            Frame::Error { code, message } => {
                e.u8(F_ERROR).u16(*code).str(message);
            }
            Frame::Ping { nonce } => {
                e.u8(F_PING).u64(*nonce);
            }
            Frame::Pong { nonce } => {
                e.u8(F_PONG).u64(*nonce);
            }
            Frame::RelayReq { target, payload } => {
                e.u8(F_RELAY_REQ).str(target).bytes(payload);
            }
            Frame::RelayDeliver { route, payload } => {
                e.u8(F_RELAY_DELIVER).u64(*route).bytes(payload);
            }
            Frame::RelayResult { route, payload } => {
                e.u8(F_RELAY_RESULT).u64(*route).bytes(payload);
            }
            Frame::RouteCancel { route } => {
                e.u8(F_ROUTE_CANCEL).u64(*route);
            }
            Frame::ListReq => {
                e.u8(F_LIST_REQ);
            }
            Frame::ListRes { peers } => {
                e.u8(F_LIST_RES).u16(peers.len() as u16);
                for p in peers {
                    e.u8(p.role)
                        .str(&p.name)
                        .str(&p.version)
                        .str(&p.os)
                        .str(&p.arch)
                        .u64(p.connected_at_ms)
                        .u64(p.last_seen_ms);
                }
            }
        }
        e.finish()
    }

    pub fn decode(buf: &[u8]) -> PResult<Frame> {
        let mut d = Decoder::new(buf);
        let tag = d.u8()?;
        let f = match tag {
            F_HELLO => Frame::Hello {
                role: d.u8()?,
                name: d.str()?,
                version: d.str()?,
                os: d.str()?,
                arch: d.str()?,
                pid: d.u32()?,
            },
            F_HELLO_OK => Frame::HelloOk { version: d.str()? },
            F_ERROR => Frame::Error {
                code: d.u16()?,
                message: d.str()?,
            },
            F_PING => Frame::Ping { nonce: d.u64()? },
            F_PONG => Frame::Pong { nonce: d.u64()? },
            F_RELAY_REQ => Frame::RelayReq {
                target: d.str()?,
                payload: d.bytes()?,
            },
            F_RELAY_DELIVER => Frame::RelayDeliver {
                route: d.u64()?,
                payload: d.bytes()?,
            },
            F_RELAY_RESULT => Frame::RelayResult {
                route: d.u64()?,
                payload: d.bytes()?,
            },
            F_ROUTE_CANCEL => Frame::RouteCancel { route: d.u64()? },
            F_LIST_REQ => Frame::ListReq,
            F_LIST_RES => {
                let n = d.u16()? as usize;
                let mut peers = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    peers.push(PeerInfo {
                        role: d.u8()?,
                        name: d.str()?,
                        version: d.str()?,
                        os: d.str()?,
                        arch: d.str()?,
                        connected_at_ms: d.u64()?,
                        last_seen_ms: d.u64()?,
                    });
                }
                Frame::ListRes { peers }
            }
            other => return err(format!("unknown frame tag {}", other)),
        };
        d.finish()?;
        Ok(f)
    }
}

impl Payload {
    pub fn id(&self) -> u64 {
        match self {
            Payload::Exec { id, .. }
            | Payload::ExecOut { id, .. }
            | Payload::ExecEnd { id, .. }
            | Payload::Error { id, .. }
            | Payload::PushBegin { id, .. }
            | Payload::PushReady { id, .. }
            | Payload::Mkdir { id, .. }
            | Payload::FileBegin { id, .. }
            | Payload::Data { id, .. }
            | Payload::FileEnd { id, .. }
            | Payload::Symlink { id, .. }
            | Payload::PushEnd { id, .. }
            | Payload::TransferDone { id, .. }
            | Payload::PullBegin { id, .. }
            | Payload::Entry { id, .. } => *id,
        }
    }

    pub fn op(&self) -> u8 {
        match self {
            Payload::Exec { .. } => OP_EXEC,
            Payload::ExecOut { .. } => OP_EXEC_OUT,
            Payload::ExecEnd { .. } => OP_EXEC_END,
            Payload::Error { .. } => OP_ERROR,
            Payload::PushBegin { .. } => OP_PUSH_BEGIN,
            Payload::PushReady { .. } => OP_PUSH_READY,
            Payload::Mkdir { .. } => OP_MKDIR,
            Payload::FileBegin { .. } => OP_FILE_BEGIN,
            Payload::Data { .. } => OP_DATA,
            Payload::FileEnd { .. } => OP_FILE_END,
            Payload::Symlink { .. } => OP_SYMLINK,
            Payload::PushEnd { .. } => OP_PUSH_END,
            Payload::TransferDone { .. } => OP_TRANSFER_DONE,
            Payload::PullBegin { .. } => OP_PULL_BEGIN,
            Payload::Entry { .. } => OP_ENTRY,
        }
    }

    /// Returns the request id from an encoded payload without fully decoding it.
    ///
    /// This is deliberately tiny because the server uses it for synthetic
    /// errors and final-route cleanup.
    pub fn peek_id(buf: &[u8]) -> Option<u64> {
        if buf.len() < 9 {
            return None;
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&buf[1..9]);
        Some(u64::from_be_bytes(b))
    }

    /// True when this is the last payload the agent will send for a route.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            Payload::ExecEnd { .. } | Payload::Error { .. } | Payload::TransferDone { .. }
        )
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        match self {
            Payload::Exec {
                id,
                timeout_secs,
                cwd,
                raw,
                command,
            } => {
                e.u8(OP_EXEC)
                    .u64(*id)
                    .u64(*timeout_secs)
                    .str(cwd)
                    .bool(*raw)
                    .str(command);
            }
            Payload::ExecOut { id, stream, data } => {
                e.u8(OP_EXEC_OUT).u64(*id).u8(*stream).bytes(data);
            }
            Payload::ExecEnd {
                id,
                rc,
                timed_out,
                reason,
            } => {
                e.u8(OP_EXEC_END)
                    .u64(*id)
                    .i32(*rc)
                    .bool(*timed_out)
                    .str(reason);
            }
            Payload::Error { id, code, message } => {
                e.u8(OP_ERROR).u64(*id).u16(*code).str(message);
            }
            Payload::PushBegin {
                id,
                remote_path,
                source_name,
                source_is_dir,
                source_files,
                source_bytes,
            } => {
                e.u8(OP_PUSH_BEGIN)
                    .u64(*id)
                    .str(remote_path)
                    .str(source_name)
                    .bool(*source_is_dir)
                    .u64(*source_files)
                    .u64(*source_bytes);
            }
            Payload::PushReady { id, remote_root } => {
                e.u8(OP_PUSH_READY).u64(*id).str(remote_root);
            }
            Payload::Mkdir { id, rel, mode } => {
                e.u8(OP_MKDIR).u64(*id).str(rel).u32(*mode);
            }
            Payload::FileBegin {
                id,
                rel,
                size,
                mode,
                sha256,
            } => {
                e.u8(OP_FILE_BEGIN)
                    .u64(*id)
                    .str(rel)
                    .u64(*size)
                    .u32(*mode)
                    .fixed32(sha256);
            }
            Payload::Data { id, data } => {
                e.u8(OP_DATA).u64(*id).bytes(data);
            }
            Payload::FileEnd { id } => {
                e.u8(OP_FILE_END).u64(*id);
            }
            Payload::Symlink {
                id,
                rel,
                target,
                mode,
            } => {
                e.u8(OP_SYMLINK).u64(*id).str(rel).str(target).u32(*mode);
            }
            Payload::PushEnd { id } => {
                e.u8(OP_PUSH_END).u64(*id);
            }
            Payload::TransferDone {
                id,
                ok,
                error,
                remote_path,
                bytes,
                sha256,
            } => {
                e.u8(OP_TRANSFER_DONE)
                    .u64(*id)
                    .bool(*ok)
                    .str(error)
                    .str(remote_path)
                    .u64(*bytes)
                    .fixed32(sha256);
            }
            Payload::PullBegin { id, remote_path } => {
                e.u8(OP_PULL_BEGIN).u64(*id).str(remote_path);
            }
            Payload::Entry {
                id,
                rel,
                kind,
                size,
                mode,
                link_target,
                sha256,
            } => {
                e.u8(OP_ENTRY)
                    .u64(*id)
                    .str(rel)
                    .u8(*kind)
                    .u64(*size)
                    .u32(*mode)
                    .str(link_target)
                    .fixed32(sha256);
            }
        }
        e.finish()
    }

    pub fn decode(buf: &[u8]) -> PResult<Payload> {
        let mut d = Decoder::new(buf);
        let tag = d.u8()?;
        let p = match tag {
            OP_EXEC => Payload::Exec {
                id: d.u64()?,
                timeout_secs: d.u64()?,
                cwd: d.str()?,
                raw: d.bool()?,
                command: d.str()?,
            },
            OP_EXEC_OUT => Payload::ExecOut {
                id: d.u64()?,
                stream: d.u8()?,
                data: d.bytes()?,
            },
            OP_EXEC_END => Payload::ExecEnd {
                id: d.u64()?,
                rc: d.i32()?,
                timed_out: d.bool()?,
                reason: d.str()?,
            },
            OP_ERROR => Payload::Error {
                id: d.u64()?,
                code: d.u16()?,
                message: d.str()?,
            },
            OP_PUSH_BEGIN => Payload::PushBegin {
                id: d.u64()?,
                remote_path: d.str()?,
                source_name: d.str()?,
                source_is_dir: d.bool()?,
                source_files: d.u64()?,
                source_bytes: d.u64()?,
            },
            OP_PUSH_READY => Payload::PushReady {
                id: d.u64()?,
                remote_root: d.str()?,
            },
            OP_MKDIR => Payload::Mkdir {
                id: d.u64()?,
                rel: d.str()?,
                mode: d.u32()?,
            },
            OP_FILE_BEGIN => Payload::FileBegin {
                id: d.u64()?,
                rel: d.str()?,
                size: d.u64()?,
                mode: d.u32()?,
                sha256: d.fixed32()?,
            },
            OP_DATA => Payload::Data {
                id: d.u64()?,
                data: d.bytes()?,
            },
            OP_FILE_END => Payload::FileEnd { id: d.u64()? },
            OP_SYMLINK => Payload::Symlink {
                id: d.u64()?,
                rel: d.str()?,
                target: d.str()?,
                mode: d.u32()?,
            },
            OP_PUSH_END => Payload::PushEnd { id: d.u64()? },
            OP_TRANSFER_DONE => Payload::TransferDone {
                id: d.u64()?,
                ok: d.bool()?,
                error: d.str()?,
                remote_path: d.str()?,
                bytes: d.u64()?,
                sha256: d.fixed32()?,
            },
            OP_PULL_BEGIN => Payload::PullBegin {
                id: d.u64()?,
                remote_path: d.str()?,
            },
            OP_ENTRY => Payload::Entry {
                id: d.u64()?,
                rel: d.str()?,
                kind: d.u8()?,
                size: d.u64()?,
                mode: d.u32()?,
                link_target: d.str()?,
                sha256: d.fixed32()?,
            },
            other => return err(format!("unknown payload op {}", other)),
        };
        d.finish()?;
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let f = Frame::RelayReq {
            target: "u0_a451@localhost".into(),
            payload: Payload::Exec {
                id: 7,
                timeout_secs: 30,
                cwd: "~/x".into(),
                raw: false,
                command: "echo hi".into(),
            }
            .encode(),
        };
        assert_eq!(f, Frame::decode(&f.encode()).unwrap());
    }

    #[test]
    fn payload_roundtrip() {
        let p = Payload::Entry {
            id: 9,
            rel: "d/f".into(),
            kind: ENTRY_FILE,
            size: 12,
            mode: 0o644,
            link_target: String::new(),
            sha256: [3u8; 32],
        };
        assert_eq!(p, Payload::decode(&p.encode()).unwrap());
    }

    #[test]
    fn peek_id_works() {
        let p = Payload::FileEnd {
            id: 0x1122334455667788,
        };
        assert_eq!(Payload::peek_id(&p.encode()), Some(0x1122334455667788));
    }
}
