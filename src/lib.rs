//! `apb` core library.
//!
//! A single static-friendly binary (`apb`) is both:
//! * the **agent** that runs on a minimal node (Termux / Android / CI runner / Linux), and
//! * the **server/controller** that runs on a host with a public IP.
//!
//! Identity and transport are a Noise `NNpsk0` session over TCP.  Runtime inputs
//! come from command-line flags, `APB_*` environment variables, or an optional
//! `KEY=VALUE` config file (`--config` / `APB_CONFIG` / `~/.config/apb/agent.conf`).
//! The agent itself never writes a config file; only files explicitly requested
//! by `push`/`pull` are written to disk.

pub mod agent;
pub mod ctrl;
pub mod delta;
pub mod proto;
pub mod server;
pub mod util;
pub mod wire;

/// Protocol version string carried in the hello frame.
pub const PROTO_VERSION: &str = "0.1.0";
