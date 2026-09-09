//! A small immediate-mode UI, in the shape microui settled on.
//!
//! Nothing here is a toolkit. The window layer is one blocking event loop and
//! the drawing is a CPU framebuffer, which rules out everything GPU-first — and
//! the alternative that does fit wants to own the window and the event loop.
//! That is the same trade the PNG writer, the HTTP and the 5×7 font already
//! made: a second runtime inside one blocking loop costs more than the thing it
//! saves.
//!
//! Three ideas are borrowed, and they are the whole design.
//!
//! **A command list.** A widget never touches the framebuffer. It pushes
//! [`Command`]s and [`flush`] draws them at the end of the frame, so a dialog
//! lands over the viewport and the HUD without either knowing it exists, and
//! the order on screen is the order they were declared in.
//!
//! **Ids come from the call site.** A slider you are dragging has to stay the
//! one you grabbed while the pointer wanders off it, and immediate mode has no
//! widget to hang that on. The id is a hash of the label mixed with the
//! enclosing scope's, which is enough to tell two sliders apart without a
//! retained tree to keep in agreement with the code.
//!
//! **Layout is one stack.** Rows and columns, pushed and popped. A dialog is a
//! column of rows; nothing here needs more, and a flexbox would be a second
//! system to reason about for no dialog anybody has asked for.

use voxel_render::overlay::{self, text_width, ADVANCE};
use voxel_render::Framebuffer;

/// What a widget leaves behind for [`flush`] to draw.
#[derive(Clone, PartialEq, Debug)]
pub enum Command {
    Fill {
        rect: Rect,
        color: u32,
    },
    Stroke {
        rect: Rect,
        color: u32,
    },
    /// Translucent, for the sheet that dims what a dialog sits over.
    Blend {
        rect: Rect,
        color: u32,
        alpha: u8,
    },
    Text {
        x: i32,
        y: i32,
        text: String,
        color: u32,
        scale: u32,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x as f32
            && y >= self.y as f32
            && x < self.x as f32 + self.w as f32
            && y < self.y as f32 + self.h as f32
    }

    /// The same rect inset on every side, for a border.
    fn inset(self, by: u32) -> Rect {
        Rect {
            x: self.x + by as i32,
            y: self.y + by as i32,
            w: self.w.saturating_sub(by * 2),
            h: self.h.saturating_sub(by * 2),
        }
    }
}

/// What the pointer and keyboard did since the last frame.
///
/// Edges rather than levels for the click, because a widget asks "was I
/// pressed" once per frame and a held button would otherwise fire every frame
/// it is held.
#[derive(Clone, Default, Debug)]
pub struct Input {
    pub mouse: (f32, f32),
    pub down: bool,
    pub pressed: bool,
    pub released: bool,
}

const TEXT_SCALE: u32 = 2;
const ROW_H: u32 = 20;
const PAD: u32 = 8;

const BG: u32 = 0x1B1F29;
const FRAME: u32 = 0x39404F;
const TEXT: u32 = 0xE8E8EC;
const DIM: u32 = 0x9098A4;
const ACCENT: u32 = 0xFFD24A;
const TRACK: u32 = 0x2A3040;

/// One frame of user interface.
pub struct Ui {
    commands: Vec<Command>,
    input: Input,
    /// The widget holding the pointer, if any.
    ///
    /// The one thing that has to persist: hovering is worked out per widget as
    /// it lays itself out and needs no state, but a *grab* has to outlive the
    /// pointer leaving the widget. A slider that let go at the edge of its own
    /// track is the failure this exists to prevent.
    active: u64,
    scope: u64,
    layout: Vec<Cursor>,
}

/// Where the next widget goes inside the current container.
#[derive(Clone, Copy, Debug)]
struct Cursor {
    rect: Rect,
    y: i32,
}

impl Ui {
    pub fn new(input: Input, active: u64) -> Self {
        Self {
            commands: Vec::new(),
            input,
            active,
            scope: 0,
            layout: Vec::new(),
        }
    }

    /// Which widget is still held, to be handed back next frame.
    ///
    /// The one piece of state that survives a frame. Everything else is
    /// rebuilt, which is what makes this immediate mode rather than a tree.
    pub fn held(&self) -> u64 {
        self.active
    }

    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    fn id(&self, label: &str) -> u64 {
        // FNV-1a, mixed with the enclosing scope so two dialogs may each have
        // an "R" slider without becoming the same widget.
        let mut h = self.scope ^ 0xcbf2_9ce4_8422_2325;
        for b in label.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        h.max(1)
    }

