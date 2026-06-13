//! A small blocking HTTP/1.1 client: just enough to probe an image URL and
//! pull byte ranges from it. Pure Rust throughout — TLS is rustls with the
//! RustCrypto provider (see [`super::tls`]), so the static build needs no C
//! toolchain.
//!
//! Supports `Range` requests (`206` / `Content-Range`), redirects
//! (301/302/303/307/308, capped), and both `Content-Length` and
//! `Transfer-Encoding: chunked` response bodies. Connections are one-shot
//! (no keep-alive): simple, and fine for the handful of fetches an analysis
//! needs.

use super::remote::{Probe, RangeFetcher};
use super::tls;
use crate::{Error, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;

const MAX_REDIRECTS: usize = 5;
// Only the connect+headers phase retries here; a chunk's body read fails on
// its own timeout (no retry), so a stalled download still aborts in ~1×
// timeout while transient connect blips get a few more tries.
const MAX_RETRIES: usize = 3;
const USER_AGENT: &str = concat!("lbx/", env!("CARGO_PKG_VERSION"));

/// A `RangeFetcher` over one http(s) URL. After [`probe`](RangeFetcher::probe)
/// resolves redirects, fetches start from the final URL.
pub struct HttpFetcher {
    start: Url,
    resolved: Mutex<Url>,
}

impl HttpFetcher {
    pub fn new(url: &str) -> Result<HttpFetcher> {
        let parsed = Url::parse(url)?;
        Ok(HttpFetcher { start: parsed.clone(), resolved: Mutex::new(parsed) })
    }
}

impl RangeFetcher for HttpFetcher {
    fn probe(&self) -> Result<Probe> {
        // A 1-byte range request distinguishes range support cleanly: a
        // server that honours it answers 206 + Content-Range/<total>; one
        // that ignores it answers 200 + Content-Length for the whole body.
        let resp = request(&self.start, Some((0, Some(0))))?;
        *self.resolved.lock().unwrap() = resp.final_url.clone();
        match resp.status {
            206 => {
                let total = resp
                    .header("content-range")
                    .and_then(parse_content_range_total)
                    .ok_or_else(|| Error::Http("206 without a usable Content-Range".into()))?;
                Ok(Probe { size: total, supports_range: true })
            }
            200 => {
                let size = resp
                    .header("content-length")
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .ok_or_else(|| Error::Http("200 without a Content-Length".into()))?;
                Ok(Probe { size, supports_range: false })
            }
            s => Err(Error::Http(format!("unexpected status {s} probing {}", self.start.display()))),
        }
    }

    fn fetch_range(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        let url = self.resolved.lock().unwrap().clone();
        let resp = request(&url, Some((start, Some(end - 1))))?;
        if resp.status != 206 {
            return Err(Error::Http(format!(
                "expected 206 for range {start}-{}, got {}",
                end - 1,
                resp.status
            )));
        }
        let want = (end - start) as usize;
        let mut buf = vec![0u8; want];
        read_full(&mut resp.into_body(), &mut buf)?;
        Ok(buf)
    }

    fn fetch_all(&self) -> Result<Box<dyn Read + Send>> {
        let url = self.resolved.lock().unwrap().clone();
        let resp = request(&url, None)?;
        if resp.status != 200 {
            return Err(Error::Http(format!("expected 200 streaming body, got {}", resp.status)));
        }
        Ok(Box::new(resp.into_body()))
    }
}

// --- URL ---

#[derive(Clone)]
struct Url {
    https: bool,
    host: String,
    port: u16,
    /// Path plus optional query, always starting with `/`.
    path: String,
}

impl Url {
    fn parse(s: &str) -> Result<Url> {
        let (https, rest) = if let Some(r) = s.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = s.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(Error::Http(format!("not an http(s) URL: {s}")));
        };
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let authority = authority.rsplit('@').next().unwrap_or(authority); // drop userinfo
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (
                h,
                p.parse().map_err(|_| Error::Http(format!("bad port in {s}")))?,
            ),
            _ => (authority, if https { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(Error::Http(format!("missing host in URL: {s}")));
        }
        // A '#' fragment is never sent to the server.
        let path = match path.split_once('#') {
            Some((p, _)) => p,
            None => path,
        };
        Ok(Url {
            https,
            host: host.to_string(),
            port,
            path: if path.is_empty() { "/".into() } else { path.to_string() },
        })
    }

    /// Resolve a `Location` header (absolute URL or path) against this URL.
    fn join(&self, location: &str) -> Result<Url> {
        if super::is_url(location) {
            return Url::parse(location);
        }
        let mut next = self.clone();
        if let Some(stripped) = location.strip_prefix('/') {
            next.path = format!("/{stripped}");
        } else {
            let dir = match self.path.rfind('/') {
                Some(i) => &self.path[..=i],
                None => "/",
            };
            next.path = format!("{dir}{location}");
        }
        Ok(next)
    }

    fn display(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}:{}{}", self.host, self.port, self.path)
    }
}

