//! WebSocket Relay Server
//!
//! A WebSocket relay that routes connections based on Host headers to different TCP backends.
//! Supports TLS termination, IP filtering, and client IP extraction from X-Forwarded-For headers.

pub mod config;
pub mod proxy;
pub mod security;
pub mod stream;
pub mod tls;

// Re-export commonly used types and functions
pub use config::{Config, ListenConfig, TargetConfig, TlsConfig, load_config};
pub use proxy::{BUFFER_SIZE, handle_connection, handle_socket};
pub use security::{is_proxy_ip_allowed, parse_original_client_ip};
pub use stream::StreamType;
pub use tls::load_tls_config;
