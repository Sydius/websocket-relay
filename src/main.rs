use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::fs;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    WebSocketStream, accept_async,
    tungstenite::{Error as TungsteniteError, Message, error::ProtocolError},
};
use tracing::{debug, error, info, warn};

#[derive(Deserialize)]
struct Config {
    listen: ListenConfig,
    target: TargetConfig,
}

#[derive(Deserialize)]
struct ListenConfig {
    ip: String,
    port: u16,
}

#[derive(Clone, Deserialize)]
struct TargetConfig {
    host: String,
    port: u16,
}

fn load_config() -> Result<Config> {
    let content = fs::read_to_string("config.toml").context("Failed to read config.toml file")?;
    toml::from_str(&content).context("Failed to parse config.toml as valid TOML")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let config = load_config()?;
    info!(
        config_file = "config.toml",
        listen_ip = %config.listen.ip,
        listen_port = config.listen.port,
        target_host = %config.target.host,
        target_port = config.target.port,
        "Configuration loaded"
    );

    let addr = format!("{}:{}", config.listen.ip, config.listen.port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind to address {addr}"))?;

    info!(
        listen_addr = %addr,
        "WebSocket proxy listening"
    );

    while let Ok((stream, addr)) = listener.accept().await {
        let target_config = config.target.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, &target_config).await {
                error!(client_addr = %addr, error = %e, "Connection failed");
            }
        });
    }

    Ok(())
}

#[tracing::instrument(skip(stream, target_config), fields(client_addr = %stream.peer_addr().unwrap_or_else(|_| "unknown".parse().unwrap())))]
async fn handle_connection(stream: TcpStream, target_config: &TargetConfig) -> Result<()> {
    let ws_stream = accept_async(stream)
        .await
        .context("Failed to perform WebSocket handshake")?;
    handle_socket(ws_stream, target_config).await?;
    Ok(())
}

