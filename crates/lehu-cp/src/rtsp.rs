//! RTSP-style framing on the control channel: a request line, headers, an
//! optional body sized by Content-Length. Requests are cut out of a stream
//! buffer as they complete; a partial one waits for more bytes. The
//! encrypted framing around it lives in `cipher`.

use std::fmt::Write as _;

/// A request as parsed; header names are lower-cased.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub protocol: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The path without its query string.
    pub fn path_only(&self) -> &str {
        self.path.split('?').next().unwrap_or("")
    }

    /// The phone's replies to our own requests arrive on the same stream as
    /// its requests: a status line looks like a method that starts with the
    /// protocol name.
    pub fn is_response(&self) -> bool {
        self.method.starts_with("RTSP/") || self.method.starts_with("HTTP/")
    }
}

#[derive(Debug, Default)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn status(code: u16) -> Self {
        Self {
            status: code,
            ..Self::ok()
        }
    }

    pub fn with_body(mut self, content_type: &'static str, body: Vec<u8>) -> Self {
        self.headers
            .push(("Content-Type", content_type.to_string()));
        self.body = body;
        self
    }
}

/// The largest body accepted: a GET /info or a SETUP is kilobytes; a
/// Content-Length beyond this is not a message, and the parser says so
/// instead of waiting for megabytes that never come.
pub const MAX_BODY: usize = 1 << 20;

fn header_end(buf: &[u8], from: usize) -> Option<usize> {
    buf.get(from..)?
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + from)
}

/// The outcome of a parse step.
pub enum Parsed {
    /// A request, and how many bytes of the buffer it used.
    Request(Request, usize),
    /// More bytes are needed.
    Incomplete,
    /// The bytes are not a message; the connection should end.
    Malformed,
}

/// Parse one request from the front of `buf`.
pub fn parse_one(buf: &[u8]) -> Parsed {
    let Some(end) = header_end(buf, 0) else {
        return if buf.len() > 64 * 1024 {
            Parsed::Malformed
        } else {
            Parsed::Incomplete
        };
    };
    let Ok(head) = std::str::from_utf8(&buf[..end]) else {
        return Parsed::Malformed;
    };
    let mut lines = head.split("\r\n");
    let mut req = Request::default();
    if let Some(first) = lines.next() {
        let mut words = first.split_whitespace();
        req.method = words.next().unwrap_or("").to_string();
        req.path = words.next().unwrap_or("").to_string();
        req.protocol = words.next().unwrap_or("").to_string();
    }
    if req.method.is_empty() {
        return Parsed::Malformed;
    }
    if req.protocol.is_empty() {
        req.protocol = "RTSP/1.0".to_string();
    }
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            req.headers
                .push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let length = match req.header("content-length") {
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n <= MAX_BODY => n,
            _ => return Parsed::Malformed,
        },
        None => 0,
    };
    let body_start = end + 4;
    let Some(body_end) = body_start.checked_add(length) else {
        return Parsed::Malformed;
    };
    if body_end > buf.len() {
        return Parsed::Incomplete;
    }
    req.body = buf[body_start..body_end].to_vec();
    Parsed::Request(req, body_end)
}

/// Every complete request in `buf`, which keeps the unconsumed rest.
/// `None` when the stream is malformed.
pub fn parse(buf: &mut Vec<u8>) -> Option<Vec<Request>> {
    let mut out = Vec::new();
    let mut off = 0;
    loop {
        match parse_one(&buf[off..]) {
            Parsed::Request(r, used) => {
                out.push(r);
                off += used;
            }
            Parsed::Incomplete => break,
            Parsed::Malformed => return None,
        }
    }
    buf.drain(..off);
    Some(out)
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

/// A response to `req`: its protocol and CSeq echoed, Content-Length set.
pub fn build(req: &Request, res: &Response) -> Vec<u8> {
    let mut head = String::with_capacity(128);
    let _ = write!(
        head,
        "{} {} {}\r\n",
        req.protocol,
        res.status,
        status_text(res.status)
    );
    for (k, v) in &res.headers {
        let _ = write!(head, "{k}: {v}\r\n");
    }
    if let Some(cseq) = req.header("cseq") {
        let _ = write!(head, "CSeq: {cseq}\r\n");
    }
    let _ = write!(head, "Content-Length: {}\r\n\r\n", res.body.len());
    let mut out = head.into_bytes();
    out.extend_from_slice(&res.body);
    out
}

/// A request of ours (the event channel's reverse HTTP).
pub fn build_request(
    method: &str,
    path: &str,
    cseq: u32,
    content_type: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut head = String::with_capacity(160);
    let _ = write!(
        head,
        "{method} {path} RTSP/1.0\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCSeq: {cseq}\r\n\r\n",
        body.len()
    );
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests_across_chunks() {
        let msg = b"POST /pair-setup RTSP/1.0\r\nCSeq: 3\r\nContent-Length: 4\r\nContent-Type: x\r\n\r\nabcdGET /info RTSP/1.0\r\nCSeq: 4\r\n\r\n";
        let mut buf = msg[..20].to_vec();
        assert!(parse(&mut buf).unwrap().is_empty());
        buf.extend_from_slice(&msg[20..]);
        let reqs = parse(&mut buf).unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/pair-setup");
        assert_eq!(reqs[0].header("cseq"), Some("3"));
        assert_eq!(reqs[0].body, b"abcd");
        assert_eq!(reqs[1].method, "GET");
        assert!(reqs[1].body.is_empty());
        assert!(buf.is_empty());
    }

    #[test]
    fn body_waits_and_bad_lengths_are_refused() {
        let mut buf = b"SETUP rtsp://x/y?z RTSP/1.0\r\nContent-Length: 10\r\n\r\nabc".to_vec();
        assert!(parse(&mut buf).unwrap().is_empty());
        assert_eq!(buf.len(), 54);
        let mut bad = b"SETUP / RTSP/1.0\r\nContent-Length: 99999999999\r\n\r\n".to_vec();
        assert!(parse(&mut bad).is_none());
        let mut bad = b"SETUP / RTSP/1.0\r\nContent-Length: -1\r\n\r\n".to_vec();
        assert!(parse(&mut bad).is_none());
        let mut bad = b"\r\n\r\n".to_vec();
        assert!(parse(&mut bad).is_none());
        let mut bad = vec![0xff; 70_000];
        assert!(parse(&mut bad).is_none());
        let mut bad = b"GET /x RTSP/1.0\r\nfoo\xff\r\n\r\n".to_vec();
        assert!(parse(&mut bad).is_none());
    }

    #[test]
    fn builds_responses_and_requests() {
        let mut buf = b"GET /info RTSP/1.0\r\nCSeq: 7\r\n\r\n".to_vec();
        let req = parse(&mut buf).unwrap().remove(0);
        assert_eq!(req.path_only(), "/info");
        assert!(!req.is_response());
        let out = build(
            &req,
            &Response::ok().with_body("text/plain", b"hi".to_vec()),
        );
        assert_eq!(
            out,
            b"RTSP/1.0 200 OK\r\nContent-Type: text/plain\r\nCSeq: 7\r\nContent-Length: 2\r\n\r\nhi"
        );
        let out = build_request("POST", "/command", 1, "x", b"ab");
        assert!(out.starts_with(
            b"POST /command RTSP/1.0\r\nContent-Type: x\r\nContent-Length: 2\r\nCSeq: 1\r\n\r\nab"
        ));
        let mut buf = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n".to_vec();
        assert!(parse(&mut buf).unwrap()[0].is_response());
    }
}
