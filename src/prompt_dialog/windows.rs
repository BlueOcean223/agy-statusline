//! Native Windows Win32 `WS_EX_LAYERED` + GDI+ `PixelFormat32bppPARGB` renderer for the
//! unified Google Material 3 Pure (`G1`) relaunch prompt dialog.
//!
//! Uses direct Win32 + GDI+ Flat C API FFI with zero external crates:
//! - True anti-aliased 28px M3 ExtraLarge corners and soft multi-layer drop shadow on both
//!   Windows 10 and Windows 11 via `UpdateLayeredWindow` (`ULW_ALPHA`).
//! - Per-Monitor DPI v2 scaling (`GetDpiForWindow` + `WM_DPICHANGED`), rendering shared
//!   `PaintCmd` coordinates identically to macOS at 100%/125%/150%/200% scaling.
//! - Automatic Light/Dark detection via `HKCU\...\Themes\Personalize\AppsUseLightTheme`.
//! - Falls back gracefully to native `MessageBoxW` if GDI+ or layered window init fails.

use super::{Color, DIALOG_H, DIALOG_W, FontWeight, PaintCmd, PromptResult, PromptState, Rect, TextAlign, Theme};
use std::cell::RefCell;
use std::ffi::{c_char, c_void};
use std::mem::{size_of, transmute, zeroed};
use std::ptr::{null, null_mut};

/// Logical transparent shadow gutter surrounding the 432x240 Material 3 card.
const SHADOW_PAD: f32 = 28.0;
const CANVAS_W: f32 = DIALOG_W + SHADOW_PAD * 2.0;
const CANVAS_H: f32 = DIALOG_H + SHADOW_PAD * 2.0;

const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
const RRF_RT_REG_DWORD: u32 = 0x0000_0010;

const WS_POPUP: u32 = 0x8000_0000;
const WS_EX_LAYERED: u32 = 0x0008_0000;
const WS_EX_TOPMOST: u32 = 0x0000_0008;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;

const SW_SHOW: i32 = 5;
const ULW_ALPHA: u32 = 0x0000_0002;
const AC_SRC_OVER: u8 = 0x00;
const AC_SRC_ALPHA: u8 = 0x01;
const BI_RGB: u32 = 0;
const DIB_RGB_COLORS: u32 = 0;
const LOGPIXELSX: i32 = 88;

const WM_DESTROY: u32 = 0x0002;
const WM_CLOSE: u32 = 0x0010;
const WM_SETCURSOR: u32 = 0x0020;
const WM_NCLBUTTONDOWN: u32 = 0x00A1;
const WM_KEYDOWN: u32 = 0x0100;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_DPICHANGED: u32 = 0x02E0;

const HTCLIENT: u16 = 1;
const HTCAPTION: usize = 2;
const TME_LEAVE: u32 = 0x0000_0002;

const VK_RETURN: usize = 0x0D;
const VK_ESCAPE: usize = 0x1B;
const VK_SPACE: usize = 0x20;

const IDC_ARROW: *const u16 = 32512usize as *const u16;
const IDC_HAND: *const u16 = 32649usize as *const u16;
const MONITOR_DEFAULTTONEAREST: u32 = 2;

const MB_OKCANCEL: u32 = 0x0000_0001;
const MB_ICONINFORMATION: u32 = 0x0000_0040;
const MB_SETFOREGROUND: u32 = 0x0001_0000;
const MB_TOPMOST: u32 = 0x0004_0000;
const IDOK: i32 = 1;

