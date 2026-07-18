//! Native drag-out (macOS): start an `NSDraggingSession` carrying the
//! clip's file URL, so dropping into Finder / onto another app behaves
//! exactly like a drag out of Finder. winit has no drag-*source* API
//! (only drop-target events), so this reaches under it: grab the NSView
//! from the raw window handle and seed the session from the OS's
//! current mouse event — which is why [`begin`] is only valid inside a
//! pointer-event callback (see `WindowCommand::BeginDrag`).

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::OnceCell;
    use std::path::Path;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2::{ClassType, DeclaredClass, declare_class, msg_send_id, mutability};
    use objc2_app_kit::{
        NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
        NSDraggingSource, NSEventType, NSImage, NSView, NSWorkspace,
    };
    use objc2_foundation::{
        MainThreadMarker, NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
        NSURL,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;

    /// Drag ghost width (logical px); height follows the image's aspect.
    const GHOST_W: f64 = 160.0;

    declare_class!(
        /// Minimal `NSDraggingSource`: allow Copy outside the app (Finder
        /// copies, other apps open), nothing within it (we're not a drop
        /// target). Stateless — one instance serves every session.
        struct DragSource;

        unsafe impl ClassType for DragSource {
            type Super = NSObject;
            type Mutability = mutability::MainThreadOnly;
            const NAME: &'static str = "SBDragSource";
        }

        impl DeclaredClass for DragSource {
            type Ivars = ();
        }

        unsafe impl NSObjectProtocol for DragSource {}

        unsafe impl NSDraggingSource for DragSource {
            #[method(draggingSession:sourceOperationMaskForDraggingContext:)]
            unsafe fn source_operation_mask(
                &self,
                _session: &NSDraggingSession,
                context: NSDraggingContext,
            ) -> NSDragOperation {
                if context == NSDraggingContext::OutsideApplication {
                    NSDragOperation::Copy
                } else {
                    NSDragOperation::None
                }
            }
        }
    );

    impl DragSource {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = mtm.alloc::<Self>().set_ivars(());
            unsafe { msg_send_id![super(this), init] }
        }
    }

    thread_local! {
        // The session may outlive the begin call; AppKit's retention of
        // the source is not documented, so keep one alive for the app's
        // lifetime (main thread only, like all of AppKit).
        static SOURCE: OnceCell<Retained<DragSource>> = const { OnceCell::new() };
    }

    pub fn begin(window: &Window, path: &Path, image: Option<&Path>) {
        let Some(mtm) = MainThreadMarker::new() else {
            log::warn!("drag-out requested off the main thread; ignored");
            return;
        };
        // The session must be seeded by the live mouse-drag event; winit
        // dispatches synchronously from the native handler, so the OS's
        // current event is exactly that.
        let app = NSApplication::sharedApplication(mtm);
        let Some(event) = app.currentEvent() else {
            log::warn!("drag-out with no current event; ignored");
            return;
        };
        // AppKit accepts only genuine mouse events as a session seed and
        // THROWS (NSException → process abort from inside the event
        // callback) on anything else. winit sometimes redelivers pointer
        // positions from other event types, so gate hard.
        let ty = unsafe { event.r#type() };
        if !matches!(
            ty,
            NSEventType::LeftMouseDown | NSEventType::LeftMouseDragged
        ) {
            log::debug!("drag-out skipped: current event {ty:?} can't seed a session");
            return;
        }
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::AppKit(h) = handle.as_raw() else {
            return;
        };
        let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };

        // Everything AppKit-throwing runs under exception::catch: a
        // refused session (or any other NSException) must log, not
        // unwind through the native event callback and abort. The
        // closure is defined outside the unsafe call so its own unsafe
        // spots stay individually marked.
        let go = std::panic::AssertUnwindSafe(|| {
            let path_ns = NSString::from_str(&path.to_string_lossy());
            let url = unsafe { NSURL::fileURLWithPath(&path_ns) };
            let item = unsafe {
                NSDraggingItem::initWithPasteboardWriter(
                    NSDraggingItem::alloc(),
                    ProtocolObject::from_ref(&*url),
                )
            };

            // Ghost image: the cached thumb jpeg, else the OS file icon.
            let ghost: Option<Retained<NSImage>> = image
                .and_then(|p| unsafe {
                    NSImage::initWithContentsOfFile(
                        NSImage::alloc(),
                        &NSString::from_str(&p.to_string_lossy()),
                    )
                })
                .or_else(|| unsafe { Some(NSWorkspace::sharedWorkspace().iconForFile(&path_ns)) });
            if let Some(ghost) = &ghost {
                let size = unsafe { ghost.size() };
                let (w, h) = if size.width > 0.0 && size.height > 0.0 {
                    (GHOST_W, GHOST_W * size.height / size.width)
                } else {
                    (GHOST_W, GHOST_W * 9.0 / 16.0)
                };
                // Centered under the pointer, in view coordinates
                // (matching whatever flipped-ness the view uses, since
                // the cursor position converts through the same view).
                let p = unsafe { view.convertPoint_fromView(event.locationInWindow(), None) };
                let frame = NSRect::new(
                    NSPoint::new(p.x - w * 0.5, p.y - h * 0.5),
                    NSSize::new(w, h),
                );
                let contents: &AnyObject = ghost.as_ref();
                unsafe { item.setDraggingFrame_contents(frame, Some(contents)) };
            }

            let items = NSArray::from_vec(vec![item]);
            SOURCE.with(|s| {
                let source = s.get_or_init(|| DragSource::new(mtm));
                unsafe {
                    view.beginDraggingSessionWithItems_event_source(
                        &items,
                        &event,
                        ProtocolObject::from_ref(&**source),
                    );
                }
            });
        });
        let result = unsafe { objc2::exception::catch(go) };
        match result {
            Ok(()) => log::debug!("drag-out session started: {}", path.display()),
            Err(e) => log::warn!("drag-out refused by AppKit (seed event {ty:?}): {e:?}"),
        }
    }
}

#[cfg(target_os = "macos")]
pub use imp::begin;

#[cfg(not(target_os = "macos"))]
pub fn begin(
    _window: &winit::window::Window,
    path: &std::path::Path,
    _image: Option<&std::path::Path>,
) {
    // Drag-source APIs are platform-specific; only macOS is wired up.
    log::debug!(
        "drag-out not supported on this platform: {}",
        path.display()
    );
}
