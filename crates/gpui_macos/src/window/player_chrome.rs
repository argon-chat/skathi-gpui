use super::*;
use objc2::MainThreadOnly;
use objc2_app_kit::{NSAnimationContext, NSAppearanceCustomization};

#[derive(Default)]
pub(super) struct PlayerChrome {
    pub(super) parked: Option<Retained<NSWindow>>,
    pub(super) buttons_hidden: bool,
    pub(super) button_opacity: Option<f32>,
    pub(super) transitioning: bool,
    switching_panel: bool,
    pub(super) windowed_behavior: Option<NSWindowCollectionBehavior>,
    pub(super) cover: Option<Retained<NSView>>,
}

pub(super) extern "C" fn cannot_become_main(_: &AnyObject, _: Sel) -> Bool {
    Bool::NO
}

pub(super) extern "C" fn sheet_ended(this: &AnyObject, _: Sel, _: *mut AnyObject) {
    let state = unsafe { get_window_state(this) };
    sync_panel(&state);
}

#[expect(
    deprecated,
    reason = "Older dragging sources still offer NSFilenamesPboardType"
)]
pub(super) fn register_drag_types(window: &NSWindow) {
    unsafe {
        window.registerForDraggedTypes(&NSArray::from_slice(&[
            NSFilenamesPboardType,
            &NSString::from_str("public.file-url"),
            &NSString::from_str("public.utf8-plain-text"),
            &NSString::from_str("public.url"),
            &NSString::from_str("public.html"),
            &NSString::from_str(crate::drag_source::NATIVE_MARKER),
        ]));
    }
}

pub(super) fn show_buttons(state: &MacWindowState, visible: bool) {
    if let Some(buttons) = state.traffic_light_buttons() {
        let mut changed = false;
        for button in [buttons.close, buttons.minimize, buttons.zoom] {
            let opacity = f64::from(state.player_chrome.button_opacity.unwrap_or(1.));
            if button.alphaValue() != opacity {
                button.setAlphaValue(opacity);
            }
            if button.isHidden() == visible {
                button.setHidden(!visible);
                button.updateTrackingAreas();
                changed = true;
            }
        }
        if changed {
            let view = unsafe { &*state.native_view.as_ptr().cast::<NSView>() };
            state.native_window().invalidateCursorRectsForView(view);
        }
    }
}

pub(super) fn buttons_rect(state: &MacWindowState) -> Option<Bounds<Pixels>> {
    if state.player_chrome.buttons_hidden {
        return None;
    }
    let view = unsafe { &*state.native_view.as_ptr().cast::<NSView>() };
    let buttons = state.traffic_light_buttons()?;
    let mut bounds: Option<Bounds<Pixels>> = None;
    for button in [buttons.close, buttons.minimize, buttons.zoom] {
        if button.isHiddenOrHasHiddenAncestor()
            || !button
                .window()
                .is_some_and(|w| std::ptr::eq(&*w, state.native_window()))
        {
            continue;
        }
        let visible = button.visibleRect();
        if visible.size.width <= 0. || visible.size.height <= 0. {
            continue;
        }
        let rect = button.convertRect_toView(visible, Some(view));
        let y = if view.isFlipped() {
            rect.origin.y
        } else {
            view.bounds().size.height - rect.origin.y - rect.size.height
        };
        let b = Bounds::new(
            point(px(rect.origin.x as f32), px(y as f32)),
            size(px(rect.size.width as f32), px(rect.size.height as f32)),
        );
        bounds = Some(bounds.map_or(b, |prior| prior.union(&b)));
    }
    bounds
}

fn new_panel(state: &Rc<Mutex<MacWindowState>>, source: &NSWindow) -> Retained<NSWindow> {
    // SAFETY: our registered NSPanel subclass inherits the GPUI delegate and window-state ivar.
    // AppKit assigns nonactivating keyboard focus when initializing the panel with this mask.
    unsafe {
        let allocated: *mut AnyObject = msg_send![WORKSPACE_PANEL_CLASS, alloc];
        let raw: *mut AnyObject = msg_send![allocated,
            initWithContentRect: source.contentRectForFrameRect(source.frame()),
            styleMask: source.styleMask() | NSWindowStyleMask::NonactivatingPanel,
            backing: NSBackingStoreType::Buffered, defer: Bool::NO, screen: source.screen().as_deref()
        ];
        let panel = Retained::from_raw(raw.cast::<NSWindow>()).expect("AppKit workspace panel");
        (*raw)
            .class()
            .instance_variable(WINDOW_STATE_IVAR)
            .unwrap()
            .load_ptr::<*mut c_void>(&*raw)
            .write(Rc::into_raw(state.clone()).cast_mut().cast());
        panel.setReleasedWhenClosed(false);
        panel.setHidesOnDeactivate(false);
        register_drag_types(&panel);
        panel
    }
}

