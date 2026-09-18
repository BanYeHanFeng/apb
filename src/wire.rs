//! TCP + Noise framing.  The transport has no dependency on the OS `ssh`
//! daemon: a client opens one outbound TCP connection and completes a
//! `Noise_NNpsk0` handshake with the same 32-byte key as the server.

use crate::proto::{Frame, MAGIC};
use snow::params::NoiseParams;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const MAX_WIRE: usize = 65_535;
pub const MAX_PLAIN: usize = MAX_WIRE - 16;
pub const WRITE_QUEUE: usize = 128;

const NOISE_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

fn ioerr(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn noise_params() -> io::Result<NoiseParams> {
    NOISE_PARAMS
        .parse::<NoiseParams>()
        .map_err(|e| ioerr(format!("bad noise params: {e}")))
}

fn write_u16<W: Write>(stream: &mut W, len: usize) -> io::Result<()> {
    stream.write_all(&(len as u16).to_be_bytes())
}

fn read_u16<R: Read>(stream: &mut R) -> io::Result<usize> {
    let mut b = [0u8; 2];
    stream.read_exact(&mut b)?;
    let n = u16::from_be_bytes(b) as usize;
    if n == 0 || n > MAX_WIRE {
        return Err(ioerr(format!("bad encrypted frame length {n}")));
    }
    Ok(n)
}

fn read_handshake_msg(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let n = read_u16(stream)?;
    let mut b = vec![0u8; n];
    stream.read_exact(&mut b)?;
    Ok(b)
}

/// Perform the Noise handshake on an already-connected TCP stream.
///
/// `initiator` is true on the agent/controller side and false on the server.
fn handshake(
    stream: &mut TcpStream,
    psk: &[u8; 32],
    initiator: bool,
) -> io::Result<snow::TransportState> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let params = noise_params()?;
    let mut msg = vec![0u8; MAX_WIRE];

    let transport = if initiator {
        let mut hs = snow::Builder::new(params)
            .psk(0, psk)
            .map_err(|e| ioerr(format!("noise psk: {e}")))?
            .build_initiator()
            .map_err(|e| ioerr(format!("noise initiator: {e}")))?;
        stream.write_all(MAGIC)?;
        let n = hs
            .write_message(&[], &mut msg)
            .map_err(|e| ioerr(format!("noise write: {e}")))?;
        write_u16(stream, n)?;
        stream.write_all(&msg[..n])?;
        stream.flush()?;

        let reply = read_handshake_msg(stream)?;
        let mut out = vec![0u8; MAX_WIRE];
        hs.read_message(&reply, &mut out)
            .map_err(|e| ioerr(format!("noise read: {e}")))?;
        hs.into_transport_mode()
            .map_err(|e| ioerr(format!("noise transport: {e}")))?
    } else {
        let mut magic = [0u8; 4];
        stream.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(ioerr("peer did not send APB0 magic"));
        }
        let mut hs = snow::Builder::new(params)
            .psk(0, psk)
            .map_err(|e| ioerr(format!("noise psk: {e}")))?
            .build_responder()
            .map_err(|e| ioerr(format!("noise responder: {e}")))?;
        let first = read_handshake_msg(stream)?;
        let mut out = vec![0u8; MAX_WIRE];
        hs.read_message(&first, &mut out)
            .map_err(|e| ioerr(format!("noise read: {e}")))?;
        let n = hs
            .write_message(&[], &mut msg)
            .map_err(|e| ioerr(format!("noise write: {e}")))?;
        write_u16(stream, n)?;
        stream.write_all(&msg[..n])?;
        stream.flush()?;
        hs.into_transport_mode()
            .map_err(|e| ioerr(format!("noise transport: {e}")))?
    };

    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    set_keepalive(stream);
    Ok(transport)
}

#[cfg(unix)]
fn set_keepalive(stream: &TcpStream) {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let one: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_KEEPALIVE,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of_val(&one) as libc::socklen_t,
        );
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let idle: libc::c_int = 30;
        let intvl: libc::c_int = 10;
        let cnt: libc::c_int = 6;
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPIDLE,
            &idle as *const _ as *const libc::c_void,
            std::mem::size_of_val(&idle) as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPINTVL,
            &intvl as *const _ as *const libc::c_void,
            std::mem::size_of_val(&intvl) as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_KEEPCNT,
            &cnt as *const _ as *const libc::c_void,
            std::mem::size_of_val(&cnt) as libc::socklen_t,
        );
    }
}

