# WebSocket Relay

[![Crates.io](https://img.shields.io/crates/v/websocket-relay.svg)](https://crates.io/crates/websocket-relay)
[![CI](../../actions/workflows/ci.yml/badge.svg)](../../actions/workflows/ci.yml)
[![Security Audit](../../actions/workflows/security.yml/badge.svg)](../../actions/workflows/security.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A high-performance WebSocket-to-TCP relay server with domain-based routing
built with Rust and Tokio.

## Overview

This relay accepts WebSocket connections and forwards these to TCP servers,
effectively allowing browsers or other WebSocket-based clients to communicate
with servers that only speak TCP.

Multiple target TCP servers are supported and can be routed to based on the
contents of the `Host` header. For example, requests to `foo.example.com` could
be routed to one TCP server, while `bar.example.com` go to another.

Both 'plain' WebSockets (`ws://`) and TLS-encrypted WebSockets (`wss://`) are
supported, depending on the configuration. It can also be put behind a reverse
proxy such as Nginx and restricted to accept incoming connections only from the
IP address(es) of that reverse proxy.

## Configuration

Configure the relay using a `config.toml` file:

```toml
[listen]
ip = "0.0.0.0"
port = 80
# Optional: Restrict which reverse proxy IPs can connect
# allowed_proxy_ips = ["192.168.1.0/24", "10.0.0.1", "::1"]

[targets]
"example.com" = { host = "127.0.0.1", port = 8080 }
"api.example.com" = { host = "127.0.0.1", port = 8081 }
"localhost" = { host = "127.0.0.1", port = 8080 }
```

### Configuration Options

- **listen.ip**: IP address to bind the WebSocket server
- **listen.port**: Port for the WebSocket server
- **listen.allowed_proxy_ips** (optional): List of IP addresses or CIDR ranges
  that are allowed to connect as reverse proxies. If not specified, all IPs are
  allowed.
- **listen.tls** (optional): TLS configuration for secure WebSocket connections
  (wss://)
  - **cert_file**: Path to the PEM-formatted certificate file
  - **key_file**: Path to the PEM-formatted private key file
- **targets**: Map of domain names to backend configurations
  - Each target has a `host` and `port` for the TCP backend

## Installation

You can install the relay on a system that has Cargo with:

```bash
cargo install websocket-relay
```

## Usage

1. Create a `config.toml` file with your domain mappings (you can use the
   example in this repository as a starting point).

2. Run the proxy: `websocket-relay` (it will look for the `config.toml` in the
   working directory).

The relay will:

1. Listen for WebSocket connections on the configured address
2. Extract the `Host` header from incoming requests
3. Route connections to the appropriate backend based on domain
4. Forward binary data bidirectionally between WebSocket and TCP
5. Reject connections for unknown domains

If present, it will use the `X-Forwarded-For` header to extract the originating
IP address for the purpose of logging.

## Security

### Reverse Proxy IP Filtering

For enhanced security, you can restrict which reverse proxy IP addresses are
allowed to connect to the WebSocket relay server. This is useful when you want
to ensure only specific reverse proxies can establish connections.

Configure the `allowed_proxy_ips` option in your `config.toml`:

```toml
[listen]
ip = "0.0.0.0"
port = 80
allowed_proxy_ips = [
    "192.168.1.10",      # Specific nginx server
    "10.0.0.0/8",        # Internal network range
    "::1"                # IPv6 localhost
]
```

- Individual IP addresses: `"192.168.1.10"`
- CIDR ranges: `"192.168.1.0/24"`, `"10.0.0.0/8"`
- IPv6 addresses: `"::1"`, `"2001:db8::/32"`

If `allowed_proxy_ips` is not specified or commented out, all proxy IPs are
allowed.

### TLS Configuration

To enable secure WebSocket connections (`wss://`), add a TLS configuration
section:

```toml
[listen]
ip = "0.0.0.0"
port = 443
[listen.tls]
cert_file = "cert.pem"
key_file = "key.pem"
```

The certificate and private key files must be in PEM format. You can generate
self-signed certificates for development using:

```bash
# Generate private key
openssl genrsa -out key.pem 2048

# Generate self-signed certificate
openssl req -new -x509 -key key.pem -out cert.pem -days 365
```

For production, use certificates from a trusted Certificate Authority (CA) like
Let's Encrypt.

**Note**: When TLS is enabled, the relay only accepts secure WebSocket
connections (`wss://`). To support both `ws://` and `wss://`, run two separate
instances on different ports.

## Reverse Proxy Setup

Configure your reverse proxy (Nginx, Apache, etc.) to forward WebSocket
connections to this relay with the appropriate `Host` header. The relay will
route based on the domain in the Host header.

Example Nginx configuration:

```nginx
location /websocket {
    proxy_pass http://websocket-relay:80;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
}
```

## Contributing

We welcome contributions! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for
guidelines on how to contribute to this project.
