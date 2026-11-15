//! One TLS connection to Twitch chat for every channel, on its own thread:
//! reconnects with backoff and rejoins, answers PINGs, follows RECONNECT,
//! paces joins, and echoes the viewer's own messages locally.

use crate::irc;
use crate::model::{ChatEvent, ChatMessage, event_from};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const HOST: &str = "irc.chat.twitch.tv";
const PORT: u16 = 6697;
const MAX_LINE: usize = 64 * 1024;

/// A logged-in viewer. `Debug` hides the token.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub login: String,
    pub token: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials").field("login", &self.login).finish_non_exhaustive()
    }
}

enum Cmd {
    Join(String),
    Part(String),
    Send(String, String),
    Credentials(Option<Credentials>),
    Stop,
}

pub type OnEvent = Box<dyn Fn(ChatEvent) + Send>;

/// The chat connection. Dropping it closes the connection.
pub struct ChatClient {
    tx: Sender<Cmd>,
}

impl ChatClient {
    pub fn start(credentials: Option<Credentials>, on_event: OnEvent) -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = std::thread::Builder::new().name("gk-chat".into()).spawn(move || {
            let mut conn = Connection { rx, on_event, credentials, channels: Vec::new(), userstate: HashMap::new() };
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| conn.run())).is_err() {
                log::error!("chat thread panicked");
            }
        });
        Self { tx }
    }

    pub fn join(&self, channel: &str) {
        let _ = self.tx.send(Cmd::Join(channel.to_ascii_lowercase()));
    }

    pub fn part(&self, channel: &str) {
        let _ = self.tx.send(Cmd::Part(channel.to_ascii_lowercase()));
    }

    /// Sends a message (logged in only). Newlines become spaces; Twitch's
    /// 500-character limit is applied.
    pub fn send(&self, channel: &str, text: &str) {
        let clean: String = text.chars().map(|c| if c == '\r' || c == '\n' { ' ' } else { c }).take(500).collect();
        if !clean.trim().is_empty() {
            let _ = self.tx.send(Cmd::Send(channel.to_ascii_lowercase(), clean));
        }
    }

    /// Switches account (reconnects).
    pub fn set_credentials(&self, c: Option<Credentials>) {
        let _ = self.tx.send(Cmd::Credentials(c));
    }
}

impl Drop for ChatClient {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
    }
}

fn tls_config() -> Option<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<Option<Arc<rustls::ClientConfig>>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let cfg = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .ok()?
                .with_root_certificates(roots)
                .with_no_client_auth();
            Some(Arc::new(cfg))
        })
        .clone()
}

type Tls = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

/// Own name, colour and badges per channel (from USERSTATE), for the local
/// echo of sent messages.
#[derive(Default, Clone)]
struct UserState {
    display_name: String,
    color: Option<[u8; 3]>,
    badges: Vec<(String, String)>,
}

struct Connection {
    rx: Receiver<Cmd>,
    on_event: OnEvent,
    credentials: Option<Credentials>,
    channels: Vec<String>,
    userstate: HashMap<String, UserState>,
}

enum Exit {
    Stop,
    Reconnect(String),
}

