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

[targets]
"example.com" = { host = "127.0.0.1", port = 8080 }
"api.example.com" = { host = "127.0.0.1", port = 8081 }
"localhost" = { host = "127.0.0.1", port = 8080 }
```

### Configuration Options

- **listen.ip**: IP address to bind the WebSocket server
- **listen.port**: Port for the WebSocket server
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
}
```
