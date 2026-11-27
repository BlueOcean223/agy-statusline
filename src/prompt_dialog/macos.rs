//! Native macOS Cocoa / AppKit + CoreGraphics renderer for the unified Material 3 prompt dialog.
//!
//! Uses direct Objective-C runtime & CoreGraphics FFI with zero external crates:
//! - Frameless transparent `NSWindow` with authentic WindowServer shadow around the 28px M3 shape.
//! - Subclassed flipped `NSView` (`isFlipped == YES`) executing shared `PaintCmd` commands directly
//!   in sRGB CoreGraphics + AppKit `NSAttributedString` (PingFang SC + SF Pro Retina crispness).
//! - Full ARM64 & x86_64 ABI parity (using `objc_msgSend_stret` for `NSRect` returns on x86_64).

use super::{Color, DIALOG_H, DIALOG_W, FontWeight, PaintCmd, PromptResult, PromptState, TextAlign, Theme};
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::mem::transmute;
use std::ptr::{null, null_mut};

type Id = *mut c_void;
type Class = *mut c_void;
type Sel = *mut c_void;
type CGContextRef = *mut c_void;
type CGPathRef = *mut c_void;

const NIL: Id = null_mut();
const YES: i8 = 1;
const NO: i8 = 0;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct NSPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct NSSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct NSRect {
    origin: NSPoint,
    size: NSSize,
}

impl NSRect {
    const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { origin: NSPoint { x, y }, size: NSSize { width: w, height: h } }
    }
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Class;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_allocateClassPair(superclass: Class, name: *const c_char, extra_bytes: usize) -> Class;
    fn objc_registerClassPair(cls: Class);
    fn class_addMethod(cls: Class, name: Sel, imp: *const c_void, types: *const c_char) -> i8;
    fn objc_msgSend();
    #[cfg(target_arch = "x86_64")]
    fn objc_msgSend_stret();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSFontAttributeName: Id;
    static NSForegroundColorAttributeName: Id;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPathCreateWithRoundedRect(
        rect: NSRect,
        corner_width: f64,
        corner_height: f64,
        transform: *const c_void,
    ) -> CGPathRef;
    fn CGPathRelease(path: CGPathRef);
    fn CGContextSaveGState(ctx: CGContextRef);
    fn CGContextRestoreGState(ctx: CGContextRef);
    fn CGContextAddPath(ctx: CGContextRef, path: CGPathRef);
    fn CGContextFillPath(ctx: CGContextRef);
    fn CGContextStrokePath(ctx: CGContextRef);
    fn CGContextFillEllipseInRect(ctx: CGContextRef, rect: NSRect);
    fn CGContextSetLineWidth(ctx: CGContextRef, width: f64);
    fn CGContextSetLineCap(ctx: CGContextRef, cap: i32);
    fn CGContextSetLineJoin(ctx: CGContextRef, join: i32);
    fn CGContextBeginPath(ctx: CGContextRef);
    fn CGContextMoveToPoint(ctx: CGContextRef, x: f64, y: f64);
    fn CGContextAddLineToPoint(ctx: CGContextRef, x: f64, y: f64);
}

struct AutoreleasePool(*mut c_void);

impl AutoreleasePool {
    fn push() -> Self {
        Self(unsafe { objc_autoreleasePoolPush() })
    }
}

impl Drop for AutoreleasePool {
    fn drop(&mut self) {
        unsafe { objc_autoreleasePoolPop(self.0) };
    }
}

unsafe fn sel(name: &CStr) -> Sel {
    unsafe { sel_registerName(name.as_ptr()) }
}

unsafe fn cls(name: &CStr) -> Class {
    unsafe { objc_getClass(name.as_ptr()) }
}

unsafe fn msg0(obj: Id, s: Sel) -> Id {
    let f: unsafe extern "C" fn(Id, Sel) -> Id = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(obj, s) }
}

unsafe fn msg0_void(obj: Id, s: Sel) {
    let f: unsafe extern "C" fn(Id, Sel) = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(obj, s) }
}

