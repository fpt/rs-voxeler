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
    /// What was typed since the last frame, from `event.text` rather than from
    /// key codes — the rule `Editor::rename` already set: a name is what the
    /// user's layout produces, and reconstructing that would be a
    /// keyboard-layout table this editor has no business owning.
    pub typed: String,
    /// Backspace, which arrives as a key rather than as text.
    pub backspace: bool,
}

const TEXT_SCALE: u32 = 2;
const ROW_H: u32 = 20;
const PAD: u32 = 8;

/// How long a name a text field will take.
///
/// A filename, not a paragraph: past this the field is scrolling text nobody
/// can check before pressing SAVE, and every filesystem here stops long before
/// it anyway.
const FIELD_MAX: usize = 96;

/// The width of a text field's caret.
const CARET_W: u32 = 2;

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

    /// A line of dim text: what a field is for, or where it will land.
    ///
    /// Elided at the *front* when it does not fit, because what a label here
    /// says is usually a path, and the end of a path is the part that tells
    /// you where you are. A dialog is sized by its content and a directory is
    /// not content anyone can size for, so one of them has to give.
    pub fn label(&mut self, text: &str) {
        let r = self.row();
        self.commands.push(Command::Text {
            x: r.x,
            y: r.y + 4,
            text: elide_front(text, r.w),
            color: DIM,
            scale: TEXT_SCALE,
        });
    }

    /// A line of text being typed, returning whether it changed this frame.
    ///
    /// There is no focus. A dialog has one field and it takes whatever was
    /// typed; a second would need a focus id to say which of them the keys
    /// belong to, and no dialog has asked for one — the same answer the layout
    /// stack gives a flexbox.
    ///
    /// The characters are filtered here rather than at the window layer, the
    /// rule [`crate::editor::Editor::rename_push`] set, so every platform's
    /// idea of what arrives with a key press meets the same test. Printable
    /// ASCII only: the overlay font has nothing else, and a name the field
    /// cannot draw is a name you cannot check before pressing SAVE.
    pub fn text_field(&mut self, value: &mut String) -> bool {
        let r = self.row();
        let mut changed = false;
        if self.input.backspace {
            changed = value.pop().is_some();
        }
        for c in self.input.typed.chars() {
            if c.is_ascii_graphic() || c == ' ' {
                if value.chars().count() >= FIELD_MAX {
                    break;
                }
                value.push(c);
                changed = true;
            }
        }

        let box_r = Rect {
            y: r.y + 2,
            h: ROW_H - 4,
            ..r
        };
        self.commands.push(Command::Fill {
            rect: box_r,
            color: TRACK,
        });
        self.commands.push(Command::Stroke {
            rect: box_r,
            color: ACCENT,
        });
        // A name longer than the box shows its *tail*, because the end is
        // where the caret is and where what you just typed went.
        let room = box_r.w.saturating_sub(PAD * 2 + CARET_W);
        let mut shown = value.as_str();
        while text_width(shown, TEXT_SCALE) > room {
            let Some((next, _)) = shown.char_indices().nth(1) else {
                break;
            };
            shown = &shown[next..];
        }
        let x = box_r.x + PAD as i32 / 2;
        self.commands.push(Command::Text {
            x,
            y: box_r.y + 3,
            text: shown.to_string(),
            color: TEXT,
            scale: TEXT_SCALE,
        });
        // The caret says the keyboard is here — which, with no focus ring to
        // move, is the only thing that does.
        self.commands.push(Command::Fill {
            rect: Rect {
                x: x + text_width(shown, TEXT_SCALE) as i32 + 1,
                y: box_r.y + 2,
                w: CARET_W,
                h: box_r.h - 4,
            },
            color: ACCENT,
        });
        changed
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
    /// Choosing where the document goes.
    ///
    /// Opened when a save has no file to overwrite, and by save-as whatever
    /// the document already has. `name` is what the field holds — a bare file
    /// name, or a path — and `dir` is what a relative one is resolved against,
    /// carried here only so the dialog can say where it will land. The
    /// resolving itself stays in [`crate::editor::Editor`], with the writing.
    Save { name: String, dir: String },
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
    /// Whether confirming would have something to act on.
    ///
    /// One predicate rather than a test inside the button and another beside
    /// the Enter key: those are the same question, and two answers to it is
    /// how the mouse and the keyboard come to disagree.
    pub fn can_confirm(&self) -> bool {
        match self {
            Dialog::Color { .. } => true,
            Dialog::Save { name, .. } => !name.trim().is_empty(),
        }
    }

    /// Lay the dialog out and read the input, returning what it wants.
    ///
    /// Takes the colour by `&mut` rather than reaching for the editor, so this
    /// module stays a *layout* and the applying stays where the undo history
    /// is.
    pub fn run(&mut self, ui: &mut Ui, screen: Rect) -> Outcome {
        // Asked once, before the match takes the dialog apart, so the button
        // below and `can_confirm`'s other caller — the Enter key — cannot come
        // to different answers.
        let ready = self.can_confirm();
        match self {
            Dialog::Color { index, rgb } => {
                let slot = *index;
                ui.dialog(&format!("COLOUR {slot}"), screen, 260, 6);
                let mut moved = false;
                moved |= ui.slider("R", &mut rgb[0], 255);
                moved |= ui.slider("G", &mut rgb[1], 255);
                moved |= ui.slider("B", &mut rgb[2], 255);
                ui.swatch(((rgb[0] as u32) << 16) | ((rgb[1] as u32) << 8) | rgb[2] as u32);
                let ok = ui.button("OK") && ready;
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
            Dialog::Save { name, dir } => {
                ui.dialog("SAVE AS", screen, 480, 4);
                // Where a relative name lands, or why SAVE is doing nothing.
                // A field with an empty name and an inert button would
                // otherwise be a click that changed nothing and did not say
                // why.
                ui.label(if name.trim().is_empty() {
                    "NAME THE FILE"
                } else {
                    dir
                });
                ui.text_field(name);
                let ok = ui.button("SAVE") && ready;
                let cancel = ui.button("CANCEL");
                ui.end();
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

/// As much of the *end* of `s` as fits in `width` pixels, marked with an
/// ellipsis when something was dropped.
///
/// Measured against the font rather than counted in characters: a row is a
/// number of pixels wide, and a character count that matched it at one text
/// scale would overflow at the next.
fn elide_front(s: &str, width: u32) -> String {
    if text_width(s, TEXT_SCALE) <= width {
        return s.to_string();
    }
    // Three dots rather than an ellipsis: the overlay font is 0x20..0x60 and
    // draws anything outside it as nothing at all, so "…" would elide the
    // mark that says something was elided.
    const MARK: &str = "...";
    let room = width.saturating_sub(text_width(MARK, TEXT_SCALE) + ADVANCE * TEXT_SCALE);
    let mut shown = s;
    while !shown.is_empty() && text_width(shown, TEXT_SCALE) > room {
        let Some((next, _)) = shown.char_indices().nth(1) else {
            break;
        };
        shown = &shown[next..];
    }
    format!("{MARK}{shown}")
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
        if let Dialog::Color { rgb: after, .. } = d {
            *rgb = after;
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

    /// Lay out the save dialog once, the way `run` does, and hand back what
    /// the field holds afterwards.
    fn save(input: Input, held: u64, name: &mut String) -> (Ui, Outcome) {
        let mut d = Dialog::Save {
            name: name.clone(),
            dir: "/tmp/models".into(),
        };
        let mut ui = Ui::new(input, held);
        let outcome = d.run(&mut ui, SCREEN);
        if let Dialog::Save { name: after, .. } = d {
            *name = after;
        }
        (ui, outcome)
    }

    fn typed(text: &str) -> Input {
        Input {
            typed: text.to_string(),
            ..Default::default()
        }
    }

    /// The field takes what the platform says was typed, and backspace, which
    /// arrives as a key rather than as text.
    #[test]
    fn a_text_field_takes_typed_characters_and_backspace() {
        let mut name = String::new();
        save(typed("rob"), 0, &mut name);
        assert_eq!(name, "rob");
        save(typed("ot"), 0, &mut name);
        assert_eq!(name, "robot");
        save(
            Input {
                backspace: true,
                ..Default::default()
            },
            0,
            &mut name,
        );
        assert_eq!(name, "robo");

        // The keys a dialog answers to are not characters, whatever the
        // platform hands over with them, and neither is anything the 5x7 font
        // cannot draw.
        save(typed("\u{1b}\r\u{8}\u{3042}"), 0, &mut name);
        assert_eq!(name, "robo", "control characters and non-ASCII are dropped");

        // And it stops rather than growing without limit.
        let long = "x".repeat(FIELD_MAX * 2);
        save(typed(&long), 0, &mut name);
        assert_eq!(name.chars().count(), FIELD_MAX);
    }

    /// One predicate behind the button and the Enter key: SAVE must not fire
    /// on a name there is nothing of.
    #[test]
    fn save_does_not_fire_on_an_empty_name() {
        let mut name = "  ".to_string();
        let d = Dialog::Save {
            name: name.clone(),
            dir: ".".into(),
        };
        assert!(!d.can_confirm());

        let btn = text_at(&save(Input::default(), 0, &mut name).0, "SAVE").expect("SAVE is drawn");
        let over = (btn.0 as f32 + 4.0, btn.1 as f32 + 2.0);
        let (ui, _) = save(
            Input {
                mouse: over,
                down: true,
                pressed: true,
                ..Default::default()
            },
            0,
            &mut name,
        );
        let (_, outcome) = save(
            Input {
                mouse: over,
                released: true,
                ..Default::default()
            },
            ui.held(),
            &mut name,
        );
        assert_eq!(outcome, Outcome::Open, "an empty name is not a save");

        // And the dialog says why, in place of the directory it would name.
        assert!(
            text_at(&save(Input::default(), 0, &mut name).0, "NAME THE FILE").is_some(),
            "a click that changed nothing says why"
        );

        // With a name, the same click is a save.
        let mut name = "robot.vxm".to_string();
        let (ui, _) = save(
            Input {
                mouse: over,
                down: true,
                pressed: true,
                ..Default::default()
            },
            0,
            &mut name,
        );
        let (_, outcome) = save(
            Input {
                mouse: over,
                released: true,
                ..Default::default()
            },
            ui.held(),
            &mut name,
        );
        assert_eq!(outcome, Outcome::Done);
    }

    /// A deep directory is elided at the front, because the end is the part
    /// that says where you are — and it is measured, so it fits the row it is
    /// drawn in rather than a character count that only matched at one scale.
    #[test]
    fn a_long_directory_keeps_its_tail_and_fits_its_row() {
        assert_eq!(elide_front("/tmp/models", 400), "/tmp/models");
        let long = elide_front("/a/very/long/way/down/to/the/models/dir", 120);
        assert!(text_width(&long, TEXT_SCALE) <= 120, "{long}");
        assert!(long.starts_with("...") && long.ends_with("s/dir"), "{long}");

        // And the row the save dialog actually draws it in holds it.
        let deep = "/Users/someone/Documents/scratch/rs-voxeler/models/characters";
        let mut name = "robot.vxm".to_string();
        let mut d = Dialog::Save {
            name: name.clone(),
            dir: deep.into(),
        };
        let mut ui = Ui::new(Input::default(), 0);
        d.run(&mut ui, SCREEN);
        name.clear();
        let row = ui
            .commands()
            .iter()
            .find_map(|c| match c {
                Command::Text { text, .. } if text.ends_with("characters") => Some(text.clone()),
                _ => None,
            })
            .expect("the directory is drawn");
        assert!(row.starts_with("..."), "{row}");
        assert!(text_width(&row, TEXT_SCALE) <= 480 - PAD * 2, "{row}");
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
