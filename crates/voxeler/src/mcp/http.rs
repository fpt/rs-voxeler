//! Just enough HTTP/1.1 to carry MCP's SSE transport.
//!
//! Two routes and three verbs, hand-rolled on `std::net` for the reason the PNG
//! writer and the 5×7 font are hand-rolled: the alternative is an async runtime
//! and a dependency tree inside a program whose entire architecture is one
//! blocking event loop. What is *not* hand-rolled is the JSON — a parser is the
//! one piece here worth buying.
//!
//! # The transport
//!
//! MCP's HTTP+SSE transport is two halves of one session:
//!
//! ```text
//! GET  /sse                     -> text/event-stream, held open
//!        event: endpoint          the URL to post to, carrying a session id
//!        event: message           every response, pushed down this stream
//! POST /message?sessionId=...   -> 202 Accepted, body is one JSON-RPC request
//! ```
//!
//! The reply to a POST does **not** come back in that POST's response — it is
//! written to the session's open SSE stream. That asymmetry is the whole design:
//! it is what lets the server speak first, and what makes the transport worth
//! having over stdio here at all.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::Sender;
use std::sync::Mutex;

/// A parsed request line and body. Headers are dropped once `Content-Length`
/// has been read off them — nothing else here needs one.
#[derive(Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// One query parameter, without a general parser: the only one this server
    /// reads is the session id.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == name).then_some(v)
        })
    }
}

/// The request line and headers, split from an already-read head.
///
/// Separate from the socket so it can be tested against bytes: the failure this
/// guards is a header parser that works on the requests one client happens to
/// send and falls over on the next one's.
pub fn parse_head(head: &str) -> Option<(HttpRequest, usize)> {
    let mut lines = head.split("\r\n");
    let mut start = lines.next()?.split(' ');
    let method = start.next()?.to_string();
    let target = start.next()?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };

    let mut length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        // Header names are case-insensitive, and clients do differ.
        if name.trim().eq_ignore_ascii_case("content-length") {
            length = value.trim().parse().ok()?;
        }
    }
    Some((
        HttpRequest {
            method,
            path,
            query,
            body: Vec::new(),
        },
        length,
    ))
}

/// Read one request off a connection. `Ok(None)` means the peer hung up
/// cleanly, which is the ordinary end of a connection rather than an error.
pub fn read_request(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<HttpRequest>> {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        // A header line that is not UTF-8 is not a request we can serve; treat
        // it as a closed connection rather than guessing at it.
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::InvalidData => return Ok(None),
            Err(e) => return Err(e),
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        head.push_str(line.trim_end_matches('\n').trim_end_matches('\r'));
        head.push_str("\r\n");
        // A head this long is not a request anyone means to send.
        if head.len() > 64 * 1024 {
            return Ok(None);
        }
    }
    if head.is_empty() {
        return Ok(None);
    }
    let Some((mut req, length)) = parse_head(&head) else {
        return Ok(None);
    };
    if length > 8 * 1024 * 1024 {
        return Ok(None);
    }
    req.body = vec![0; length];
    reader.read_exact(&mut req.body)?;
    Ok(Some(req))
}

/// Loopback only, so the permissive origin costs nothing — and it is what lets
/// the browser-based MCP Inspector talk to this server at all.
const CORS: &str = "Access-Control-Allow-Origin: *\r\n\
                    Access-Control-Allow-Headers: *\r\n\
                    Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n";

pub fn respond(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         {CORS}\
         Connection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// The response head of an SSE stream. No `Content-Length`: the body is the
/// rest of the connection's life.
pub fn open_stream(stream: &mut TcpStream) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         {CORS}\
         Connection: keep-alive\r\n\r\n"
    )?;
    stream.flush()
}

