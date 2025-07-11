# WebSocket Proxy

A high-performance WebSocket-to-TCP proxy server with domain-based routing built with Rust and Tokio.

## Overview

This proxy accepts WebSocket connections from reverse proxies and forwards binary data to target TCP servers based on the Host header. It creates a bridge between WebSocket clients and multiple TCP backends, with routing determined by domain names.

## Configuration

Configure the proxy using a `config.toml` file:

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
- **listen.allowed_proxy_ips** (optional): List of IP addresses or CIDR ranges that are allowed to connect as reverse proxies. If not specified, all IPs are allowed.
- **listen.tls** (optional): TLS configuration for secure WebSocket connections (wss://)
  - **cert_file**: Path to the PEM-formatted certificate file
  - **key_file**: Path to the PEM-formatted private key file
- **targets**: Map of domain names to backend configurations
  - Each target has a `host` and `port` for the TCP backend

## Usage

1. Create a `config.toml` file with your domain mappings
2. Run the proxy:

   ```bash
   cargo run
   ```

The proxy will:

1. Listen for WebSocket connections on the configured address
2. Extract the Host header from incoming requests
3. Route connections to the appropriate backend based on domain
4. Forward binary data bidirectionally between WebSocket and TCP
5. Reject connections for unknown domains

## Security

### Reverse Proxy IP Filtering

For enhanced security, you can restrict which reverse proxy IP addresses are allowed to connect to the WebSocket proxy server. This is useful when you want to ensure only specific reverse proxies can establish connections.

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

If `allowed_proxy_ips` is not specified or commented out, all proxy IPs are allowed.

### TLS Configuration

To enable secure WebSocket connections (wss://), add a TLS configuration section:

```toml
[listen]
ip = "0.0.0.0"
port = 443
[listen.tls]
cert_file = "cert.pem"
key_file = "key.pem"
```

The certificate and private key files must be in PEM format. You can generate self-signed certificates for development using:

```bash
# Generate private key
openssl genrsa -out key.pem 2048

# Generate self-signed certificate
openssl req -new -x509 -key key.pem -out cert.pem -days 365
```

For production, use certificates from a trusted Certificate Authority (CA) like Let's Encrypt.

**Note**: When TLS is enabled, the proxy only accepts secure WebSocket connections (wss://). To support both ws:// and wss://, run two separate instances on different ports.

## Reverse Proxy Setup

Configure your reverse proxy (nginx, Apache, etc.) to forward WebSocket connections to this proxy with the appropriate Host header. The proxy will route based on the domain in the Host header.

Example nginx configuration:

```nginx
location /websocket {
    proxy_pass http://wsproxy:80;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
}
```
