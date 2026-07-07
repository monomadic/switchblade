//! sb-window: owns the winit event loop and the wgpu surface.
//!
//! Boundary rule (PLAN.md §12): this crate owns the loop and calls *into* an
//! [`App`] trait. No winit or wgpu types cross the boundary — the app sees
//! normalized input events and hands back a plain frame description.

mod render;

use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

/// Normalized keyboard keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Left,
    Right,
    Up,
    Down,
    Enter,
    Space,
    Escape,
    Char(char),
}

/// Normalized input events. Coordinates and deltas are in logical pixels.
#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    Key { key: Key, repeat: bool },
    /// Trackpad / mouse wheel scroll.
    Scroll { dx: f32, dy: f32 },
    /// Trackpad pinch; positive delta = fingers spreading (zoom in).
    Pinch { delta: f32 },
    CursorMoved { x: f32, y: f32 },
    MouseDown { x: f32, y: f32 },
}

/// Requests the app makes of the window layer.
#[derive(Debug, Clone)]
pub enum WindowCommand {
    Quit,
    ToggleFullscreen,
    SetTitle(String),
}

#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

/// Thumbnail atlas geometry, chosen by the app at startup (from tuning).
/// The renderer owns one fixed-slot RGBA atlas; the app allocates and
/// evicts slot indices.
#[derive(Debug, Clone, Copy)]
pub struct AtlasCfg {
    pub slot_w: u32,
    pub slot_h: u32,
    pub cols: u32,
    pub rows: u32,
}

impl AtlasCfg {
    pub fn slots(&self) -> usize {
        (self.cols * self.rows) as usize
    }
    pub fn tex_w(&self) -> u32 {
        self.cols * self.slot_w
    }
    pub fn tex_h(&self) -> u32 {
        self.rows * self.slot_h
    }

    /// Normalized atlas UV rect `[x, y, w, h]` for a `tw × th` region
    /// within `slot`, offset by `(ox, oy)` pixels and inset half a texel
    /// against bleeding. This is how tiles say which part of which thumb
    /// they show.
    pub fn uv(&self, slot: usize, ox: f32, oy: f32, tw: f32, th: f32) -> [f32; 4] {
        let (col, row) = (
            (slot % self.cols as usize) as f32,
            (slot / self.cols as usize) as f32,
        );
        let (aw, ah) = (self.tex_w() as f32, self.tex_h() as f32);
        let x = col * self.slot_w as f32 + ox + 0.5;
        let y = row * self.slot_h as f32 + oy + 0.5;
        [x / aw, y / ah, (tw - 1.0).max(0.0) / aw, (th - 1.0).max(0.0) / ah]
    }
}

/// Pixels to copy into one atlas slot this frame. Thumbs keep their source
/// aspect ratio, so `w × h` may be smaller than the slot; they land at the
/// slot's top-left.
pub struct ThumbUpload {
    pub slot: usize,
    pub w: u32,
    pub h: u32,
    /// Exactly `w × h × 4` RGBA bytes.
    pub rgba: Vec<u8>,
}

/// One tile to draw. Position/size in logical pixels, origin top-left.
/// Later tiles draw on top of earlier ones.
#[derive(Debug, Clone, Copy)]
pub struct Tile {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [f32; 4],
    pub border_color: [f32; 4],
    pub corner_radius: f32,
    pub border_width: f32,
    /// Normalized atlas UV rect (see [`AtlasCfg::uv`]); zero size = none.
    pub uv: [f32; 4],
    /// Second UV rect blended over the first by `frame_fade` — used to
    /// crossfade between anim-sheet frames.
    pub uv2: [f32; 4],
    /// 0..1 blend from `uv` to `uv2`.
    pub frame_fade: f32,
    /// 0..1 crossfade from placeholder color to texture.
    pub tex_mix: f32,
}

/// Everything the renderer needs for one frame.
pub struct Frame {
    pub clear: [f32; 3],
    pub tiles: Vec<Tile>,
    pub uploads: Vec<ThumbUpload>,
}

