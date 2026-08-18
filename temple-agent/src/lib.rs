//! temple-agent — the agent engine: loop, routing, permissions, memory,
//! tools, signal, cron, and the WebSocket server that fronts them.
//!
//! All modules moved here from `temple-server` so the per-user daemons can
//! host the full agent without the separate server process.

pub mod agent;
pub mod agent_server;
pub mod auth;
pub mod backend;
pub mod config;
pub mod cron;
pub mod direct_tools;
pub mod local_tools;
pub mod memory;
pub mod nextcloud;
pub mod openwebui;
pub mod permissions;
pub mod queue;
pub mod router;
pub mod server;
pub mod session_log;
pub mod signal;