// GDI+ Flat API constants (verified against GdiplusEnums.h & GdiplusPixelFormats.h)
const PIXEL_FORMAT_32BPP_PARGB: i32 = 0x000E_200B;
const SMOOTHING_MODE_ANTI_ALIAS: i32 = 4;
const PIXEL_OFFSET_MODE_HIGH_QUALITY: i32 = 2;
const TEXT_RENDERING_HINT_ANTI_ALIAS_GRID_FIT: i32 = 3;
const UNIT_PIXEL: i32 = 2;
const FONT_STYLE_REGULAR: i32 = 0;
const FONT_STYLE_BOLD: i32 = 1;
const LINE_CAP_ROUND: i32 = 2;
const LINE_JOIN_ROUND: i32 = 2;
const STRING_ALIGNMENT_NEAR: i32 = 0;
const STRING_ALIGNMENT_CENTER: i32 = 1;
const FILL_MODE_WINDING: i32 = 1;
const MATRIX_ORDER_PREPEND: i32 = 0;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Size {
    cx: i32,
    cy: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WinRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PointF {
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RectF {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[repr(C)]
struct BlendFunction {
    blend_op: u8,
    blend_flags: u8,
    source_constant_alpha: u8,
    alpha_format: u8,
}

#[repr(C)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    bmi_header: BitmapInfoHeader,
    bmi_colors: [u32; 1],
}

#[repr(C)]
struct MonitorInfo {
    cb_size: u32,
    rc_monitor: WinRect,
    rc_work: WinRect,
    dw_flags: u32,
}

#[repr(C)]
struct TrackMouseEventStruct {
    cb_size: u32,
    dw_flags: u32,
    hwnd_track: *mut c_void,
    dw_hover_time: u32,
}

#[repr(C)]
struct WndClassExW {
    cb_size: u32,
    style: u32,
    lpfn_wnd_proc: unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    h_instance: *mut c_void,
    h_icon: *mut c_void,
    h_cursor: *mut c_void,
    hbr_background: *mut c_void,
    lpsz_menu_name: *const u16,
    lpsz_class_name: *const u16,
    h_icon_sm: *mut c_void,
}

#[repr(C)]
struct Msg {
    hwnd: *mut c_void,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt: Point,
}

#[repr(C)]
struct GdiplusStartupInput {
    gdiplus_version: u32,
    debug_event_callback: *const c_void,
    suppress_background_thread: i32,
    suppress_external_codecs: i32,
}

type SetThreadDpiAwarenessContextFn = unsafe extern "system" fn(isize) -> isize;
type GetDpiForWindowFn = unsafe extern "system" fn(*mut c_void) -> u32;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(lp_module_name: *const u16) -> *mut c_void;
    fn GetProcAddress(h_module: *mut c_void, lp_proc_name: *const c_char) -> *const c_void;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        hkey: isize,
        lp_sub_key: *const u16,
        lp_value: *const u16,
        dw_flags: u32,
        pdw_type: *mut u32,
        pv_data: *mut c_void,
        pcb_data: *mut u32,
    ) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassExW(lpwcx: *const WndClassExW) -> u16;
    fn UnregisterClassW(lp_class_name: *const u16, h_instance: *mut c_void) -> i32;
    fn CreateWindowExW(
        dw_ex_style: u32,
        lp_class_name: *const u16,
        lp_window_name: *const u16,
        dw_style: u32,
        x: i32,
        y: i32,
        n_width: i32,
        n_height: i32,
        h_wnd_parent: *mut c_void,
        h_menu: *mut c_void,
        h_instance: *mut c_void,
        lp_param: *mut c_void,
    ) -> *mut c_void;
    fn DestroyWindow(h_wnd: *mut c_void) -> i32;
    fn ShowWindow(h_wnd: *mut c_void, n_cmd_show: i32) -> i32;
    fn SetForegroundWindow(h_wnd: *mut c_void) -> i32;
    fn GetWindowRect(h_wnd: *mut c_void, lp_rect: *mut WinRect) -> i32;
    fn GetCursorPos(lp_point: *mut Point) -> i32;
    fn MonitorFromPoint(pt: Point, dw_flags: u32) -> *mut c_void;
    fn GetMonitorInfoW(h_monitor: *mut c_void, lpmi: *mut MonitorInfo) -> i32;
    fn LoadCursorW(h_instance: *mut c_void, lp_cursor_name: *const u16) -> *mut c_void;
    fn SetCursor(h_cursor: *mut c_void) -> *mut c_void;
    fn SetCapture(h_wnd: *mut c_void) -> *mut c_void;
    fn ReleaseCapture() -> i32;
    fn TrackMouseEvent(lp_event_track: *mut TrackMouseEventStruct) -> i32;
    fn SendMessageW(h_wnd: *mut c_void, msg: u32, w_param: usize, l_param: isize) -> isize;
    fn DefWindowProcW(h_wnd: *mut c_void, msg: u32, w_param: usize, l_param: isize) -> isize;
    fn GetMessageW(lp_msg: *mut Msg, h_wnd: *mut c_void, w_msg_filter_min: u32, w_msg_filter_max: u32) -> i32;
    fn TranslateMessage(lp_msg: *const Msg) -> i32;
    fn DispatchMessageW(lp_msg: *const Msg) -> isize;
    fn PostQuitMessage(n_exit_code: i32);
    fn GetDC(h_wnd: *mut c_void) -> *mut c_void;
    fn ReleaseDC(h_wnd: *mut c_void, h_dc: *mut c_void) -> i32;
    fn UpdateLayeredWindow(
        h_wnd: *mut c_void,
        hdc_dst: *mut c_void,
        ppt_dst: *const Point,
        psize: *const Size,
        hdc_src: *mut c_void,
        ppt_src: *const Point,
        cr_key: u32,
        pblend: *const BlendFunction,
        dw_flags: u32,
    ) -> i32;
    fn MessageBoxW(h_wnd: *mut c_void, lp_text: *const u16, lp_caption: *const u16, u_type: u32) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(hdc: *mut c_void) -> *mut c_void;
    fn DeleteDC(hdc: *mut c_void) -> i32;
    fn CreateDIBSection(
        hdc: *mut c_void,
        pbmi: *const BitmapInfo,
        usage: u32,
        ppv_bits: *mut *mut c_void,
        h_section: *mut c_void,
        offset: u32,
    ) -> *mut c_void;
    fn SelectObject(hdc: *mut c_void, h: *mut c_void) -> *mut c_void;
    fn DeleteObject(ho: *mut c_void) -> i32;
    fn GetDeviceCaps(hdc: *mut c_void, index: i32) -> i32;
}