#[cfg(not(unix))]
fn set_keepalive(_stream: &TcpStream) {}

struct Inner {
    stream: TcpStream,
    state: Mutex<snow::TransportState>,
}

impl Inner {
    fn recv(&self) -> io::Result<Vec<u8>> {
        let mut r = &self.stream;
        let n = read_u16(&mut r)?;
        let mut ct = vec![0u8; n];
        (&self.stream).read_exact(&mut ct)?;
        let mut pt = vec![0u8; n];
        let plen = {
            let mut st = self
                .state
                .lock()
                .map_err(|_| ioerr("noise session poisoned"))?;
            st.read_message(&ct, &mut pt)
                .map_err(|e| ioerr(format!("noise decrypt: {e}")))?
        };
        pt.truncate(plen);
        Ok(pt)
    }

    fn send(&self, plain: &[u8]) -> io::Result<()> {
        if plain.len() > MAX_PLAIN {
            return Err(ioerr(format!("plaintext frame too big: {}", plain.len())));
        }
        let mut ct = vec![0u8; plain.len() + 16];
        let n = {
            let mut st = self
                .state
                .lock()
                .map_err(|_| ioerr("noise session poisoned"))?;
            st.write_message(plain, &mut ct)
                .map_err(|e| ioerr(format!("noise encrypt: {e}")))?
        };
        let mut w = &self.stream;
        write_u16(&mut w, n)?;
        w.write_all(&ct[..n])?;
        Ok(())
    }

    fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// A bidirectional encrypted frame connection.  `send` may be called from
/// any thread; one writer thread preserves frame order.
pub struct Conn {
    inner: Arc<Inner>,
    tx: SyncSender<Vec<u8>>,
}

impl Conn {
    /// Wrap an existing connected stream and run the handshake.
    pub fn from_stream(mut stream: TcpStream, psk: &[u8; 32], initiator: bool) -> io::Result<Conn> {
        let state = handshake(&mut stream, psk, initiator)?;
        let inner = Arc::new(Inner {
            stream,
            state: Mutex::new(state),
        });
        let (tx, rx): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::sync_channel(WRITE_QUEUE);
        let win = inner.clone();
        let _writer = thread::Builder::new()
            .name("apb-writer".into())
            .spawn(move || {
                while let Ok(msg) = rx.recv() {
                    if win.send(&msg).is_err() {
                        break;
                    }
                }
                win.shutdown();
            })?;
        Ok(Conn { inner, tx })
    }

    pub fn recv(&self) -> io::Result<Vec<u8>> {
        self.inner.recv()
    }

    pub fn send(&self, plain: Vec<u8>) -> io::Result<()> {
        self.tx
            .send(plain)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "connection writer closed"))
    }

    pub fn send_frame(&self, frame: &Frame) -> io::Result<()> {
        self.send(frame.encode())
    }

    pub fn sender_clone(&self) -> SyncSender<Vec<u8>> {
        self.tx.clone()
    }

    pub fn close(&self) {
        self.inner.shutdown();
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.inner.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Frame, Payload};
    use std::net::TcpListener;

    #[test]
    fn noise_roundtrip() {
        let key = [42u8; 32];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let t = std::thread::spawn(move || -> Conn {
            let s = TcpStream::connect(addr).unwrap();
            let c = Conn::from_stream(s, &key, true).unwrap();
            let f = Frame::RelayReq {
                target: "a".into(),
                payload: Payload::Exec {
                    id: 1,
                    timeout_secs: 2,
                    cwd: "".into(),
                    raw: true,
                    command: "echo hi".into(),
                }
                .encode(),
            };
            c.send_frame(&f).unwrap();
            c
        });
        let (s, _) = listener.accept().unwrap();
        let c = Conn::from_stream(s, &key, false).unwrap();
        let raw = c.recv().unwrap();
        let f = Frame::decode(&raw).unwrap();
        match f {
            Frame::RelayReq { target, payload } => {
                assert_eq!(target, "a");
                match Payload::decode(&payload).unwrap() {
                    Payload::Exec { command, .. } => assert_eq!(command, "echo hi"),
                    other => panic!("unexpected {other:?}"),
                }
            }
            other => panic!("unexpected {other:?}"),
        }
        drop(c);
        let client = t.join().unwrap();
        client.close();
    }
}
