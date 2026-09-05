// rtsp.rs — native RTSP client for NVR playback (PlaybackStream.swift port).
//
// Playback deliberately does NOT go through ffmpeg's RTSP client: its RTP
// depacketizer sits on the NVR's initial burst for ~4 s before emitting the
// first packet (measured on the Mac; the NVR itself delivers video ~0.25 s
// after PLAY). Speaking RTSP directly gets the first frame on screen in
// ~0.3 s and lets us send the `Scale:` header for fast playback natively.
//
// Scope: TCP-interleaved RTP, video track only, digest auth, HEVC (RFC 7798)
// and H.264 (RFC 6184) depacketization into Annex B. The Mac feeds its own
// parser; here the NAL stream is written into a sink — ffmpeg's stdin, for
// decoding (stream.rs) or for muxing a clip (media.rs). Timestamps aren't
// taken from RTP — the NVR paces delivery at the requested speed, and the
// decoder stamps frames on arrival, exactly like the live path.

use md5::{Digest, Md5};
use std::io::{BufReader, BufWriter, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One playback session's parameters.
pub struct Request {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// "/Streaming/tracks/101/?starttime=…&endtime=…"
    pub path: String,
    /// "20260719T083000Z" (NVR-local fake-UTC)
    pub start_clock: String,
    /// 1, 2 or 4 — the Scale header for >1.
    pub scale: u32,
    /// "hevc" / "h264"
    pub codec: &'static str,
}

/// Handle on a running session: `stop` unblocks the reader and ends it.
pub struct Session {
    stopped: Arc<AtomicBool>,
    sock: Arc<Mutex<Option<TcpStream>>>,
}

impl Session {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(s) = self.sock.lock().unwrap().take() {
            let _ = s.shutdown(Shutdown::Both); // unblocks the reader thread
        }
    }
}

/// Run the session on its own thread: connect, DESCRIBE/SETUP/PLAY, then
/// depacketize into `sink` until the NVR stops sending (or `stop`). Status
/// strings go to `state` ("connecting…", failures, "ended"); dropping the
/// sink when the thread ends is what tells the consumer the stream is over.
pub fn start(
    req: Request,
    sink: impl Write + Send + 'static,
    state: impl Fn(&str) + Send + 'static,
) -> Session {
    let stopped = Arc::new(AtomicBool::new(false));
    let sock = Arc::new(Mutex::new(None));
    let session = Session {
        stopped: stopped.clone(),
        sock: sock.clone(),
    };
    std::thread::spawn(move || {
        let mut sink = BufWriter::with_capacity(1 << 16, sink);
        let mut c = Client {
            req,
            stopped,
            sock,
            cseq: Arc::new(AtomicU32::new(0)),
            realm: String::new(),
            nonce: String::new(),
            session: String::new(),
            reader: None,
            fu: Vec::new(),
            state: Box::new(state),
        };
        c.run(&mut sink);
        let _ = sink.flush();
    });
    session
}

struct Client {
    req: Request,
    stopped: Arc<AtomicBool>,
    sock: Arc<Mutex<Option<TcpStream>>>,
    cseq: Arc<AtomicU32>,
    realm: String,
    nonce: String,
    session: String,
    reader: Option<BufReader<TcpStream>>,
    /// Fragmented-NAL reassembly.
    fu: Vec<u8>,
    state: Box<dyn Fn(&str) + Send>,
}

/// RTSP keepalive — Hikvision expires sessions without traffic (~60 s).
const KEEPALIVE: Duration = Duration::from_secs(25);
/// Stalled session watchdog: a read blocked this long ends the session.
const READ_TIMEOUT: Duration = Duration::from_secs(12);

impl Client {
    fn uri(&self) -> String {
        format!(
            "rtsp://{}:{}{}",
            self.req.host, self.req.port, self.req.path
        )
    }

    fn report(&self, s: &str) {
        (self.state)(s);
    }