#[link(name = "gdiplus")]
unsafe extern "system" {
    fn GdiplusStartup(token: *mut usize, input: *const GdiplusStartupInput, output: *mut c_void) -> i32;
    fn GdiplusShutdown(token: usize);
    fn GdipCreateBitmapFromScan0(
        width: i32,
        height: i32,
        stride: i32,
        format: i32,
        scan0: *mut u8,
        bitmap: *mut *mut c_void,
    ) -> i32;
    fn GdipDisposeImage(image: *mut c_void) -> i32;
    fn GdipGetImageGraphicsContext(image: *mut c_void, graphics: *mut *mut c_void) -> i32;
    fn GdipDeleteGraphics(graphics: *mut c_void) -> i32;
    fn GdipSetSmoothingMode(graphics: *mut c_void, smoothing_mode: i32) -> i32;
    fn GdipSetPixelOffsetMode(graphics: *mut c_void, pixel_offset_mode: i32) -> i32;
    fn GdipSetTextRenderingHint(graphics: *mut c_void, mode: i32) -> i32;
    fn GdipScaleWorldTransform(graphics: *mut c_void, sx: f32, sy: f32, order: i32) -> i32;
    fn GdipTranslateWorldTransform(graphics: *mut c_void, dx: f32, dy: f32, order: i32) -> i32;
    fn GdipCreatePath(brush_mode: i32, path: *mut *mut c_void) -> i32;
    fn GdipDeletePath(path: *mut c_void) -> i32;
    fn GdipAddPathArc(path: *mut c_void, x: f32, y: f32, width: f32, height: f32, start: f32, sweep: f32) -> i32;
    fn GdipClosePathFigure(path: *mut c_void) -> i32;
    fn GdipFillPath(graphics: *mut c_void, brush: *mut c_void, path: *mut c_void) -> i32;
    fn GdipDrawPath(graphics: *mut c_void, pen: *mut c_void, path: *mut c_void) -> i32;
    fn GdipFillEllipse(graphics: *mut c_void, brush: *mut c_void, x: f32, y: f32, width: f32, height: f32) -> i32;
    fn GdipDrawLine(graphics: *mut c_void, pen: *mut c_void, x1: f32, y1: f32, x2: f32, y2: f32) -> i32;
    fn GdipDrawLines(graphics: *mut c_void, pen: *mut c_void, points: *const PointF, count: i32) -> i32;
    fn GdipCreateSolidFill(color: u32, brush: *mut *mut c_void) -> i32;
    fn GdipDeleteBrush(brush: *mut c_void) -> i32;
    fn GdipCreatePen1(color: u32, width: f32, unit: i32, pen: *mut *mut c_void) -> i32;
    fn GdipSetPenStartCap(pen: *mut c_void, cap: i32) -> i32;
    fn GdipSetPenEndCap(pen: *mut c_void, cap: i32) -> i32;
    fn GdipSetPenLineJoin(pen: *mut c_void, join: i32) -> i32;
    fn GdipDeletePen(pen: *mut c_void) -> i32;
    fn GdipCreateFontFamilyFromName(
        name: *const u16,
        font_collection: *mut c_void,
        font_family: *mut *mut c_void,
    ) -> i32;
    fn GdipDeleteFontFamily(font_family: *mut c_void) -> i32;
    fn GdipCreateFont(font_family: *const c_void, em_size: f32, style: i32, unit: i32, font: *mut *mut c_void) -> i32;
    fn GdipDeleteFont(font: *mut c_void) -> i32;
    fn GdipStringFormatGetGenericTypographic(format: *mut *mut c_void) -> i32;
    fn GdipCloneStringFormat(format: *const c_void, new_format: *mut *mut c_void) -> i32;
    fn GdipDeleteStringFormat(format: *mut c_void) -> i32;
    fn GdipSetStringFormatAlign(format: *mut c_void, align: i32) -> i32;
    fn GdipSetStringFormatLineAlign(format: *mut c_void, align: i32) -> i32;
    fn GdipDrawString(
        graphics: *mut c_void,
        string: *const u16,
        length: i32,
        font: *const c_void,
        layout_rect: *const RectF,
        string_format: *const c_void,
        brush: *const c_void,
    ) -> i32;
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct DpiGuard {
    set_fn: Option<SetThreadDpiAwarenessContextFn>,
    prev: isize,
}

impl DpiGuard {
    fn enter_per_monitor_v2() -> Self {
        let user32_name = to_wide_null("user32.dll");
        let user32 = unsafe { GetModuleHandleW(user32_name.as_ptr()) };
        if user32.is_null() {
            return Self { set_fn: None, prev: 0 };
        }
        let proc = unsafe { GetProcAddress(user32, c"SetThreadDpiAwarenessContext".as_ptr()) };
        if proc.is_null() {
            return Self { set_fn: None, prev: 0 };
        }
        let set_fn = unsafe { transmute::<*const c_void, SetThreadDpiAwarenessContextFn>(proc) };
        let prev = unsafe { set_fn(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self { set_fn: if prev != 0 { Some(set_fn) } else { None }, prev }
    }
}

impl Drop for DpiGuard {
    fn drop(&mut self) {
        if let Some(set_fn) = self.set_fn {
            unsafe {
                set_fn(self.prev);
            }
        }
    }
}

fn detect_system_theme() -> Theme {
    let subkey = to_wide_null(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = to_wide_null("AppsUseLightTheme");
    let mut data: u32 = 1;
    let mut data_size: u32 = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut data as *mut u32).cast::<c_void>(),
            &mut data_size,
        )
    };
    if status == 0 && data == 0 { Theme::Dark } else { Theme::Light }
}

fn window_dpi(hwnd: *mut c_void) -> u32 {
    let user32_name = to_wide_null("user32.dll");
    let user32 = unsafe { GetModuleHandleW(user32_name.as_ptr()) };
    if !user32.is_null() {
        let proc = unsafe { GetProcAddress(user32, c"GetDpiForWindow".as_ptr()) };
        if !proc.is_null() {
            let get_dpi = unsafe { transmute::<*const c_void, GetDpiForWindowFn>(proc) };
            let dpi = unsafe { get_dpi(hwnd) };
            if dpi >= 72 {
                return dpi;
            }
        }
    }
    let hdc = unsafe { GetDC(null_mut()) };
    let dpi = if hdc.is_null() { 96 } else { unsafe { GetDeviceCaps(hdc, LOGPIXELSX) }.max(96) as u32 };
    if !hdc.is_null() {
        unsafe { ReleaseDC(null_mut(), hdc) };
    }
    dpi
}

unsafe fn create_round_rect_path(rect: Rect, radius: f32) -> *mut c_void {
    let mut path = null_mut();
    if unsafe { GdipCreatePath(FILL_MODE_WINDING, &mut path) } != 0 || path.is_null() {
        return null_mut();
    }
    let r = radius.min(rect.w * 0.5).min(rect.h * 0.5).max(0.5);
    let d = r * 2.0;
    unsafe {
        GdipAddPathArc(path, rect.x, rect.y, d, d, 180.0, 90.0);
        GdipAddPathArc(path, rect.x + rect.w - d, rect.y, d, d, 270.0, 90.0);
        GdipAddPathArc(path, rect.x + rect.w - d, rect.y + rect.h - d, d, d, 0.0, 90.0);
        GdipAddPathArc(path, rect.x, rect.y + rect.h - d, d, d, 90.0, 90.0);
        GdipClosePathFigure(path);
    }
    path
}

unsafe fn fill_round_rect(g: *mut c_void, rect: Rect, radius: f32, color: Color) {
    let path = unsafe { create_round_rect_path(rect, radius) };
    if path.is_null() {
        return;
    }
    let mut brush = null_mut();
    if unsafe { GdipCreateSolidFill(color.to_argb_u32(), &mut brush) } == 0 && !brush.is_null() {
        unsafe {
            GdipFillPath(g, brush, path);
            GdipDeleteBrush(brush);
        }
    }
    unsafe { GdipDeletePath(path) };
}

unsafe fn paint_soft_elevation_shadow(g: *mut c_void, theme: Theme) {
    // Soft multi-pass Material 3 elevation shadow rendered inside the 28px transparent gutter.
    let layers: &[(f32, f32, u8)] = match theme {
        Theme::Light => &[(18.0, 10.0, 6), (13.0, 8.0, 9), (9.0, 5.0, 12), (5.0, 3.0, 14), (2.0, 1.5, 16)],
        Theme::Dark => &[(20.0, 11.0, 16), (14.0, 8.0, 22), (9.0, 5.0, 28), (5.0, 3.0, 34), (2.0, 1.5, 40)],
    };
    for &(expand, dy, alpha) in layers {
        let rect = Rect::new(-expand, -expand + dy, DIALOG_W + expand * 2.0, DIALOG_H + expand * 2.0);
        let radius = super::DIALOG_RADIUS + expand * 0.55;
        unsafe { fill_round_rect(g, rect, radius, Color::rgba(0, 0, 0, alpha)) };
    }
}

unsafe fn load_ui_font_family() -> *mut c_void {
    for name in ["Microsoft YaHei UI", "Segoe UI", "Microsoft YaHei"] {
        let wide = to_wide_null(name);
        let mut fam = null_mut();
        if unsafe { GdipCreateFontFamilyFromName(wide.as_ptr(), null_mut(), &mut fam) } == 0 && !fam.is_null() {
            return fam;
        }
    }
    null_mut()
}

unsafe fn execute_paint_cmds(g: *mut c_void, font_family: *mut c_void, cmds: &[PaintCmd]) {
    let mut base_fmt = null_mut();
    let _ = unsafe { GdipStringFormatGetGenericTypographic(&mut base_fmt) };

    for cmd in cmds {
        match cmd {
            PaintCmd::FillRoundRect { rect, radius, color } => unsafe {
                fill_round_rect(g, *rect, *radius, *color);
            },
            PaintCmd::StrokeRoundRect { rect, radius, width, color } => unsafe {
                let path = create_round_rect_path(*rect, *radius);
                if path.is_null() {
                    continue;
                }
                let mut pen = null_mut();
                if GdipCreatePen1(color.to_argb_u32(), *width, UNIT_PIXEL, &mut pen) == 0 && !pen.is_null() {
                    GdipDrawPath(g, pen, path);
                    GdipDeletePen(pen);
                }
                GdipDeletePath(path);
            },
            PaintCmd::FillCircle { cx, cy, r, color } => unsafe {
                let mut brush = null_mut();
                if GdipCreateSolidFill(color.to_argb_u32(), &mut brush) == 0 && !brush.is_null() {
                    GdipFillEllipse(g, brush, cx - r, cy - r, r * 2.0, r * 2.0);
                    GdipDeleteBrush(brush);
                }
            },
            PaintCmd::StrokeLine { x1, y1, x2, y2, width, color } => unsafe {
                let mut pen = null_mut();
                if GdipCreatePen1(color.to_argb_u32(), *width, UNIT_PIXEL, &mut pen) == 0 && !pen.is_null() {
                    GdipSetPenStartCap(pen, LINE_CAP_ROUND);
                    GdipSetPenEndCap(pen, LINE_CAP_ROUND);
                    GdipDrawLine(g, pen, *x1, *y1, *x2, *y2);
                    GdipDeletePen(pen);
                }
            },
            PaintCmd::StrokePolyline { pts, width, color } => {
                if pts.len() < 2 {
                    continue;
                }
                let gpts: Vec<PointF> = pts.iter().map(|&(x, y)| PointF { x, y }).collect();
                unsafe {
                    let mut pen = null_mut();
                    if GdipCreatePen1(color.to_argb_u32(), *width, UNIT_PIXEL, &mut pen) == 0 && !pen.is_null() {
                        GdipSetPenStartCap(pen, LINE_CAP_ROUND);
                        GdipSetPenEndCap(pen, LINE_CAP_ROUND);
                        GdipSetPenLineJoin(pen, LINE_JOIN_ROUND);
                        GdipDrawLines(g, pen, gpts.as_ptr(), gpts.len() as i32);
                        GdipDeletePen(pen);
                    }
                }
            }
            PaintCmd::DrawText { text, rect, size, weight, color, align } => {
                if font_family.is_null() {
                    continue;
                }
                // Microsoft YaHei UI only has Regular & Bold weights; use Bold for the 21px headline
                // and Regular for smaller 12–14px UI labels so ClearType stays crisp and balanced.
                let style = if matches!(weight, FontWeight::Medium) && *size >= 18.0 {
                    FONT_STYLE_BOLD
                } else {
                    FONT_STYLE_REGULAR
                };
                let wide: Vec<u16> = text.encode_utf16().collect();
                unsafe {
                    let mut font = null_mut();
                    if GdipCreateFont(font_family, *size, style, UNIT_PIXEL, &mut font) != 0 || font.is_null() {
                        continue;
                    }
                    let mut fmt = null_mut();
                    if !base_fmt.is_null() && GdipCloneStringFormat(base_fmt, &mut fmt) == 0 && !fmt.is_null() {
                        let a = match align {
                            TextAlign::Left => STRING_ALIGNMENT_NEAR,
                            TextAlign::Center => STRING_ALIGNMENT_CENTER,
                        };
                        GdipSetStringFormatAlign(fmt, a);
                        GdipSetStringFormatLineAlign(fmt, STRING_ALIGNMENT_CENTER);
                    }
                    let mut brush = null_mut();
                    if GdipCreateSolidFill(color.to_argb_u32(), &mut brush) == 0 && !brush.is_null() {
                        let layout = RectF { x: rect.x, y: rect.y, width: rect.w, height: rect.h };
                        GdipDrawString(g, wide.as_ptr(), wide.len() as i32, font, &layout, fmt, brush);
                        GdipDeleteBrush(brush);
                    }
                    if !fmt.is_null() {
                        GdipDeleteStringFormat(fmt);
                    }
                    GdipDeleteFont(font);
                }
            }
        }
    }
}

struct WinRuntime {
    state: PromptState,
    scale: f32,
    font_family: *mut c_void,
    tracking_mouse: bool,
    cursor_arrow: *mut c_void,
    cursor_hand: *mut c_void,
}

thread_local! {
    static WIN_RT: RefCell<Option<WinRuntime>> = const { RefCell::new(None) };
}

fn repaint_layered(hwnd: *mut c_void, move_to: Option<Point>) {
    WIN_RT.with(|cell| {
        let guard = cell.borrow();
        let Some(rt) = guard.as_ref() else {
            return;
        };
        let scale = rt.scale;
        let px_w = (CANVAS_W * scale).round() as i32;
        let px_h = (CANVAS_H * scale).round() as i32;
        let cmds = rt.state.paint_commands();
        let theme = rt.state.theme;
        let font_family = rt.font_family;

        unsafe {
            let hdc_screen = GetDC(null_mut());
            if hdc_screen.is_null() {
                return;
            }
            let hdc_mem = CreateCompatibleDC(hdc_screen);
            if hdc_mem.is_null() {
                ReleaseDC(null_mut(), hdc_screen);
                return;
            }
            let mut bmi: BitmapInfo = zeroed();
            bmi.bmi_header.bi_size = size_of::<BitmapInfoHeader>() as u32;
            bmi.bmi_header.bi_width = px_w;
            bmi.bmi_header.bi_height = -px_h; // top-down DIB
            bmi.bmi_header.bi_planes = 1;
            bmi.bmi_header.bi_bit_count = 32;
            bmi.bmi_header.bi_compression = BI_RGB;

            let mut bits: *mut c_void = null_mut();
            let hbmp = CreateDIBSection(hdc_screen, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if hbmp.is_null() || bits.is_null() {
                DeleteDC(hdc_mem);
                ReleaseDC(null_mut(), hdc_screen);
                return;
            }
            std::ptr::write_bytes(bits.cast::<u8>(), 0, (px_w * px_h * 4) as usize);

            let mut gp_bmp = null_mut();
            if GdipCreateBitmapFromScan0(px_w, px_h, px_w * 4, PIXEL_FORMAT_32BPP_PARGB, bits.cast::<u8>(), &mut gp_bmp)
                == 0
                && !gp_bmp.is_null()
            {
                let mut g = null_mut();
                if GdipGetImageGraphicsContext(gp_bmp, &mut g) == 0 && !g.is_null() {
                    GdipSetSmoothingMode(g, SMOOTHING_MODE_ANTI_ALIAS);
                    GdipSetPixelOffsetMode(g, PIXEL_OFFSET_MODE_HIGH_QUALITY);
                    GdipSetTextRenderingHint(g, TEXT_RENDERING_HINT_ANTI_ALIAS_GRID_FIT);
                    // Row-vector GDI+ prepend: p * (T * S) = (p + SHADOW_PAD) * scale.
                    GdipScaleWorldTransform(g, scale, scale, MATRIX_ORDER_PREPEND);
                    GdipTranslateWorldTransform(g, SHADOW_PAD, SHADOW_PAD, MATRIX_ORDER_PREPEND);
                    paint_soft_elevation_shadow(g, theme);
                    execute_paint_cmds(g, font_family, &cmds);
                    GdipDeleteGraphics(g);
                }
                GdipDisposeImage(gp_bmp);
            }

            let old = SelectObject(hdc_mem, hbmp);
            let dst_opt = move_to.or_else(|| {
                let mut wr = WinRect::default();
                (GetWindowRect(hwnd, &mut wr) != 0).then_some(Point { x: wr.left, y: wr.top })
            });
            let dst_ptr = dst_opt.as_ref().map_or(null(), |p| p as *const Point);
            let size = Size { cx: px_w, cy: px_h };
            let src = Point { x: 0, y: 0 };
            let blend = BlendFunction {
                blend_op: AC_SRC_OVER,
                blend_flags: 0,
                source_constant_alpha: 255,
                alpha_format: AC_SRC_ALPHA,
            };
            UpdateLayeredWindow(hwnd, hdc_screen, dst_ptr, &size, hdc_mem, &src, 0, &blend, ULW_ALPHA);

            SelectObject(hdc_mem, old);
            DeleteObject(hbmp);
            DeleteDC(hdc_mem);
            ReleaseDC(null_mut(), hdc_screen);
        }
    });
}

fn client_to_logical(l_param: isize, scale: f32) -> (f32, f32) {
    let raw_x = (l_param & 0xFFFF) as i16 as f32;
    let raw_y = ((l_param >> 16) & 0xFFFF) as i16 as f32;
    (raw_x / scale - SHADOW_PAD, raw_y / scale - SHADOW_PAD)
}

unsafe extern "system" fn wnd_proc(hwnd: *mut c_void, msg: u32, w_param: usize, l_param: isize) -> isize {
    match msg {
        WM_MOUSEMOVE => {
            let (dirty, need_track) = WIN_RT.with(|cell| {
                let mut guard = cell.borrow_mut();
                let Some(rt) = guard.as_mut() else {
                    return (false, false);
                };
                let (x, y) = client_to_logical(l_param, rt.scale);
                let changed = rt.state.on_mouse_move(x, y);
                let track = !rt.tracking_mouse;
                rt.tracking_mouse = true;
                (changed, track)
            });
            if need_track {
                let mut tme = TrackMouseEventStruct {
                    cb_size: size_of::<TrackMouseEventStruct>() as u32,
                    dw_flags: TME_LEAVE,
                    hwnd_track: hwnd,
                    dw_hover_time: 0,
                };
                unsafe { TrackMouseEvent(&mut tme) };
            }
            if dirty {
                repaint_layered(hwnd, None);
            }
            0
        }
        WM_MOUSELEAVE => {
            let dirty = WIN_RT.with(|cell| {
                let mut guard = cell.borrow_mut();
                let Some(rt) = guard.as_mut() else {
                    return false;
                };
                rt.tracking_mouse = false;
                rt.state.on_mouse_leave()
            });
            if dirty {
                repaint_layered(hwnd, None);
            }
            0
        }
        WM_SETCURSOR => {
            if (l_param & 0xFFFF) as u16 == HTCLIENT {
                let cur = WIN_RT.with(|cell| {
                    let guard = cell.borrow();
                    let rt = guard.as_ref()?;
                    Some(if rt.state.hovered.is_interactive() { rt.cursor_hand } else { rt.cursor_arrow })
                });
                if let Some(c) = cur
                    && !c.is_null()
                {
                    unsafe { SetCursor(c) };
                    return 1;
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, w_param, l_param) }
        }
        WM_LBUTTONDOWN => {
            let (drag, dirty) = WIN_RT.with(|cell| {
                let mut guard = cell.borrow_mut();
                let Some(rt) = guard.as_mut() else {
                    return (false, false);
                };
                let (x, y) = client_to_logical(l_param, rt.scale);
                rt.state.on_mouse_down(x, y)
            });
            if drag {
                unsafe {
                    ReleaseCapture();
                    SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION, 0);
                }
            } else {
                unsafe { SetCapture(hwnd) };
                if dirty {
                    repaint_layered(hwnd, None);
                }
            }
            0
        }
        WM_LBUTTONUP => {
            unsafe { ReleaseCapture() };
            let (outcome, dirty) = WIN_RT.with(|cell| {
                let mut guard = cell.borrow_mut();
                let Some(rt) = guard.as_mut() else {
                    return (None, false);
                };
                let (x, y) = client_to_logical(l_param, rt.scale);
                rt.state.on_mouse_up(x, y)
            });
            if outcome.is_some() {
                unsafe { DestroyWindow(hwnd) };
            } else if dirty {
                repaint_layered(hwnd, None);
            }
            0
        }
        WM_KEYDOWN => {
            // Bit 30: the key was already down (auto-repeat).
            let repeat = (l_param >> 30) & 1 == 1;
            match w_param {
                VK_RETURN => {
                    let done = WIN_RT
                        .with(|cell| cell.borrow_mut().as_mut().and_then(|rt| rt.state.on_confirm_key(repeat)))
                        .is_some();
                    if done {
                        unsafe { DestroyWindow(hwnd) };
                    }
                }
                VK_ESCAPE => {
                    WIN_RT.with(|cell| {
                        if let Some(rt) = cell.borrow_mut().as_mut() {
                            rt.state.on_cancel_key();
                        }
                    });
                    unsafe { DestroyWindow(hwnd) };
                }
                VK_SPACE => {
                    let changed =
                        WIN_RT.with(|cell| cell.borrow_mut().as_mut().is_some_and(|rt| rt.state.on_toggle_key(repeat)));
                    if changed {
                        repaint_layered(hwnd, None);
                    }
                }
                _ => {}
            }
            0
        }
        WM_DPICHANGED => {
            let new_dpi = (w_param & 0xFFFF) as u32;
            let suggested = if l_param != 0 {
                let r = unsafe { *(l_param as *const WinRect) };
                Some(Point { x: r.left, y: r.top })
            } else {
                None
            };
            WIN_RT.with(|cell| {
                if let Some(rt) = cell.borrow_mut().as_mut() {
                    rt.scale = (new_dpi.max(72) as f32) / 96.0;
                }
            });
            repaint_layered(hwnd, suggested);
            0
        }
        WM_CLOSE => {
            // Alt+F4: "later", keeping "不再询问" as ticked.
            WIN_RT.with(|cell| {
                if let Some(rt) = cell.borrow_mut().as_mut() {
                    rt.state.on_close();
                }
            });
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, w_param, l_param) },
    }
}