unsafe fn msg0_bool(obj: Id, s: Sel) -> bool {
    let f: unsafe extern "C" fn(Id, Sel) -> i8 = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(obj, s) != 0 }
}

unsafe fn msg0_u16(obj: Id, s: Sel) -> u16 {
    let f: unsafe extern "C" fn(Id, Sel) -> u16 = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(obj, s) }
}

unsafe fn msg0_cstr(obj: Id, s: Sel) -> *const c_char {
    let f: unsafe extern "C" fn(Id, Sel) -> *const c_char = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(obj, s) }
}

unsafe fn msg0_point(obj: Id, s: Sel) -> NSPoint {
    let f: unsafe extern "C" fn(Id, Sel) -> NSPoint = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(obj, s) }
}

unsafe fn msg0_rect(obj: Id, s: Sel) -> NSRect {
    #[cfg(target_arch = "x86_64")]
    {
        let mut out = NSRect::default();
        let f: unsafe extern "C" fn(*mut NSRect, Id, Sel) = unsafe { transmute(objc_msgSend_stret as *const ()) };
        unsafe { f(&mut out, obj, s) };
        out
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let f: unsafe extern "C" fn(Id, Sel) -> NSRect = unsafe { transmute(objc_msgSend as *const ()) };
        unsafe { f(obj, s) }
    }
}

unsafe fn msg1_id(obj: Id, s: Sel, a: Id) -> Id {
    let f: unsafe extern "C" fn(Id, Sel, Id) -> Id = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(obj, s, a) }
}

unsafe fn msg1_void_id(obj: Id, s: Sel, a: Id) {
    let f: unsafe extern "C" fn(Id, Sel, Id) = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(obj, s, a) }
}

unsafe fn msg1_void_bool(obj: Id, s: Sel, a: i8) {
    let f: unsafe extern "C" fn(Id, Sel, i8) = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(obj, s, a) }
}

unsafe fn msg1_void_isize(obj: Id, s: Sel, a: isize) {
    let f: unsafe extern "C" fn(Id, Sel, isize) = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(obj, s, a) }
}

unsafe fn ns_string(text: &str) -> Id {
    let Ok(cstr) = CString::new(text) else {
        return NIL;
    };
    let f: unsafe extern "C" fn(Class, Sel, *const c_char) -> Id = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(cls(c"NSString"), sel(c"stringWithUTF8String:"), cstr.as_ptr()) }
}

unsafe fn ns_color(c: Color) -> Id {
    let (r, g, b, a) = c.to_rgba_f64();
    let f: unsafe extern "C" fn(Class, Sel, f64, f64, f64, f64) -> Id = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(cls(c"NSColor"), sel(c"colorWithSRGBRed:green:blue:alpha:"), r, g, b, a) }
}

unsafe fn ns_font(size: f64, weight: FontWeight) -> Id {
    let w = match weight {
        FontWeight::Regular => 0.0_f64,
        FontWeight::Medium => 0.23_f64,
    };
    let f: unsafe extern "C" fn(Class, Sel, f64, f64) -> Id = unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(cls(c"NSFont"), sel(c"systemFontOfSize:weight:"), size, w) }
}

unsafe fn text_attrs(font: Id, color: Id) -> Id {
    let objs = [font, color];
    let keys = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    let f: unsafe extern "C" fn(Class, Sel, *const Id, *const Id, usize) -> Id =
        unsafe { transmute(objc_msgSend as *const ()) };
    unsafe { f(cls(c"NSDictionary"), sel(c"dictionaryWithObjects:forKeys:count:"), objs.as_ptr(), keys.as_ptr(), 2) }
}

