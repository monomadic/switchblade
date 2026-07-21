//! sb-window: owns the winit event loop and the wgpu surface.
//!
//! Boundary rule (PLAN.md §12): this crate owns the loop and calls *into* an
//! [`App`] trait. No winit or wgpu types cross the boundary — the app sees
//! normalized input events and hands back a plain frame description.

mod drag;
mod render;
pub mod schedule;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
    Tab,
    Char(char),
}

/// Keyboard modifiers held during a pointer event, normalized so apps
/// never see winit's modifier types. `cmd` is the platform primary
/// modifier (⌘ on macOS, Ctrl elsewhere).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub cmd: bool,
}

/// Normalized input events. Coordinates and deltas are in logical pixels.
#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    Key {
        key: Key,
        repeat: bool,
    },
    /// Trackpad / mouse wheel scroll.
    Scroll {
        dx: f32,
        dy: f32,
    },
    /// Trackpad pinch; positive delta = fingers spreading (zoom in).
    Pinch {
        delta: f32,
    },
    CursorMoved {
        x: f32,
        y: f32,
    },
    MouseDown {
        x: f32,
        y: f32,
        /// Modifiers held at press time (cmd/shift-click multi-select).
        mods: Mods,
    },
    /// Left button released (ends a seekbar drag-scrub).
    MouseUp {
        x: f32,
        y: f32,
    },
    /// Window gained/lost focus (drives pause-when-unfocused).
    Focus {
        focused: bool,
    },
}