fn show_custom_dialog(theme: Theme) -> Option<PromptResult> {
    let mut gp_token: usize = 0;
    let startup = GdiplusStartupInput {
        gdiplus_version: 1,
        debug_event_callback: null(),
        suppress_background_thread: 0,
        suppress_external_codecs: 0,
    };
    if unsafe { GdiplusStartup(&mut gp_token, &startup, null_mut()) } != 0 {
        return None;
    }

    let h_inst = unsafe { GetModuleHandleW(null()) };
    let class_name = to_wide_null("AgyStatuslinePromptWindow");
    let title = to_wide_null("AGY Statusline");
    let cursor_arrow = unsafe { LoadCursorW(null_mut(), IDC_ARROW) };
    let cursor_hand = unsafe { LoadCursorW(null_mut(), IDC_HAND) };

    let wc = WndClassExW {
        cb_size: size_of::<WndClassExW>() as u32,
        style: 0,
        lpfn_wnd_proc: wnd_proc,
        cb_cls_extra: 0,
        cb_wnd_extra: 0,
        h_instance: h_inst,
        h_icon: null_mut(),
        h_cursor: cursor_arrow,
        hbr_background: null_mut(),
        lpsz_menu_name: null(),
        lpsz_class_name: class_name.as_ptr(),
        h_icon_sm: null_mut(),
    };
    if unsafe { RegisterClassExW(&wc) } == 0 {
        unsafe { GdiplusShutdown(gp_token) };
        return None;
    }

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            CANVAS_W as i32,
            CANVAS_H as i32,
            null_mut(),
            null_mut(),
            h_inst,
            null_mut(),
        )
    };
    if hwnd.is_null() {
        unsafe {
            UnregisterClassW(class_name.as_ptr(), h_inst);
            GdiplusShutdown(gp_token);
        }
        return None;
    }

    let dpi = window_dpi(hwnd);
    let scale = dpi as f32 / 96.0;
    let px_w = (CANVAS_W * scale).round() as i32;
    let px_h = (CANVAS_H * scale).round() as i32;

    // Center on the monitor containing the mouse cursor (slightly above vertical center).
    let mut pt = Point::default();
    unsafe { GetCursorPos(&mut pt) };
    let mon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    let mut mi: MonitorInfo = unsafe { zeroed() };
    mi.cb_size = size_of::<MonitorInfo>() as u32;
    let work = if !mon.is_null() && unsafe { GetMonitorInfoW(mon, &mut mi) } != 0 {
        mi.rc_work
    } else {
        WinRect { left: 0, top: 0, right: 1920, bottom: 1080 }
    };
    let pos_x = work.left + ((work.right - work.left) - px_w) / 2;
    let pos_y = work.top + ((work.bottom - work.top) - px_h) * 44 / 100;

    let font_family = unsafe { load_ui_font_family() };
    WIN_RT.with(|cell| {
        *cell.borrow_mut() = Some(WinRuntime {
            state: PromptState::new(theme),
            scale,
            font_family,
            tracking_mouse: false,
            cursor_arrow,
            cursor_hand,
        });
    });

    repaint_layered(hwnd, Some(Point { x: pos_x, y: pos_y }));
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }

    let mut msg: Msg = unsafe { zeroed() };
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    let outcome = WIN_RT.with(|cell| {
        let rt = cell.borrow_mut().take();
        if let Some(rt) = rt {
            if !rt.font_family.is_null() {
                unsafe { GdipDeleteFontFamily(rt.font_family) };
            }
            rt.state.outcome
        } else {
            None
        }
    });

    unsafe {
        UnregisterClassW(class_name.as_ptr(), h_inst);
        GdiplusShutdown(gp_token);
    }
    Some(outcome.unwrap_or_default())
}

fn show_fallback_message_box() -> PromptResult {
    let title = to_wide_null("AGY Statusline");
    let text = to_wide_null(
        "需要重启 Antigravity\n\n\
         本次未开启调试端口，状态栏和中文界面暂无法加载。\n\
         重启会保留对话记录，但会中断正在运行的任务。\n\n\
         （可在侧边栏 Settings 中修改该行为。）",
    );
    let ret = unsafe {
        MessageBoxW(
            null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OKCANCEL | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
        )
    };
    PromptResult { relaunch: ret == IDOK, dont_ask_again: false }
}

pub fn show_dialog(theme_override: Option<Theme>) -> PromptResult {
    let _dpi = DpiGuard::enter_per_monitor_v2();
    let theme = theme_override.unwrap_or_else(detect_system_theme);
    show_custom_dialog(theme).unwrap_or_else(show_fallback_message_box)
}
