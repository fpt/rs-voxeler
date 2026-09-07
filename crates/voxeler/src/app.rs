//! The window: winit events in, a blitted framebuffer out.
//!
//! Presentation and input mapping only, the same division `kessel`'s player
//! draws. Every decision about what a click *means* lives in [`crate::editor`],
//! which is why that module is the one with tests and this one has almost none
//! worth writing.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;

use voxel_core::Span;
use voxel_render::Framebuffer;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::editor::{Editor, Target, Tool};
use crate::{hud, mcp, view};

/// The rendered image is capped at this many pixels and upscaled to fill the
/// window.
///
/// A software rasterizer on a 5K display would otherwise spend its whole budget
/// on pixels nobody is looking closely at. Around 1.4 M is a full-screen window
/// at 1× on a laptop, and on a Retina display it lands at 2× — which is exactly
/// where the upscale is invisible in ordinary use.
const MAX_PIXELS: u32 = 1_400_000;

/// What the pointer is currently doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Gesture {
    None,
    Editing,
    Orbit,
    Pan,
}

pub struct App {
    editor: Editor,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    fb: Framebuffer,
    /// Framebuffer pixels per window pixel.
    scale: u32,

    cursor: Option<(f32, f32)>,
    hover: Option<Target>,
    gesture: Gesture,
    last_drag: (f32, f32),
    modifiers: ModifiersState,
    /// Where the button went down, and whether the pointer has since travelled
    /// far enough for this to be a drag rather than a click. See [`DEAD_ZONE`].
    press_at: (f32, f32),
    dragging: bool,
    shown_title: String,
    /// Tool calls waiting to be run against `editor`, when `--mcp` is on.
    ///
    /// Drained here rather than applied on the server's own thread, so an edit
    /// from an agent lands between two frames like every other edit — and the
    /// editor stays the single-threaded thing the rest of this program assumes.
    bridge: Option<mcp::Bridge>,
    /// The session being watched, when this window is `voxeler attach`.
    viewer: Option<crate::attach::client::Attached>,
}

/// What a background thread sends to wake the event loop. Carries nothing: the
/// message *is* "come and look at the queue".
struct Wake;

pub fn run(editor: Editor, mcp_port: Option<u16>) -> Result<(), String> {
    let event_loop = EventLoop::<Wake>::with_user_event()
        .build()
        .map_err(|e| format!("event loop: {e}"))?;
    // Wait for events rather than spinning: an editor changes only when the
    // user does something, and a redraw is requested explicitly when it does.
    // That is also why the MCP server needs a proxy to wake this — otherwise an
    // agent's call would land only on the next mouse move.
    event_loop.set_control_flow(ControlFlow::Wait);

    let bridge = match mcp_port {
        Some(port) => {
            // `send_event` needs `&self`, and several connection threads share
            // one proxy; a mutex makes it `Sync` without asking winit to be.
            let proxy = std::sync::Mutex::new(event_loop.create_proxy());
            let wake = std::sync::Arc::new(move || {
                if let Ok(p) = proxy.lock() {
                    let _ = p.send_event(Wake);
                }
            });
            let (bridge, addr) = mcp::serve_sse(port, wake)
                .map_err(|e| format!("mcp: cannot listen on 127.0.0.1:{port}: {e}"))?;
            eprintln!("voxeler: mcp sse at http://{addr}/sse");
            Some(bridge)
        }
        None => None,
    };

    let mut app = App {
        editor,
        window: None,
        surface: None,
        fb: Framebuffer::new(1, 1),
        scale: 1,
        cursor: None,
        hover: None,
        gesture: Gesture::None,
        last_drag: (0.0, 0.0),
        modifiers: ModifiersState::empty(),
        press_at: (0.0, 0.0),
        dragging: false,
        shown_title: String::new(),
        bridge,
        viewer: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|e| format!("event loop: {e}"))
}

impl App {
    fn ctrl(&self) -> bool {
        // Command on macOS, Control everywhere else. Accepting both means one
        // key table works on every platform without a cfg.
        self.modifiers.control_key() || self.modifiers.super_key()
    }

    fn title(&self) -> String {
        format!(
            "voxeler — {}{}",
            self.editor.path().display(),
            if self.editor.is_dirty() { " *" } else { "" }
        )
    }

    fn request_redraw(&mut self) {
        let title = self.title();
        if let Some(w) = &self.window {
            if title != self.shown_title {
                w.set_title(&title);
                self.shown_title = title;
            }
            w.request_redraw();
        }
    }