/// Requests the app makes of the window layer.
#[derive(Debug, Clone)]
pub enum WindowCommand {
    Quit,
    /// `fast` = borrow the whole screen with a borderless desktop-sized
    /// window (instant, same Space — mpv's no-native-fs) instead of the
    /// OS's native fullscreen (own Space + animation on macOS). Either
    /// way, toggling while in *any* fullscreen mode exits it.
    ToggleFullscreen {
        fast: bool,
    },
    SetTitle(String),
    /// Start a native drag-out of `path` (macOS: an `NSDraggingSession`
    /// whose pasteboard carries the file URL, so dropping into Finder or
    /// onto another app behaves exactly like a drag out of Finder).
    /// Only honored from inside a pointer-event callback — the platform
    /// layer seeds the session from the OS's current mouse event, so the
    /// app must emit it in direct response to a MouseDown/CursorMoved.
    /// `image` is the drag ghost (the clip's cached thumb jpeg); absent,
    /// the OS file icon stands in.
    BeginDrag {
        path: PathBuf,
        image: Option<PathBuf>,
    },
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
    /// Dedicated high-resolution texture beside the atlas (quickview
    /// playback); tiles opt into it via [`Tile::hires`].
    pub hires_w: u32,
    pub hires_h: u32,
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
        [
            x / aw,
            y / ah,
            (tw - 1.0).max(0.0) / aw,
            (th - 1.0).max(0.0) / ah,
        ]
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

/// Pixels for the high-res texture (top-left anchored, `w × h ≤ hires
/// dims`). One producer at a time — the quickview player. The pixels
/// ride an `Arc` (P1.5): the renderer only reads them, and the app keeps
/// a second handle so the ~33MB buffer can go back to the decoder's
/// recycle pool after the upload instead of being freed every frame.
pub struct HiresFrame {
    pub w: u32,
    pub h: u32,
    pub rgba: Arc<Vec<u8>>,
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
    /// Sample the high-res texture instead of the atlas (`uv` is then
    /// normalized against the hires dims).
    pub hires: bool,
    /// Pie-clip (the auto-skip timer): 0 = off (normal tile); otherwise
    /// the tile — fill and border alike, so a border-only circle becomes
    /// an arc — is clipped to a wedge spanning |pie| (0..=1) of the
    /// circle. Positive: the wedge ends at 12 o'clock and drains
    /// clockwise as it shrinks (countdown). Negative: it starts at 12
    /// and grows clockwise (count-up).
    pub pie: f32,
}

/// Frosted-glass backdrop: `tiles[..split]` also render offscreen, get
/// downsampled `levels` times, and the blurred result draws fullscreen
/// (blended in at `fade`) beneath `tiles[split..]`.
#[derive(Debug, Clone, Copy)]
pub struct Blur {
    pub split: usize,
    /// Downsample count: each level halves the resolution (softer blur).
    pub levels: u32,
    /// 0..1 blend of the blurred layer over the sharp one.
    pub fade: f32,
}

/// Everything the renderer needs for one frame.
pub struct Frame {
    pub clear: [f32; 3],
    pub tiles: Vec<Tile>,
    pub uploads: Vec<ThumbUpload>,
    pub hires_upload: Option<HiresFrame>,
    /// Blur the tiles below `split` (quickview's grid backdrop).
    pub blur: Option<Blur>,
    /// False when nothing on screen is in motion: the loop drops to a
    /// slow idle tick instead of redrawing every vsync.
    pub animating: bool,
    /// One-shot redraw deadline while NOT animating (P1.4): live video
    /// with UI settled reports the next queued frame's due time here, so
    /// a 30fps clip presents ~30 times a second instead of every vsync.
    /// Ignored while `animating` (UI motion owns the cadence) and while
    /// occluded.
    pub redraw_at: Option<Instant>,
}

pub trait App {
    /// Atlas geometry, read once at window/GPU init.
    fn atlas(&self) -> AtlasCfg;
    fn event(&mut self, event: InputEvent);
    /// Advance the simulation by `dt` seconds and describe the frame.
    fn frame(&mut self, dt: f32, viewport: Viewport) -> Frame;
    /// Drain pending window commands (quit, fullscreen, title).
    fn commands(&mut self) -> Vec<WindowCommand>;
    /// The wake handle the app's worker threads fire when they deliver
    /// something the next frame should service (media result, ingested
    /// path). [`run`] arms it with the event loop; apps that keep one
    /// must return a clone of the *same* instance every call. The
    /// default is a fresh handle nobody fires — pure-polling apps need
    /// nothing more.
    fn waker(&self) -> Waker {
        Waker::new()
    }
}

/// Cross-thread render-loop wake handle (PERFORMANCE-TASKS.md P0.2).
///
/// Worker threads call [`Waker::wake`] after delivering work; the window
/// layer turns that into a single redraw, so background completions repaint
/// promptly *without* the app having to stay in continuous-animation mode
/// while a sweep or an open stdin producer is merely alive. Wakes coalesce:
/// between redraws only the first notifies, so a burst of results costs one
/// event-loop nudge. Before [`run`] arms the handle (or in headless tests)
/// `wake` just sets the flag — nothing is lost, the first armed/serviced
/// frame sees it.
#[derive(Clone)]
pub struct Waker {
    inner: Arc<WakerInner>,
}

struct WakerInner {
    pending: AtomicBool,
    notify: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Waker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(WakerInner {
                pending: AtomicBool::new(false),
                notify: Mutex::new(None),
            }),
        }
    }

    /// Nudge the render loop: one redraw will follow (coalesced).
    /// Callable from any thread, any time.
    pub fn wake(&self) {
        if !self.inner.pending.swap(true, Ordering::AcqRel)
            && let Some(f) = &*self.inner.notify.lock().unwrap()
        {
            f();
        }
    }

    /// Install the event-loop nudge. Fires immediately if a wake raced
    /// ahead of arming (worker finished during GPU init).
    fn arm(&self, f: impl Fn() + Send + 'static) {
        *self.inner.notify.lock().unwrap() = Some(Box::new(f));
        if self.inner.pending.load(Ordering::Acquire) {
            (self.inner.notify.lock().unwrap().as_ref().unwrap())();
        }
    }

    /// Clear the coalescing latch (the redraw that services this batch is
    /// underway); returns whether a wake had arrived.
    fn take_pending(&self) -> bool {
        self.inner.pending.swap(false, Ordering::AcqRel)
    }
}

impl Default for Waker {
    fn default() -> Self {
        Self::new()
    }
}

/// Give the bare binary a real Dock / cmd-tab icon. Any process whose
/// NSApplication activation policy is Regular gets a Dock tile (winit
/// arranges that); the icon itself is set at runtime, mpv-style — no
/// .app bundle required.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::ClassType;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(include_bytes!("../assets/icon.png"));
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        let app = NSApplication::sharedApplication(mtm);
        unsafe { app.setApplicationIconImage(Some(&image)) };
    }
}
#[cfg(not(target_os = "macos"))]
fn set_dock_icon() {}

pub fn run(app: impl App) -> anyhow::Result<()> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    // Arm the app's wake handle: worker-thread wakes become user events,
    // which become single redraws (see `user_event`).
    let waker = app.waker();
    let proxy = event_loop.create_proxy();
    waker.arm(move || {
        let _ = proxy.send_event(()); // closed loop = shutting down
    });
    let mut runner = Runner {
        app,
        waker,
        window: None,
        gpu: None,
        last_frame: Instant::now(),
        cursor: (0.0, 0.0),
        mods: Mods::default(),
        animating: true,
        redraw_at: None,
        occluded: false,
    };
    event_loop.run_app(&mut runner)?;
    Ok(())
}