    fn run(&mut self, sink: &mut impl Write) {
        self.report("connecting…");
        let Some(stream) = self.connect() else {
            return self.fail("NVR unreachable");
        };
        {
            let mut guard = self.sock.lock().unwrap();
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }
            *guard = stream.try_clone().ok();
        }
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
        let _ = stream.set_nodelay(true);
        self.reader = Some(BufReader::with_capacity(1 << 16, stream));

        // DESCRIBE (expect a 401 first to learn realm/nonce), SETUP, PLAY.
        let uri = self.uri();
        let Some(mut resp) = self.request("DESCRIBE", &uri, &["Accept: application/sdp"]) else {
            return self.fail("NVR unreachable");
        };
        if resp.0 == 401 {
            if !self.parse_auth_challenge(&resp.1) {
                return self.fail("auth failed");
            }
            match self.request("DESCRIBE", &uri, &["Accept: application/sdp"]) {
                Some(again) => resp = again,
                None => return self.fail("auth failed"),
            }
        }
        if resp.0 != 200 {
            return self.fail(&format!("playback refused ({})", resp.0));
        }

        let setup = self.request(
            "SETUP",
            &format!("{uri}/trackID=video"),
            &["Transport: RTP/AVP/TCP;unicast;interleaved=0-1"],
        );
        let Some(sess) = setup
            .filter(|r| r.0 == 200)
            .and_then(|r| header_value(&r.1, "Session").map(|v| v.to_string()))
        else {
            return self.fail("playback setup failed");
        };
        self.session = sess;

        let session_header = format!("Session: {}", self.session);
        let range = format!("Range: clock={}-", self.req.start_clock);
        let mut play_headers = vec![session_header.as_str(), range.as_str()];
        let scale = format!("Scale: {}.000", self.req.scale);
        if self.req.scale > 1 {
            play_headers.push(scale.as_str());
        }
        match self.request("PLAY", &uri, &play_headers) {
            Some(r) if r.0 == 200 => {}
            _ => return self.fail("playback failed"),
        }

