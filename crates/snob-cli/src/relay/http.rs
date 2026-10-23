//! Just enough HTTP/1.1 for one page, its two assets, an event stream and the
//! input it posts back.
//!
//! Hand-written rather than a server crate: the whole surface is five routes
//! on a connection only this user's tunnel reaches, and the parsing that
//! matters is the refusing — a head too long, a body too long, a `Host` that
//! is not a loopback name. A request this does not understand is answered 400
//! and the connection is closed.

use std::collections::HashMap;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The longest request head taken. A browser's is a few hundred bytes.
const MOST_HEAD: usize = 16 * 1024;

/// The longest body taken: a batch of input events, which the page keeps
/// small, or a paste.
pub(super) const MOST_BODY: usize = 64 * 1024;

/// One request, as far as this server reads it.
#[derive(Debug)]
pub(super) struct Request {
    pub method: String,
    /// The path, without the query.
    pub path: String,
    /// Header names in lower case; a repeated header keeps its last value.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// The value of the cookie `name`, if the request carries it.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.header("cookie")?.split(';').find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then_some(value)
        })
    }

    /// Whether the connection is to be kept for another request.
    pub fn keeps_alive(&self) -> bool {
        !self
            .header("connection")
            .is_some_and(|c| c.eq_ignore_ascii_case("close"))
    }
}

/// Why a request was not read.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Unread {
    /// The other end closed before a request began: the normal end of a
    /// connection kept alive.
    Closed,
    /// Something that is not a request this server takes.
    Bad,
}

/// Reads the next request off `stream`. `buffered` holds what was read past
/// the end of the previous one.
pub(super) async fn read<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffered: &mut Vec<u8>,
) -> Result<Request, Unread> {
    let head_end = loop {
        if let Some(end) = find(buffered, b"\r\n\r\n") {
            break end;
        }
        if buffered.len() > MOST_HEAD {
            return Err(Unread::Bad);
        }
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).await.map_err(|_| Unread::Closed)?;
        if read == 0 {
            return Err(if buffered.is_empty() {
                Unread::Closed
            } else {
                Unread::Bad
            });
        }
        buffered.extend_from_slice(&chunk[..read]);
    };
    let head = std::str::from_utf8(&buffered[..head_end]).map_err(|_| Unread::Bad)?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().ok_or(Unread::Bad)?.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (first.next(), first.next(), first.next(), first.next())
    else {
        return Err(Unread::Bad);
    };
    if !version.starts_with("HTTP/1.") {
        return Err(Unread::Bad);
    }
    let path = target.split('?').next().unwrap_or_default().to_string();
    let method = method.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(Unread::Bad)?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    // A chunked body is never sent by the page, and refusing it is simpler
    // than reading it.
    if headers.contains_key("transfer-encoding") {
        return Err(Unread::Bad);
    }
    let length = match headers.get("content-length") {
        Some(length) => length.parse::<usize>().map_err(|_| Unread::Bad)?,
        None => 0,
    };
    if length > MOST_BODY {
        return Err(Unread::Bad);
    }
    buffered.drain(..head_end + 4);
    while buffered.len() < length {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).await.map_err(|_| Unread::Bad)?;
        if read == 0 {
            return Err(Unread::Bad);
        }
        buffered.extend_from_slice(&chunk[..read]);
    }
    let body = buffered.drain(..length).collect();
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Whether `host`, a `Host` header, names this machine's loopback: what the
/// tunnel's near end is reached by. Anything else is a page on some other
/// name that resolved here, which is how a site the person has open would
/// reach the relay (DNS rebinding), and is refused.
pub(super) fn is_loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(name, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) {
                name
            } else {
                host
            }
        }),
    };
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "::1"
}

/// The headers every answer carries: nothing is kept, nothing is told where
/// it came from, nothing frames it, and it runs only what it serves itself.
const COMMON: &str = "Cache-Control: no-store\r\n\
     Referrer-Policy: no-referrer\r\n\
     X-Content-Type-Options: nosniff\r\n\
     X-Frame-Options: DENY\r\n\
     Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; \
     img-src data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; \
     form-action 'none'\r\n";

/// Writes a whole answer.
pub(super) async fn respond<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: &str,
    content_type: &str,
    extra: &[(&str, &str)],
    body: &[u8],
    keep_alive: bool,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\n{COMMON}Content-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: {}\r\n",
        body.len(),
        if keep_alive { "keep-alive" } else { "close" }
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}

/// Writes the head of an event stream, which then stays open.
pub(super) async fn start_events<S: AsyncWrite + Unpin>(stream: &mut S) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\n{COMMON}Content-Type: text/event-stream\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await
}

/// One event of an event stream. `data` must hold no line break, which JSON
/// and base64 never do.
pub(super) fn event(name: &str, data: &str) -> String {
    debug_assert!(!data.contains('\n'));
    format!("event: {name}\ndata: {data}\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(raw: &[u8]) -> Result<Request, Unread> {
        let mut stream = raw;
        read(&mut stream, &mut Vec::new()).await
    }

    #[tokio::test]
    async fn a_request_and_its_body_are_read() {
        let request = parse(
            b"POST /input?x=1 HTTP/1.1\r\nHost: 127.0.0.1:8765\r\nContent-Length: 4\r\n\
              Cookie: a=1; snob=secret\r\n\r\n[{}]",
        )
        .await
        .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/input");
        assert_eq!(request.header("host"), Some("127.0.0.1:8765"));
        assert_eq!(request.cookie("snob"), Some("secret"));
        assert_eq!(request.cookie("b"), None);
        assert_eq!(request.body, b"[{}]");
        assert!(request.keeps_alive());
    }

    /// Two requests on one connection, the second already in the buffer.
    #[tokio::test]
    async fn requests_kept_alive_are_read_one_after_another() {
        let raw: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n\
                           GET /page.js HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let mut stream = raw;
        let mut buffered = Vec::new();
        let first = read(&mut stream, &mut buffered).await.unwrap();
        let second = read(&mut stream, &mut buffered).await.unwrap();
        assert_eq!(first.path, "/");
        assert_eq!(second.path, "/page.js");
        assert!(!second.keeps_alive());
        assert_eq!(
            read(&mut stream, &mut buffered).await.unwrap_err(),
            Unread::Closed
        );
    }

    #[tokio::test]
    async fn what_is_not_a_request_is_refused() {
        for raw in [
            &b"GET /\r\n\r\n"[..],
            b"GET / HTTP/2\r\n\r\n",
            b"GET / HTTP/1.1\r\nno colon here\r\n\r\n",
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: 999999999\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nshort",
            b"GET / HTTP/1.1\r\nHost: half a head",
        ] {
            assert_eq!(
                parse(raw).await.unwrap_err(),
                Unread::Bad,
                "{}",
                String::from_utf8_lossy(raw)
            );
        }
        let endless = vec![b'a'; MOST_HEAD + 8192];
        assert_eq!(parse(&endless).await.unwrap_err(), Unread::Bad);
    }

    /// Only the loopback names the tunnel's near end is reached by, with or
    /// without a port. A name that merely resolves here is a page on another
    /// site trying its luck.
    #[test]
    fn only_a_loopback_host_is_taken() {
        for host in [
            "127.0.0.1:8765",
            "127.0.0.1",
            "localhost:1",
            "LOCALHOST",
            "[::1]:8765",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in [
            "evil.example:8765",
            "127.0.0.1.evil.example",
            "localhost.evil.example:80",
            "",
            "[::2]:80",
        ] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }
}