struct Runner<A: App> {
    app: A,
    /// The app's wake handle (same instance its workers fire) — cleared
    /// each redraw so the next background delivery notifies again.
    waker: Waker,
    window: Option<Arc<Window>>,
    gpu: Option<render::Gpu>,
    last_frame: Instant,
    cursor: (f32, f32),
    /// Current keyboard modifiers, tracked from `ModifiersChanged` and
    /// stamped onto MouseDown (cmd/shift-click).
    mods: Mods,
    animating: bool,
    /// The last frame's one-shot deadline (next live-frame due time);
    /// caps the idle wait so video presents on schedule (P1.4).
    redraw_at: Option<Instant>,
    /// Window fully hidden (minimized / covered / other Space). The
    /// surface won't present, so the continuous-redraw path must not
    /// run — see `about_to_wait`.
    occluded: bool,
}

impl<A: App> Runner<A> {
    fn scale(&self) -> f64 {
        self.window.as_ref().map_or(1.0, |w| w.scale_factor())
    }

    fn apply_commands(&mut self, event_loop: &ActiveEventLoop) {
        for cmd in self.app.commands() {
            match cmd {
                WindowCommand::Quit => event_loop.exit(),
                WindowCommand::ToggleFullscreen { fast } => {
                    if let Some(w) = &self.window {
                        toggle_fullscreen(w, fast);
                    }
                }
                WindowCommand::SetTitle(title) => {
                    if let Some(w) = &self.window {
                        w.set_title(&title);
                    }
                }
                WindowCommand::BeginDrag { path, image } => {
                    if let Some(w) = &self.window {
                        drag::begin(w, &path, image.as_deref());
                    }
                }
            }
        }
    }
}

/// On macOS the two fullscreen flavors are distinct window states:
/// native (`Fullscreen::Borderless`) animates into its own Space, while
/// simple fullscreen just resizes the window over the desktop. Exiting
/// checks both so one `f` always gets you out, whichever mode (or CLI
/// flag) put you in.
fn toggle_fullscreen(w: &Window, fast: bool) {
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::WindowExtMacOS;
        if w.simple_fullscreen() {
            w.set_simple_fullscreen(false);
            set_window_shadow(w, true);
        } else if w.fullscreen().is_some() {
            w.set_fullscreen(None);
        } else if fast {
            // macOS Tahoe (26) draws a ~1px translucent contour around
            // every non-native-fullscreen window. A simple-fullscreen
            // window is still a normal-level window, so it gets the
            // border (native `Fullscreen::Borderless` lives in its own
            // Space and escapes it). AppKit ties that edge to the window
            // shadow — dropping `hasShadow` removes the contour, and a
            // desktop-filling window has no visible shadow to lose.
            w.set_simple_fullscreen(true);
            set_window_shadow(w, false);
        } else {
            w.set_fullscreen(Some(Fullscreen::Borderless(None)));
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        // No Spaces elsewhere: borderless fullscreen already behaves
        // like the fast mode.
        let _ = fast;
        let next = if w.fullscreen().is_some() {
            None
        } else {
            Some(Fullscreen::Borderless(None))
        };
        w.set_fullscreen(next);
    }
}

/// Toggle the native window shadow. On macOS Tahoe this doubles as the
/// only lever over the system-drawn window contour: the border is drawn
/// with the shadow, so `hasShadow(false)` suppresses it (see
/// `toggle_fullscreen`). No-op if the AppKit handle can't be obtained.
#[cfg(target_os = "macos")]
fn set_window_shadow(w: &Window, on: bool) {
    use objc2_app_kit::NSView;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = w.window_handle() else { return };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return;
    };
    let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
    if let Some(window) = view.window() {
        window.setHasShadow(on);
    }
}

