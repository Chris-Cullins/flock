use std::sync::{Arc, Mutex, mpsc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use anyhow::Result;
use fl_storage::ws_protocol::{WsClientMessage, WsServerMessage};

/// WebSocket connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsConnectionState {
    Connecting,
    Authenticating,
    Connected,
    Reconnecting,
    Disconnected,
}

/// WebSocket client configuration
#[derive(Debug, Clone)]
pub struct WsClientConfig {
    pub url: String,
    pub token: String,
    pub heartbeat_interval: Duration,
    pub pong_timeout: Duration,
    pub max_reconnect_attempts: u32,
}

impl WsClientConfig {
    /// Create a new configuration with the given URL and token, using default values for other fields
    pub fn new(url: String, token: String) -> Self {
        Self {
            url,
            token,
            heartbeat_interval: Duration::from_secs(30),
            pong_timeout: Duration::from_secs(10),
            max_reconnect_attempts: 10,
        }
    }
}

/// WebSocket client with background thread for handling connections
pub struct WsClient {
    sender: mpsc::Sender<WsClientMessage>,
    receiver: mpsc::Receiver<WsServerMessage>,
    state: Arc<Mutex<WsConnectionState>>,
    shutdown: Arc<AtomicBool>,
    _thread: Option<thread::JoinHandle<()>>,
}

impl WsClient {
    /// Connect to a WebSocket server with the given configuration
    pub fn connect(config: WsClientConfig) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(WsConnectionState::Connecting));
        let shutdown = Arc::new(AtomicBool::new(false));

        let state_clone = Arc::clone(&state);
        let shutdown_clone = Arc::clone(&shutdown);

        let thread = thread::spawn(move || {
            ws_event_loop(config, cmd_rx, evt_tx, state_clone, shutdown_clone);
        });

        Ok(Self {
            sender: cmd_tx,
            receiver: evt_rx,
            state,
            shutdown,
            _thread: Some(thread),
        })
    }

    /// Send a message to the server
    pub fn send(&self, msg: WsClientMessage) -> Result<()> {
        self.sender
            .send(msg)
            .map_err(|e| anyhow::anyhow!("Failed to send message: {}", e))
    }

    /// Try to receive a message from the server without blocking
    pub fn try_recv(&self) -> Option<WsServerMessage> {
        self.receiver.try_recv().ok()
    }

    /// Receive a message from the server with a timeout
    pub fn recv_timeout(&self, timeout: Duration) -> Option<WsServerMessage> {
        self.receiver.recv_timeout(timeout).ok()
    }

    /// Disconnect from the server
    pub fn disconnect(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Get the current connection state
    pub fn state(&self) -> WsConnectionState {
        *self.state.lock().unwrap()
    }

    /// Check if the client is connected
    pub fn is_connected(&self) -> bool {
        self.state() == WsConnectionState::Connected
    }
}

impl Drop for WsClient {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Background event loop for handling WebSocket connections
fn ws_event_loop(
    config: WsClientConfig,
    cmd_rx: mpsc::Receiver<WsClientMessage>,
    evt_tx: mpsc::Sender<WsServerMessage>,
    state: Arc<Mutex<WsConnectionState>>,
    shutdown: Arc<AtomicBool>,
) {
    let mut reconnect_attempts = 0;
    let mut backoff = Duration::from_secs(1);

    'outer: loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Try to connect
        *state.lock().unwrap() = WsConnectionState::Connecting;
        let connect_result = tungstenite::connect(&config.url);
        let (mut socket, _response) = match connect_result {
            Ok(r) => r,
            Err(_) => {
                // Reconnect logic
                reconnect_attempts += 1;
                if reconnect_attempts > config.max_reconnect_attempts {
                    break;
                }
                *state.lock().unwrap() = WsConnectionState::Reconnecting;
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };

        reconnect_attempts = 0;
        backoff = Duration::from_secs(1);

        // Set read timeout on underlying TCP stream
        match socket.get_ref() {
            tungstenite::stream::MaybeTlsStream::Plain(tcp) => {
                let _ = tcp.set_read_timeout(Some(Duration::from_millis(100)));
            }
            tungstenite::stream::MaybeTlsStream::NativeTls(tls) => {
                let _ = tls.get_ref().set_read_timeout(Some(Duration::from_millis(100)));
            }
            _ => {}
        }

        // Auth
        *state.lock().unwrap() = WsConnectionState::Authenticating;
        let auth_msg = WsClientMessage::Auth {
            token: config.token.clone(),
        };
        if socket
            .send(tungstenite::Message::Text(
                serde_json::to_string(&auth_msg).unwrap(),
            ))
            .is_err()
        {
            continue 'outer; // reconnect
        }

        let mut last_ping = Instant::now();
        let mut last_pong = Instant::now();
        let mut ping_seq: u64 = 0;

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break 'outer;
            }

            // Try read from WebSocket
            match socket.read() {
                Ok(msg) => match msg {
                    tungstenite::Message::Text(text) => {
                        if let Ok(server_msg) = serde_json::from_str::<WsServerMessage>(&text) {
                            // Handle AuthResult to update state
                            if matches!(&server_msg, WsServerMessage::AuthResult { success, .. } if *success)
                            {
                                *state.lock().unwrap() = WsConnectionState::Connected;
                            }
                            if matches!(&server_msg, WsServerMessage::Pong { .. }) {
                                last_pong = Instant::now();
                            }
                            let _ = evt_tx.send(server_msg);
                        }
                    }
                    tungstenite::Message::Close(_) => {
                        *state.lock().unwrap() = WsConnectionState::Reconnecting;
                        continue 'outer;
                    }
                    _ => {}
                },
                Err(tungstenite::Error::Io(ref e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // Timeout — expected, just continue
                }
                Err(_) => {
                    *state.lock().unwrap() = WsConnectionState::Reconnecting;
                    continue 'outer;
                }
            }

            // Send queued commands
            while let Ok(cmd) = cmd_rx.try_recv() {
                let text = serde_json::to_string(&cmd).unwrap();
                if socket
                    .send(tungstenite::Message::Text(text))
                    .is_err()
                {
                    *state.lock().unwrap() = WsConnectionState::Reconnecting;
                    continue 'outer;
                }
            }

            // Heartbeat
            if last_ping.elapsed() >= config.heartbeat_interval {
                ping_seq += 1;
                let ping = WsClientMessage::Ping { seq: ping_seq };
                let text = serde_json::to_string(&ping).unwrap();
                if socket.send(tungstenite::Message::Text(text)).is_err() {
                    continue 'outer;
                }
                last_ping = Instant::now();
            }

            // Pong timeout check
            if last_ping > last_pong
                && last_pong.elapsed() > config.heartbeat_interval + config.pong_timeout
            {
                *state.lock().unwrap() = WsConnectionState::Reconnecting;
                continue 'outer;
            }
        }
    }

    *state.lock().unwrap() = WsConnectionState::Disconnected;
}