    /// Resize the render target to match the window, choosing an upscale factor
    /// that keeps the pixel count sane.
    fn sync_framebuffer(&mut self) {
        let Some(window) = &self.window else { return };
        let size = window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        let mut scale = 1;
        while (w / scale) * (h / scale) > MAX_PIXELS {
            scale += 1;
        }
        self.scale = scale;
        self.fb.resize((w / scale).max(1), (h / scale).max(1));
    }

    /// Window pixels to framebuffer pixels.
    fn to_fb(&self, x: f64, y: f64) -> (f32, f32) {
        (x as f32 / self.scale as f32, y as f32 / self.scale as f32)
    }

    /// Recompute what is under the cursor. Cheap enough to do on every move —
    /// it is one grid walk — and doing it eagerly is what keeps the highlight
    /// in step with the model after an edit.
    fn update_hover(&mut self) {
        if self.editor.viewing {
            // No tool, so no cell a click would land on, so nothing to outline.
            self.hover = None;
            return;
        }
        self.hover = match self.cursor {
            Some((x, y)) if !hud::over_panel(self.fb.width(), self.editor.model().layer_count(), x, y) => {
                self.editor
                    .target_at(x, y, self.fb.width(), self.fb.height())
            }
            _ => None,
        };
    }

    fn redraw(&mut self) {
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return; // minimised
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };

        view::render(&mut self.fb, &mut self.editor, self.hover);
        if !self.editor.viewing {
            hud::draw_tools(&mut self.fb, &self.editor);
        }
        hud::draw(&mut self.fb, &self.editor);

