//! `voxeler` — a MagicaVoxel-style model editor.
//!
//! ```text
//! voxeler [FILE] [--size N]
//! ```
//!
//! `FILE` defaults to `model.vxm` in the current directory and does not have to
//! exist yet — a missing file starts an empty volume, which is how a model
//! begins. A `.vox` extension is read and written as MagicaVoxel's format;
//! anything else is the editor's own `.vxm`.
//!
//! `--thumbnail OUT.png` renders one framed view and exits without opening a
//! window. It is how a model gets an icon, and how the renderer can be checked
//! on a machine that has no display at all.
//!
//! Two transports serve an AI agent, because they answer different questions:
//!
//! - `voxeler mcp [DIR]` is a **stdio** server, headless, rooted at `DIR`. The
//!   agent starts it, so "is it running?" never comes up — and `voxeler attach`
//!   opens a window onto it whenever a person wants to look.
//! - `voxeler FILE --mcp [PORT]` serves **SSE** from a window that is already
//!   open, for when you were editing first and want an agent to join you.

mod app;
mod attach;
mod editor;
mod mcp;
mod hud;
mod view;

use std::path::PathBuf;

use editor::{open_or_create, DEFAULT_SIZE};

/// Reported to an MCP client in `initialize`, so an agent's transcript records
/// which editor it was talking to.
pub const NAME: &str = "voxeler";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The default MCP port. Unassigned by IANA and unlikely to collide with a
/// development server, which is the whole of the requirement for a loopback
/// socket a person types into a config file once.
pub const DEFAULT_MCP_PORT: u16 = 8730;

