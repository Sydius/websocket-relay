# WebSocket Proxy

A high-performance WebSocket-to-TCP proxy server built with Rust, using Axum and Tokio.

## Overview

This proxy accepts WebSocket connections and forwards binary data to a target TCP server, creating a bridge between WebSocket clients and TCP backends. Text messages are dropped - only binary data is forwarded.

## Configuration

Configure the proxy using environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `LISTEN_IP` | `0.0.0.0` | IP address to bind the WebSocket server |
| `LISTEN_PORT` | `80` | Port for the WebSocket server |
| `TARGET_IP` | `127.0.0.1` | IP address of the target TCP server |
| `TARGET_PORT` | `8080` | Port of the target TCP server |