        blit(&mut buffer, size.width, size.height, &self.fb, self.scale);
        let _ = buffer.present();
    }

    fn on_mouse_down(&mut self, button: MouseButton) {
        let Some((x, y)) = self.cursor else { return };

        // A click on a panel is a choice, never an edit — and never a camera
        // drag either, or picking a colour would spin the model.
        let layers = self.editor.model().layer_count();
        if button == MouseButton::Left && hud::over_panel(self.fb.width(), layers, x, y) {
            if let Some(index) = hud::palette_hit(self.fb.width(), x, y) {
                self.editor.color = index;
                self.editor.set_status(format!("colour {index}"));
            } else if let Some(hit) = hud::layer_hit(self.fb.width(), layers, x, y) {
                self.editor.select_layer(hit.index);
                if hit.on_eye {
                    self.editor.toggle_layer_visible();
                }
                self.update_hover();
            }
            return;
        }

        self.last_drag = (x, y);
        self.press_at = (x, y);
        self.dragging = false;
        // A viewer holds somebody else's document. The camera is still yours,
        // so orbit and pan stay; the left button stops being a tool.
        if self.editor.viewing {
            self.gesture = match button {
                MouseButton::Left if self.modifiers.shift_key() => Gesture::Pan,
                MouseButton::Left | MouseButton::Right => Gesture::Orbit,
                MouseButton::Middle => Gesture::Pan,
                _ => Gesture::None,
            };
            return;
        }
        self.gesture = match button {
            // Alt+left orbits, matching what a three-button mouse does on its
            // right button — a laptop trackpad has no comfortable right drag.
            MouseButton::Left if self.modifiers.alt_key() => Gesture::Orbit,
            MouseButton::Left if self.modifiers.shift_key() => Gesture::Pan,
            MouseButton::Left => {
                if let Some(target) = self.hover {
                    self.editor.begin_stroke(target);
                    self.update_hover();
                }
                Gesture::Editing
            }
            MouseButton::Right if self.modifiers.shift_key() => Gesture::Pan,
            MouseButton::Right => Gesture::Orbit,
            MouseButton::Middle => Gesture::Pan,
            _ => Gesture::None,
        };
    }

    fn on_mouse_move(&mut self, x: f32, y: f32) {
        let (dx, dy) = (x - self.last_drag.0, y - self.last_drag.1);
        self.last_drag = (x, y);
        self.cursor = Some((x, y));

        match self.gesture {
            Gesture::Orbit => {
                // Radians per framebuffer pixel. Tuned so a drag across half
                // the window is roughly a half turn.
                const RATE: f32 = 0.008;
                self.editor.camera.orbit(-dx * RATE, dy * RATE);
                self.update_hover();
            }
            Gesture::Pan => {
                // Scale panning with distance so the model tracks the cursor at
                // any zoom; at a fixed rate it crawls when zoomed out.
                let rate = self.editor.camera.distance * 0.0022;
                self.editor.camera.pan(-dx * rate, dy * rate);
                self.update_hover();
            }
            Gesture::Editing => {
                self.update_hover();
                // A click is not a drag. Pressing a button emits a move or two
                // of its own, and near the horizon one pixel of the work plane
                // can be a whole cell away — so without a dead zone a click
                // lands a voxel where it was aimed and then another beside it.
                self.dragging |= !is_click(self.press_at, (x, y));
                if let (true, Some(target)) = (self.dragging, self.hover) {
                    self.editor.continue_stroke(target);
                    self.update_hover();
                }
            }
            Gesture::None => self.update_hover(),
        }
    }

    /// Keys, while the rename prompt is open.
    ///
    /// `event.text` rather than the physical key: a name is what the user's
    /// layout produces, and reconstructing that from key codes would be a
    /// keyboard-layout table this editor has no business owning.
    fn on_rename_key(&mut self, event: &winit::event::KeyEvent) {
        match event.physical_key {
            PhysicalKey::Code(KeyCode::Escape) => return self.editor.cancel_rename(),
            PhysicalKey::Code(KeyCode::Enter) | PhysicalKey::Code(KeyCode::NumpadEnter) => {
                return self.editor.commit_rename()
            }
            PhysicalKey::Code(KeyCode::Backspace) => return self.editor.rename_backspace(),
            _ => {}
        }
        if let Some(text) = &event.text {
            for c in text.chars() {
                self.editor.rename_push(c);
            }
        }
    }

    /// The keys a viewer keeps: the camera, the grid, the slice and the help
    /// card. Everything else edits a document this window does not own, and a
    /// key that silently did nothing would read as a broken editor.
    fn on_viewer_key(&mut self, code: KeyCode, event_loop: &ActiveEventLoop) {
        if self.ctrl() {
            if code == KeyCode::KeyQ {
                event_loop.exit();
            }
            return;
        }
        match code {
            KeyCode::KeyG => self.editor.show_grid = !self.editor.show_grid,
            KeyCode::KeyH => self.editor.show_help = !self.editor.show_help,
            KeyCode::KeyF => self.editor.frame_model(),
            KeyCode::KeyR => self.editor.reset_view(),
            KeyCode::Comma => self.editor.nudge_slice(-1),
            KeyCode::Period => self.editor.nudge_slice(1),
            KeyCode::Backslash => self.editor.set_slice(None),
            KeyCode::Escape if self.editor.show_help => self.editor.show_help = false,
            _ => return,
        }
        self.update_hover();
    }

    fn on_key(&mut self, code: KeyCode, event_loop: &ActiveEventLoop) {
        if self.editor.viewing {
            return self.on_viewer_key(code, event_loop);
        }
        if self.ctrl() {
            match code {
                KeyCode::KeyZ if self.modifiers.shift_key() => self.editor.redo(),
                KeyCode::KeyZ => self.editor.undo(),
                KeyCode::KeyY => self.editor.redo(),
                KeyCode::KeyS => self.editor.save(),
                KeyCode::KeyE => self.editor.export_vox(),
                KeyCode::KeyR => self.editor.reload(),
                KeyCode::KeyN => self.editor.clear(),
                KeyCode::KeyQ => event_loop.exit(),
                _ => return,
            }
            self.update_hover();
            return;
        }

        match code {
            KeyCode::KeyB => self.editor.tool = Tool::Build,
            KeyCode::KeyE => self.editor.tool = Tool::Erase,
            KeyCode::KeyP => self.editor.tool = Tool::Paint,
            KeyCode::KeyI => self.editor.tool = Tool::Pick,
            KeyCode::BracketLeft => self.editor.nudge_color(-1),
            KeyCode::BracketRight => self.editor.nudge_color(1),
            KeyCode::Minus => self.editor.nudge_color(-16),
            KeyCode::Equal => self.editor.nudge_color(16),
            // Span: the four keys run left to right in order of reach, so the
            // row on screen and the row on the keyboard are the same order.
            KeyCode::Digit1 => self.editor.set_span(Span::Voxel),
            KeyCode::Digit2 => self.editor.set_span(Span::Axis),
            KeyCode::Digit3 => self.editor.set_span(Span::Plane),
            KeyCode::Digit4 => self.editor.set_span(Span::Volume),
            KeyCode::Digit9 => self.editor.nudge_brush(-1),
            KeyCode::Digit0 => self.editor.nudge_brush(1),
            KeyCode::KeyC => self.editor.toggle_brush_shape(),
            // Each mirror plane on its own key, and `M` for the X one because
            // that is the axis a character is symmetric about and the binding
            // this editor already had.
            KeyCode::KeyX | KeyCode::KeyM => self.editor.toggle_mirror(0),
            KeyCode::KeyY => self.editor.toggle_mirror(1),
            KeyCode::KeyZ => self.editor.toggle_mirror(2),
            // Layers. `L` walks the stack the way `[` and `]` walk the palette,
            // and the rest sit under the fingers that reach for it.
            KeyCode::KeyL if self.modifiers.shift_key() => self.editor.cycle_layer(-1),
            KeyCode::KeyL => self.editor.cycle_layer(1),
            KeyCode::KeyA => self.editor.add_layer(),
            KeyCode::KeyD => self.editor.delete_layer(),
            KeyCode::KeyV => self.editor.toggle_layer_visible(),
            KeyCode::KeyN => self.editor.begin_rename(),
            KeyCode::KeyK => self.editor.move_layer(true),
            KeyCode::KeyJ => self.editor.move_layer(false),
            KeyCode::KeyU => self.editor.merge_layer_down(),
            KeyCode::KeyT => self.editor.trim_layer(),
            KeyCode::KeyG => self.editor.show_grid = !self.editor.show_grid,
            KeyCode::KeyH => self.editor.show_help = !self.editor.show_help,
            KeyCode::KeyF => self.editor.frame_model(),
            KeyCode::KeyR => self.editor.reset_view(),
            KeyCode::Comma => self.editor.nudge_slice(-1),
            KeyCode::Period => self.editor.nudge_slice(1),
            KeyCode::Backslash => self.editor.set_slice(None),
            KeyCode::Escape if self.editor.show_help => self.editor.show_help = false,
            _ => return,
        }
        self.update_hover();
    }
}

