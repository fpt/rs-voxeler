//! `voxeler --mcp` — the editor as an MCP server over SSE.
//!
//! # Why SSE and not stdio
//!
//! `kessel mcp` speaks stdio, because a fantasy console an agent is debugging
//! needs no window. This is the opposite case: the point of driving a *model
//! editor* from an agent is that a person can watch the model being built and
//! say "no, not like that". Stdio would own the terminal and give the agent a
//! process of its own; SSE is a socket, so the editor is an ordinary window that
//! happens to also be listening.
//!
//! # One editor, two drivers
//!
//! The agent and the user share one `Editor`, one undo history and one file.
//! They do not share it through a lock: the HTTP threads put [`Job`]s on a
//! channel, and the **event loop** runs them between frames, against the editor
//! it already owns.
//!
//! That is deliberate. A `Mutex<Editor>` would work, but it would put a lock
//! around every field the window layer touches, and leave open the question of
//! what a tool call does mid-frame. A queue answers it: a tool call happens
//! between two frames, never inside one, and the editor stays the
//! single-threaded thing every other module here assumes it is.
//!
//! # Loopback only
//!
//! The listener binds `127.0.0.1`, never `0.0.0.0`. This hands arbitrary
//! control of the user's document to whoever connects, and that must not be
//! reachable off-box.

mod http;
pub mod session;
pub mod stdio;
mod tools;
mod wire;

pub use stdio::serve_stdio;
pub use tools::Roots;

use std::io::{BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use serde_json::{json, Value};

use crate::editor::Editor;
use http::{HttpRequest, Sessions};
use wire::{
    CallParams, CallResult, Request, Response, INTERNAL_ERROR, METHOD_NOT_FOUND, PARSE_ERROR,
};

/// How long a connection thread waits for the event loop to run its tool call.
///
/// Long enough that a big fill on a large volume finishes, short enough that a
/// closed window answers the agent instead of hanging it forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// How often an idle SSE stream sends a comment. Nothing on loopback needs the
/// keep-alive, but it is also how a stream notices its client has gone: the
/// write fails, and the thread cleans the session up instead of leaking.
const PING_EVERY: Duration = Duration::from_secs(15);

/// One tool call waiting for the event loop.
struct Job {
    call: CallParams,
    reply: SyncSender<CallResult>,
}

/// The editor thread's end of the bridge.
pub struct Bridge {
    jobs: Receiver<Job>,
}

impl Bridge {
    /// Run every queued tool call against the editor, and report whether any
    /// did — which is the window's cue to redraw.
    ///
    /// Non-blocking: called from the event loop, which must not stop pumping
    /// because no agent happens to be connected.
    pub fn drain(&self, editor: &mut Editor) -> bool {
        let mut any = false;
        while let Ok(job) = self.jobs.try_recv() {
            let result = tools::call(editor, &job.call.name, &job.call.arguments);
            // A dropped receiver means the connection went away mid-call. The
            // edit stands — it was applied to the user's document, and undoing
            // it because nobody was listening would be worse.
            let _ = job.reply.send(result);
            any = true;
        }
        any
    }
}

/// Start the server. Returns the bridge for the event loop to drain, and the
/// address to tell the user about.
///
/// `wake` is called whenever a job is queued, so the event loop stops waiting
/// on window events and comes to look. Without it an idle editor would sit in
/// `ControlFlow::Wait` and the agent's call would land only on the next mouse
/// move.
pub fn serve_sse(
    port: u16,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> std::io::Result<(Bridge, SocketAddr)> {
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))?;
    let addr = listener.local_addr()?;
    let (tx, rx) = mpsc::channel();
    let sessions = Arc::new(Sessions::default());

    std::thread::spawn(move || {
        let mut counter = 0u64;
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            counter += 1;
            let ctx = Context {
                jobs: tx.clone(),
                sessions: sessions.clone(),
                wake: wake.clone(),
                addr,
                counter,
            };
            std::thread::spawn(move || {
                if let Err(e) = ctx.serve_connection(stream) {
                    // A client that hangs up mid-request is ordinary, not news.
                    if e.kind() != std::io::ErrorKind::BrokenPipe {
                        eprintln!("voxeler mcp: connection ended: {e}");
                    }
                }
            });
        }
    });

    Ok((Bridge { jobs: rx }, addr))
}

/// What one connection thread needs to serve a request.
struct Context {
    jobs: mpsc::Sender<Job>,
    sessions: Arc<Sessions>,
    wake: Arc<dyn Fn() + Send + Sync>,
    addr: SocketAddr,
    counter: u64,
}

