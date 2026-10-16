//! Minimal blocking HTTP/1.0 client over TcpStream.
//! Used for the Relay notes-email ingest poll and by integration tests.
//! Localhost JSON APIs only — no TLS, no chunked encoding.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub struct HttpResponse {
    pub status: u16,
    #[allow(dead_code)]
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn body_str(&self) -> Result<&str, String> {
        std::str::from_utf8(&self.body).map_err(|e| e.to_string())
    }
    #[allow(dead_code)]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn parse_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url.strip_prefix("http://").ok_or("only http:// urls supported")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rfind(':') {
        Some(i) => {
            let p: u16 = hostport[i + 1..].parse().map_err(|_| "bad port")?;
            (hostport[..i].to_string(), p)
        }
        None => (hostport.to_string(), 80),
    };
    Ok((host, port, path.to_string()))
}

pub fn http_request(method: &str, url: &str, body: Option<&[u8]>, timeout: Duration) -> Result<HttpResponse, String> {
    let (host, port, path) = parse_url(url)?;
    let addr: SocketAddr = format!("{}:{}", host, port)
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("dns resolve failed")?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    let mut req = format!(
        "{} {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n",
        method, path, host
    );
    if let Some(b) = body {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    if let Some(b) = body {
        stream.write_all(b).map_err(|e| e.to_string())?;
    }
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("bad http response")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.lines();
    let status_line = lines.next().ok_or("empty response")?;
    let status: u16 = status_line.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    let mut headers = Vec::new();
    for line in lines {
        if let Some(i) = line.find(':') {
            headers.push((line[..i].trim().to_string(), line[i + 1..].trim().to_string()));
        }
    }
    Ok(HttpResponse { status, headers, body: raw[split + 4..].to_vec() })
}

pub fn http_get(url: &str, timeout: Duration) -> Result<HttpResponse, String> {
    http_request("GET", url, None, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_url_basic() {
        let (h, p, path) = parse_url("http://127.0.0.1:3006/api/x?since=1").unwrap();
        assert_eq!((h.as_str(), p, path.as_str()), ("127.0.0.1", 3006, "/api/x?since=1"));
        let (h2, p2, path2) = parse_url("http://localhost/").unwrap();
        assert_eq!((h2.as_str(), p2, path2.as_str()), ("localhost", 80, "/"));
    }
}
