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

    let target_port = env::var("TARGET_PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()
        .expect("TARGET_PORT must be a valid port number");

    let app = Router::new()
        .route(
            "/",
            get({ move |ws: WebSocketUpgrade| async move { websocket_handler(ws, target_port) } }),
        )
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([0, 0, 0, 0], 80));
    info!(
        "WebSocket proxy listening on {}, forwarding to port {}",
        addr, target_port
    );

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn websocket_handler(ws: WebSocketUpgrade, target_port: u16) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, target_port))
}

async fn handle_socket(websocket: WebSocket, target_port: u16) {
    let target_addr = format!("127.0.0.1:{target_port}");

    let tcp_stream = match TcpStream::connect(&target_addr).await {
        Ok(stream) => stream,
        Err(e) => {
            error!("Failed to connect to target {}: {}", target_addr, e);
            return;
        }
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
