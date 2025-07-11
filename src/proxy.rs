use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    hash::BuildHasher,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Error as TungsteniteError, Message,
        error::ProtocolError,
        handshake::server::{Request, Response},
    },
};
use tracing::{debug, error, info, warn};

use crate::config::TargetConfig;
use crate::security::parse_original_client_ip;
use crate::stream::StreamType;

pub const BUFFER_SIZE: usize = 8192;

#[tracing::instrument(skip(stream, targets), fields(client_addr = %stream.peer_addr().unwrap_or_else(|_| "unknown".parse().unwrap())))]
pub async fn handle_connection<S: BuildHasher + Sync>(
    stream: StreamType,
    targets: &HashMap<String, TargetConfig, S>,
) -> Result<()> {
    let host_header = Arc::new(Mutex::new(None::<String>));
    let host_header_clone = host_header.clone();
    // Get client address before moving the stream
    let client_addr = stream
        .peer_addr()
        .unwrap_or_else(|_| "unknown".parse().unwrap());

    let client_ip = Arc::new(Mutex::new(None::<String>));
    let client_ip_clone = client_ip.clone();

    let callback = move |req: &Request, response: Response| {
        if let Some(host) = req.headers().get("host") {
            if let Ok(host_str) = host.to_str() {
                if let Ok(mut guard) = host_header_clone.lock() {
                    *guard = Some(host_str.to_string());
                }
            }
        }

        // Extract original client IP from X-Forwarded-For header
        if let Some(xff) = req.headers().get("x-forwarded-for") {
            if let Ok(xff_str) = xff.to_str() {
                if let Some(original_ip) = parse_original_client_ip(xff_str) {
                    if let Ok(mut guard) = client_ip_clone.lock() {
                        *guard = Some(original_ip);
                    }
                }
            }
        }

        Ok(response)
    };

    let ws_stream = accept_hdr_async(stream, callback)
        .await
        .context("Failed to perform WebSocket handshake")?;

    let host = host_header
        .lock()
        .unwrap()
        .as_ref()
        .ok_or_else(|| anyhow!("No Host header found in request"))?
        .clone();

    let original_client_ip = client_ip.lock().unwrap().clone();

    let target_config = targets
        .get(&host)
        .ok_or_else(|| anyhow!("No target configured for domain: {}", host))?;

    // Log with original client IP if available, otherwise use direct connection IP
    match original_client_ip {
        Some(ref ip) => {
            info!(
                host = %host,
                target_host = %target_config.host,
                target_port = target_config.port,
                client_ip = %ip,
                direct_addr = %client_addr,
                "Routing request"
            );
        }
        None => {
            info!(
                host = %host,
                target_host = %target_config.host,
                target_port = target_config.port,
                client_ip = %client_addr,
                "Routing request"
            );
        }
    }

    handle_socket(ws_stream, target_config, original_client_ip).await?;
    Ok(())
}

#[tracing::instrument(skip(websocket, target_config, client_ip))]
pub async fn handle_socket(
    websocket: WebSocketStream<StreamType>,
    target_config: &TargetConfig,
    client_ip: Option<String>,
) -> Result<()> {
    let target_addr = format!("{}:{}", target_config.host, target_config.port);

    if let Some(ref ip) = client_ip {
        debug!(target_addr = %target_addr, client_ip = %ip, "Attempting to connect to target server");
    } else {
        debug!(target_addr = %target_addr, "Attempting to connect to target server");
    }

    let tcp_stream = TcpStream::connect(&target_addr)
        .await
        .with_context(|| format!("Failed to connect to target {target_addr}"))?;

    if let Some(ref ip) = client_ip {
        info!(target_addr = %target_addr, client_ip = %ip, "Connected to target server");
    } else {
        info!(target_addr = %target_addr, "Connected to target server");
    }

    let (mut ws_sender, mut ws_receiver) = websocket.split();
    let (mut tcp_reader, mut tcp_writer) = tcp_stream.into_split();

    let ws_to_tcp = async {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    debug!(bytes = data.len(), "Forwarding data from WebSocket to TCP");
                    if let Err(e) = tcp_writer.write_all(&data).await {
                        error!(error = %e, bytes = data.len(), "Failed to write to TCP");
                        return Err(e).context("Failed to write WebSocket data to TCP connection");
                    }
                }
                Ok(Message::Text(_)) => {
                    warn!("Dropping text message (binary only)");
                }
                Ok(Message::Close(_)) => {
                    info!("WebSocket connection closed");
                    break;
                }
                Err(e) => {
                    match e {
                        TungsteniteError::ConnectionClosed
                        | TungsteniteError::Protocol(ProtocolError::ResetWithoutClosingHandshake) =>
                        {
                            debug!("Client disconnected: {e}");
                        }
                        _ => {
                            error!("WebSocket error: {e}");
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
        Ok(())
    };

    let tcp_to_ws = async {
        let mut buffer = [0u8; BUFFER_SIZE];

        loop {
            match tcp_reader.read(&mut buffer).await {
                Ok(0) => {
                    info!("TCP connection closed");
                    break;
                }
                Ok(n) => {
                    let data = &buffer[..n];
                    debug!(bytes = n, "Forwarding data from TCP to WebSocket");
                    if let Err(e) = ws_sender.send(Message::Binary(data.to_vec().into())).await {
                        error!(error = %e, bytes = data.len(), "Failed to send WebSocket message");
                        return Err(e).context("Failed to send TCP data via WebSocket");
                    }
                }
                Err(e) => {
                    error!("Failed to read from TCP: {e}");
                    break;
                }
            }
        }
        Ok(())
    };

    tokio::select! {
        result = ws_to_tcp => result?,
        result = tcp_to_ws => result?,
    }

    if let Some(ref ip) = client_ip {
        info!(client_ip = %ip, "Proxy connection closed");
    } else {
        info!("Proxy connection closed");
    }
    Ok(())
}