impl ApplicationHandler<Wake> for App {
    /// An agent has queued something. Run it here, on the thread that owns the
    /// editor, then refresh what the window shows: the hover highlight is
    /// resolved against the model, so an edit from outside invalidates it just
    /// as one from the mouse does.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _wake: Wake) {
        let mut ran = false;
        if let Some(bridge) = self.bridge.take() {
            ran |= bridge.drain(&mut self.editor);
            self.bridge = Some(bridge);
        }
        // An attached session has sent a newer model. Only the newest is taken:
        // a viewer that fell behind should catch up, not replay.
        if let Some(model) = self.viewer.as_ref().and_then(|v| v.latest()) {
            self.editor.show(model);
            ran = true;
        }
        if ran {
            self.update_hover();
            // `request_redraw` also refreshes the title, which is where the
            // unsaved-changes marker lives — an agent's edit dirties the
            // document exactly as the user's does.
            self.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        self.shown_title = self.title();
        let attrs = Window::default_attributes()
            .with_title(&self.shown_title)
            .with_inner_size(winit::dpi::LogicalSize::new(1200, 800));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("voxeler: create window: {e}");
                event_loop.exit();
                return;
            }
        };
        match softbuffer::Context::new(window.clone())
            .and_then(|ctx| softbuffer::Surface::new(&ctx, window.clone()))
        {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                eprintln!("voxeler: create surface: {e}");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
        self.sync_framebuffer();
        self.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                if self.editor.is_dirty() {
                    // The window is closing either way — refusing would trap
                    // the user — but saying so beats losing work silently.
                    eprintln!(
                        "voxeler: closing with unsaved changes to {}",
                        self.editor.path().display()
                    );
                }
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.sync_framebuffer();
                self.update_hover();
                self.request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                // A rename is modal: while it is open every key belongs to the
                // name, or typing "BODY" would fire build, erase and pick on
                // the way through.
                if self.editor.renaming().is_some() {
                    self.on_rename_key(&event);
                    self.request_redraw();
                    return;
                }
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                self.on_key(code, event_loop);
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = self.to_fb(position.x, position.y);
                if self.cursor.is_none() {
                    // First move after entering: seed the drag origin so the
                    // next delta is not measured from wherever the pointer was
                    // when it left.
                    self.last_drag = (x, y);
                }
                self.on_mouse_move(x, y);
                self.request_redraw();
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                self.hover = None;
                self.request_redraw();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Pressed {
                    self.on_mouse_down(button);
                } else {
                    if self.gesture == Gesture::Editing {
                        self.editor.end_stroke();
                    }
                    self.gesture = Gesture::None;
                }
                self.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let ticks = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    // A trackpad reports pixels; 40 per notch keeps the two
                    // input kinds at a comparable speed.
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                };
                self.editor.camera.zoom(0.9f32.powf(ticks));
                self.update_hover();
                self.request_redraw();
            }
            _ => {}
        }
    }
}