impl Context {
    fn serve_connection(&self, stream: TcpStream) -> std::io::Result<()> {
        stream.set_nodelay(true).ok();
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut stream = stream;
        let Some(req) = http::read_request(&mut reader)? else {
            return Ok(());
        };

        match (req.method.as_str(), req.path.as_str()) {
            // The inspector is a browser page, and a browser preflights.
            ("OPTIONS", _) => http::respond(&mut stream, "204 No Content", ""),
            ("GET", "/sse") => self.open_session(stream),
            // Both spellings are in the wild; serving one and 404ing the other
            // is a session that connects and then silently never works.
            ("POST", "/message") | ("POST", "/messages") => self.take_message(&req, &mut stream),
            _ => http::respond(
                &mut stream,
                "404 Not Found",
                r#"{"error":"try GET /sse or POST /message"}"#,
            ),
        }
    }

    /// Hold an SSE stream open, writing whatever the session's POSTs produce.
    fn open_session(&self, mut stream: TcpStream) -> std::io::Result<()> {
        let id = http::session_id(self.counter);
        let (tx, rx) = mpsc::channel::<String>();
        self.sessions.insert(id.clone(), tx);
        eprintln!(
            "voxeler mcp: session {id} opened ({} now connected)",
            self.sessions.count()
        );

        let result = (|| -> std::io::Result<()> {
            http::open_stream(&mut stream)?;
            // The first thing a client must be told: where to post. Absolute,
            // because a relative path leaves the client guessing at the scheme
            // and port it already connected on.
            let endpoint = format!("http://{}/message?sessionId={id}", self.addr);
            stream.write_all(http::frame("endpoint", &endpoint).as_bytes())?;
            stream.flush()?;

            loop {
                match rx.recv_timeout(PING_EVERY) {
                    Ok(message) => {
                        stream.write_all(http::frame("message", &message).as_bytes())?;
                        stream.flush()?;
                    }
                    // A comment, which SSE ignores — but the *write* is the
                    // point: it is how a stream finds out its client is gone.
                    Err(RecvTimeoutError::Timeout) => {
                        stream.write_all(b": ping\n\n")?;
                        stream.flush()?;
                    }
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
            }
        })();

        self.sessions.remove(&id);
        eprintln!(
            "voxeler mcp: session {id} closed ({} still connected)",
            self.sessions.count()
        );
        result
    }

    /// Take one JSON-RPC request, answer it down the session's SSE stream, and
    /// acknowledge the POST itself with 202.
    fn take_message(&self, req: &HttpRequest, stream: &mut TcpStream) -> std::io::Result<()> {
        let Some(session) = req.param("sessionId").map(str::to_string) else {
            return http::respond(
                stream,
                "400 Bad Request",
                r#"{"error":"missing sessionId; open GET /sse first"}"#,
            );
        };

        let response = match serde_json::from_slice::<Request>(&req.body) {
            Ok(rpc) => self.handle(rpc),
            Err(e) => {
                // A malformed frame has no id to answer against; JSON-RPC says
                // to reply with a null id rather than staying silent.
                Some(Response::error(
                    Value::Null,
                    PARSE_ERROR,
                    format!("parse error: {e}"),
                ))
            }
        };

        if let Some(response) = response {
            let json = serde_json::to_string(&response)
                .unwrap_or_else(|e| format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":{INTERNAL_ERROR},"message":"{e}"}}}}"#));
            if !self.sessions.send(&session, &json) {
                return http::respond(
                    stream,
                    "404 Not Found",
                    r#"{"error":"no such session; its SSE stream has closed"}"#,
                );
            }
        }
        http::respond(stream, "202 Accepted", "")
    }

    fn handle(&self, req: Request) -> Option<Response> {
        dispatch(req, self)
    }
}

impl ToolHost for Context {
    /// Hand a tool call to the event loop and wait for it.
    ///
    /// A timeout comes back as a tool error rather than a protocol one: the
    /// window being gone is something the agent should read and stop for, not a
    /// fault that should tear its session down.
    fn call(&self, call: CallParams) -> CallResult {
        let name = call.name.clone();
        let (reply, answer) = mpsc::sync_channel(1);
        if self.jobs.send(Job { call, reply }).is_err() {
            return CallResult::failure("the editor has closed");
        }
        (self.wake)();
        match answer.recv_timeout(CALL_TIMEOUT) {
            Ok(result) => result,
            Err(_) => CallResult::failure(format!(
                "{name} did not finish within {}s — the editor window may be closed or busy",
                CALL_TIMEOUT.as_secs()
            )),
        }
    }
}

/// Anything that can run one tool call.
///
/// The two transports differ only here. Over SSE the editor is owned by a winit
/// event loop, so a call is queued and waited on; over stdio there is no event
/// loop and the server owns the editor outright. Everything else about the
/// protocol — the methods, the errors, the notification rule — is the same, and
/// [`dispatch`] is where that sameness lives.
pub trait ToolHost: Send + Sync {
    fn call(&self, call: CallParams) -> CallResult;

    /// What the agent is told at `initialize` — which directories this server
    /// reads and writes under.
    ///
    /// A desktop MCP client spawns its servers with whatever working directory
    /// the app happened to have, so "somewhere under the working directory" is
    /// no answer at all. Saying it here means the agent knows before it tries,
    /// rather than after a refused save.
    fn instructions(&self) -> String {
        String::new()
    }
}

/// Dispatch one JSON-RPC method. `None` for a notification, which by the
/// specification gets no reply at all.
pub fn dispatch(req: Request, host: &dyn ToolHost) -> Option<Response> {
    if req.is_notification() {
        return None;
    }
    let id = req.id.clone().unwrap_or(Value::Null);
    let result = match req.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": wire::negotiate_version(req.params.as_ref()),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": crate::NAME, "version": crate::VERSION},
            "instructions": host.instructions(),
        })),
        // Clients ping to check the session is alive. An empty result is the
        // whole of the answer.
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools::list()})),
        "tools/call" => {
            match serde_json::from_value::<CallParams>(req.params.clone().unwrap_or(Value::Null)) {
                Ok(call) => Ok(json!(host.call(call))),
                Err(e) => Err((INTERNAL_ERROR, format!("bad tools/call params: {e}"))),
            }
        }
        other => Err((METHOD_NOT_FOUND, format!("no method {other}"))),
    };
    Some(match result {
        Ok(value) => Response::success(id, value),
        Err((code, message)) => Response::error(id, code, message),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use voxel_core::VoxelModel;

    fn context() -> (Context, Receiver<Job>) {
        let (tx, rx) = mpsc::channel();
        (
            Context {
                jobs: tx,
                sessions: Arc::new(Sessions::default()),
                wake: Arc::new(|| {}),
                addr: "127.0.0.1:8730".parse().unwrap(),
                counter: 1,
            },
            rx,
        )
    }

    fn request(method: &str, params: Value) -> Request {
        serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params
        }))
        .unwrap()
    }

    #[test]
    fn initialize_reports_a_tools_capability_and_this_server() {
        let (ctx, _rx) = context();
        let r = ctx.handle(request(
            "initialize",
            json!({"protocolVersion": "2025-06-18"}),
        ));
        let v = serde_json::to_value(r.unwrap()).unwrap();
        assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
        assert!(v["result"]["capabilities"]["tools"].is_object());
        assert_eq!(v["result"]["serverInfo"]["name"], crate::NAME);
    }

    /// A notification gets no reply. Answering one puts an unmatched id on the
    /// stream, which a strict client treats as a protocol violation.
    #[test]
    fn a_notification_is_answered_with_silence() {
        let (ctx, _rx) = context();
        let req: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(ctx.handle(req).is_none());
    }

    #[test]
    fn tools_list_advertises_the_editing_surface() {
        let (ctx, _rx) = context();
        let v =
            serde_json::to_value(ctx.handle(request("tools/list", json!({}))).unwrap()).unwrap();
        let names: Vec<&str> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for expected in ["describe_model", "put_voxel", "put_rect", "paint", "fill"] {
            assert!(
                names.contains(&expected),
                "{expected} missing from {names:?}"
            );
        }
    }

    #[test]
    fn an_unknown_method_is_a_jsonrpc_error() {
        let (ctx, _rx) = context();
        let v = serde_json::to_value(ctx.handle(request("tools/wat", json!({}))).unwrap()).unwrap();
        assert_eq!(v["error"]["code"], METHOD_NOT_FOUND);
    }

    /// The bridge is the whole design: the HTTP thread queues, the event loop
    /// runs it against the editor it owns, and the answer goes back.
    #[test]
    fn a_tool_call_crosses_to_the_editor_and_its_answer_comes_back() {
        let (ctx, rx) = context();
        let bridge = Bridge { jobs: rx };
        let mut editor = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));

        // The call blocks on the event loop, so it has to run on its own thread
        // — which is exactly the arrangement in the real server.
        let worker = std::thread::spawn(move || {
            ctx.handle(request(
                "tools/call",
                json!({"name": "put_voxel", "arguments": {"x": 1, "y": 2, "z": 3, "color": 7}}),
            ))
        });

        // Stand in for the event loop.
        let mut ran = false;
        for _ in 0..200 {
            if bridge.drain(&mut editor) {
                ran = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ran, "the job never reached the editor");

        let v = serde_json::to_value(worker.join().unwrap().unwrap()).unwrap();
        assert_eq!(editor.model().get(1, 2, 3), 7);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"added\":1"), "{text}");
    }

    /// With no event loop to run it, the agent gets an answer rather than a
    /// hung session.
    #[test]
    fn a_call_with_nobody_draining_times_out_as_a_tool_error() {
        let (mut ctx, rx) = context();
        // Nothing will ever drain `rx`, and a short timeout keeps the test
        // honest about what it is measuring.
        let result = {
            ctx.counter = 1;
            let (reply, answer) = mpsc::sync_channel(1);
            ctx.jobs
                .send(Job {
                    call: CallParams {
                        name: "put_voxel".into(),
                        arguments: json!({}),
                    },
                    reply,
                })
                .unwrap();
            answer.recv_timeout(Duration::from_millis(50))
        };
        assert!(result.is_err(), "nothing should have answered");
        drop(rx);

        // And once the editor is gone entirely, the send itself fails.
        assert_eq!(
            ctx.call(CallParams {
                name: "put_voxel".into(),
                arguments: json!({}),
            })
            .is_error,
            Some(true)
        );
    }

    /// The one test that exercises the transport as a client meets it: a real
    /// socket, a real SSE handshake, and answers arriving on the stream rather
    /// than in the POST's own response. Every other test here mocks that away,
    /// and it is exactly the half most likely to be wrong.
    #[test]
    fn a_client_can_drive_the_editor_over_a_real_socket() {
        use std::io::{BufRead, Read, Write};
        use std::net::TcpStream;

        let (bridge, addr) = serve_sse(0, Arc::new(|| {})).expect("listen on an ephemeral port");

        // Stand in for the event loop: drain until the test says stop.
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = done.clone();
        let editor = std::thread::spawn(move || {
            let mut editor = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                bridge.drain(&mut editor);
                std::thread::sleep(Duration::from_millis(2));
            }
            editor
        });

        // Open the stream and read the endpoint the server hands back.
        let mut sse = std::io::BufReader::new(TcpStream::connect(addr).unwrap());
        sse.get_ref()
            .write_all(format!("GET /sse HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
            .unwrap();

        let endpoint = loop {
            let mut line = String::new();
            sse.read_line(&mut line).unwrap();
            if let Some(rest) = line.strip_prefix("data: http") {
                break format!("http{}", rest.trim());
            }
            assert!(!line.is_empty(), "the stream closed before the endpoint");
        };
        let session = endpoint.split("sessionId=").nth(1).unwrap().to_string();

        // POST a request, and read its answer back off the SSE stream.
        let mut exchange = |body: &str| -> Value {
            let mut post = TcpStream::connect(addr).unwrap();
            post.write_all(
                format!(
                    "POST /message?sessionId={session} HTTP/1.1\r\nHost: {addr}\r\n\
                     Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
            let mut ack = String::new();
            post.read_to_string(&mut ack).unwrap();
            assert!(ack.starts_with("HTTP/1.1 202"), "{ack}");

            loop {
                let mut line = String::new();
                sse.read_line(&mut line).unwrap();
                assert!(!line.is_empty(), "the stream closed before answering");
                if let Some(json) = line.strip_prefix("data: ") {
                    return serde_json::from_str(json.trim()).unwrap();
                }
            }
        };

        let init = exchange(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
        );
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["serverInfo"]["name"], crate::NAME);

        let listed = exchange(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        assert!(!listed["result"]["tools"].as_array().unwrap().is_empty());

        let called = exchange(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"put_rect",
                "arguments":{"from":[0,0,0],"to":[1,1,1],"color":5}}}"#,
        );
        let text = called["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"added\":8"), "{text}");

        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let editor = editor.join().unwrap();
        assert_eq!(editor.model().filled_count(), 8);
        assert_eq!(editor.undo_depth(), 1, "the agent's box is one undo step");
    }

    /// A POST naming a session that was never opened must be refused, not
    /// silently dropped — an agent whose stream died needs to be told.
    #[test]
    fn a_post_without_a_live_session_is_refused() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let (_bridge, addr) = serve_sse(0, Arc::new(|| {})).unwrap();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

        let answer = |target: &str| {
            let mut post = TcpStream::connect(addr).unwrap();
            post.write_all(
                format!(
                    "POST {target} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
            let mut s = String::new();
            post.read_to_string(&mut s).unwrap();
            s
        };

        assert!(answer("/message?sessionId=nope").starts_with("HTTP/1.1 404"));
        assert!(answer("/message").starts_with("HTTP/1.1 400"));
        assert!(answer("/elsewhere").starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn draining_an_empty_bridge_does_nothing_and_says_so() {
        let (_ctx, rx) = context();
        let bridge = Bridge { jobs: rx };
        let mut editor = Editor::new(VoxelModel::new(8, 8, 8), PathBuf::from("t.vxm"));
        assert!(!bridge.drain(&mut editor));
    }
}