unsafe fn detect_system_theme() -> Theme {
    let _pool = AutoreleasePool::push();
    let defaults = unsafe { msg0(cls(c"NSUserDefaults"), sel(c"standardUserDefaults")) };
    if defaults.is_null() {
        return Theme::Light;
    }
    let key = unsafe { ns_string("AppleInterfaceStyle") };
    let val = unsafe { msg1_id(defaults, sel(c"stringForKey:"), key) };
    if val.is_null() {
        return Theme::Light;
    }
    let ptr = unsafe { msg0_cstr(val, sel(c"UTF8String")) };
    if ptr.is_null() {
        return Theme::Light;
    }
    let s = unsafe { CStr::from_ptr(ptr) }.to_string_lossy();
    if s.eq_ignore_ascii_case("dark") { Theme::Dark } else { Theme::Light }
}

struct Runtime {
    state: PromptState,
    app: Id,
    window: Id,
}

thread_local! {
    static RT: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// Closes the dialog once a `PromptState` handler has stored its outcome.
fn finish_and_stop() {
    RT.with(|cell| {
        if let Some(rt) = cell.borrow_mut().as_mut() {
            let app = rt.app;
            let win = rt.window;
            unsafe {
                msg1_void_id(win, sel(c"orderOut:"), NIL);
                msg1_void_id(app, sel(c"stop:"), NIL);
                // Post a synthetic ApplicationDefined event so [NSApp run] exits immediately.
                let ev_fn: unsafe extern "C" fn(
                    Class,
                    Sel,
                    usize,
                    NSPoint,
                    usize,
                    f64,
                    isize,
                    Id,
                    i16,
                    isize,
                    isize,
                ) -> Id = transmute(objc_msgSend as *const ());
                let ev = ev_fn(
                    cls(c"NSEvent"),
                    sel(c"otherEventWithType:location:modifierFlags:timestamp:windowNumber:context:subtype:data1:data2:"),
                    15, // NSEventTypeApplicationDefined
                    NSPoint::default(),
                    0,
                    0.0,
                    0,
                    NIL,
                    0,
                    0,
                    0,
                );
                let post_fn: unsafe extern "C" fn(Id, Sel, Id, i8) = transmute(objc_msgSend as *const ());
                post_fn(app, sel(c"postEvent:atStart:"), ev, YES);
            }
        }
    });
}

fn set_cursor(interactive: bool) {
    unsafe {
        let cur = if interactive {
            msg0(cls(c"NSCursor"), sel(c"pointingHandCursor"))
        } else {
            msg0(cls(c"NSCursor"), sel(c"arrowCursor"))
        };
        msg0_void(cur, sel(c"set"));
    }
}

unsafe fn event_view_point(view: Id, event: Id) -> (f32, f32) {
    let win_pt = unsafe { msg0_point(event, sel(c"locationInWindow")) };
    let conv: unsafe extern "C" fn(Id, Sel, NSPoint, Id) -> NSPoint = unsafe { transmute(objc_msgSend as *const ()) };
    let local = unsafe { conv(view, sel(c"convertPoint:fromView:"), win_pt, NIL) };
    (local.x as f32, local.y as f32)
}

unsafe fn execute_paint_cmds(ctx: CGContextRef, cmds: &[PaintCmd]) {
    const K_CG_LINE_CAP_ROUND: i32 = 1;
    const K_CG_LINE_JOIN_ROUND: i32 = 1;

    for cmd in cmds {
        match cmd {
            PaintCmd::FillRoundRect { rect, radius, color } => unsafe {
                CGContextSaveGState(ctx);
                msg0_void(ns_color(*color), sel(c"setFill"));
                let r = f64::from(*radius).min(f64::from(rect.w) * 0.5).min(f64::from(rect.h) * 0.5);
                let path = CGPathCreateWithRoundedRect(
                    NSRect::new(f64::from(rect.x), f64::from(rect.y), f64::from(rect.w), f64::from(rect.h)),
                    r,
                    r,
                    null(),
                );
                CGContextAddPath(ctx, path);
                CGContextFillPath(ctx);
                CGPathRelease(path);
                CGContextRestoreGState(ctx);
            },
            PaintCmd::StrokeRoundRect { rect, radius, width, color } => unsafe {
                CGContextSaveGState(ctx);
                msg0_void(ns_color(*color), sel(c"setStroke"));
                CGContextSetLineWidth(ctx, f64::from(*width));
                let r = f64::from(*radius).min(f64::from(rect.w) * 0.5).min(f64::from(rect.h) * 0.5);
                let path = CGPathCreateWithRoundedRect(
                    NSRect::new(f64::from(rect.x), f64::from(rect.y), f64::from(rect.w), f64::from(rect.h)),
                    r,
                    r,
                    null(),
                );
                CGContextAddPath(ctx, path);
                CGContextStrokePath(ctx);
                CGPathRelease(path);
                CGContextRestoreGState(ctx);
            },
            PaintCmd::FillCircle { cx, cy, r, color } => unsafe {
                CGContextSaveGState(ctx);
                msg0_void(ns_color(*color), sel(c"setFill"));
                let rf = f64::from(*r);
                let rect = NSRect::new(f64::from(*cx) - rf, f64::from(*cy) - rf, rf * 2.0, rf * 2.0);
                CGContextFillEllipseInRect(ctx, rect);
                CGContextRestoreGState(ctx);
            },
            PaintCmd::StrokeLine { x1, y1, x2, y2, width, color } => unsafe {
                CGContextSaveGState(ctx);
                msg0_void(ns_color(*color), sel(c"setStroke"));
                CGContextSetLineWidth(ctx, f64::from(*width));
                CGContextSetLineCap(ctx, K_CG_LINE_CAP_ROUND);
                CGContextBeginPath(ctx);
                CGContextMoveToPoint(ctx, f64::from(*x1), f64::from(*y1));
                CGContextAddLineToPoint(ctx, f64::from(*x2), f64::from(*y2));
                CGContextStrokePath(ctx);
                CGContextRestoreGState(ctx);
            },
            PaintCmd::StrokePolyline { pts, width, color } => {
                if pts.len() < 2 {
                    continue;
                }
                unsafe {
                    CGContextSaveGState(ctx);
                    msg0_void(ns_color(*color), sel(c"setStroke"));
                    CGContextSetLineWidth(ctx, f64::from(*width));
                    CGContextSetLineCap(ctx, K_CG_LINE_CAP_ROUND);
                    CGContextSetLineJoin(ctx, K_CG_LINE_JOIN_ROUND);
                    CGContextBeginPath(ctx);
                    CGContextMoveToPoint(ctx, f64::from(pts[0].0), f64::from(pts[0].1));
                    for &(px, py) in &pts[1..] {
                        CGContextAddLineToPoint(ctx, f64::from(px), f64::from(py));
                    }
                    CGContextStrokePath(ctx);
                    CGContextRestoreGState(ctx);
                }
            }
            PaintCmd::DrawText { text, rect, size, weight, color, align } => unsafe {
                let s = ns_string(text);
                let font = ns_font(f64::from(*size), *weight);
                let col = ns_color(*color);
                let attrs = text_attrs(font, col);

                let measure: unsafe extern "C" fn(Id, Sel, Id) -> NSSize = transmute(objc_msgSend as *const ());
                let measured = measure(s, sel(c"sizeWithAttributes:"), attrs);

                let x = match align {
                    TextAlign::Left => f64::from(rect.x),
                    TextAlign::Center => f64::from(rect.x) + (f64::from(rect.w) - measured.width) * 0.5,
                };
                let y = f64::from(rect.y) + (f64::from(rect.h) - measured.height) * 0.5;
                let draw: unsafe extern "C" fn(Id, Sel, NSPoint, Id) = transmute(objc_msgSend as *const ());
                draw(s, sel(c"drawAtPoint:withAttributes:"), NSPoint { x, y }, attrs);
            },
        }
    }
}

extern "C" fn win_can_become_key(_this: Id, _cmd: Sel) -> i8 {
    YES
}

extern "C" fn view_is_flipped(_this: Id, _cmd: Sel) -> i8 {
    YES
}

extern "C" fn view_accepts_first_responder(_this: Id, _cmd: Sel) -> i8 {
    YES
}

extern "C" fn view_accepts_first_mouse(_this: Id, _cmd: Sel, _event: Id) -> i8 {
    YES
}

extern "C" fn view_draw_rect(_this: Id, _cmd: Sel, _dirty: NSRect) {
    let _pool = AutoreleasePool::push();
    let cmds = RT.with(|cell| cell.borrow().as_ref().map(|rt| rt.state.paint_commands()));
    let Some(cmds) = cmds else {
        return;
    };
    unsafe {
        let gctx = msg0(cls(c"NSGraphicsContext"), sel(c"currentContext"));
        if gctx.is_null() {
            return;
        }
        let cg_ctx = msg0(gctx, sel(c"CGContext"));
        if cg_ctx.is_null() {
            return;
        }
        execute_paint_cmds(cg_ctx, &cmds);
    }
}

extern "C" fn view_mouse_moved(this: Id, _cmd: Sel, event: Id) {
    let _pool = AutoreleasePool::push();
    let (x, y) = unsafe { event_view_point(this, event) };
    let (dirty, interactive) = RT.with(|cell| {
        if let Some(rt) = cell.borrow_mut().as_mut() {
            let changed = rt.state.on_mouse_move(x, y);
            (changed, rt.state.hovered.is_interactive())
        } else {
            (false, false)
        }
    });
    set_cursor(interactive);
    if dirty {
        unsafe { msg1_void_bool(this, sel(c"setNeedsDisplay:"), YES) };
    }
}

extern "C" fn view_mouse_exited(this: Id, _cmd: Sel, _event: Id) {
    let _pool = AutoreleasePool::push();
    let dirty = RT.with(|cell| cell.borrow_mut().as_mut().is_some_and(|rt| rt.state.on_mouse_leave()));
    set_cursor(false);
    if dirty {
        unsafe { msg1_void_bool(this, sel(c"setNeedsDisplay:"), YES) };
    }
}

extern "C" fn view_mouse_down(this: Id, _cmd: Sel, event: Id) {
    let _pool = AutoreleasePool::push();
    let (x, y) = unsafe { event_view_point(this, event) };
    let (drag, dirty, win) = RT.with(|cell| {
        if let Some(rt) = cell.borrow_mut().as_mut() {
            let (d, ch) = rt.state.on_mouse_down(x, y);
            (d, ch, rt.window)
        } else {
            (false, false, NIL)
        }
    });
    if drag && !win.is_null() {
        unsafe { msg1_void_id(win, sel(c"performWindowDragWithEvent:"), event) };
    } else if dirty {
        unsafe { msg1_void_bool(this, sel(c"setNeedsDisplay:"), YES) };
    }
}

extern "C" fn view_mouse_dragged(this: Id, _cmd: Sel, event: Id) {
    view_mouse_moved(this, _cmd, event);
}

extern "C" fn view_mouse_up(this: Id, _cmd: Sel, event: Id) {
    let _pool = AutoreleasePool::push();
    let (x, y) = unsafe { event_view_point(this, event) };
    let (outcome, dirty, interactive) = RT.with(|cell| {
        if let Some(rt) = cell.borrow_mut().as_mut() {
            let (out, ch) = rt.state.on_mouse_up(x, y);
            (out, ch, rt.state.hovered.is_interactive())
        } else {
            (None, false, false)
        }
    });
    set_cursor(interactive);
    if outcome.is_some() {
        finish_and_stop();
    } else if dirty {
        unsafe { msg1_void_bool(this, sel(c"setNeedsDisplay:"), YES) };
    }
}

extern "C" fn view_key_down(this: Id, _cmd: Sel, event: Id) {
    let _pool = AutoreleasePool::push();
    let keycode = unsafe { msg0_u16(event, sel(c"keyCode")) };
    let repeat = unsafe { msg0_bool(event, sel(c"isARepeat")) };
    match keycode {
        36 | 76 => {
            // Return / Numpad Enter
            let res = RT.with(|cell| cell.borrow_mut().as_mut().and_then(|rt| rt.state.on_confirm_key(repeat)));
            if res.is_some() {
                finish_and_stop();
            }
        }
        53 => {
            // Escape
            let res = RT.with(|cell| cell.borrow_mut().as_mut().map(|rt| rt.state.on_cancel_key()));
            if res.is_some() {
                finish_and_stop();
            }
        }
        49 => {
            // Space toggles "不再询问"
            let changed = RT.with(|cell| cell.borrow_mut().as_mut().is_some_and(|rt| rt.state.on_toggle_key(repeat)));
            if changed {
                unsafe { msg1_void_bool(this, sel(c"setNeedsDisplay:"), YES) };
            }
        }
        _ => {}
    }
}

unsafe fn register_classes() -> (Class, Class) {
    let mut win_cls = unsafe { cls(c"AgyPromptWindow") };
    if win_cls.is_null() {
        win_cls = unsafe { objc_allocateClassPair(cls(c"NSWindow"), c"AgyPromptWindow".as_ptr(), 0) };
        unsafe {
            class_addMethod(win_cls, sel(c"canBecomeKeyWindow"), win_can_become_key as *const c_void, c"c@:".as_ptr());
            class_addMethod(win_cls, sel(c"canBecomeMainWindow"), win_can_become_key as *const c_void, c"c@:".as_ptr());
            objc_registerClassPair(win_cls);
        }
    }

    let mut view_cls = unsafe { cls(c"AgyPromptView") };
    if view_cls.is_null() {
        view_cls = unsafe { objc_allocateClassPair(cls(c"NSView"), c"AgyPromptView".as_ptr(), 0) };
        unsafe {
            class_addMethod(view_cls, sel(c"isFlipped"), view_is_flipped as *const c_void, c"c@:".as_ptr());
            class_addMethod(
                view_cls,
                sel(c"acceptsFirstResponder"),
                view_accepts_first_responder as *const c_void,
                c"c@:".as_ptr(),
            );
            class_addMethod(
                view_cls,
                sel(c"acceptsFirstMouse:"),
                view_accepts_first_mouse as *const c_void,
                c"c@:@".as_ptr(),
            );
            class_addMethod(
                view_cls,
                sel(c"drawRect:"),
                view_draw_rect as *const c_void,
                c"v@:{CGRect={CGPoint=dd}{CGSize=dd}}".as_ptr(),
            );
            class_addMethod(view_cls, sel(c"mouseMoved:"), view_mouse_moved as *const c_void, c"v@:@".as_ptr());
            class_addMethod(view_cls, sel(c"mouseExited:"), view_mouse_exited as *const c_void, c"v@:@".as_ptr());
            class_addMethod(view_cls, sel(c"mouseDown:"), view_mouse_down as *const c_void, c"v@:@".as_ptr());
            class_addMethod(view_cls, sel(c"mouseDragged:"), view_mouse_dragged as *const c_void, c"v@:@".as_ptr());
            class_addMethod(view_cls, sel(c"mouseUp:"), view_mouse_up as *const c_void, c"v@:@".as_ptr());
            class_addMethod(view_cls, sel(c"keyDown:"), view_key_down as *const c_void, c"v@:@".as_ptr());
            objc_registerClassPair(view_cls);
        }
    }

    (win_cls, view_cls)
}

pub fn show_dialog(theme_override: Option<Theme>) -> PromptResult {
    let _pool = AutoreleasePool::push();
    let theme = theme_override.unwrap_or_else(|| unsafe { detect_system_theme() });

    unsafe {
        let app = msg0(cls(c"NSApplication"), sel(c"sharedApplication"));
        // NSApplicationActivationPolicyAccessory = 1 (no Dock icon, no menu bar takeover).
        msg1_void_isize(app, sel(c"setActivationPolicy:"), 1);

        let (win_cls, view_cls) = register_classes();

        // Position centered horizontally and slightly above vertical midpoint on main screen.
        let screen = msg0(cls(c"NSScreen"), sel(c"mainScreen"));
        let screen_frame = if screen.is_null() {
            NSRect::new(0.0, 0.0, 1440.0, 900.0)
        } else {
            msg0_rect(screen, sel(c"visibleFrame"))
        };
        let w = f64::from(DIALOG_W);
        let h = f64::from(DIALOG_H);
        let x = (screen_frame.origin.x + (screen_frame.size.width - w) * 0.5).round();
        let y = (screen_frame.origin.y + (screen_frame.size.height - h) * 0.56).round();
        let content_rect = NSRect::new(x, y, w, h);

        let win_alloc = msg0(win_cls, sel(c"alloc"));
        let init_win: unsafe extern "C" fn(Id, Sel, NSRect, usize, usize, i8) -> Id =
            transmute(objc_msgSend as *const ());
        // styleMask = 0 (NSWindowStyleMaskBorderless), backing = 2 (NSBackingStoreBuffered)
        let window = init_win(win_alloc, sel(c"initWithContentRect:styleMask:backing:defer:"), content_rect, 0, 2, NO);
        if window.is_null() {
            return PromptResult::default();
        }

        msg1_void_bool(window, sel(c"setOpaque:"), NO);
        let clear = msg0(cls(c"NSColor"), sel(c"clearColor"));
        msg1_void_id(window, sel(c"setBackgroundColor:"), clear);
        msg1_void_bool(window, sel(c"setHasShadow:"), YES);
        msg1_void_bool(window, sel(c"setAcceptsMouseMovedEvents:"), YES);
        // NSFloatingWindowLevel = 3 (floats above Antigravity cleanly).
        msg1_void_isize(window, sel(c"setLevel:"), 3);
        msg1_void_id(window, sel(c"setTitle:"), ns_string("AGY Statusline"));

        let view_alloc = msg0(view_cls, sel(c"alloc"));
        let init_view: unsafe extern "C" fn(Id, Sel, NSRect) -> Id = transmute(objc_msgSend as *const ());
        let view = init_view(view_alloc, sel(c"initWithFrame:"), NSRect::new(0.0, 0.0, w, h));

        // Add tracking area for smooth hover highlights & M3 checkbox tooltip.
        let ta_alloc = msg0(cls(c"NSTrackingArea"), sel(c"alloc"));
        let init_ta: unsafe extern "C" fn(Id, Sel, NSRect, usize, Id, Id) -> Id = transmute(objc_msgSend as *const ());
        // MouseEnteredAndExited (0x01) | MouseMoved (0x02) | ActiveAlways (0x80) | InVisibleRect (0x200)
        let ta = init_ta(
            ta_alloc,
            sel(c"initWithRect:options:owner:userInfo:"),
            NSRect::new(0.0, 0.0, w, h),
            0x01 | 0x02 | 0x80 | 0x200,
            view,
            NIL,
        );
        msg1_void_id(view, sel(c"addTrackingArea:"), ta);

        msg1_void_id(window, sel(c"setContentView:"), view);
        msg1_id(window, sel(c"makeFirstResponder:"), view);

        RT.with(|cell| {
            *cell.borrow_mut() = Some(Runtime { state: PromptState::new(theme), app, window });
        });

        // Draw first frame and let WindowServer compute the exact 28px rounded shadow.
        msg1_void_bool(view, sel(c"setNeedsDisplay:"), YES);
        msg0_void(window, sel(c"displayIfNeeded"));
        msg0_void(window, sel(c"invalidateShadow"));

        msg1_void_bool(app, sel(c"activateIgnoringOtherApps:"), YES);
        msg1_void_id(window, sel(c"makeKeyAndOrderFront:"), NIL);

        msg0_void(app, sel(c"run"));

        let result = RT.with(|cell| cell.borrow_mut().take().and_then(|rt| rt.state.outcome)).unwrap_or_default();
        // Keep `view` alive for the full modal lifetime (already retained by `window`).
        let _ = view;
        result
    }
}