#[tracing::instrument(skip(websocket, target_config))]
async fn handle_socket(
    websocket: WebSocketStream<TcpStream>,
    target_config: &TargetConfig,
) -> Result<()> {
    let target_addr = format!("{}:{}", target_config.host, target_config.port);

    debug!(target_addr = %target_addr, "Attempting to connect to target server");
    let tcp_stream = TcpStream::connect(&target_addr)
        .await
        .with_context(|| format!("Failed to connect to target {target_addr}"))?;

    info!(target_addr = %target_addr, "Connected to target server");

    let (mut ws_sender, mut ws_receiver) = websocket.split();
    let (mut tcp_reader, mut tcp_writer) = tcp_stream.into_split();

    let ws_to_tcp = async {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    debug!(bytes = data.len(), "Forwarding data from WebSocket to TCP");
                    if let Err(e) =
                        tokio::io::AsyncWriteExt::write_all(&mut tcp_writer, &data).await
                    {
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
        use tokio::io::AsyncReadExt;
        let mut buffer = [0u8; 1024];

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

    info!("Proxy connection closed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::{sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
        time::{sleep, timeout},
    };
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    const TEST_TIMEOUT: Duration = Duration::from_secs(1);
    const SERVER_STARTUP_DELAY: Duration = Duration::from_millis(100);
    const DATA_PROCESSING_DELAY: Duration = Duration::from_millis(200);

    type WsSender = futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >;
    type WsReceiver = futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >;

    /// Connects to WebSocket server and returns split sender/receiver
    async fn connect_websocket(port: u16) -> Result<(WsSender, WsReceiver)> {
        let url = format!("ws://127.0.0.1:{port}/");
        let (ws_stream, _) = connect_async(&url)
            .await
            .context("Failed to connect to WebSocket server")?;
        Ok(ws_stream.split())
    }

    /// Sends binary data through WebSocket
    async fn send_binary_message(sender: &mut WsSender, data: &[u8]) -> Result<()> {
        sender
            .send(Message::Binary(data.to_vec().into()))
            .await
            .context("Failed to send WebSocket binary message")?;
        Ok(())
    }

    /// Receives binary message from WebSocket with timeout
    async fn receive_binary_message(receiver: &mut WsReceiver) -> Result<Vec<u8>> {
        let response = timeout(TEST_TIMEOUT, receiver.next())
            .await
            .context("Timeout waiting for message")?
            .context("No message received")?
            .context("WebSocket error")?;

        match response {
            Message::Binary(data) => Ok(data.to_vec()),
            other => anyhow::bail!("Expected binary message, got: {other:?}"),
        }
    }

    /// Finds an unused port by binding to port 0
    async fn find_free_port() -> Result<u16> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("Failed to bind to localhost to find free port")?;
        let port = listener
            .local_addr()
            .context("Failed to get bound listener local address")?
            .port();
        drop(listener);
        Ok(port)
    }

    /// Starts proxy server on free port, returns port number
    async fn start_proxy_server(target_port: u16) -> Result<u16> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("Failed to bind proxy server")?;
        let port = listener
            .local_addr()
            .context("Failed to get proxy server local address")?
            .port();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let target_config = TargetConfig {
                    host: "127.0.0.1".to_string(),
                    port: target_port,
                };
                tokio::spawn(async move {
                    let _ = handle_connection(stream, &target_config).await;
                });
            }
        });

        Ok(port)
    }

    /// Starts TCP echo server on free port, returns port number
    async fn start_echo_server() -> Result<u16> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("Failed to bind echo server")?;
        let port = listener
            .local_addr()
            .context("Failed to get echo server local address")?
            .port();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = [0; 4096];
                    loop {
                        match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) if stream.write_all(&buffer[..n]).await.is_err() => break,
                            Ok(_) => {}
                        }
                    }
                });
            }
        });

        Ok(port)
    }

    /// Sets up proxy server with echo server backend
    async fn setup_proxy_with_echo_server() -> Result<(u16, u16)> {
        let tcp_port = start_echo_server().await?;
        let ws_port = start_proxy_server(tcp_port).await?;
        sleep(SERVER_STARTUP_DELAY).await;
        Ok((ws_port, tcp_port))
    }

    mod proxy_functionality {
        use super::*;

        #[tokio::test]
        async fn forwards_binary_data() {
            let (ws_port, _) = setup_proxy_with_echo_server().await.unwrap();
            let (mut sender, mut receiver) = connect_websocket(ws_port).await.unwrap();

            let test_data = b"Hello WebSocket Proxy!";
            send_binary_message(&mut sender, test_data).await.unwrap();

            let received = receive_binary_message(&mut receiver).await.unwrap();
            assert_eq!(received, test_data);
        }

        #[tokio::test]
        async fn drops_text_messages() {
            let (ws_port, _) = setup_proxy_with_echo_server().await.unwrap();
            let (mut sender, mut receiver) = connect_websocket(ws_port).await.unwrap();

            // Send text message (should be dropped)
            sender
                .send(Message::Text("This should be dropped".to_string().into()))
                .await
                .unwrap();

            // Send binary message (should work)
            let binary_data = b"This should work";
            send_binary_message(&mut sender, binary_data).await.unwrap();

            let received = receive_binary_message(&mut receiver).await.unwrap();
            assert_eq!(received, binary_data);
        }

        #[tokio::test]
        async fn handles_multiple_messages() {
            let (ws_port, _) = setup_proxy_with_echo_server().await.unwrap();
            let (mut sender, mut receiver) = connect_websocket(ws_port).await.unwrap();

            let messages = [b"First message".as_slice(), b"Second message"];

            for &msg in &messages {
                send_binary_message(&mut sender, msg).await.unwrap();
                let received = receive_binary_message(&mut receiver).await.unwrap();
                assert_eq!(received, msg);
            }
        }

        #[tokio::test]
        async fn handles_large_messages() {
            let (ws_port, _) = setup_proxy_with_echo_server().await.unwrap();
            let (mut sender, mut receiver) = connect_websocket(ws_port).await.unwrap();

            let large_data = vec![0xAB; 2048];
            send_binary_message(&mut sender, &large_data).await.unwrap();

            // Large messages are split into chunks due to 1024-byte buffer
            let mut received_data = Vec::new();
            let expected_chunks = large_data.len().div_ceil(1024);

            for _ in 0..expected_chunks {
                let chunk = receive_binary_message(&mut receiver).await.unwrap();
                received_data.extend_from_slice(&chunk);
            }

            assert_eq!(received_data, large_data);
        }

        #[tokio::test]
        async fn handles_concurrent_connections() {
            let (ws_port, _) = setup_proxy_with_echo_server().await.unwrap();

            let tasks: Vec<_> = (0..3)
                .map(|i| {
                    tokio::spawn(async move {
                        let (mut sender, mut receiver) = connect_websocket(ws_port).await.unwrap();
                        let test_data = format!("Message from client {i}").into_bytes();

                        send_binary_message(&mut sender, &test_data).await.unwrap();
                        let received = receive_binary_message(&mut receiver).await.unwrap();
                        assert_eq!(received, test_data);
                    })
                })
                .collect();

            for task in tasks {
                task.await.unwrap();
            }
        }
    }

    mod tcp_verification {
        use super::*;

        /// Creates TCP server that captures all received data
        async fn create_capturing_tcp_server() -> (u16, Arc<Mutex<Vec<u8>>>) {
            let received_data = Arc::new(Mutex::new(Vec::new()));
            let received_data_clone = received_data.clone();

            let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let tcp_port = tcp_listener.local_addr().unwrap().port();

            tokio::spawn(async move {
                if let Ok((mut stream, _)) = tcp_listener.accept().await {
                    let mut buffer = [0u8; 1024];
                    while let Ok(n) = stream.read(&mut buffer).await {
                        if n == 0 {
                            break;
                        }
                        received_data_clone
                            .lock()
                            .await
                            .extend_from_slice(&buffer[..n]);
                    }
                }
            });

            (tcp_port, received_data)
        }

        /// Creates TCP server that sends data to first connection
        async fn create_sending_tcp_server(data: Vec<u8>) -> u16 {
            let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let tcp_port = tcp_listener.local_addr().unwrap().port();

            tokio::spawn(async move {
                if let Ok((mut stream, _)) = tcp_listener.accept().await {
                    sleep(SERVER_STARTUP_DELAY).await;
                    let _ = stream.write_all(&data).await;
                }
            });

            tcp_port
        }

        #[tokio::test]
        async fn websocket_to_tcp_forwarding() {
            let (tcp_port, received_data) = create_capturing_tcp_server().await;
            let ws_port = start_proxy_server(tcp_port).await.unwrap();
            sleep(SERVER_STARTUP_DELAY).await;

            let (mut sender, _) = connect_websocket(ws_port).await.unwrap();
            let test_data = b"Direct TCP test data";
            send_binary_message(&mut sender, test_data).await.unwrap();

            sleep(DATA_PROCESSING_DELAY).await;

            let received = {
                let guard = received_data.lock().await;
                guard.clone()
            };
            assert_eq!(received, test_data);
        }

        #[tokio::test]
        async fn tcp_to_websocket_forwarding() {
            let test_data = b"Data from TCP server".to_vec();
            let tcp_port = create_sending_tcp_server(test_data.clone()).await;
            let ws_port = start_proxy_server(tcp_port).await.unwrap();
            sleep(SERVER_STARTUP_DELAY).await;

            let (_, mut receiver) = connect_websocket(ws_port).await.unwrap();
            let received = receive_binary_message(&mut receiver).await.unwrap();
            assert_eq!(received, test_data);
        }

        #[tokio::test]
        async fn text_messages_not_forwarded_to_tcp() {
            let (tcp_port, received_data) = create_capturing_tcp_server().await;
            let ws_port = start_proxy_server(tcp_port).await.unwrap();
            sleep(SERVER_STARTUP_DELAY).await;

            let (mut sender, _) = connect_websocket(ws_port).await.unwrap();

            // Send text message (should not reach TCP)
            sender
                .send(Message::Text(
                    "This should not reach TCP".to_string().into(),
                ))
                .await
                .unwrap();

            // Send binary message (should reach TCP)
            let binary_data = b"This should reach TCP";
            send_binary_message(&mut sender, binary_data).await.unwrap();

            sleep(DATA_PROCESSING_DELAY).await;

            let received = {
                let guard = received_data.lock().await;
                guard.clone()
            };
            assert_eq!(received, binary_data);
        }
    }

    mod error_handling {
        use super::*;

        #[tokio::test]
        async fn handles_tcp_connection_failure() {
            let ws_port = find_free_port().await.unwrap();
            let nonexistent_tcp_port = find_free_port().await.unwrap();

            tokio::spawn(async move {
                let listener = TcpListener::bind(("127.0.0.1", ws_port)).await.unwrap();
                while let Ok((stream, _)) = listener.accept().await {
                    let target_config = TargetConfig {
                        host: "127.0.0.1".to_string(),
                        port: nonexistent_tcp_port,
                    };
                    tokio::spawn(async move {
                        let _ = handle_connection(stream, &target_config).await;
                    });
                }
            });

            sleep(SERVER_STARTUP_DELAY).await;

            let url = format!("ws://127.0.0.1:{ws_port}/");
            let result = timeout(TEST_TIMEOUT, connect_async(&url)).await;

            if let Ok(Ok((ws_stream, _))) = result {
                let (_, mut receiver) = ws_stream.split();
                let close_result = timeout(TEST_TIMEOUT, receiver.next()).await;
                assert!(close_result.is_ok());
            }
            // Connection may fail immediately or close after handshake - both are acceptable
        }
    }
}