impl<A: App> ApplicationHandler for Runner<A> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Must run after the app finishes launching: winit applies the
        // Regular activation policy during launch, which resets any icon
        // set before the event loop starts.
        set_dock_icon();
        let attrs = Window::default_attributes()
            .with_title("switchblade")
            .with_inner_size(LogicalSize::new(1280.0, 800.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let gpu = pollster::block_on(render::Gpu::new(window.clone(), self.app.atlas()))
            .expect("init gpu");
        self.window = Some(window);
        self.gpu = Some(gpu);
        self.last_frame = Instant::now();
        // Startup commands (--fullscreen / --fast-fullscreen queue a
        // toggle) apply before the first frame ever presents windowed.
        self.apply_commands(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Any interaction wakes the loop optimistically; the next frame's
        // `animating` flag decides whether it stays awake.
        if matches!(
            event,
            WindowEvent::KeyboardInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::PinchGesture { .. }
                | WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::Resized(_)
                | WindowEvent::Focused(_)
        ) {
            self.animating = true;
        }
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
                    WinitKey::Named(NamedKey::Tab) => Some(Key::Tab),
                    WinitKey::Character(s) => s.chars().next().map(Key::Char),
                    _ => None,
                };
                if let Some(key) = key {
                    self.app.event(InputEvent::Key {
                        key,
                        repeat: event.repeat,
                    });
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
                self.app.event(InputEvent::Pinch {
                    delta: delta as f32,
                });
            }
            WindowEvent::Focused(focused) => {
                self.app.event(InputEvent::Focus { focused });
            }
            WindowEvent::Occluded(occluded) => {
                log::debug!("window occluded: {occluded}");
                self.occluded = occluded;
                if !occluded {
                    // Repaint promptly when the window comes back.
                    self.animating = true;
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let p = position.to_logical::<f32>(self.scale());
                self.cursor = (p.x, p.y);
                self.app.event(InputEvent::CursorMoved { x: p.x, y: p.y });
                // Drain right here, inside the callback: a drag-out
                // matures on pointer travel, and BeginDrag must run
                // while the OS's current event is still this mouse-drag.
                self.apply_commands(event_loop);
            }
            WindowEvent::ModifiersChanged(m) => {
                let s = m.state();
                self.mods = Mods {
                    shift: s.shift_key(),
                    // ⌘ on macOS, Ctrl elsewhere — the platform's primary
                    // click modifier.
                    cmd: if cfg!(target_os = "macos") {
                        s.super_key()
                    } else {
                        s.control_key()
                    },
                };
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let (x, y) = self.cursor;
                self.app.event(match state {
                    ElementState::Pressed => InputEvent::MouseDown {
                        x,
                        y,
                        mods: self.mods,
                    },
                    ElementState::Released => InputEvent::MouseUp { x, y },
                });
                self.apply_commands(event_loop);
            }
            WindowEvent::RedrawRequested => {
                // This frame services whatever the wake batched; clear the
                // latch so the next background delivery notifies again.
                self.waker.take_pending();
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
                self.animating = frame.animating;
                self.redraw_at = frame.redraw_at;
                gpu.render(&frame, viewport);
                self.apply_commands(event_loop);
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        // A worker delivered work (media result, ingested path): one
        // redraw services and shows it; the frame's own `animating`
        // verdict decides whether the loop stays hot afterwards. While
        // occluded, skip — the pending latch stays set (suppressing event
        // spam), the 10Hz idle tick keeps draining, and un-occlusion
        // repaints promptly anyway.
        if self.occluded {
            return;
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // The redraw cadence lives in `schedule::next_frame` — game-style
        // continuous redraw while anything moves, a slow idle tick
        // otherwise, capped by any live-video deadline. The Tier-A
        // benchmark runner calls the same function so a headless loop
        // paces exactly like this one; see that module for the rules.
        let Some(w) = &self.window else { return };
        match schedule::next_frame(
            self.animating,
            self.occluded,
            self.redraw_at,
            self.last_frame,
            Instant::now(),
        ) {
            schedule::NextFrame::Now { poll } => {
                if poll {
                    event_loop.set_control_flow(ControlFlow::Poll);
                }
                w.request_redraw();
            }
            schedule::NextFrame::At(next) => {
                event_loop.set_control_flow(ControlFlow::WaitUntil(next));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn waker_coalesces_until_serviced() {
        let w = Waker::new();
        let hits = Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        w.arm(move || {
            h.fetch_add(1, Ordering::SeqCst);
        });
        // A burst notifies once…
        w.wake();
        w.wake();
        w.wake();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // …until the redraw clears the latch, then the next wake notifies.
        assert!(w.take_pending());
        w.wake();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert!(w.take_pending());
        assert!(!w.take_pending());
    }

    #[test]
    fn wake_before_arm_fires_on_arm() {
        let w = Waker::new();
        w.wake(); // worker finished during GPU init
        let hits = Arc::new(AtomicU32::new(0));
        let h = hits.clone();
        w.arm(move || {
            h.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unarmed_wake_is_a_noop_flag() {
        let w = Waker::new();
        w.wake();
        assert!(w.take_pending()); // headless apps just see the flag
    }
}