    /// Open a dialog: a dimmed sheet, a frame, and a column to fill.
    ///
    /// Centred, and sized by its content rather than by the window, so it does
    /// not move when the window is resized mid-edit.
    pub fn dialog(&mut self, title: &str, screen: Rect, w: u32, rows: u32) {
        self.scope = self.id(title);
        let h = ROW_H + PAD * 2 + rows * ROW_H;
        let rect = Rect {
            x: screen.x + (screen.w.saturating_sub(w) / 2) as i32,
            y: screen.y + (screen.h.saturating_sub(h) / 2) as i32,
            w,
            h,
        };
        // The sheet is what makes it modal to the eye as well as to the
        // keyboard: the model is still there, and visibly not what you are
        // typing at.
        self.commands.push(Command::Blend {
            rect: screen,
            color: 0x000000,
            alpha: 120,
        });
        self.commands.push(Command::Fill { rect, color: BG });
        self.commands.push(Command::Stroke { rect, color: FRAME });
        self.commands.push(Command::Text {
            x: rect.x + PAD as i32,
            y: rect.y + 6,
            text: title.to_string(),
            color: DIM,
            scale: TEXT_SCALE,
        });
        self.layout.push(Cursor {
            rect: rect.inset(PAD),
            y: rect.y + ROW_H as i32,
        });
    }

    pub fn end(&mut self) {
        self.layout.pop();
        self.scope = 0;
    }

    /// The next full-width row, and the y after it.
    fn row(&mut self) -> Rect {
        let Some(c) = self.layout.last_mut() else {
            return Rect::default();
        };
        let rect = Rect {
            x: c.rect.x,
            y: c.y,
            w: c.rect.w,
            h: ROW_H,
        };
        c.y += ROW_H as i32;
        rect
    }

    /// A button. True on the frame it is released over, which is what a click
    /// is — pressing and dragging away should not fire it.
    pub fn button(&mut self, label: &str) -> bool {
        let id = self.id(label);
        let r = self.row();
        let over = r.contains(self.input.mouse.0, self.input.mouse.1);
        if over && self.input.pressed {
            self.active = id;
        }
        let fired = self.input.released && self.active == id && over;
        let held = self.active == id && self.input.down;
        self.commands.push(Command::Fill {
            rect: r,
            color: if held { FRAME } else { TRACK },
        });
        self.commands.push(Command::Stroke {
            rect: r,
            color: if over { ACCENT } else { FRAME },
        });
        let w = text_width(label, TEXT_SCALE) as i32;
        self.commands.push(Command::Text {
            x: r.x + (r.w as i32 - w) / 2,
            y: r.y + 4,
            text: label.to_string(),
            color: if over { ACCENT } else { TEXT },
            scale: TEXT_SCALE,
        });
        fired
    }

    /// A slider over `0..=max`, returning whether it moved.
    pub fn slider(&mut self, label: &str, value: &mut u8, max: u8) -> bool {
        let id = self.id(label);
        let r = self.row();
        let name_w = (ADVANCE * TEXT_SCALE * 2) as i32;
        let track = Rect {
            x: r.x + name_w,
            y: r.y + 4,
            w: r.w.saturating_sub(name_w as u32 + 44),
            h: ROW_H - 8,
        };
        if self.input.pressed && track.contains(self.input.mouse.0, self.input.mouse.1) {
            self.active = id;
        }
        let mut moved = false;
        // Held, not hovered: once grabbed the track follows the pointer even
        // when it leaves the widget, which is what dragging a slider means.
        if self.active == id && self.input.down && track.w > 0 {
            let t = ((self.input.mouse.0 - track.x as f32) / track.w as f32).clamp(0.0, 1.0);
            let next = (t * max as f32).round() as u8;
            moved = next != *value;
            *value = next;
        }
        self.commands.push(Command::Fill {
            rect: track,
            color: TRACK,
        });
        let filled = if max == 0 {
            0
        } else {
            track.w * *value as u32 / max as u32
        };
        self.commands.push(Command::Fill {
            rect: Rect { w: filled, ..track },
            color: if self.active == id { ACCENT } else { DIM },
        });
        self.commands.push(Command::Text {
            x: r.x,
            y: r.y + 4,
            text: label.to_string(),
            color: DIM,
            scale: TEXT_SCALE,
        });
        self.commands.push(Command::Text {
            x: track.x + track.w as i32 + 8,
            y: r.y + 4,
            text: value.to_string(),
            color: TEXT,
            scale: TEXT_SCALE,
        });
        moved
    }

