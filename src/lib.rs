//! `apb` core library.
//!
//! A single static-friendly binary (`apb`) is both:
//! * the **agent** that runs on a minimal node (Termux / Android / CI runner / Linux), and
//! * the **server/controller** that runs on a host with a public IP.
//!
//! Identity and transport are a Noise `NNpsk0` session over TCP.  The only
//! runtime inputs are environment variables (or command-line flags):
//! server IP, port and a 32-byte pre-shared key.  Nothing is written to disk
//! except the files explicitly requested by `push`/`pull`.

pub mod agent;
pub mod ctrl;
pub mod proto;
pub mod server;
pub mod util;
pub mod wire;

/// Protocol version string carried in the hello frame.
pub const PROTO_VERSION: &str = "0.1.0";