fn copy_window_settings(source: &NSWindow, target: &NSWindow, is_panel: bool) {
    let mut style = source.styleMask();
    style.set(NSWindowStyleMask::NonactivatingPanel, is_panel);
    target.setStyleMask(style);
    target.setTitle(&source.title());
    target.setTitleVisibility(source.titleVisibility());
    target.setTitlebarAppearsTransparent(source.titlebarAppearsTransparent());
    target.setDocumentEdited(source.isDocumentEdited());
    target.setAppearance(source.appearance().as_deref());
    target.setMovable(source.isMovable());
    target.setMovableByWindowBackground(source.isMovableByWindowBackground());
    target.setAcceptsMouseMovedEvents(source.acceptsMouseMovedEvents());
    target.setIgnoresMouseEvents(source.ignoresMouseEvents());
    target.setOpaque(source.isOpaque());
    target.setBackgroundColor(Some(&source.backgroundColor()));
    target.setHasShadow(source.hasShadow());
    target.setAlphaValue(source.alphaValue());
    target.setLevel(source.level());
    target.setCollectionBehavior(source.collectionBehavior());
    target.setContentMaxSize(source.contentMaxSize());
    target.setContentMinSize(source.contentMinSize());
    let aspect = source.contentAspectRatio();
    if aspect.width > 0. && aspect.height > 0. {
        target.setContentAspectRatio(aspect);
    } else {
        target.setContentResizeIncrements(source.contentResizeIncrements());
    }
    target.setSharingType(source.sharingType());
    if let (Some(old), Some(new)) = (
        source.standardWindowButton(NSWindowButton::ZoomButton),
        target.standardWindowButton(NSWindowButton::ZoomButton),
    ) {
        new.setEnabled(old.isEnabled());
    }
    target.setFrame_display(source.frame(), false);
}

pub(super) fn sync_panel(state: &Rc<Mutex<MacWindowState>>) {
    let (source_ptr, enter, other) = {
        let mut s = state.lock();
        let w = s.native_window();
        let enter = s.spaces_before_override.is_some()
            && s.player_chrome.windowed_behavior.is_none()
            && w.level() == NSFloatingWindowLevel;
        let current = unsafe { (*s.native_window).class() };
        if s.player_chrome.transitioning
            || s.player_chrome.switching_panel
            || s.is_fullscreen()
            || w.attachedSheet().is_some()
            || std::ptr::eq(current, unsafe { WORKSPACE_PANEL_CLASS }) == enter
            || (enter
                && (s.sheet_parent.is_some()
                    || w.styleMask().contains(NSWindowStyleMask::NonactivatingPanel)
                    || (!std::ptr::eq(current, unsafe { WINDOW_CLASS })
                        && !std::ptr::eq(current, unsafe { PANEL_CLASS }))))
        {
            return;
        }
        s.player_chrome.switching_panel = true;
        (s.native_window, enter, s.player_chrome.parked.take())
    };

    // SAFETY: MacWindow owns one +1 reference to its active window. Move that reference to
    // parked and transfer the other +1 to MacWindow. Both native objects stay live until close.
    let source = unsafe { Retained::from_raw(source_ptr.cast::<NSWindow>()).unwrap() };
    let target = other.unwrap_or_else(|| new_panel(state, &source));
    let content = source.contentView().expect("GPUI window content view");
    let responder = source.firstResponder();
    let key = source.isKeyWindow();
    let visible = source.isVisible();
    let target_ptr = Retained::as_ptr(&target).cast_mut().cast::<AnyObject>();
    source.setDelegate(None);
    copy_window_settings(&source, &target, enter);
    {
        let mut s = state.lock();
        s.restore_traffic_light();
        s.native_window = target_ptr;
        s.traffic_light_frames = None;
    }
    source.setContentView(None);
    target.setContentView(Some(&content));
    unsafe {
        let _: () = msg_send![target_ptr, setDelegate: target_ptr];
    }
    let moved_responder = responder.as_deref().filter(|r| unsafe {
        let is_view: bool = msg_send![*r, isKindOfClass: class!(NSView)];
        is_view && {
            let owner: *mut AnyObject = msg_send![*r, window];
            owner == target_ptr
        }
    });
    let input_view = state.lock().native_view;
    let fallback = unsafe { &*input_view.as_ptr().cast::<NSView>() };
    target.makeFirstResponder(Some(moved_responder.unwrap_or(fallback)));
    if visible {
        target.orderWindow_relativeTo(NSWindowOrderingMode::Above, source.windowNumber());
        if key {
            target.makeKeyWindow();
        }
    }
    source.orderOut(None);
    let _owned_by_mac_window = Retained::into_raw(target);
    let mut s = state.lock();
    s.player_chrome.parked = Some(source);
    s.player_chrome.switching_panel = false;
    s.move_traffic_light();
}