// --- request / response ---

/// Any Read+Write transport (plain TCP or a rustls TLS stream).
trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    final_url: Url,
    reader: BufReader<Box<dyn Stream>>,
    framing: Framing,
}

#[derive(Clone, Copy)]
enum Framing {
    Length(u64),
    Chunked,
    /// Read until the connection closes (no length, not chunked).
    ToEnd,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn into_body(self) -> BodyReader {
        BodyReader { reader: self.reader, framing: self.framing, left: match self.framing {
            Framing::Length(n) => n,
            _ => 0,
        }, chunk_left: 0, done: false }
    }
}

/// As [`request_once`], retrying a few times on transient network errors
/// (connect/resolve/read failures — common when hammering a CDN in parallel).
/// An HTTP error *status* is a successful `Resp`, so it isn't retried here.
fn request(url: &Url, range: Option<(u64, Option<u64>)>) -> Result<Resp> {
    let mut attempt = 0;
    loop {
        match request_once(url, range) {
            Ok(resp) => return Ok(resp),
            Err(_) if attempt < MAX_RETRIES => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(300 * attempt as u64));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Connect, send one GET (optionally ranged), follow redirects, and return
/// the response positioned at the start of the body.
fn request_once(url: &Url, range: Option<(u64, Option<u64>)>) -> Result<Resp> {
    let mut current = url.clone();
    for _ in 0..=MAX_REDIRECTS {
        let mut reader = BufReader::new(connect(&current)?);
        send_request(reader.get_mut(), &current, range)?;
        let (status, headers) = read_head(&mut reader)?;

        if matches!(status, 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = header_of(&headers, "location")
        {
            current = current.join(loc)?;
            continue; // new connection, re-send (with the same Range)
        }
        let framing = framing_of(&headers, status);
        return Ok(Resp { status, headers, final_url: current, reader, framing });
    }
    Err(Error::Http(format!("too many redirects from {}", url.display())))
}

fn connect(url: &Url) -> Result<Box<dyn Stream>> {
    // Resolve via our own resolver (the system one is dead on static-musl
    // Android), then try each address. TLS still uses the hostname for SNI
    // and cert checks even though we dial by IP.
    let timeout = super::http_timeout();
    let ips = super::dns::resolve(&url.host)?;
    let mut tcp = None;
    let mut last_err = None;
    for ip in ips {
        match TcpStream::connect_timeout(&std::net::SocketAddr::new(ip, url.port), timeout) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let tcp = tcp.ok_or_else(|| {
        Error::Http(format!(
            "connect {}:{}{}",
            url.host,
            url.port,
            last_err.map(|e| format!(": {e}")).unwrap_or_default()
        ))
    })?;
    tcp.set_read_timeout(Some(timeout)).ok();
    tcp.set_write_timeout(Some(timeout)).ok();
    if url.https {
        Ok(Box::new(tls::connect(tcp, &url.host)?))
    } else {
        Ok(Box::new(tcp))
    }
}

fn send_request(stream: &mut dyn Write, url: &Url, range: Option<(u64, Option<u64>)>) -> Result<()> {
    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {USER_AGENT}\r\nAccept: */*\r\nConnection: close\r\n",
        url.path, url.host
    );
    if let Some((start, end)) = range {
        match end {
            Some(end) => req.push_str(&format!("Range: bytes={start}-{end}\r\n")),
            None => req.push_str(&format!("Range: bytes={start}-\r\n")),
        }
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).map_err(|e| Error::Http(format!("send: {e}")))?;
    stream.flush().map_err(|e| Error::Http(format!("send: {e}")))?;
    Ok(())
}

/// Read the status line + headers, leaving `reader` at the body.
fn read_head(reader: &mut BufReader<Box<dyn Stream>>) -> Result<(u16, Vec<(String, String)>)> {
    let line = read_line(reader)?;
    // "HTTP/1.1 206 Partial Content"
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| Error::Http(format!("bad status line: {line:?}")))?;
    let mut headers = Vec::new();
    loop {
        let line = read_line(reader)?;
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

/// Read one CRLF-terminated header line as a String (CRLF stripped).
fn read_line(reader: &mut BufReader<Box<dyn Stream>>) -> Result<String> {
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).map_err(|e| Error::Http(format!("read header: {e}")))?;
    if line.is_empty() {
        return Err(Error::Http("connection closed mid-header".into()));
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

fn framing_of(headers: &[(String, String)], status: u16) -> Framing {
    if header_of(headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        Framing::Chunked
    } else if let Some(len) = header_of(headers, "content-length").and_then(|v| v.trim().parse().ok())
    {
        Framing::Length(len)
    } else if status == 204 || status == 304 {
        Framing::Length(0)
    } else {
        Framing::ToEnd
    }
}

/// `bytes 0-0/12345` → `12345`.
fn parse_content_range_total(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse().ok()
}

// --- body decoding ---

/// Decodes a response body per its framing into a plain byte stream.
struct BodyReader {
    reader: BufReader<Box<dyn Stream>>,
    framing: Framing,
    /// Remaining bytes for `Length`.
    left: u64,
    /// Remaining bytes in the current chunk for `Chunked`.
    chunk_left: u64,
    done: bool,
}

impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        match self.framing {
            Framing::Length(_) => {
                if self.left == 0 {
                    self.done = true;
                    return Ok(0);
                }
                let want = (buf.len() as u64).min(self.left) as usize;
                let n = self.reader.read(&mut buf[..want])?;
                if n == 0 {
                    self.done = true;
                }
                self.left -= n as u64;
                Ok(n)
            }
            Framing::ToEnd => {
                let n = self.reader.read(buf)?;
                if n == 0 {
                    self.done = true;
                }
                Ok(n)
            }
            Framing::Chunked => self.read_chunked(buf),
        }
    }
}

impl BodyReader {
    fn read_chunked(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.chunk_left == 0 {
            // Read the chunk-size line (skip a trailing CRLF from a prior chunk).
            let mut line = String::new();
            loop {
                line.clear();
                read_crlf_line(&mut self.reader, &mut line)?;
                if !line.is_empty() {
                    break;
                }
            }
            let size_hex = line.split(';').next().unwrap_or("").trim();
            let size = u64::from_str_radix(size_hex, 16)
                .map_err(|_| std::io::Error::other(format!("bad chunk size {size_hex:?}")))?;
            if size == 0 {
                self.done = true;
                return Ok(0);
            }
            self.chunk_left = size;
        }
        let want = (buf.len() as u64).min(self.chunk_left) as usize;
        let n = self.reader.read(&mut buf[..want])?;
        self.chunk_left -= n as u64;
        Ok(n)
    }
}

fn read_crlf_line(reader: &mut BufReader<Box<dyn Stream>>, out: &mut String) -> std::io::Result<()> {
    let mut raw = Vec::new();
    reader.read_until(b'\n', &mut raw)?;
    while matches!(raw.last(), Some(b'\n' | b'\r')) {
        raw.pop();
    }
    out.push_str(&String::from_utf8_lossy(&raw));
    Ok(())
}

fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Err(Error::Http("body ended before expected length".into())),
            Ok(n) => filled += n,
            Err(e) => return Err(Error::Http(format!("read body: {e}"))),
        }
    }
    Ok(())
}