pub trait App {
    /// Atlas geometry, read once at window/GPU init.
    fn atlas(&self) -> AtlasCfg;
    fn event(&mut self, event: InputEvent);
    /// Advance the simulation by `dt` seconds and describe the frame.
    fn frame(&mut self, dt: f32, viewport: Viewport) -> Frame;
    /// Drain pending window commands (quit, fullscreen, title).
    fn commands(&mut self) -> Vec<WindowCommand>;
}

pub fn run(app: impl App) -> anyhow::Result<()> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut runner = Runner {
        app,
        window: None,
        gpu: None,
        last_frame: Instant::now(),
        cursor: (0.0, 0.0),
    };
    event_loop.run_app(&mut runner)?;
    Ok(())
}

struct Runner<A: App> {
    app: A,
    window: Option<Arc<Window>>,
    gpu: Option<render::Gpu>,
    last_frame: Instant,
    cursor: (f32, f32),
}

impl<A: App> Runner<A> {
    fn scale(&self) -> f64 {
        self.window.as_ref().map_or(1.0, |w| w.scale_factor())
    }

    fn apply_commands(&mut self, event_loop: &ActiveEventLoop) {
        for cmd in self.app.commands() {
            match cmd {
                WindowCommand::Quit => event_loop.exit(),
                WindowCommand::ToggleFullscreen => {
                    if let Some(w) = &self.window {
                        let next = if w.fullscreen().is_some() {
                            None
                        } else {
                            Some(Fullscreen::Borderless(None))
                        };
                        w.set_fullscreen(next);
                    }
                }
                WindowCommand::SetTitle(title) => {
                    if let Some(w) = &self.window {
                        w.set_title(&title);
                    }
                }
            }
        }
    }
}

impl<A: App> ApplicationHandler for Runner<A> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("switchblade")
            .with_inner_size(LogicalSize::new(1280.0, 800.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let gpu = pollster::block_on(render::Gpu::new(window.clone(), self.app.atlas()))
            .expect("init gpu");
        self.window = Some(window);
        self.gpu = Some(gpu);
        self.last_frame = Instant::now();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size.width, size.height);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                let key = match &event.logical_key {
                    WinitKey::Named(NamedKey::ArrowLeft) => Some(Key::Left),
                    WinitKey::Named(NamedKey::ArrowRight) => Some(Key::Right),
                    WinitKey::Named(NamedKey::ArrowUp) => Some(Key::Up),
                    WinitKey::Named(NamedKey::ArrowDown) => Some(Key::Down),
                    WinitKey::Named(NamedKey::Enter) => Some(Key::Enter),
                    WinitKey::Named(NamedKey::Space) => Some(Key::Space),
                    WinitKey::Named(NamedKey::Escape) => Some(Key::Escape),
                    WinitKey::Character(s) => s.chars().next().map(Key::Char),
                    _ => None,
                };
                if let Some(key) = key {
                    self.app.event(InputEvent::Key { key, repeat: event.repeat });
                    self.apply_commands(event_loop);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::PixelDelta(p) => {
                        let s = self.scale() as f32;
                        (p.x as f32 / s, p.y as f32 / s)
                    }
                    MouseScrollDelta::LineDelta(x, y) => (x * 40.0, y * 40.0),
                };
                self.app.event(InputEvent::Scroll { dx, dy });
            }
            WindowEvent::PinchGesture { delta, .. } => {
                self.app.event(InputEvent::Pinch { delta: delta as f32 });
            }
            WindowEvent::CursorMoved { position, .. } => {
                let p = position.to_logical::<f32>(self.scale());
                self.cursor = (p.x, p.y);
                self.app.event(InputEvent::CursorMoved { x: p.x, y: p.y });
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let (x, y) = self.cursor;
                self.app.event(InputEvent::MouseDown { x, y });
                self.apply_commands(event_loop);
            }
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let dt = (now - self.last_frame).as_secs_f32().min(0.05);
                self.last_frame = now;
                let (Some(window), Some(gpu)) = (&self.window, &mut self.gpu) else {
                    return;
                };
                let scale = window.scale_factor() as f32;
                let size = window.inner_size();
                let viewport = Viewport {
                    width: size.width as f32 / scale,
                    height: size.height as f32 / scale,
                };
                let frame = self.app.frame(dt, viewport);
                gpu.render(&frame, viewport);
                self.apply_commands(event_loop);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Game-style continuous redraw; the app's motion is dt-scaled.
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}