pub(super) fn toggle_fullscreen(state: &Rc<Mutex<MacWindowState>>, fade: bool) {
    let state = state.clone();
    let executor = state.lock().foreground_executor.clone();
    executor
        .spawn(async move {
            {
                let mut s = state.lock();
                if s.closed.load(Ordering::Acquire) {
                    return;
                }
                if std::ptr::eq(unsafe { (*s.native_window).class() }, unsafe {
                    WORKSPACE_PANEL_CLASS
                }) {
                    s.player_chrome.windowed_behavior =
                        Some(s.native_window().collectionBehavior());
                }
            }
            sync_panel(&state);
            let (raw, behavior) = {
                let s = state.lock();
                (s.native_window, s.player_chrome.windowed_behavior)
            };
            let window = unsafe { &*raw.cast::<NSWindow>() };
            if let Some(behavior) = behavior {
                let auxiliary = NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
                    | NSWindowCollectionBehavior::FullScreenNone;
                window.setCollectionBehavior(
                    (behavior & !auxiliary) | NSWindowCollectionBehavior::FullScreenPrimary,
                );
            }
            if let Some(content) = window.contentView().filter(|_| fade) {
                let cover = NSView::initWithFrame(
                    NSView::alloc(MainThreadMarker::new().unwrap()),
                    content.bounds(),
                );
                cover.setAutoresizingMask(
                    NSAutoresizingMaskOptions::ViewWidthSizable
                        | NSAutoresizingMaskOptions::ViewHeightSizable,
                );
                cover.setWantsLayer(true);
                unsafe {
                    let layer: *mut AnyObject = msg_send![&*cover, layer];
                    let _: () =
                        msg_send![layer, setBackgroundColor: &*NSColor::blackColor().CGColor()];
                }
                content.addSubview(&cover);
                state.lock().player_chrome.cover = Some(cover);
                window.displayIfNeeded();
                unsafe {
                    let _: () = msg_send![class!(CATransaction), flush];
                }
            }
            unsafe {
                let _: () = msg_send![raw, toggleFullScreen: ptr::null_mut::<AnyObject>()];
            }
        })
        .detach();
}

pub(super) fn finish_fullscreen(state: &Rc<Mutex<MacWindowState>>, fullscreen: bool) {
    let (cover, executor, behavior, raw) = {
        let mut s = state.lock();
        s.player_chrome.transitioning = false;
        if s.player_chrome.button_opacity.is_some() {
            show_buttons(&s, !s.player_chrome.buttons_hidden);
        }
        let behavior = if fullscreen {
            None
        } else {
            s.player_chrome.windowed_behavior.take()
        };
        (
            s.player_chrome.cover.take(),
            s.foreground_executor.clone(),
            behavior,
            s.native_window,
        )
    };
    if let Some(cover) = cover {
        NSAnimationContext::runAnimationGroup_completionHandler(
            &RcBlock::new(|context: NonNull<NSAnimationContext>| unsafe {
                context.as_ref().setDuration(0.15);
                let animator: *mut AnyObject = msg_send![&*cover, animator];
                let _: () = msg_send![animator, setAlphaValue: 0f64];
            }),
            Some(&RcBlock::new({
                let cover = cover.clone();
                move || cover.removeFromSuperview()
            })),
        );
    }
    if let Some(behavior) = behavior {
        unsafe { &*raw.cast::<NSWindow>() }.setCollectionBehavior(behavior);
    }
    let weak = Rc::downgrade(state);
    executor
        .spawn(async move {
            if let Some(s) = weak.upgrade() {
                if !s.lock().closed.load(Ordering::Acquire) {
                    sync_panel(&s);
                }
            }
        })
        .detach();
}