/// One SSE event.
///
/// Every line of the payload gets its own `data:` prefix — a bare newline inside
/// one would otherwise end the event early and split a JSON-RPC message in two.
/// Serialised JSON has no newlines in practice, which is exactly why a bug here
/// would wait for the first message that did.
pub fn frame(event: &str, data: &str) -> String {
    let mut out = format!("event: {event}\n");
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

/// The open SSE streams, by session id.
///
/// A POST arrives on a different connection from the stream its answer goes
/// out on, so the two have to find each other by name. The id is the name.
#[derive(Default)]
pub struct Sessions {
    open: Mutex<HashMap<String, Sender<String>>>,
}

impl Sessions {
    pub fn insert(&self, id: String, tx: Sender<String>) {
        self.lock().insert(id, tx);
    }

    pub fn remove(&self, id: &str) {
        self.lock().remove(id);
    }

    /// Push a message to one session. `false` when that session is gone, which
    /// is an answer with nowhere to go rather than an error.
    pub fn send(&self, id: &str, message: &str) -> bool {
        match self.lock().get(id) {
            Some(tx) => tx.send(message.to_string()).is_ok(),
            None => false,
        }
    }

    pub fn count(&self) -> usize {
        self.lock().len()
    }

    /// A poisoned registry is recovered rather than propagated: one panicking
    /// connection thread must not take the server's other sessions down.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Sender<String>>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A session id, without a random-number dependency.
///
/// Only has to be unguessable enough that two sessions of *this* process do not
/// collide — the server is bound to loopback, so the id is a name, not a secret.
pub fn session_id(counter: u64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:016x}{:04x}", nanos, counter & 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_line_splits_into_path_and_query() {
        let (r, len) =
            parse_head("GET /message?sessionId=abc&x=1 HTTP/1.1\r\nHost: x\r\n").unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/message");
        assert_eq!(r.param("sessionId"), Some("abc"));
        assert_eq!(r.param("x"), Some("1"));
        assert_eq!(r.param("nope"), None);
        assert_eq!(len, 0);
    }

    #[test]
    fn a_path_without_a_query_has_an_empty_one() {
        let (r, _) = parse_head("GET /sse HTTP/1.1\r\n").unwrap();
        assert_eq!(r.path, "/sse");
        assert_eq!(r.query, "");
        assert_eq!(r.param("sessionId"), None);
    }

    /// Header names are case-insensitive and clients genuinely differ, so this
    /// is not hypothetical tidiness.
    #[test]
    fn content_length_is_found_whatever_case_it_arrives_in() {
        for name in ["Content-Length", "content-length", "CONTENT-LENGTH"] {
            let head = format!("POST /message HTTP/1.1\r\n{name}: 42\r\n");
            assert_eq!(parse_head(&head).unwrap().1, 42, "{name}");
        }
    }

    #[test]
    fn a_head_that_is_not_a_request_is_rejected() {
        assert!(parse_head("").is_none());
        assert!(parse_head("GET\r\n").is_none());
    }

    /// The bug this guards: a payload containing a newline would end the event
    /// early and split one JSON-RPC message across two.
    #[test]
    fn every_line_of_a_frame_is_prefixed_and_the_event_is_terminated() {
        assert_eq!(frame("message", "{}"), "event: message\ndata: {}\n\n");
        assert_eq!(
            frame("message", "one\ntwo"),
            "event: message\ndata: one\ndata: two\n\n"
        );
    }

    #[test]
    fn a_session_is_found_by_its_id_until_it_is_removed() {
        let sessions = Sessions::default();
        let (tx, rx) = std::sync::mpsc::channel();
        sessions.insert("abc".into(), tx);
        assert_eq!(sessions.count(), 1);

        assert!(sessions.send("abc", "hello"));
        assert_eq!(rx.recv().unwrap(), "hello");
        assert!(
            !sessions.send("other", "hello"),
            "an unknown session is not an error"
        );

        sessions.remove("abc");
        assert!(!sessions.send("abc", "hello"));
        assert_eq!(sessions.count(), 0);
    }

    /// A dropped stream leaves a sender with no receiver; posting to it has to
    /// report the session as gone rather than panic.
    #[test]
    fn a_closed_stream_reports_its_session_as_gone() {
        let sessions = Sessions::default();
        let (tx, rx) = std::sync::mpsc::channel();
        sessions.insert("abc".into(), tx);
        drop(rx);
        assert!(!sessions.send("abc", "hello"));
    }

    #[test]
    fn session_ids_do_not_collide_within_one_process() {
        let ids: std::collections::HashSet<_> = (0..64).map(session_id).collect();
        assert_eq!(ids.len(), 64);
    }
}