fn main() {
    match run() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("voxeler: {e}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    // A leading subcommand, before the flag parser sees it. `voxeler mcp` and
    // `voxeler attach` take a *directory*; everything else takes a file, and
    // splitting them here keeps one parser from having to mean both.
    match argv.first().map(String::as_str) {
        Some("mcp") => return serve_mcp(&argv[1..]),
        Some("attach") => return attach_to_session(&argv[1..]),
        _ => {}
    }
    if argv.first().is_some_and(|a| a == "--") {
        // The escape hatch for a file genuinely named `mcp`.
        argv.remove(0);
    }

    let args = Args::parse(argv.into_iter())?;
    if args.help {
        println!("{USAGE}");
        return Ok(());
    }

    let model = open_or_create(&args.path, args.size)?;
    let [x, y, z] = model.size();
    eprintln!(
        "voxeler: {} — {x}x{y}x{z}, {} voxels",
        args.path.display(),
        model.filled_count()
    );

    match args.thumbnail {
        // A thumbnail exits as soon as it has written its PNG, so there would
        // be nobody to serve and nothing to watch.
        Some(out) if args.mcp.is_some() => {
            let _ = out;
            Err("--mcp and --thumbnail are opposites: one opens a window to watch, \
                 the other exits without one"
                .into())
        }
        Some(out) => thumbnail(model, args.path, &out, args.width, args.height),
        None => app::launch(model, args.path, args.mcp),
    }
}

/// `voxeler mcp [DIR]` — the headless stdio server.
///
/// The model starts empty and unnamed: an agent's first move is `new_model` or
/// `open_model`, and inventing a file for it to overwrite would be a worse
/// default than no file at all.
fn serve_mcp(args: &[String]) -> Result<(), String> {
    if args.first().is_some_and(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }

    let mut dirs: Vec<PathBuf> = Vec::new();
    for arg in args {
        let dir = PathBuf::from(arg);
        if !dir.is_dir() {
            return Err(format!("{} is not a directory", dir.display()));
        }
        dirs.push(dir);
    }
    if dirs.is_empty() {
        let cwd = std::env::current_dir().map_err(|e| format!("no working directory: {e}"))?;
        // A desktop MCP client spawns its servers with whatever working
        // directory the app happened to have, which is often the filesystem
        // root. Falling back to that would quietly hand an agent every file on
        // the machine, so the implicit case refuses it — while an explicit
        // `voxeler mcp /` still means what it says.
        if cwd.parent().is_none() {
            return Err(
                "no directory given and the working directory is the filesystem root — \
                 name the directory your models live in, e.g. `voxeler mcp ~/models`"
                    .into(),
            );
        }
        dirs.push(cwd);
    }
    let root = mcp::Roots::new(dirs);
    let primary = root.primary().expect("at least one directory").to_path_buf();

    let editor = editor::Editor::new(editor::new_model(DEFAULT_SIZE), primary.join("untitled.vxm"));
    let shared = mcp::stdio::Shared::new(editor);

    // Held until this returns, so the session file goes away when the server
    // does. A failure is reported and ignored: the attach listener is a
    // convenience, and an agent's session must not die because a port was busy.
    let _attach = match attach::server::start(shared.clone(), &primary) {
        Ok(server) => {
            eprintln!(
                "voxeler mcp: attach with `voxeler attach {}`",
                primary.display()
            );
            Some(server)
        }
        Err(e) => {
            eprintln!("voxeler mcp: no attach listener ({e}); tools still work");
            None
        }
    };
    mcp::serve_stdio(shared, root);
    Ok(())
}

/// `voxeler attach [DIR]` — a window onto a running server.
fn attach_to_session(args: &[String]) -> Result<(), String> {
    let wanted = match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(d) => Some(PathBuf::from(d)),
        None => None,
    };
    match mcp::session::discover(wanted.as_deref()) {
        mcp::session::Discovery::Found(session) => {
            eprintln!("voxeler: attaching to {} (pid {})", session.root, session.pid);
            app::attach(&session)
        }
        mcp::session::Discovery::None => Err(match &wanted {
            Some(d) => format!(
                "no voxeler mcp session at {} — start one with `voxeler mcp {}`",
                d.display(),
                d.display()
            ),
            None => "no voxeler mcp session is running — start one with `voxeler mcp`".into(),
        }),
        // Naming one is the only way to resolve this, so the message is the
        // list of names rather than an apology.
        mcp::session::Discovery::Ambiguous(sessions) => Err(format!(
            "several sessions are running; name the one you mean:\n{}",
            sessions
                .iter()
                .map(|s| format!("    voxeler attach {}", s.root))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

/// Render one framed view of the model to a PNG and exit.
fn thumbnail(
    model: voxel_core::VoxelModel,
    path: PathBuf,
    out: &std::path::Path,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let mut editor = editor::Editor::new(model, path);
    // No pointer, so no hover highlight -- a thumbnail should show the model,
    // not the editor's gizmos.
    editor.show_grid = false;
    editor.frame_model();
    let mut fb = voxel_render::Framebuffer::new(width.max(1), height.max(1));
    view::render_with_options(&mut fb, &mut editor, None, view::RenderOptions::default());
    voxel_render::png::write(out, fb.width(), fb.height(), fb.color())
        .map_err(|e| format!("{}: {e}", out.display()))?;
    eprintln!("voxeler: wrote {} ({}x{})", out.display(), fb.width(), fb.height());
    Ok(())
}

const USAGE: &str = "\
voxeler — a voxel model editor

USAGE:
    voxeler [FILE] [--size N] [--mcp [PORT]]
    voxeler mcp [DIR...]        serve an agent over stdio, headless
    voxeler attach [DIR]        open a window onto a running `voxeler mcp`

ARGS:
    FILE        model to open or create (default: model.vxm)
                .vox is read and written as MagicaVoxel's format

OPTIONS:
    --size N            edge length for a new model (default: 32, max: 256)
                        ignored when FILE exists -- a saved model keeps its own
    --mcp [PORT]        also serve the editor to an AI agent over MCP/SSE on
                        127.0.0.1:PORT (default: 8730). The window still opens

`voxeler mcp` takes the directories its file tools may read and write. Name them
when a desktop MCP client starts the server for you, or its working directory --
and so where a bare file name lands -- is anyone's guess. Paths given to the
tools may be absolute inside those directories, or relative to the first.

    --thumbnail OUT     render one view to OUT.png and exit, no window
    --width N           thumbnail width  (default: 512)
    --height N          thumbnail height (default: 512)
    -h, --help          print this
";

struct Args {
    path: PathBuf,
    size: u16,
    help: bool,
    /// `Some(port)` when `--mcp` was given. A port of 0 asks the OS for a free
    /// one, which is what the tests use.
    mcp: Option<u16>,
    thumbnail: Option<PathBuf>,
    width: u32,
    height: u32,
}

impl Args {
    /// Hand-rolled, because the whole surface is one path and one flag; a
    /// parser dependency would be larger than the thing it parses.
    fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
        let mut out = Args {
            path: PathBuf::from("model.vxm"),
            size: DEFAULT_SIZE,
            help: false,
            mcp: None,
            thumbnail: None,
            width: 512,
            height: 512,
        };
        let mut seen_path = false;
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => out.help = true,
                "--size" => {
                    let v = args
                        .next()
                        .ok_or_else(|| "--size needs a number".to_string())?;
                    out.size = v
                        .parse::<u16>()
                        .map_err(|_| format!("--size: {v} is not a number"))?;
                    if out.size == 0 || out.size > voxel_core::MAX_DIM {
                        return Err(format!(
                            "--size: {} is outside 1..={}",
                            out.size,
                            voxel_core::MAX_DIM
                        ));
                    }
                }
                // The port is optional, so it is only consumed when the next
                // argument actually looks like one — `--mcp model.vxm` means
                // the default port and that file, not a parse error.
                "--mcp" => {
                    let port = match args.peek().and_then(|a| a.parse::<u16>().ok()) {
                        Some(p) => {
                            args.next();
                            p
                        }
                        None => DEFAULT_MCP_PORT,
                    };
                    out.mcp = Some(port);
                }
                "--thumbnail" => {
                    out.thumbnail = Some(PathBuf::from(
                        args.next()
                            .ok_or_else(|| "--thumbnail needs a path".to_string())?,
                    ));
                }
                "--width" => out.width = number(args.next(), "--width")?,
                "--height" => out.height = number(args.next(), "--height")?,
                s if s.starts_with("--") => return Err(format!("unknown option {s}")),
                s if seen_path => return Err(format!("unexpected argument {s}")),
                s => {
                    out.path = PathBuf::from(s);
                    seen_path = true;
                }
            }
        }
        Ok(out)
    }
}

fn number(arg: Option<String>, flag: &str) -> Result<u32, String> {
    let v = arg.ok_or_else(|| format!("{flag} needs a number"))?;
    let n = v
        .parse::<u32>()
        .map_err(|_| format!("{flag}: {v} is not a number"))?;
    if n == 0 || n > 8192 {
        return Err(format!("{flag}: {n} is outside 1..=8192"));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        Args::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_to_a_32_cubed_model_vxm() {
        let a = parse(&[]).unwrap();
        assert_eq!(a.path, PathBuf::from("model.vxm"));
        assert_eq!(a.size, 32);
    }

    /// The larger volume is still one flag away, and a loaded file's own size
    /// wins over both — `--size` only ever describes a model that does not
    /// exist yet.
    #[test]
    fn size_64_is_still_accepted() {
        assert_eq!(parse(&["--size", "64"]).unwrap().size, 64);
    }

    #[test]
    fn a_path_and_a_size_are_both_accepted_in_either_order() {
        for args in [
            vec!["robot.vox", "--size", "32"],
            vec!["--size", "32", "robot.vox"],
        ] {
            let a = parse(&args).unwrap();
            assert_eq!(a.path, PathBuf::from("robot.vox"));
            assert_eq!(a.size, 32);
        }
    }

    #[test]
    fn a_size_outside_the_grids_limits_is_refused() {
        assert!(parse(&["--size", "0"]).is_err());
        assert!(parse(&["--size", "257"]).is_err());
        assert!(parse(&["--size", "big"]).is_err());
        assert!(parse(&["--size"]).is_err());
    }

    #[test]
    fn a_thumbnail_request_carries_its_path_and_size() {
        let a = parse(&["r.vxm", "--thumbnail", "out.png", "--width", "320", "--height", "240"])
            .unwrap();
        assert_eq!(a.thumbnail, Some(PathBuf::from("out.png")));
        assert_eq!((a.width, a.height), (320, 240));
    }

    #[test]
    fn a_thumbnail_size_of_zero_is_refused() {
        assert!(parse(&["--width", "0"]).is_err());
        assert!(parse(&["--height", "99999"]).is_err());
        assert!(parse(&["--thumbnail"]).is_err());
    }

    #[test]
    fn unknown_options_and_extra_paths_are_errors() {
        assert!(parse(&["--wat"]).is_err());
        assert!(parse(&["a.vxm", "b.vxm"]).is_err());
    }
}
