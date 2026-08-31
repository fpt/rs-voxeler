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

mod app;
mod editor;
mod hud;
mod view;

use std::path::PathBuf;

use editor::{open_or_create, DEFAULT_SIZE};

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
    let args = Args::parse(std::env::args().skip(1))?;
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
        Some(out) => thumbnail(model, args.path, &out, args.width, args.height),
        None => app::launch(model, args.path),
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
    let mut fb = voxel_render::Framebuffer::new(width.max(1), height.max(1));
    view::render(&mut fb, &mut editor, None);
    voxel_render::png::write(out, fb.width(), fb.height(), fb.color())
        .map_err(|e| format!("{}: {e}", out.display()))?;
    eprintln!("voxeler: wrote {} ({}x{})", out.display(), fb.width(), fb.height());
    Ok(())
}

const USAGE: &str = "\
voxeler — a voxel model editor

USAGE:
    voxeler [FILE] [--size N]

ARGS:
    FILE        model to open or create (default: model.vxm)
                .vox is read and written as MagicaVoxel's format

OPTIONS:
    --size N            edge length for a new model (default: 64, max: 256)
    --thumbnail OUT     render one view to OUT.png and exit, no window
    --width N           thumbnail width  (default: 512)
    --height N          thumbnail height (default: 512)
    -h, --help          print this
";

struct Args {
    path: PathBuf,
    size: u16,
    help: bool,
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
    fn defaults_to_a_64_cubed_model_vxm() {
        let a = parse(&[]).unwrap();
        assert_eq!(a.path, PathBuf::from("model.vxm"));
        assert_eq!(a.size, 64);
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