        if std::env::var_os("HIK_DEBUG").is_some() {
            eprintln!("[rtsp] PLAY ok");
        }
        self.start_keepalive();
        self.read_loop(sink);
    }

    /// After PLAY: interleaved binary frames ($-prefixed) mixed with the odd
    /// RTSP text response (keepalive replies) — video is channel 0.
    fn read_loop(&mut self, sink: &mut impl Write) {
        let mut head = [0u8; 4];
        loop {
            if self.read_exact(&mut head).is_err() {
                return self.ended();
            }
            if head[0] == b'$' {
                let channel = head[1];
                let len = usize::from(head[2]) << 8 | usize::from(head[3]);
                let mut payload = vec![0u8; len];
                if self.read_exact(&mut payload).is_err() {
                    return self.ended();
                }
                if channel == 0 && !self.handle_rtp(&payload, sink) {
                    return self.ended(); // sink gone (ffmpeg exited)
                }
            } else if self.consume_text_response(&head).is_none() {
                return self.ended();
            }
        }
    }

    fn ended(&self) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        self.report("ended");
    }

    fn fail(&self, status: &str) {
        self.report(status);
    }

    fn start_keepalive(&self) {
        let Some(sock) = self
            .sock
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.try_clone().ok())
        else {
            return;
        };
        let stopped = self.stopped.clone();
        let cseq = self.cseq.clone();
        let uri = self.uri();
        let session = self.session.clone();
        let auth = self.auth_header("OPTIONS", &uri);
        std::thread::spawn(move || {
            let mut sock = sock;
            let mut since = std::time::Instant::now();
            while !stopped.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(1));
                if since.elapsed() < KEEPALIVE {
                    continue;
                }
                since = std::time::Instant::now();
                let n = cseq.fetch_add(1, Ordering::SeqCst) + 1;
                let msg = format!(
                    "OPTIONS {uri} RTSP/1.0\r\nCSeq: {n}\r\nSession: {session}\r\n{auth}\r\n"
                );
                if sock.write_all(msg.as_bytes()).is_err() {
                    return;
                }
            }
        });
    }

    // MARK: RTP depacketization -> Annex B -> sink

    /// False when the sink is gone.
    fn handle_rtp(&mut self, p: &[u8], sink: &mut impl Write) -> bool {
        if p.len() <= 12 || p[0] >> 6 != 2 {
            return true;
        }
        let mut offset = 12 + usize::from(p[0] & 0x0F) * 4; // fixed header + CSRCs
        if p[0] & 0x10 != 0 {
            // extension header
            if p.len() < offset + 4 {
                return true;
            }
            offset += 4 + (usize::from(p[offset + 2]) << 8 | usize::from(p[offset + 3])) * 4;
        }
        let mut end = p.len();
        if p[0] & 0x20 != 0 {
            end -= usize::from(p[end - 1]); // padding
        }
        if offset >= end {
            return true;
        }
        let marker = p[1] & 0x80 != 0; // last packet of the access unit
        let payload = &p[offset..end];
        let ok = match self.req.codec {
            "hevc" => self.depacketize_hevc(payload, sink),
            _ => self.depacketize_h264(payload, sink),
        };
        // Hand the decoder a complete access unit at once rather than
        // trickling packets through the pipe.
        ok && (!marker || sink.flush().is_ok())
    }

    fn emit(nal: &[u8], sink: &mut impl Write) -> bool {
        if nal.is_empty() {
            return true;
        }
        sink.write_all(&[0, 0, 0, 1]).is_ok() && sink.write_all(nal).is_ok()
    }

    /// RFC 7798: 48 = aggregation packet, 49 = fragmentation unit, else one NAL.
    fn depacketize_hevc(&mut self, p: &[u8], sink: &mut impl Write) -> bool {
        if p.len() < 2 {
            return true;
        }
        match (p[0] >> 1) & 0x3F {
            48 => {
                let mut i = 2;
                while i + 2 <= p.len() {
                    let size = usize::from(p[i]) << 8 | usize::from(p[i + 1]);
                    i += 2;
                    if size == 0 || i + size > p.len() {
                        return true;
                    }
                    if !Self::emit(&p[i..i + size], sink) {
                        return false;
                    }
                    i += size;
                }
                true
            }
            49 => {
                if p.len() < 3 {
                    return true;
                }
                let fu = p[2];
                let (start, end) = (fu & 0x80 != 0, fu & 0x40 != 0);
                if start {
                    self.fu = vec![(p[0] & 0x81) | ((fu & 0x3F) << 1), p[1]];
                }
                if self.fu.is_empty() {
                    return true; // lost the start fragment
                }
                self.fu.extend_from_slice(&p[3..]);
                if end {
                    let nal = std::mem::take(&mut self.fu);
                    return Self::emit(&nal, sink);
                }
                true
            }
            _ => Self::emit(p, sink),
        }
    }

    /// RFC 6184: 24 = STAP-A, 28 = FU-A, else one NAL.
    fn depacketize_h264(&mut self, p: &[u8], sink: &mut impl Write) -> bool {
        if p.is_empty() {
            return true;
        }
        match p[0] & 0x1F {
            24 => {
                let mut i = 1;
                while i + 2 <= p.len() {
                    let size = usize::from(p[i]) << 8 | usize::from(p[i + 1]);
                    i += 2;
                    if size == 0 || i + size > p.len() {
                        return true;
                    }
                    if !Self::emit(&p[i..i + size], sink) {
                        return false;
                    }
                    i += size;
                }
                true
            }
            28 => {
                if p.len() < 2 {
                    return true;
                }
                let fu = p[1];
                let (start, end) = (fu & 0x80 != 0, fu & 0x40 != 0);
                if start {
                    self.fu = vec![(p[0] & 0xE0) | (fu & 0x1F)];
                }
                if self.fu.is_empty() {
                    return true;
                }
                self.fu.extend_from_slice(&p[2..]);
                if end {
                    let nal = std::mem::take(&mut self.fu);
                    return Self::emit(&nal, sink);
                }
                true
            }
            _ => Self::emit(p, sink),
        }
    }

    // MARK: RTSP plumbing

    /// Send one request and read its response: (status code, head).
    fn request(&mut self, method: &str, uri: &str, headers: &[&str]) -> Option<(u32, String)> {
        let n = self.cseq.fetch_add(1, Ordering::SeqCst) + 1;
        let mut msg = format!("{method} {uri} RTSP/1.0\r\nCSeq: {n}\r\n");
        msg += &self.auth_header(method, uri);
        for h in headers {
            msg += h;
            msg += "\r\n";
        }
        msg += "\r\n";
        self.reader
            .as_mut()?
            .get_mut()
            .write_all(msg.as_bytes())
            .ok()?;
        self.read_response()
    }

    /// RFC 2069-style digest, exactly what the Mac sends: no qop, no cnonce.
    fn auth_header(&self, method: &str, uri: &str) -> String {
        if self.realm.is_empty() {
            return String::new();
        }
        let ha1 = md5_hex(&format!(
            "{}:{}:{}",
            self.req.user, self.realm, self.req.password
        ));
        let ha2 = md5_hex(&format!("{method}:{uri}"));
        let response = md5_hex(&format!("{ha1}:{}:{ha2}", self.nonce));
        format!(
            "Authorization: Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", response=\"{response}\"\r\n",
            self.req.user, self.realm, self.nonce
        )
    }

    fn parse_auth_challenge(&mut self, head: &str) -> bool {
        match (quoted(head, "realm=\""), quoted(head, "nonce=\"")) {
            (Some(r), Some(n)) => {
                self.realm = r.to_string();
                self.nonce = n.to_string();
                true
            }
            _ => false,
        }
    }

    fn read_response(&mut self) -> Option<(u32, String)> {
        let head = self.read_head(&[])?;
        let code = head
            .strip_prefix("RTSP/1.0 ")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        Some((code, head))
    }

    /// A text response arriving mid-stream (keepalive reply) whose first 4
    /// bytes were already consumed by the interleave reader.
    fn consume_text_response(&mut self, already: &[u8]) -> Option<()> {
        self.read_head(already).map(|_| ())
    }

    /// Head up to the blank line (with any Content-Length body consumed).
    fn read_head(&mut self, already: &[u8]) -> Option<String> {
        let mut acc = already.to_vec();
        let mut b = [0u8; 1];
        while !acc.ends_with(b"\r\n\r\n") {
            self.read_exact(&mut b).ok()?;
            acc.push(b[0]);
            if acc.len() > 65536 {
                return None;
            }
        }
        let head = String::from_utf8_lossy(&acc).into_owned();
        if let Some(cl) =
            header_value(&head, "Content-Length").and_then(|v| v.parse::<usize>().ok())
            && cl > 0
        {
            let mut body = vec![0u8; cl];
            self.read_exact(&mut body).ok()?;
        }
        Some(head)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        match self.reader.as_mut() {
            Some(r) => r.read_exact(buf), // EOF, error, or the 12 s stall
            None => Err(std::io::ErrorKind::NotConnected.into()),
        }
    }

    fn connect(&self) -> Option<TcpStream> {
        let addr = (self.req.host.as_str(), self.req.port)
            .to_socket_addrs()
            .ok()?
            .find(|a| a.is_ipv4())?;
        TcpStream::connect_timeout(&addr, Duration::from_secs(8)).ok()
    }
}

/// `Name: value` from a response head (case-sensitive, like the Mac's
/// regexes); the value stops at ';' or the line end.
fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    let line = head
        .lines()
        .find(|l| l.starts_with(name) && l[name.len()..].starts_with(':'))?;
    let v = line[name.len() + 1..].trim_start();
    Some(v.split(';').next().unwrap_or(v).trim_end())
}

/// The quoted value after `key` (`realm="…"`).
fn quoted<'a>(head: &'a str, key: &str) -> Option<&'a str> {
    let start = head.find(key)? + key.len();
    let end = head[start..].find('"')?;
    Some(&head[start..start + end])
}

fn md5_hex(s: &str) -> String {
    let d = Md5::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}
