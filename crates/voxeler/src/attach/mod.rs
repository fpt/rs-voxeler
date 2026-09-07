//! `voxeler attach` — a window onto a running `voxeler mcp`.
//!
//! The stdio server is headless: an agent starts it, and nobody is watching. An
//! attached viewer is how a person joins that session — orbit, zoom, slice,
//! frame — and sees the model being built as it happens.
//!
//! It is a **viewer**, not a second editor. There is one document and one undo
//! history, and they live in the server; a viewer that could also edit would
//! need a rule for what happens when both write at once, and the honest ones are
//! all worse than "the picture is live and the keyboard is the agent's". Editing
//! by hand is `voxeler FILE`, or `--mcp` with the window in charge.

pub mod client;
pub mod protocol;
pub mod server;