    /// A block of colour, for showing what the sliders add up to.
    pub fn swatch(&mut self, color: u32) {
        let r = self.row();
        let box_r = Rect {
            h: ROW_H - 6,
            y: r.y + 3,
            ..r
        };
        self.commands.push(Command::Fill { rect: box_r, color });
        self.commands.push(Command::Stroke {
            rect: box_r,
            color: FRAME,
        });
    }
}

/// Draw a frame's commands. The one place this module touches pixels.
pub fn flush(fb: &mut Framebuffer, commands: &[Command]) {
    for c in commands {
        match c {
            Command::Fill { rect, color } => {
                overlay::fill_rect(fb, rect.x, rect.y, rect.w, rect.h, *color)
            }
            Command::Stroke { rect, color } => {
                overlay::stroke_rect(fb, rect.x, rect.y, rect.w, rect.h, *color)
            }
            Command::Blend { rect, color, alpha } => {
                overlay::blend_rect(fb, rect.x, rect.y, rect.w, rect.h, *color, *alpha)
            }
            Command::Text {
                x,
                y,
                text,
                color,
                scale,
            } => overlay::text(fb, *x, *y, text, *color, *scale),
        }
    }
}

/// The dialogs this editor has, and the state each needs to keep between
/// frames.
///
/// On `Editor` rather than in `app.rs` for the reason `Editor::rename` is: a
/// dialog is a mode the editor is in, and the window layer's job is to route
/// input into it and draw what comes back.
#[derive(Clone, PartialEq, Debug)]
pub enum Dialog {
    /// Editing what one palette slot means.
    ///
    /// Holds the slot and the working RGB. The edit is applied as the sliders
    /// move — `set_palette_color` is already undoable and drops a no-op — so
    /// the model shows the colour while you choose it, which is the whole point
    /// of a picker.
    Color { index: u8, rgb: [u8; 3] },
}

/// What a dialog asked the editor to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Still open.
    Open,
    /// Closed, keeping what was done.
    Done,
    /// Closed, and the caller should put back what it saved.
    Cancelled,
}

