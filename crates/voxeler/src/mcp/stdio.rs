//! `voxeler mcp` — the editor as an MCP **stdio** server.
//!
//! The transport an agent starts for itself. SSE needs the editor to already be
//! running, which is a step someone has to remember; a stdio server is spawned
//! by the client that wants it, so "is it up?" stops being a question. The
//! window is then optional, and comes from `voxeler attach`.
//!
//! **stdout is the protocol channel** — every diagnostic goes to stderr, or the
//! first `eprintln!` written to the wrong stream desynchronises the session.
//!
//! There is no event loop here, so unlike the SSE transport the server owns the
//! editor outright. It shares it with the attach listener through a mutex, which
//! is sound precisely because there is no frame to be caught in the middle of.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use voxel_core::VoxelModel;

use super::tools::{self, Roots};
use super::wire::{CallParams, CallResult, Request, Response, PARSE_ERROR};
use super::ToolHost;
use crate::editor::Editor;

/// The model a headless server holds, and how many times it has changed.
///
/// `revision` is what `voxeler attach` polls. Bumped on every tool call rather
/// than on every mutation: a call that changed nothing costs an attached viewer
/// one re-fetch of a few kilobytes, where threading a dirty flag through every
/// mutator would cost a chance to forget one.
pub struct Live {
    editor: Editor,
    revision: u64,
}

impl Live {
    pub fn new(editor: Editor) -> Self {
        Self {
            editor,
            revision: 1,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn model(&self) -> &VoxelModel {
        self.editor.model()
    }

    /// Reach the editor directly. Only the tests do — everything in the running
    /// program goes through a tool call, which is what keeps the revision and
    /// the model changing together.
    #[cfg(test)]
    pub fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// Mark the model as changed, so attached viewers fetch it.
    pub fn bump(&mut self) {
        self.revision += 1;
    }

}

/// The editor, shared between the stdio loop and the attach listener.
#[derive(Clone)]
pub struct Shared(Arc<Mutex<Live>>);

impl Shared {
    pub fn new(editor: Editor) -> Self {
        Self(Arc::new(Mutex::new(Live::new(editor))))
    }

    /// A poisoned lock is recovered rather than propagated: one panicking
    /// connection thread must not make the agent's session unusable.
    pub fn lock(&self) -> MutexGuard<'_, Live> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Runs tool calls directly, holding the lock only for the call itself.
struct Direct {
    shared: Shared,
    root: Roots,
}

impl ToolHost for Direct {
    fn call(&self, call: CallParams) -> CallResult {
        let mut live = self.shared.lock();
        let result = tools::call_in(&mut live.editor, &self.root, &call.name, &call.arguments);
        live.bump();
        result
    }

    fn instructions(&self) -> String {
        self.root.instructions()
    }
}

/// Serve on stdin/stdout until the input closes.
///
/// `shared` is handed in rather than made here so the caller can also give it to
/// the attach listener — the two are the same editor, which is the whole point.
pub fn serve_stdio(shared: Shared, root: Roots) {
    let host = Direct {
        shared,
        root: root.clone(),
    };
    eprintln!(
        "{} {} serving voxel tools, root={}",
        crate::NAME,
        crate::VERSION,
        root.display()
    );
    serve(&host, std::io::stdin().lock(), std::io::stdout().lock());
}

/// The read → dispatch → write loop, over any pair of streams so it can be
/// tested without a process.
///
/// One request at a time. The model is a single document with one history, and
/// running two edits concurrently would mean deciding which happened first —
/// a question an editor has no way to answer and no reason to ask.
fn serve(host: &dyn ToolHost, input: impl BufRead, mut output: impl Write) {
    for line in input.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("voxeler mcp: stdin closed: {e}");
                return;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => super::dispatch(req, host),
            Err(e) => {
                // A malformed frame has no id to answer against; JSON-RPC says
                // to reply with a null id rather than staying silent.
                eprintln!("voxeler mcp: parse error: {e}");
                Some(Response::error(
                    serde_json::Value::Null,
                    PARSE_ERROR,
                    format!("parse error: {e}"),
                ))
            }
        };

        if let Some(resp) = response {
            let json = match serde_json::to_string(&resp) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("voxeler mcp: could not serialize response: {e}");
                    continue;
                }
            };
            if writeln!(output, "{json}").is_err() || output.flush().is_err() {
                eprintln!("voxeler mcp: stdout closed");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;

    fn host() -> Direct {
        Direct {
            shared: Shared::new(Editor::new(
                VoxelModel::new(8, 8, 8),
                PathBuf::from("t.vxm"),
            )),
            root: Roots::new([std::env::temp_dir()]),
        }
    }

    /// Drive the real loop over in-memory pipes: the framing, the
    /// notification-gets-no-reply rule and the flushing all have to line up.
    #[test]
    fn serves_a_session_over_the_stdio_framing() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            "\n", // blank lines are skipped, not errors
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
        );
        let mut out = Vec::new();
        serve(&host(), input.as_bytes(), &mut out);

        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        // Two requests and one notification → exactly two responses.
        assert_eq!(lines.len(), 2, "got: {text}");

        let init: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

        let list: Value = serde_json::from_str(lines[1]).unwrap();
        assert!(!list["result"]["tools"].as_array().unwrap().is_empty());
    }

    #[test]
    fn malformed_json_gets_a_parse_error_with_a_null_id() {
        let mut out = Vec::new();
        serve(&host(), "not json at all\n".as_bytes(), &mut out);
        let v: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
        assert!(v["id"].is_null());
        assert_eq!(v["error"]["code"], PARSE_ERROR);
    }

    /// A tool call edits the shared editor and moves the revision an attached
    /// viewer is watching.
    #[test]
    fn a_tool_call_edits_the_model_and_advances_the_revision() {
        let host = host();
        let shared = host.shared.clone();
        let before = shared.lock().revision();

        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":"#,
            r#"{"name":"put_voxel","arguments":{"x":1,"y":2,"z":3,"color":7}}}"#,
            "\n",
        );
        let mut out = Vec::new();
        serve(&host, input.as_bytes(), &mut out);

        let v: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"added\":1"), "{text}");
        assert_eq!(shared.lock().model().get(1, 2, 3), 7);
        assert!(shared.lock().revision() > before);
    }
}