/// Upscale `src` into the window buffer by an integer factor.
///
/// Nearest-neighbour and integer-scaled, so the HUD's 5×7 text stays crisp
/// rather than blurring. The clamp on the source index handles a window whose
/// size is not an exact multiple of the scale: the last row and column repeat
/// instead of reading out of bounds.
fn blit(dst: &mut [u32], dst_w: u32, dst_h: u32, src: &Framebuffer, scale: u32) {
    let scale = scale.max(1);
    let (sw, sh) = (src.width(), src.height());
    if sw == 0 || sh == 0 {
        return;
    }
    let colors = src.color();
    for y in 0..dst_h {
        let sy = (y / scale).min(sh - 1);
        let src_row = (sy * sw) as usize;
        let dst_row = (y * dst_w) as usize;
        for x in 0..dst_w {
            let sx = (x / scale).min(sw - 1);
            dst[dst_row + x as usize] = colors[src_row + sx as usize];
        }
    }
}

/// How far the pointer may travel between press and release and still be a
/// click. Framebuffer pixels, so it is the same physical distance whatever the
/// display's density.
const DEAD_ZONE: f32 = 3.0;

/// Whether a press that has reached `to` is still a click rather than a drag.
fn is_click(from: (f32, f32), to: (f32, f32)) -> bool {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    dx * dx + dy * dy <= DEAD_ZONE * DEAD_ZONE
}

/// Open a read-only window following a running `voxeler mcp`.
pub fn attach(session: &crate::mcp::session::Session) -> Result<(), String> {
    let event_loop = EventLoop::<Wake>::with_user_event()
        .build()
        .map_err(|e| format!("event loop: {e}"))?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = std::sync::Mutex::new(event_loop.create_proxy());
    let wake = std::sync::Arc::new(move || {
        if let Ok(p) = proxy.lock() {
            let _ = p.send_event(Wake);
        }
    });
    let (viewer, model) = crate::attach::client::connect(session, wake)?;

    // The path is the session's root rather than a file: a viewer holds no
    // document, and a title naming one would invite `ctrl+S`.
    let mut editor = Editor::new(model, PathBuf::from(&session.root));
    editor.viewing = true;
    editor.set_status(format!("attached to {}", viewer.label));

    let mut app = App {
        editor,
        window: None,
        surface: None,
        fb: Framebuffer::new(1, 1),
        scale: 1,
        cursor: None,
        hover: None,
        gesture: Gesture::None,
        last_drag: (0.0, 0.0),
        modifiers: ModifiersState::empty(),
        press_at: (0.0, 0.0),
        dragging: false,
        shown_title: String::new(),
        bridge: None,
        viewer: Some(viewer),
    };
    event_loop
        .run_app(&mut app)
        .map_err(|e| format!("event loop: {e}"))
}

/// Open a window on `editor`. Split out so `main` reads as a pipeline.
pub fn launch(
    model: voxel_core::VoxelModel,
    path: PathBuf,
    mcp_port: Option<u16>,
) -> Result<(), String> {
    run(Editor::new(model, path), mcp_port)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A press emits a move or two of its own, and near the horizon one pixel
    /// of the work plane can be a whole cell away. Without a dead zone a click
    /// lands a voxel where it was aimed and then another beside it.
    #[test]
    fn a_press_that_barely_moves_is_still_a_click() {
        let from = (100.0, 100.0);
        assert!(is_click(from, from));
        assert!(is_click(from, (101.0, 100.0)));
        assert!(is_click(from, (102.0, 102.0)), "under three pixels diagonally");
        assert!(!is_click(from, (104.0, 100.0)));
        assert!(!is_click(from, (100.0, 96.0)), "backwards counts too");
    }

    #[test]
    fn the_blit_repeats_the_last_pixel_rather_than_reading_past_the_buffer() {
        let mut src = Framebuffer::new(2, 2);
        src.clear(0);
        src.set(0, 0, 1);
        src.set(1, 0, 2);
        src.set(0, 1, 3);
        src.set(1, 1, 4);

        // A 5x5 window at scale 2 does not divide evenly: the last row and
        // column have to come from somewhere.
        let mut dst = vec![0u32; 25];
        blit(&mut dst, 5, 5, &src, 2);
        assert_eq!(dst[0], 1);
        assert_eq!(dst[2], 2);
        assert_eq!(dst[4], 2, "the odd column repeats the last source pixel");
        assert_eq!(dst[24], 4);
    }

    #[test]
    fn blitting_an_empty_framebuffer_does_nothing() {
        let src = Framebuffer::new(0, 0);
        let mut dst = vec![7u32; 4];
        blit(&mut dst, 2, 2, &src, 1);
        assert_eq!(dst, vec![7; 4]);
    }
}
