//! One WebSocket to IaR Live Audio per channel, on its own thread, so a slow or dead connection
//! never stalls the recorder. The thread logs in, sends what the lane queues, pings every
//! [`PING_EVERY`], and reconnects with backoff. Nothing is queued while disconnected: the audio
//! would be out of date by the time it arrived.

use std::net::TcpStream;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::json;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::channel::Cmd;

/// Commands queued for the worker. About 15 seconds of 60 ms packets.
const QUEUE: usize = 256;
const PING_EVERY: Duration = Duration::from_secs(30);
const BACKOFF_START: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long a read or write may block: short, so queued commands go out promptly.
const POLL: Duration = Duration::from_millis(50);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// What the worker has seen, for the plugin's health report.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkStats {
    pub connected: bool,
    /// Successful connections so far.
    pub connects: u64,
    /// Connections lost after they were up. Audio sent while down was dropped, so the lane must
    /// end its transmission when this changes.
    pub losses: u64,
    /// Why the last attempt failed. Empty once connected.
    pub last_error: String,
    /// Commands dropped because the link was down or its queue was full.
    pub dropped: u64,
    /// Audio packets written to the connection, and their bytes, so far.
    pub packets: u64,
    pub bytes: u64,
    /// Unix seconds when the last audio packet was written (0: none yet).
    pub last_audio: f64,
}

/// A channel's connection. Dropping it stops the worker after it sends what's queued.
pub struct Link {
    tx: SyncSender<Cmd>,
    stats: Arc<Mutex<LinkStats>>,
    _worker: JoinHandle<()>,
}

impl Link {
    /// Start connecting to `<base><channel_id>` with `api_key`. Returns at once.
    pub fn start(base: &str, channel_id: &str, api_key: &str) -> Link {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let stats = Arc::new(Mutex::new(LinkStats::default()));
        let (url, key, worker_stats) = (format!("{base}{channel_id}"), api_key.to_string(), stats.clone());
        let worker = thread::spawn(move || run(&url, &key, rx, &worker_stats));
        Link { tx, stats, _worker: worker }
    }

    /// Queue a command. Never blocks: a full queue drops it and counts the drop.
    pub fn send(&self, cmd: Cmd) {
        if self.tx.try_send(cmd).is_err() {
            update(&self.stats, |s| s.dropped += 1);
        }
    }

    pub fn stats(&self) -> LinkStats {
        self.stats.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

fn run(url: &str, api_key: &str, rx: mpsc::Receiver<Cmd>, stats: &Mutex<LinkStats>) {
    let mut backoff = BACKOFF_START;
    loop {
        match connect(url, api_key) {
            Ok(mut ws) => {
                backoff = BACKOFF_START;
                update(stats, |s| {
                    s.connected = true;
                    s.connects += 1;
                    s.last_error.clear();
                });
                let outcome = pump(&mut ws, &rx, stats);
                update(stats, |s| s.connected = false);
                match outcome {
                    Ok(()) => return,
                    Err(e) => update(stats, |s| {
                        s.last_error = e;
                        s.losses += 1;
                    }),
                }
            }
            Err(e) => update(stats, |s| s.last_error = e),
        }
        // Wait out the backoff, dropping what arrives: the lane will end its transmission when it
        // sees the connection change.
        let deadline = Instant::now() + backoff;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left.min(POLL * 4)) {
                Ok(_) => update(stats, |s| s.dropped += 1),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Send and receive until the plugin drops the link (`Ok`) or the connection fails (`Err`).
fn pump(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>, rx: &mpsc::Receiver<Cmd>, stats: &Mutex<LinkStats>) -> Result<(), String> {
    set_timeouts(ws);
    let mut last_ping = Instant::now();
    loop {
        match rx.recv_timeout(POLL) {
            Ok(Cmd::Control(text)) => ws.send(Message::Text(text.into())).map_err(|e| format!("send failed ({e})"))?,
            Ok(Cmd::Audio(packet)) => {
                let len = packet.len() as u64;
                ws.send(Message::Binary(packet.into())).map_err(|e| format!("send failed ({e})"))?;
                update(stats, |s| {
                    s.packets += 1;
                    s.bytes += len;
                    s.last_audio = unix_secs();
                });
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = ws.close(None);
                return Ok(());
            }
        }
        match ws.read() {
            Ok(Message::Close(_)) => return Err("IaR Live Audio closed the connection".into()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(format!("connection lost ({e})")),
        }
        if last_ping.elapsed() >= PING_EVERY {
            ws.send(Message::Ping(Vec::new().into())).map_err(|e| format!("ping failed ({e})"))?;
            last_ping = Instant::now();
        }
    }
}

/// Connect, with the key in the header, then log in as IaR-Stream does. Errors never carry the key.
fn connect(url: &str, api_key: &str) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, String> {
    let mut request = url.into_client_request().map_err(|_| "bad channel address".to_string())?;
    let key = HeaderValue::from_str(api_key).map_err(|_| "the API key has characters a header can't carry".to_string())?;
    request.headers_mut().insert("X-Api-Key", key);
    let (mut ws, _) = tungstenite::connect(request).map_err(|e| format!("couldn't connect ({e})"))?;
    let login = json!({ "action": "login", "v": 1, "api_key": api_key, "access_token": null }).to_string();
    ws.send(Message::Text(login.into())).map_err(|e| format!("login failed ({e})"))?;
    Ok(ws)
}

fn set_timeouts(ws: &WebSocket<MaybeTlsStream<TcpStream>>) {
    let tcp = match ws.get_ref() {
        MaybeTlsStream::Plain(s) => Some(s),
        MaybeTlsStream::Rustls(s) => Some(s.get_ref()),
        _ => None,
    };
    if let Some(tcp) = tcp {
        let _ = tcp.set_read_timeout(Some(POLL));
        let _ = tcp.set_write_timeout(Some(WRITE_TIMEOUT));
    }
}

fn update(stats: &Mutex<LinkStats>, f: impl FnOnce(&mut LinkStats)) {
    f(&mut stats.lock().unwrap_or_else(|p| p.into_inner()));
}

fn unix_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}