impl Connection {
    fn run(&mut self) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            match self.session() {
                Exit::Stop => return,
                Exit::Reconnect(reason) => {
                    (self.on_event)(ChatEvent::Disconnected { reason: reason.clone() });
                    log::warn!("chat disconnected: {reason}");
                }
            }
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(1);
            }
            // Wait, but keep taking commands (joins update the channel list).
            let end = Instant::now() + backoff;
            while Instant::now() < end {
                match self.rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(Cmd::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Ok(cmd) => self.apply_offline(cmd),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    fn apply_offline(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Join(c) => {
                if !self.channels.contains(&c) {
                    self.channels.push(c);
                }
            }
            Cmd::Part(c) => self.channels.retain(|x| x != &c),
            Cmd::Credentials(c) => self.credentials = c,
            Cmd::Send(..) | Cmd::Stop => {}
        }
    }

    fn connect(&self) -> Result<Tls, String> {
        let cfg = tls_config().ok_or("TLS setup failed")?;
        let addr = (HOST, PORT).to_socket_addrs().map_err(|e| e.to_string())?.next().ok_or("no address")?;
        let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(8)).map_err(|e| e.to_string())?;
        let _ = tcp.set_nodelay(true);
        tcp.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
        tcp.set_write_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
        let name = rustls::pki_types::ServerName::try_from(HOST).map_err(|e| e.to_string())?;
        let conn = rustls::ClientConnection::new(cfg, name).map_err(|e| e.to_string())?;
        let mut tls = rustls::StreamOwned::new(conn, tcp);
        // Handshake with a generous timeout; afterwards reads poll every
        // 100 ms so commands are never held up.
        while tls.conn.is_handshaking() {
            tls.conn.complete_io(&mut tls.sock).map_err(|e| format!("TLS handshake: {e}"))?;
        }
        tls.sock.set_read_timeout(Some(Duration::from_millis(100))).map_err(|e| e.to_string())?;
        Ok(tls)
    }

    fn session(&mut self) -> Exit {
        let mut tls = match self.connect() {
            Ok(t) => t,
            Err(e) => return Exit::Reconnect(e),
        };
        let mut out = String::new();
        out.push_str("CAP REQ :twitch.tv/tags twitch.tv/commands\r\n");
        let login = match &self.credentials {
            Some(c) => {
                out.push_str(&format!("PASS oauth:{}\r\nNICK {}\r\n", c.token, c.login));
                c.login.clone()
            }
            None => {
                let n = u32::from_str_radix(&gk_random(), 16).unwrap_or(4242) % 89_999 + 10_000;
                let nick = format!("justinfan{n}");
                out.push_str(&format!("PASS SCHMOOPIIE\r\nNICK {nick}\r\n"));
                nick
            }
        };
        if let Err(e) = tls.write_all(out.as_bytes()).and_then(|_| tls.flush()) {
            return Exit::Reconnect(e.to_string());
        }
        let mut welcomed = false;
        let mut joined: HashSet<String> = HashSet::new();
        let mut join_times: VecDeque<Instant> = VecDeque::new();
        let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
        let mut chunk = [0u8; 16 * 1024];
        let mut last_rx = Instant::now();
        let mut ping_sent: Option<Instant> = None;
        loop {
            // Commands.
            loop {
                match self.rx.try_recv() {
                    Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => {
                        let _ = tls.write_all(b"QUIT\r\n");
                        return Exit::Stop;
                    }
                    Ok(Cmd::Credentials(c)) => {
                        self.credentials = c;
                        return Exit::Reconnect("account changed".into());
                    }
                    Ok(Cmd::Join(c)) => {
                        if !self.channels.contains(&c) {
                            self.channels.push(c);
                        }
                    }
                    Ok(Cmd::Part(c)) => {
                        self.channels.retain(|x| x != &c);
                        if joined.remove(&c) {
                            let _ = tls.write_all(format!("PART #{c}\r\n").as_bytes());
                        }
                    }
                    Ok(Cmd::Send(c, text)) => {
                        if self.credentials.is_none() {
                            (self.on_event)(ChatEvent::Notice { channel: c, text: "Log in to chat.".into() });
                        } else if let Err(e) = tls.write_all(format!("PRIVMSG #{c} :{text}\r\n").as_bytes()) {
                            return Exit::Reconnect(e.to_string());
                        } else {
                            self.echo(&c, &login, text);
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
            // Joins, paced at 15 per 10 s (Twitch allows 20).
            if welcomed {
                let now = Instant::now();
                while join_times.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(10)) {
                    join_times.pop_front();
                }
                for c in self.channels.clone() {
                    if joined.contains(&c) || join_times.len() >= 15 {
                        continue;
                    }
                    if let Err(e) = tls.write_all(format!("JOIN #{c}\r\n").as_bytes()) {
                        return Exit::Reconnect(e.to_string());
                    }
                    joined.insert(c);
                    join_times.push_back(now);
                }
            }
            let _ = tls.flush();
            // Read whatever arrived (the socket times out after 100 ms).
            match tls.read(&mut chunk) {
                Ok(0) => return Exit::Reconnect("connection closed".into()),
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    last_rx = Instant::now();
                    ping_sent = None;
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => return Exit::Reconnect(e.to_string()),
            }
            if buf.len() > MAX_LINE * 4 {
                return Exit::Reconnect("oversized line".into());
            }
            while let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
                let line_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
                let text = String::from_utf8_lossy(&line_bytes[..pos]);
                let Some(line) = irc::parse(&text) else { continue };
                match line.command.as_str() {
                    "PING" => {
                        let arg = line.params.first().cloned().unwrap_or_else(|| "tmi.twitch.tv".into());
                        if let Err(e) = tls.write_all(format!("PONG :{arg}\r\n").as_bytes()) {
                            return Exit::Reconnect(e.to_string());
                        }
                    }
                    "RECONNECT" => return Exit::Reconnect("server asked to reconnect".into()),
                    "001" => {
                        welcomed = true;
                        (self.on_event)(ChatEvent::Connected { login: login.clone() });
                    }
                    "NOTICE" if !welcomed => {
                        // Login failed ("Login authentication failed").
                        (self.on_event)(ChatEvent::Notice { channel: String::new(), text: line.text().to_owned() });
                        return Exit::Reconnect(line.text().to_owned());
                    }
                    "USERSTATE" | "GLOBALUSERSTATE" => {
                        let st = UserState {
                            display_name: line.tag("display-name").unwrap_or(&login).to_owned(),
                            color: line.tag("color").and_then(|c| {
                                let v = u32::from_str_radix(c.trim_start_matches('#'), 16).ok()?;
                                Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
                            }),
                            badges: line
                                .tag("badges")
                                .map(|b| b.split(',').filter_map(|p| p.split_once('/')).map(|(s, v)| (s.into(), v.into())).collect())
                                .unwrap_or_default(),
                        };
                        self.userstate.insert(line.channel().to_owned(), st);
                    }
                    _ => {
                        if let Some(ev) = event_from(&line) {
                            // Our own JOIN echo only matters for our login.
                            if matches!(ev, ChatEvent::Joined { .. }) && !line.nick.eq_ignore_ascii_case(&login) {
                                continue;
                            }
                            (self.on_event)(ev);
                        }
                    }
                }
            }
            // Keepalive: Twitch pings every ~5 minutes; check sooner.
            if last_rx.elapsed() > Duration::from_secs(240) {
                match ping_sent {
                    None => {
                        let _ = tls.write_all(b"PING :tmi.twitch.tv\r\n");
                        ping_sent = Some(Instant::now());
                    }
                    Some(t) if t.elapsed() > Duration::from_secs(15) => return Exit::Reconnect("ping timeout".into()),
                    _ => {}
                }
            }
        }
    }

    fn echo(&self, channel: &str, login: &str, text: String) {
        let st = self.userstate.get(channel).or_else(|| self.userstate.get("")).cloned().unwrap_or_default();
        let (text, action) = match text.strip_prefix("/me ") {
            Some(t) => (t.to_owned(), true),
            None => (text, false),
        };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        (self.on_event)(ChatEvent::Message(ChatMessage {
            id: format!("local-{now}"),
            channel: channel.to_owned(),
            login: login.to_owned(),
            display_name: if st.display_name.is_empty() { login.to_owned() } else { st.display_name },
            color: st.color,
            badges: st.badges,
            text,
            action,
            timestamp_ms: now,
            own: true,
            ..Default::default()
        }));
    }
}

fn gk_random() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u64(std::process::id().into());
    format!("{:08x}", h.finish() as u32)
}