impl Dialog {
    /// Lay the dialog out and read the input, returning what it wants.
    ///
    /// Takes the colour by `&mut` rather than reaching for the editor, so this
    /// module stays a *layout* and the applying stays where the undo history
    /// is.
    pub fn run(&mut self, ui: &mut Ui, screen: Rect) -> Outcome {
        match self {
            Dialog::Color { index, rgb } => {
                let slot = *index;
                ui.dialog(&format!("COLOUR {slot}"), screen, 260, 6);
                let mut moved = false;
                moved |= ui.slider("R", &mut rgb[0], 255);
                moved |= ui.slider("G", &mut rgb[1], 255);
                moved |= ui.slider("B", &mut rgb[2], 255);
                ui.swatch(((rgb[0] as u32) << 16) | ((rgb[1] as u32) << 8) | rgb[2] as u32);
                let ok = ui.button("OK");
                let cancel = ui.button("CANCEL");
                ui.end();
                let _ = moved;
                if ok {
                    Outcome::Done
                } else if cancel {
                    Outcome::Cancelled
                } else {
                    Outcome::Open
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect {
        x: 0,
        y: 0,
        w: 420,
        h: 300,
    };

    fn run(input: Input, held: u64, rgb: &mut [u8; 3]) -> (Ui, Outcome) {
        let mut d = Dialog::Color {
            index: 5,
            rgb: *rgb,
        };
        let mut ui = Ui::new(input, held);
        let outcome = d.run(&mut ui, SCREEN);
        match d {
            Dialog::Color { rgb: after, .. } => *rgb = after,
        }
        (ui, outcome)
    }

    /// Where a widget is, by laying the dialog out and reading its own
    /// commands — the same list the drawing uses, so a test cannot drift from
    /// the layout the way a hard-coded coordinate would.
    fn text_at(ui: &Ui, want: &str) -> Option<(i32, i32)> {
        ui.commands().iter().find_map(|c| match c {
            Command::Text { x, y, text, .. } if text == want => Some((*x, *y)),
            _ => None,
        })
    }

    /// Two widgets in one dialog must not share an id, or dragging one moves
    /// the other. The same label in two dialogs must not either.
    #[test]
    fn ids_tell_widgets_apart_within_and_between_dialogs() {
        let mut ui = Ui::new(Input::default(), 0);
        ui.scope = ui.id("COLOUR 5");
        let (r, g, b) = (ui.id("R"), ui.id("G"), ui.id("B"));
        assert_ne!(r, g);
        assert_ne!(g, b);
        assert_ne!(r, b);
        assert!([r, g, b].iter().all(|i| *i != 0), "0 means nothing is held");

        let mut other = Ui::new(Input::default(), 0);
        other.scope = other.id("COLOUR 6");
        assert_ne!(other.id("R"), r, "same label, different dialog");

        // And stable: the same call in the same scope is the same widget, or
        // nothing could be held across a frame at all.
        let mut again = Ui::new(Input::default(), 0);
        again.scope = again.id("COLOUR 5");
        assert_eq!(again.id("R"), r);
    }

    /// The rule the hot/active pair exists for: once a slider is grabbed it
    /// keeps the drag however far the pointer strays. Losing it at the edge of
    /// the track is what makes a slider feel broken.
    #[test]
    fn a_grabbed_slider_follows_the_pointer_off_its_own_track() {
        let mut rgb = [10, 10, 10];
        // Press on the middle of the G track.
        let (ui, _) = run(
            Input {
                mouse: (250.0, 122.0),
                down: true,
                pressed: true,
                ..Default::default()
            },
            0,
            &mut rgb,
        );
        let held = ui.held();
        assert_ne!(held, 0, "something was grabbed");
        let grabbed = rgb[1];
        assert!(grabbed > 10, "and it moved: {rgb:?}");
        assert_eq!(rgb[0], 10, "only the one grabbed");

        // Now drag far below the dialog entirely, still holding.
        let (_, _) = run(
            Input {
                mouse: (120.0, 290.0),
                down: true,
                ..Default::default()
            },
            held,
            &mut rgb,
        );
        assert!(rgb[1] < grabbed, "it tracked left, off the widget: {rgb:?}");
        assert_eq!(rgb[0], 10, "and still only the one");
    }

    /// A click is a press *and* a release over the same button. Pressing and
    /// dragging away is how everyone cancels a click they did not mean.
    #[test]
    fn a_button_fires_on_release_over_it_and_not_before() {
        let mut rgb = [1, 2, 3];
        let ok = text_at(&run(Input::default(), 0, &mut rgb).0, "OK").expect("OK is laid out");
        let over = (ok.0 as f32 + 4.0, ok.1 as f32 + 2.0);

        // Press alone does nothing.
        let (ui, outcome) = run(
            Input {
                mouse: over,
                down: true,
                pressed: true,
                ..Default::default()
            },
            0,
            &mut rgb,
        );
        assert_eq!(outcome, Outcome::Open, "a press is not a click");
        let held = ui.held();

        // Release somewhere else does nothing either.
        let (_, outcome) = run(
            Input {
                mouse: (5.0, 5.0),
                released: true,
                ..Default::default()
            },
            held,
            &mut rgb,
        );
        assert_eq!(outcome, Outcome::Open, "dragged away, so not a click");

        // Release over it is the click.
        let (_, outcome) = run(
            Input {
                mouse: over,
                released: true,
                ..Default::default()
            },
            held,
            &mut rgb,
        );
        assert_eq!(outcome, Outcome::Done);
    }

    /// The dimming sheet has to be the first thing drawn, or it covers the
    /// dialog it is meant to sit behind.
    #[test]
    fn the_sheet_is_drawn_before_the_dialog_it_dims() {
        let mut rgb = [0, 0, 0];
        let (ui, _) = run(Input::default(), 0, &mut rgb);
        match ui.commands().first() {
            Some(Command::Blend { rect, .. }) => assert_eq!(*rect, SCREEN),
            other => panic!("expected the sheet first, got {other:?}"),
        }
        // And the frame is over it rather than under.
        assert!(matches!(ui.commands()[1], Command::Fill { .. }));
    }

    /// Cancel is a distinct answer from OK, or the dialog has one button.
    #[test]
    fn cancel_reports_itself() {
        let mut rgb = [0, 0, 0];
        let cancel =
            text_at(&run(Input::default(), 0, &mut rgb).0, "CANCEL").expect("CANCEL is laid out");
        let over = (cancel.0 as f32 + 4.0, cancel.1 as f32 + 2.0);
        let (ui, _) = run(
            Input {
                mouse: over,
                down: true,
                pressed: true,
                ..Default::default()
            },
            0,
            &mut rgb,
        );
        let (_, outcome) = run(
            Input {
                mouse: over,
                released: true,
                ..Default::default()
            },
            ui.held(),
            &mut rgb,
        );
        assert_eq!(outcome, Outcome::Cancelled);
    }
}
