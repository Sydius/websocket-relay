use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

mod config;
use config::{TargetConfig, load_config};
mod security;
use security::{is_proxy_ip_allowed, parse_original_client_ip};
mod tls;
use tls::load_tls_config;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    spawn,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async,
    tungstenite::{
        Error as TungsteniteError, Message,
        error::ProtocolError,
        handshake::server::{Request, Response},
    },
};
use tracing::{debug, error, info, warn};

const BUFFER_SIZE: usize = 8192;

enum StreamType {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

impl AsyncRead for StreamType {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for StreamType {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        match &mut *self {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match &mut *self {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => std::pin::Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        match &mut *self {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
        }
    }
}

impl StreamType {
    fn peer_addr(&self) -> Result<SocketAddr, std::io::Error> {
        match self {
            Self::Plain(stream) => stream.peer_addr(),
            Self::Tls(stream) => stream.get_ref().0.peer_addr(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let config = load_config()?;

    let tls_acceptor = if let Some(ref tls_config) = config.listen.tls {
        let server_config = load_tls_config(tls_config)?;
        Some(TlsAcceptor::from(Arc::new(server_config)))
    } else {
        None
    };

    info!(
        config_file = "config.toml",
        listen_ip = %config.listen.ip,
        listen_port = config.listen.port,
        targets_count = config.targets.len(),
        tls_enabled = tls_acceptor.is_some(),
        "Configuration loaded"
    );

    let addr = format!("{}:{}", config.listen.ip, config.listen.port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind to address {addr}"))?;

    info!(
        listen_addr = %addr,
        tls_enabled = tls_acceptor.is_some(),
        "WebSocket proxy listening"
    );

    while let Ok((stream, addr)) = listener.accept().await {
        let targets = config.targets.clone();
        let allowed_proxy_ips = config.listen.allowed_proxy_ips.clone();
        let acceptor = tls_acceptor.clone();

        spawn(async move {
            // Check if the proxy IP is allowed
            match is_proxy_ip_allowed(addr.ip(), allowed_proxy_ips.as_ref()) {
                Ok(true) => {
                    debug!(proxy_addr = %addr, "Proxy IP allowed, accepting connection");

                    let stream_type = if let Some(acceptor) = acceptor {
                        // TLS enabled - perform TLS handshake
                        match acceptor.accept(stream).await {
                            Ok(tls_stream) => StreamType::Tls(Box::new(tls_stream)),
                            Err(e) => {
                                error!(client_addr = %addr, error = %e, "TLS handshake failed");
                                return;
                            }
                        }
                    } else {
                        // Plain TCP
                        StreamType::Plain(stream)
                    };

                    if let Err(e) = handle_connection(stream_type, &targets).await {
                        error!(client_addr = %addr, error = %e, "Connection failed");
                    }
                }
                Ok(false) => {
                    warn!(proxy_addr = %addr, "Proxy IP not in allowlist, rejecting connection");
                    // Connection is automatically dropped when stream goes out of scope
                }
                Err(e) => {
                    error!(proxy_addr = %addr, error = %e, "Error validating proxy IP, rejecting connection");
                    // Connection is automatically dropped when stream goes out of scope
                }
            }
        });
    }

    Ok(())
}

#[tracing::instrument(skip(stream, targets), fields(client_addr = %stream.peer_addr().unwrap_or_else(|_| "unknown".parse().unwrap())))]
async fn handle_connection(
    stream: StreamType,
    targets: &HashMap<String, TargetConfig>,
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
async fn handle_socket(
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use futures_util::{SinkExt, StreamExt};
    use std::{sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
        time::{sleep, timeout},
    };
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{Message, client::IntoClientRequest},
    };

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
            other => bail!("Expected binary message, got: {other:?}"),
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

        spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut targets = HashMap::new();
                targets.insert(
                    "127.0.0.1:8080".to_string(),
                    TargetConfig {
                        host: "127.0.0.1".to_string(),
                        port: target_port,
                    },
                );
                targets.insert(
                    format!("127.0.0.1:{port}"),
                    TargetConfig {
                        host: "127.0.0.1".to_string(),
                        port: target_port,
                    },
                );
                targets.insert(
                    "localhost".to_string(),
                    TargetConfig {
                        host: "127.0.0.1".to_string(),
                        port: target_port,
                    },
                );
                spawn(async move {
                    let _ = handle_connection(StreamType::Plain(stream), &targets).await;
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

        spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                spawn(async move {
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

    /// Helper to start proxy with specific domain mappings and IP filtering
    async fn start_proxy_with_ip_filtering(
        domains: HashMap<String, u16>,
        allowed_proxy_ips: Option<Vec<String>>,
    ) -> Result<u16> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let ws_port = listener.local_addr()?.port();

        spawn(async move {
            while let Ok((stream, addr)) = listener.accept().await {
                let mut targets = HashMap::new();
                for (domain, tcp_port) in &domains {
                    targets.insert(
                        domain.clone(),
                        TargetConfig {
                            host: "127.0.0.1".to_string(),
                            port: *tcp_port,
                        },
                    );
                }
                let allowed_ips = allowed_proxy_ips.clone();
                spawn(async move {
                    if matches!(
                        is_proxy_ip_allowed(addr.ip(), allowed_ips.as_ref()),
                        Ok(true)
                    ) {
                        let _ = handle_connection(StreamType::Plain(stream), &targets).await;
                    } else {
                        // Connection rejected, stream drops automatically
                    }
                });
            }
        });

        sleep(SERVER_STARTUP_DELAY).await;
        Ok(ws_port)
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

            // Large messages are split into chunks due to buffer size
            let mut received_data = Vec::new();
            let expected_chunks = large_data.len().div_ceil(BUFFER_SIZE);

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
                    spawn(async move {
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

            spawn(async move {
                if let Ok((mut stream, _)) = tcp_listener.accept().await {
                    let mut buffer = [0u8; BUFFER_SIZE];
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

            spawn(async move {
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

    mod domain_routing {
        use super::*;
        use std::collections::HashMap;
        use tokio_tungstenite::tungstenite::http::HeaderValue;

        /// Helper to create WebSocket connection with custom Host header
        async fn connect_websocket_with_host(
            ws_port: u16,
            host: &str,
        ) -> Result<(WsSender, WsReceiver)> {
            let url = format!("ws://127.0.0.1:{ws_port}/");
            let mut request = IntoClientRequest::into_client_request(url)?;
            request
                .headers_mut()
                .insert("host", HeaderValue::from_str(host)?);

            let (ws_stream, _) = connect_async(request)
                .await
                .context("Failed to connect to WebSocket server")?;
            Ok(ws_stream.split())
        }

        /// Helper to start proxy with specific domain mappings
        async fn start_proxy_with_domains(domains: HashMap<String, u16>) -> Result<u16> {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let ws_port = listener.local_addr()?.port();

            spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let mut targets = HashMap::new();
                    for (domain, tcp_port) in &domains {
                        targets.insert(
                            domain.clone(),
                            TargetConfig {
                                host: "127.0.0.1".to_string(),
                                port: *tcp_port,
                            },
                        );
                    }
                    spawn(async move {
                        let _ = handle_connection(StreamType::Plain(stream), &targets).await;
                    });
                }
            });

            sleep(SERVER_STARTUP_DELAY).await;
            Ok(ws_port)
        }

        #[tokio::test]
        async fn routes_different_domains_to_different_targets() {
            let tcp_port1 = start_echo_server().await.unwrap();
            let tcp_port2 = start_echo_server().await.unwrap();

            let mut domains = HashMap::new();
            domains.insert("domain1.test".to_string(), tcp_port1);
            domains.insert("domain2.test".to_string(), tcp_port2);

            let ws_port = start_proxy_with_domains(domains).await.unwrap();

            // Test domain1.test routes to tcp_port1
            let (mut sender1, mut receiver1) = connect_websocket_with_host(ws_port, "domain1.test")
                .await
                .unwrap();
            let test_data1 = b"Message for domain1";
            send_binary_message(&mut sender1, test_data1).await.unwrap();
            let received1 = receive_binary_message(&mut receiver1).await.unwrap();
            assert_eq!(received1, test_data1);

            // Test domain2.test routes to tcp_port2
            let (mut sender2, mut receiver2) = connect_websocket_with_host(ws_port, "domain2.test")
                .await
                .unwrap();
            let test_data2 = b"Message for domain2";
            send_binary_message(&mut sender2, test_data2).await.unwrap();
            let received2 = receive_binary_message(&mut receiver2).await.unwrap();
            assert_eq!(received2, test_data2);
        }

        #[tokio::test]
        async fn server_continues_after_unknown_domain() {
            // Test that server continues to handle valid requests after encountering unknown domain
            let tcp_port = start_echo_server().await.unwrap();

            let mut domains = HashMap::new();
            domains.insert("known-domain.test".to_string(), tcp_port);

            let ws_port = start_proxy_with_domains(domains).await.unwrap();

            // Try unknown domain (we don't care about the specific failure mode)
            let _ = connect_websocket_with_host(ws_port, "unknown-domain.test").await;

            // The real test: verify server still processes valid domains correctly
            let (mut sender, mut receiver) =
                connect_websocket_with_host(ws_port, "known-domain.test")
                    .await
                    .unwrap();
            let test_data = b"Server still works";
            send_binary_message(&mut sender, test_data).await.unwrap();
            let received = receive_binary_message(&mut receiver).await.unwrap();
            assert_eq!(received, test_data);
        }

        #[tokio::test]
        async fn handles_host_header_with_port() {
            let tcp_port = start_echo_server().await.unwrap();

            let mut domains = HashMap::new();
            domains.insert("example.com:8080".to_string(), tcp_port);

            let ws_port = start_proxy_with_domains(domains).await.unwrap();

            // Test with port in Host header
            let (mut sender, mut receiver) =
                connect_websocket_with_host(ws_port, "example.com:8080")
                    .await
                    .unwrap();
            let test_data = b"Message with port";
            send_binary_message(&mut sender, test_data).await.unwrap();
            let received = receive_binary_message(&mut receiver).await.unwrap();
            assert_eq!(received, test_data);
        }

        #[tokio::test]
        async fn routes_based_on_configured_domains() {
            let tcp_port1 = start_echo_server().await.unwrap();
            let tcp_port2 = start_echo_server().await.unwrap();

            let mut domains = HashMap::new();
            domains.insert("service-a.test".to_string(), tcp_port1);
            domains.insert("service-b.test".to_string(), tcp_port2);

            let ws_port = start_proxy_with_domains(domains).await.unwrap();

            // Test service-a.test routes correctly
            let (mut sender1, mut receiver1) =
                connect_websocket_with_host(ws_port, "service-a.test")
                    .await
                    .unwrap();
            let test_data1 = b"Message for service A";
            send_binary_message(&mut sender1, test_data1).await.unwrap();
            let received1 = receive_binary_message(&mut receiver1).await.unwrap();
            assert_eq!(received1, test_data1);

            // Test service-b.test routes correctly
            let (mut sender2, mut receiver2) =
                connect_websocket_with_host(ws_port, "service-b.test")
                    .await
                    .unwrap();
            let test_data2 = b"Message for service B";
            send_binary_message(&mut sender2, test_data2).await.unwrap();
            let received2 = receive_binary_message(&mut receiver2).await.unwrap();
            assert_eq!(received2, test_data2);
        }

        #[tokio::test]
        async fn handles_missing_host_header() {
            let tcp_port = start_echo_server().await.unwrap();

            let mut domains = HashMap::new();
            domains.insert("example.com".to_string(), tcp_port);

            let ws_port = start_proxy_with_domains(domains).await.unwrap();

            // Connect without Host header - test that server handles this gracefully
            let url = format!("ws://127.0.0.1:{ws_port}/");
            let mut request = IntoClientRequest::into_client_request(url).unwrap();
            // Remove Host header if it exists
            request.headers_mut().remove("host");

            let _result = timeout(TEST_TIMEOUT, connect_async(request)).await;

            // Connection may fail at various points - what matters is no server crash
            // Test that server continues working by making a valid connection
            let (mut sender, mut receiver) = connect_websocket_with_host(ws_port, "example.com")
                .await
                .unwrap();
            let test_data = b"Server still works";
            send_binary_message(&mut sender, test_data).await.unwrap();
            let received = receive_binary_message(&mut receiver).await.unwrap();
            assert_eq!(received, test_data);
        }
    }

    mod error_handling {
        use super::*;

        #[tokio::test]
        async fn handles_tcp_connection_failure() {
            let ws_port = find_free_port().await.unwrap();
            let nonexistent_tcp_port = find_free_port().await.unwrap();

            spawn(async move {
                let listener = TcpListener::bind(("127.0.0.1", ws_port)).await.unwrap();
                while let Ok((stream, _)) = listener.accept().await {
                    let mut targets = HashMap::new();
                    targets.insert(
                        "localhost".to_string(),
                        TargetConfig {
                            host: "127.0.0.1".to_string(),
                            port: nonexistent_tcp_port,
                        },
                    );
                    spawn(async move {
                        let _ = handle_connection(StreamType::Plain(stream), &targets).await;
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

    mod proxy_ip_filtering {
        use super::*;

        #[test]
        fn allows_all_when_no_restrictions() {
            let proxy_ip = "192.168.1.100".parse().unwrap();
            let allowed_ips = None;
            assert!(is_proxy_ip_allowed(proxy_ip, allowed_ips).unwrap());
        }

        #[test]
        fn allows_exact_ip_match() {
            let proxy_ip = "192.168.1.100".parse().unwrap();
            let allowed_ips = Some(vec!["192.168.1.100".to_string()]);
            assert!(is_proxy_ip_allowed(proxy_ip, allowed_ips.as_ref()).unwrap());
        }

        #[test]
        fn rejects_non_matching_ip() {
            let proxy_ip = "192.168.1.100".parse().unwrap();
            let allowed_ips = Some(vec!["192.168.1.101".to_string()]);
            assert!(!is_proxy_ip_allowed(proxy_ip, allowed_ips.as_ref()).unwrap());
        }

        #[test]
        fn allows_ip_in_cidr_range() {
            let proxy_ip = "192.168.1.100".parse().unwrap();
            let allowed_ips = Some(vec!["192.168.1.0/24".to_string()]);
            assert!(is_proxy_ip_allowed(proxy_ip, allowed_ips.as_ref()).unwrap());
        }

        #[test]
        fn rejects_ip_outside_cidr_range() {
            let proxy_ip = "192.168.2.100".parse().unwrap();
            let allowed_ips = Some(vec!["192.168.1.0/24".to_string()]);
            assert!(!is_proxy_ip_allowed(proxy_ip, allowed_ips.as_ref()).unwrap());
        }

        #[test]
        fn allows_ipv6_exact_match() {
            let proxy_ip = "::1".parse().unwrap();
            let allowed_ips = Some(vec!["::1".to_string()]);
            assert!(is_proxy_ip_allowed(proxy_ip, allowed_ips.as_ref()).unwrap());
        }

        #[test]
        fn allows_ipv6_in_cidr_range() {
            let proxy_ip = "2001:db8::1".parse().unwrap();
            let allowed_ips = Some(vec!["2001:db8::/32".to_string()]);
            assert!(is_proxy_ip_allowed(proxy_ip, allowed_ips.as_ref()).unwrap());
        }

        #[test]
        fn allows_mixed_ipv4_and_ipv6() {
            let proxy_ip_v4 = "192.168.1.100".parse().unwrap();
            let proxy_ip_v6 = "::1".parse().unwrap();
            let allowed_ips = Some(vec!["192.168.1.0/24".to_string(), "::1".to_string()]);
            assert!(is_proxy_ip_allowed(proxy_ip_v4, allowed_ips.as_ref()).unwrap());
            assert!(is_proxy_ip_allowed(proxy_ip_v6, allowed_ips.as_ref()).unwrap());
        }

        #[test]
        fn returns_error_for_invalid_ip_format() {
            let proxy_ip = "192.168.1.100".parse().unwrap();
            let allowed_ips = Some(vec!["invalid-ip".to_string()]);
            assert!(is_proxy_ip_allowed(proxy_ip, allowed_ips.as_ref()).is_err());
        }

        #[test]
        fn returns_error_for_invalid_cidr_format() {
            let proxy_ip = "192.168.1.100".parse().unwrap();
            let allowed_ips = Some(vec!["192.168.1.0/99".to_string()]);
            assert!(is_proxy_ip_allowed(proxy_ip, allowed_ips.as_ref()).is_err());
        }
    }

    mod client_ip_extraction {
        use super::*;

        #[test]
        fn parses_single_ip_from_xff_header() {
            let xff = "192.168.1.100";
            let result = parse_original_client_ip(xff);
            assert_eq!(result, Some("192.168.1.100".to_string()));
        }

        #[test]
        fn parses_first_ip_from_xff_chain() {
            let xff = "203.0.113.195, 198.51.100.178, 192.168.1.1";
            let result = parse_original_client_ip(xff);
            assert_eq!(result, Some("203.0.113.195".to_string()));
        }

        #[test]
        fn handles_ipv6_addresses() {
            let xff = "2001:db8:85a3:8d3:1319:8a2e:370:7348, 192.168.1.1";
            let result = parse_original_client_ip(xff);
            assert_eq!(
                result,
                Some("2001:db8:85a3:8d3:1319:8a2e:370:7348".to_string())
            );
        }

        #[test]
        fn trims_whitespace_around_ips() {
            let xff = "  192.168.1.100  , 10.0.0.1  ";
            let result = parse_original_client_ip(xff);
            assert_eq!(result, Some("192.168.1.100".to_string()));
        }

        #[test]
        fn returns_none_for_empty_header() {
            let xff = "";
            let result = parse_original_client_ip(xff);
            assert_eq!(result, None);
        }

        #[test]
        fn returns_none_for_whitespace_only() {
            let xff = "   ";
            let result = parse_original_client_ip(xff);
            assert_eq!(result, None);
        }

        #[test]
        fn handles_single_comma() {
            let xff = "192.168.1.100,";
            let result = parse_original_client_ip(xff);
            assert_eq!(result, Some("192.168.1.100".to_string()));
        }

        #[tokio::test]
        async fn websocket_extracts_xff_header() {
            use tokio_tungstenite::tungstenite::http::HeaderValue;

            let tcp_port = start_echo_server().await.unwrap();
            let ws_port = start_proxy_server(tcp_port).await.unwrap();
            sleep(SERVER_STARTUP_DELAY).await;

            // Create a WebSocket request with X-Forwarded-For header
            let url = format!("ws://127.0.0.1:{ws_port}/");
            let mut request = IntoClientRequest::into_client_request(url).unwrap();
            request.headers_mut().insert(
                "x-forwarded-for",
                HeaderValue::from_str("203.0.113.195, 192.168.1.1").unwrap(),
            );

            // Connect and send a test message
            let (ws_stream, _) = connect_async(request).await.unwrap();
            let (mut sender, mut receiver) = ws_stream.split();

            let test_data = b"Test with XFF header";
            send_binary_message(&mut sender, test_data).await.unwrap();
            let received = receive_binary_message(&mut receiver).await.unwrap();
            assert_eq!(received, test_data);

            // The test passes if no errors occur - the IP extraction happens in logging
            // which we can't easily test here, but we verify the connection works with XFF headers
        }
    }

    mod proxy_ip_filtering_integration {
        use super::*;
        use tokio_tungstenite::tungstenite::http::HeaderValue;

        /// Helper to create WebSocket connection with custom Host header
        async fn connect_websocket_with_host(
            ws_port: u16,
            host: &str,
        ) -> Result<(WsSender, WsReceiver)> {
            let url = format!("ws://127.0.0.1:{ws_port}/");
            let mut request = IntoClientRequest::into_client_request(url)?;
            request
                .headers_mut()
                .insert("host", HeaderValue::from_str(host)?);

            let (ws_stream, _) = connect_async(request)
                .await
                .context("Failed to connect to WebSocket server")?;
            Ok(ws_stream.split())
        }

        #[tokio::test]
        async fn allows_connections_from_allowed_proxy_ip() {
            let tcp_port = start_echo_server().await.unwrap();

            let mut domains = HashMap::new();
            domains.insert("localhost".to_string(), tcp_port);

            // Allow connections from localhost (127.0.0.1)
            let allowed_ips = Some(vec!["127.0.0.1".to_string()]);
            let ws_port = start_proxy_with_ip_filtering(domains, allowed_ips)
                .await
                .unwrap();

            // This should succeed since we're connecting from 127.0.0.1
            let (mut sender, mut receiver) = connect_websocket_with_host(ws_port, "localhost")
                .await
                .unwrap();
            let test_data = b"Test with allowed IP";
            send_binary_message(&mut sender, test_data).await.unwrap();
            let received = receive_binary_message(&mut receiver).await.unwrap();
            assert_eq!(received, test_data);
        }

        #[tokio::test]
        async fn rejects_connections_from_disallowed_proxy_ip() {
            let tcp_port = start_echo_server().await.unwrap();

            let mut domains = HashMap::new();
            domains.insert("localhost".to_string(), tcp_port);

            // Only allow connections from a different IP (not 127.0.0.1)
            let allowed_ips = Some(vec!["192.168.1.100".to_string()]);
            let ws_port = start_proxy_with_ip_filtering(domains, allowed_ips)
                .await
                .unwrap();

            // This should fail since we're connecting from 127.0.0.1, not 192.168.1.100
            let url = format!("ws://127.0.0.1:{ws_port}/");
            let result = timeout(TEST_TIMEOUT, connect_async(&url)).await;

            // Connection should either fail immediately or close quickly
            if let Ok(Ok((ws_stream, _))) = result {
                let (_, mut receiver) = ws_stream.split();
                // If connection succeeds initially, it should close quickly
                let close_result = timeout(TEST_TIMEOUT, receiver.next()).await;
                assert!(close_result.is_ok()); // Should receive close or timeout
            } else {
                // Connection failed immediately, which is expected
            }
        }

        #[tokio::test]
        async fn allows_connections_with_cidr_range() {
            let tcp_port = start_echo_server().await.unwrap();

            let mut domains = HashMap::new();
            domains.insert("localhost".to_string(), tcp_port);

            // Allow connections from 127.0.0.0/8 range (includes 127.0.0.1)
            let allowed_ips = Some(vec!["127.0.0.0/8".to_string()]);
            let ws_port = start_proxy_with_ip_filtering(domains, allowed_ips)
                .await
                .unwrap();

            // This should succeed since 127.0.0.1 is in the 127.0.0.0/8 range
            let (mut sender, mut receiver) = connect_websocket_with_host(ws_port, "localhost")
                .await
                .unwrap();
            let test_data = b"Test with CIDR range";
            send_binary_message(&mut sender, test_data).await.unwrap();
            let received = receive_binary_message(&mut receiver).await.unwrap();
            assert_eq!(received, test_data);
        }
    }
}
