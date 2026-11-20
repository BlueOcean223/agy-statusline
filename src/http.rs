//! Minimal HTTP/1.1 client for loopback services (language_server Connect API, CDP `/json/list`).
//! Only plain HTTP is supported; both targets listen on 127.0.0.1 without TLS.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

pub fn request(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> std::io::Result<Response> {
    // Only loopback addresses are used here, where connecting succeeds or is refused at once, so no
    // connect timeout is needed. `connect` tries each address `localhost` resolves to in turn.
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut req =
        format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes())?;
    stream.write_all(body)?;

    // Read until the body is complete instead of waiting for EOF: Chrome's DevTools HTTP server
    // ignores `Connection: close` and keeps the socket open after responding.
    let mut raw = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if is_complete(&raw) {
                    break;
                }
            }
            Err(e) if raw.is_empty() => return Err(e),
            Err(_) => break,
        }
    }
    parse_response(&raw)
}

fn is_complete(raw: &[u8]) -> bool {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else { return false };
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let body = &raw[split + 4..];
    for line in head.split("\r\n") {
        if let Some(v) = line.strip_prefix("content-length:") {
            return v.trim().parse::<usize>().is_ok_and(|n| body.len() >= n);
        }
        if line.starts_with("transfer-encoding:") && line.contains("chunked") {
            return body.ends_with(b"0\r\n\r\n");
        }
    }
    false
}

fn parse_response(raw: &[u8]) -> std::io::Result<Response> {
    let split =
        raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| std::io::Error::other("no header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.split("\r\n");
    let status = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse().ok()).unwrap_or(0);
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let rest = &raw[split + 4..];
    let body = if chunked { dechunk(rest) } else { rest.to_vec() };
    Ok(Response { status, body })
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    while let Some(eol) = data.windows(2).position(|w| w == b"\r\n") {
        let size_str = String::from_utf8_lossy(&data[..eol]);
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        data = &data[eol + 2..];
        if size == 0 || data.len() < size {
            out.extend_from_slice(&data[..size.min(data.len())]);
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = data.get(size + 2..).unwrap_or(&[]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chunked_body() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"abcde");
    }

    #[test]
    fn detects_complete_body() {
        assert!(is_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"));
        assert!(!is_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nok"));
        assert!(is_complete(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n"));
    }

    #[test]
    fn parses_plain_body() {
        let raw = b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\nno";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 404);
        assert_eq!(r.body, b"no");
    }
}
