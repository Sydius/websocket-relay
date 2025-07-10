use axum::{
    Router,
    extract::{WebSocketUpgrade, ws::WebSocket},
    response::Response,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use std::{env, net::SocketAddr};
use tokio::net::TcpStream;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let target_port = get_target_port();
    let app = create_app(target_port);

    let addr = SocketAddr::from(([0, 0, 0, 0], 80));
    info!(
        "WebSocket proxy listening on {}, forwarding to port {}",
        addr, target_port
    );

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn get_target_port() -> u16 {
    env::var("TARGET_PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()
        .expect("TARGET_PORT must be a valid port number")
}

fn create_app(target_port: u16) -> Router {
    Router::new()
        .route(
            "/",
            get(move |ws: WebSocketUpgrade| async move { websocket_handler(ws, target_port) }),
        )
        .layer(TraceLayer::new_for_http())
}

fn websocket_handler(ws: WebSocketUpgrade, target_port: u16) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, target_port))
}

pub async fn handle_socket(websocket: WebSocket, target_port: u16) {
    let target_addr = format!("127.0.0.1:{target_port}");

    let Ok(tcp_stream) = TcpStream::connect(&target_addr).await else {
        error!("Failed to connect to target {}", target_addr);
        return;
    };

    info!("Connected to target server at {}", target_addr);

    let (mut ws_sender, mut ws_receiver) = websocket.split();
    let (mut tcp_reader, mut tcp_writer) = tcp_stream.into_split();

    let ws_to_tcp = async {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(axum::extract::ws::Message::Binary(data)) => {
                    if let Err(e) =
                        tokio::io::AsyncWriteExt::write_all(&mut tcp_writer, &data).await
                    {
                        error!("Failed to write to TCP: {}", e);
                        break;
                    }
                }
                Ok(axum::extract::ws::Message::Text(_)) => {
                    warn!("Dropping text message (binary only)");
                }
                Ok(axum::extract::ws::Message::Close(_)) => {
                    info!("WebSocket connection closed");
                    break;
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }
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
                    if let Err(e) = ws_sender
                        .send(axum::extract::ws::Message::Binary(data.to_vec().into()))
                        .await
                    {
                        error!("Failed to send WebSocket message: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    error!("Failed to read from TCP: {}", e);
                    break;
                }
            }
        }
    };

    tokio::select! {
        () = ws_to_tcp => {},
        () = tcp_to_ws => {},
    }

    info!("Proxy connection closed");
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
    async fn connect_websocket(port: u16) -> (WsSender, WsReceiver) {
        let url = format!("ws://127.0.0.1:{port}/");
        let (ws_stream, _) = connect_async(&url).await.expect("Failed to connect");
        ws_stream.split()
    }

    /// Sends binary data through WebSocket
    async fn send_binary_message(sender: &mut WsSender, data: &[u8]) {
        sender
            .send(Message::Binary(data.to_vec().into()))
            .await
            .unwrap();
    }

    /// Receives binary message from WebSocket with timeout
    async fn receive_binary_message(receiver: &mut WsReceiver) -> Vec<u8> {
        let response = timeout(TEST_TIMEOUT, receiver.next())
            .await
            .expect("Timeout")
            .expect("No message")
            .expect("WebSocket error");

        match response {
            Message::Binary(data) => data.to_vec(),
            other => panic!("Expected binary message, got: {other:?}"),
        }
    }

    /// Finds an unused port by binding to port 0
    async fn find_free_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    /// Starts proxy server on free port, returns port number
    async fn start_proxy_server(target_port: u16) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let app = create_app(target_port);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        port
    }

    /// Starts TCP echo server on free port, returns port number
    async fn start_echo_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

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

        port
    }

    /// Sets up proxy server with echo server backend
    async fn setup_proxy_with_echo_server() -> (u16, u16) {
        let tcp_port = start_echo_server().await;
        let ws_port = start_proxy_server(tcp_port).await;
        sleep(SERVER_STARTUP_DELAY).await;
        (ws_port, tcp_port)
    }

    mod proxy_functionality {
        use super::*;

        #[tokio::test]
        async fn forwards_binary_data() {
            let (ws_port, _) = setup_proxy_with_echo_server().await;
            let (mut sender, mut receiver) = connect_websocket(ws_port).await;

            let test_data = b"Hello WebSocket Proxy!";
            send_binary_message(&mut sender, test_data).await;

            let received = receive_binary_message(&mut receiver).await;
            assert_eq!(received, test_data);
        }

        #[tokio::test]
        async fn drops_text_messages() {
            let (ws_port, _) = setup_proxy_with_echo_server().await;
            let (mut sender, mut receiver) = connect_websocket(ws_port).await;

            // Send text message (should be dropped)
            sender
                .send(Message::Text("This should be dropped".to_string().into()))
                .await
                .unwrap();

            // Send binary message (should work)
            let binary_data = b"This should work";
            send_binary_message(&mut sender, binary_data).await;

            let received = receive_binary_message(&mut receiver).await;
            assert_eq!(received, binary_data);
        }

        #[tokio::test]
        async fn handles_multiple_messages() {
            let (ws_port, _) = setup_proxy_with_echo_server().await;
            let (mut sender, mut receiver) = connect_websocket(ws_port).await;

            let messages = [b"First message".as_slice(), b"Second message"];

            for &msg in &messages {
                send_binary_message(&mut sender, msg).await;
                let received = receive_binary_message(&mut receiver).await;
                assert_eq!(received, msg);
            }
        }

        #[tokio::test]
        async fn handles_large_messages() {
            let (ws_port, _) = setup_proxy_with_echo_server().await;
            let (mut sender, mut receiver) = connect_websocket(ws_port).await;

            let large_data = vec![0xAB; 2048];
            send_binary_message(&mut sender, &large_data).await;

            // Large messages are split into chunks due to 1024-byte buffer
            let mut received_data = Vec::new();
            let expected_chunks = large_data.len().div_ceil(1024);

            for _ in 0..expected_chunks {
                let chunk = receive_binary_message(&mut receiver).await;
                received_data.extend_from_slice(&chunk);
            }

            assert_eq!(received_data, large_data);
        }

        #[tokio::test]
        async fn handles_concurrent_connections() {
            let (ws_port, _) = setup_proxy_with_echo_server().await;

            let tasks: Vec<_> = (0..3)
                .map(|i| {
                    tokio::spawn(async move {
                        let (mut sender, mut receiver) = connect_websocket(ws_port).await;
                        let test_data = format!("Message from client {i}").into_bytes();

                        send_binary_message(&mut sender, &test_data).await;
                        let received = receive_binary_message(&mut receiver).await;
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
            let ws_port = start_proxy_server(tcp_port).await;
            sleep(SERVER_STARTUP_DELAY).await;

            let (mut sender, _) = connect_websocket(ws_port).await;
            let test_data = b"Direct TCP test data";
            send_binary_message(&mut sender, test_data).await;

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
            let ws_port = start_proxy_server(tcp_port).await;
            sleep(SERVER_STARTUP_DELAY).await;

            let (_, mut receiver) = connect_websocket(ws_port).await;
            let received = receive_binary_message(&mut receiver).await;
            assert_eq!(received, test_data);
        }

        #[tokio::test]
        async fn text_messages_not_forwarded_to_tcp() {
            let (tcp_port, received_data) = create_capturing_tcp_server().await;
            let ws_port = start_proxy_server(tcp_port).await;
            sleep(SERVER_STARTUP_DELAY).await;

            let (mut sender, _) = connect_websocket(ws_port).await;

            // Send text message (should not reach TCP)
            sender
                .send(Message::Text(
                    "This should not reach TCP".to_string().into(),
                ))
                .await
                .unwrap();

            // Send binary message (should reach TCP)
            let binary_data = b"This should reach TCP";
            send_binary_message(&mut sender, binary_data).await;

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
            let ws_port = find_free_port().await;
            let nonexistent_tcp_port = find_free_port().await;

            let app = create_app(nonexistent_tcp_port);
            tokio::spawn(async move {
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", ws_port))
                    .await
                    .unwrap();
                axum::serve(listener, app).await.unwrap();
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
