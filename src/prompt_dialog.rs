//! Cross-platform custom-drawn Material 3 Pure confirmation dialog (`G1`) for debug-port relaunch.
//!
//! Architecture:
//! - Single shared Rust layout, palette, hit-testing, interaction state machine, and 2D vector
//!   draw-command list (`PaintCmd`) in this module — zero external GUI frameworks.
//! - Thin native OS painters:
//!   - macOS (`macos.rs`): Cocoa `NSWindow`/`NSView` + CoreGraphics/AppKit (sRGB, Retina 2x).
//!   - Windows (`windows.rs`): Win32 `WS_EX_LAYERED` + `UpdateLayeredWindow` + GDI+ `32bppPARGB`
//!     with Per-Monitor DPI v2 scaling and native `MessageBoxW` fallback.
//! - Spawned via a short-lived `agy-statusline relaunch-prompt` subprocess so macOS AppKit runs
//!   cleanly on its main thread and the resident sidecar keeps zero GUI libraries loaded.

// Only the macOS and Windows painters draw this layout; other platforms never show the dialog.
#![cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::process::Command;

pub const DIALOG_W: f32 = 432.0;
pub const DIALOG_H: f32 = 240.0;
pub const DIALOG_RADIUS: f32 = 28.0;

pub const EXIT_RELAUNCH: i32 = 10;
pub const EXIT_RELAUNCH_DONT_ASK: i32 = 11;
pub const EXIT_LATER: i32 = 20;
pub const EXIT_LATER_DONT_ASK: i32 = 21;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PromptResult {
    pub relaunch: bool,
    pub dont_ask_again: bool,
}

impl PromptResult {
    pub fn to_exit_code(self) -> i32 {
        match (self.relaunch, self.dont_ask_again) {
            (true, false) => EXIT_RELAUNCH,
            (true, true) => EXIT_RELAUNCH_DONT_ASK,
            (false, false) => EXIT_LATER,
            (false, true) => EXIT_LATER_DONT_ASK,
        }
    }

    pub fn from_exit_code(code: Option<i32>) -> Self {
        match code {
            Some(EXIT_RELAUNCH) => Self { relaunch: true, dont_ask_again: false },
            Some(EXIT_RELAUNCH_DONT_ASK) => Self { relaunch: true, dont_ask_again: true },
            Some(EXIT_LATER_DONT_ASK) => Self { relaunch: false, dont_ask_again: true },
            _ => Self { relaunch: false, dont_ask_again: false },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    pub fn parse_override(raw: Option<&str>) -> Option<Self> {
        match raw.map(str::trim) {
            Some(s) if s.eq_ignore_ascii_case("light") => Some(Self::Light),
            Some(s) if s.eq_ignore_ascii_case("dark") => Some(Self::Dark),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    #[cfg(target_os = "windows")]
    pub const fn to_argb_u32(self) -> u32 {
        ((self.a as u32) << 24) | ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }

    #[cfg(target_os = "macos")]
    pub fn to_rgba_f64(self) -> (f64, f64, f64, f64) {
        (f64::from(self.r) / 255.0, f64::from(self.g) / 255.0, f64::from(self.b) / 255.0, f64::from(self.a) / 255.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontWeight {
    Regular,
    Medium,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PaintCmd {
    FillRoundRect { rect: Rect, radius: f32, color: Color },
    StrokeRoundRect { rect: Rect, radius: f32, width: f32, color: Color },
    FillCircle { cx: f32, cy: f32, r: f32, color: Color },
    StrokeLine { x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: Color },
    StrokePolyline { pts: Vec<(f32, f32)>, width: f32, color: Color },
    DrawText { text: &'static str, rect: Rect, size: f32, weight: FontWeight, color: Color, align: TextAlign },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitZone {
    None,
    DragArea,
    Checkbox,
    Later,
    Relaunch,
}

impl HitZone {
    pub fn is_interactive(self) -> bool {
        matches!(self, Self::Checkbox | Self::Later | Self::Relaunch)
    }
}

/// Exact Google Material 3 Pure (`G1`) tokens for Light and Dark themes.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub surface: Color,
    pub surface_highlight: Color,
    pub outline_variant: Color,
    pub on_surface: Color,
    pub on_surface_variant: Color,
    pub primary: Color,
    pub primary_hover: Color,
    pub primary_active: Color,
    pub primary_mark_fill: Color,
    pub on_primary: Color,
    pub state_primary_hover: Color,
    pub state_primary_active: Color,
    pub state_neutral_hover: Color,
    pub state_neutral_active: Color,
    pub checkbox_border: Color,
    pub checkbox_border_hover: Color,
    pub tooltip_bg: Color,
    pub tooltip_fg: Color,
    pub primary_hover_glow: Color,
}

impl Palette {
    pub const fn for_theme(theme: Theme) -> Self {
        match theme {
            Theme::Light => Self {
                surface: Color::rgb(0xF0, 0xF4, 0xF9),
                surface_highlight: Color::rgba(255, 255, 255, 230),
                outline_variant: Color::rgba(0xC4, 0xC7, 0xC5, 122),
                on_surface: Color::rgb(0x1F, 0x1F, 0x1F),
                on_surface_variant: Color::rgb(0x44, 0x47, 0x46),
                primary: Color::rgb(0x0B, 0x57, 0xD0),
                primary_hover: Color::rgb(0x08, 0x42, 0xA0),
                primary_active: Color::rgb(0x06, 0x35, 0x80),
                primary_mark_fill: Color::rgba(0x0B, 0x57, 0xD0, 31),
                on_primary: Color::rgb(0xFF, 0xFF, 0xFF),
                state_primary_hover: Color::rgba(0x0B, 0x57, 0xD0, 20),
                state_primary_active: Color::rgba(0x0B, 0x57, 0xD0, 36),
                state_neutral_hover: Color::rgba(0x1F, 0x1F, 0x1F, 18),
                state_neutral_active: Color::rgba(0x1F, 0x1F, 0x1F, 31),
                checkbox_border: Color::rgba(0x44, 0x47, 0x46, 204),
                checkbox_border_hover: Color::rgb(0x1F, 0x1F, 0x1F),
                tooltip_bg: Color::rgb(0x30, 0x30, 0x30),
                tooltip_fg: Color::rgb(0xF2, 0xF2, 0xF2),
                primary_hover_glow: Color::rgba(0x0B, 0x57, 0xD0, 36),
            },
            Theme::Dark => Self {
                surface: Color::rgb(0x28, 0x29, 0x2C),
                surface_highlight: Color::rgba(255, 255, 255, 17),
                outline_variant: Color::rgba(0x44, 0x47, 0x46, 166),
                on_surface: Color::rgb(0xE3, 0xE3, 0xE3),
                on_surface_variant: Color::rgb(0xC4, 0xC7, 0xC5),
                primary: Color::rgb(0xA8, 0xC7, 0xFA),
                primary_hover: Color::rgb(0xB8, 0xD2, 0xFC),
                primary_active: Color::rgb(0x93, 0xB8, 0xF7),
                primary_mark_fill: Color::rgba(0xA8, 0xC7, 0xFA, 31),
                on_primary: Color::rgb(0x06, 0x2E, 0x6F),
                state_primary_hover: Color::rgba(0xA8, 0xC7, 0xFA, 26),
                state_primary_active: Color::rgba(0xA8, 0xC7, 0xFA, 41),
                state_neutral_hover: Color::rgba(0xE3, 0xE3, 0xE3, 20),
                state_neutral_active: Color::rgba(0xE3, 0xE3, 0xE3, 36),
                checkbox_border: Color::rgba(0xC4, 0xC7, 0xC5, 189),
                checkbox_border_hover: Color::rgb(0xE3, 0xE3, 0xE3),
                tooltip_bg: Color::rgb(0xE3, 0xE3, 0xE3),
                tooltip_fg: Color::rgb(0x1F, 0x1F, 0x1F),
                primary_hover_glow: Color::rgba(0xA8, 0xC7, 0xFA, 32),
            },
        }
    }
}

// Shared exact G1 Material 3 layout boxes (logical px inside 432 x 240).
const CARD_RECT: Rect = Rect::new(0.0, 0.0, DIALOG_W, DIALOG_H);
const CHECKBOX_HIT: Rect = Rect::new(15.0, 178.0, 108.0, 36.0);
const LATER_BTN: Rect = Rect::new(224.0, 176.0, 68.0, 40.0);
const RELAUNCH_BTN: Rect = Rect::new(300.0, 176.0, 108.0, 40.0);
const TOOLTIP_RECT: Rect = Rect::new(24.0, 146.0, 250.0, 26.0);

#[derive(Clone, Debug)]
pub struct PromptState {
    pub theme: Theme,
    pub dont_ask_again: bool,
    pub hovered: HitZone,
    pub pressed: HitZone,
    pub outcome: Option<PromptResult>,
}

impl PromptState {
    pub fn new(theme: Theme) -> Self {
        Self { theme, dont_ask_again: false, hovered: HitZone::None, pressed: HitZone::None, outcome: None }
    }

    pub fn hit_test(x: f32, y: f32) -> HitZone {
        if !CARD_RECT.contains(x, y) {
            return HitZone::None;
        }
        if RELAUNCH_BTN.contains(x, y) {
            HitZone::Relaunch
        } else if LATER_BTN.contains(x, y) {
            HitZone::Later
        } else if CHECKBOX_HIT.contains(x, y) {
            HitZone::Checkbox
        } else {
            HitZone::DragArea
        }
    }

    pub fn on_mouse_move(&mut self, x: f32, y: f32) -> bool {
        let next = Self::hit_test(x, y);
        if next != self.hovered {
            self.hovered = next;
            true
        } else {
            false
        }
    }

    pub fn on_mouse_leave(&mut self) -> bool {
        if self.hovered != HitZone::None {
            self.hovered = HitZone::None;
            true
        } else {
            false
        }
    }

    /// Returns `true` if the press starts a window drag rather than a control press.
    pub fn on_mouse_down(&mut self, x: f32, y: f32) -> (bool, bool) {
        let zone = Self::hit_test(x, y);
        self.hovered = zone;
        if zone == HitZone::DragArea {
            return (true, false);
        }
        if zone.is_interactive() {
            let changed = self.pressed != zone;
            self.pressed = zone;
            (false, changed)
        } else {
            (false, false)
        }
    }

    /// Returns `Some(PromptResult)` if the dialog should close, plus whether a repaint is needed.
    pub fn on_mouse_up(&mut self, x: f32, y: f32) -> (Option<PromptResult>, bool) {
        let was_pressed = self.pressed;
        self.pressed = HitZone::None;
        let zone = Self::hit_test(x, y);
        self.hovered = zone;
        if was_pressed == HitZone::None {
            return (None, false);
        }
        if was_pressed == zone {
            match zone {
                HitZone::Checkbox => {
                    self.dont_ask_again = !self.dont_ask_again;
                    return (None, true);
                }
                HitZone::Later => {
                    let res = PromptResult { relaunch: false, dont_ask_again: self.dont_ask_again };
                    self.outcome = Some(res);
                    return (Some(res), true);
                }
                HitZone::Relaunch => {
                    let res = PromptResult { relaunch: true, dont_ask_again: self.dont_ask_again };
                    self.outcome = Some(res);
                    return (Some(res), true);
                }
                _ => {}
            }
        }
        (None, true)
    }

    pub fn on_confirm_key(&mut self) -> PromptResult {
        let res = PromptResult { relaunch: true, dont_ask_again: self.dont_ask_again };
        self.outcome = Some(res);
        res
    }

    pub fn on_cancel_key(&mut self) -> PromptResult {
        let res = PromptResult { relaunch: false, dont_ask_again: self.dont_ask_again };
        self.outcome = Some(res);
        res
    }

    pub fn toggle_dont_ask(&mut self) {
        self.dont_ask_again = !self.dont_ask_again;
    }

    /// Builds the ordered 2D vector command list for the current UI state.
    pub fn paint_commands(&self) -> Vec<PaintCmd> {
        let p = Palette::for_theme(self.theme);
        let mut cmds = Vec::with_capacity(28);

        // 1. Main Material 3 ExtraLarge surface card (432 x 240, r = 28)
        cmds.push(PaintCmd::FillRoundRect { rect: CARD_RECT, radius: DIALOG_RADIUS, color: p.surface });
        cmds.push(PaintCmd::StrokeRoundRect {
            rect: Rect::new(0.5, 0.5, DIALOG_W - 1.0, DIALOG_H - 1.0),
            radius: DIALOG_RADIUS - 0.5,
            width: 1.0,
            color: p.outline_variant,
        });
        cmds.push(PaintCmd::StrokeLine {
            x1: DIALOG_RADIUS,
            y1: 1.25,
            x2: DIALOG_W - DIALOG_RADIUS,
            y2: 1.25,
            width: 1.0,
            color: p.surface_highlight,
        });

        // 2. Eyebrow: 14x14 minimalist 3-primitive AGY Statusline mark + "AGY Statusline"
        let mx = 24.0;
        let my = 25.0;
        cmds.push(PaintCmd::FillRoundRect {
            rect: Rect::new(mx + 1.25, my + 2.25, 11.5, 9.5),
            radius: 2.6,
            color: p.primary_mark_fill,
        });
        cmds.push(PaintCmd::StrokeRoundRect {
            rect: Rect::new(mx + 1.25, my + 2.25, 11.5, 9.5),
            radius: 2.6,
            width: 1.35,
            color: p.primary,
        });
        cmds.push(PaintCmd::FillRoundRect {
            rect: Rect::new(mx + 3.1, my + 8.1, 5.2, 1.8),
            radius: 0.9,
            color: p.primary,
        });
        cmds.push(PaintCmd::FillCircle { cx: mx + 10.2, cy: my + 9.0, r: 0.95, color: p.primary });
        cmds.push(PaintCmd::DrawText {
            text: "AGY Statusline",
            rect: Rect::new(45.0, 24.0, 200.0, 16.0),
            size: 12.0,
            weight: FontWeight::Medium,
            color: p.on_surface_variant,
            align: TextAlign::Left,
        });

        // 3. Headline
        cmds.push(PaintCmd::DrawText {
            text: "需要重启 Antigravity",
            rect: Rect::new(24.0, 54.0, 384.0, 28.0),
            size: 21.0,
            weight: FontWeight::Medium,
            color: p.on_surface,
            align: TextAlign::Left,
        });

        // 4. Distilled 2-line body
        cmds.push(PaintCmd::DrawText {
            text: "本次未开启调试端口，状态栏和中文界面暂无法加载。",
            rect: Rect::new(24.0, 94.0, 384.0, 23.0),
            size: 14.0,
            weight: FontWeight::Regular,
            color: p.on_surface_variant,
            align: TextAlign::Left,
        });
        cmds.push(PaintCmd::DrawText {
            text: "重启会保留对话记录，但会中断正在运行的任务。",
            rect: Rect::new(24.0, 117.0, 384.0, 23.0),
            size: 14.0,
            weight: FontWeight::Regular,
            color: p.on_surface_variant,
            align: TextAlign::Left,
        });

        // 5. Left M3 checkbox: "不再询问"
        let cb_hover = self.hovered == HitZone::Checkbox;
        let cb_active = self.pressed == HitZone::Checkbox && cb_hover;
        if cb_active || cb_hover {
            let halo = match (self.dont_ask_again, cb_active) {
                (true, true) => p.state_primary_active,
                (true, false) => p.state_primary_hover,
                (false, true) => p.state_neutral_active,
                (false, false) => p.state_neutral_hover,
            };
            cmds.push(PaintCmd::FillCircle { cx: 33.0, cy: 196.0, r: 18.0, color: halo });
        }

        let box_rect = Rect::new(24.0, 187.0, 18.0, 18.0);
        if self.dont_ask_again {
            let fill = if cb_hover { p.primary_hover } else { p.primary };
            cmds.push(PaintCmd::FillRoundRect { rect: box_rect, radius: 3.0, color: fill });
            cmds.push(PaintCmd::StrokePolyline {
                pts: vec![(27.8, 196.2), (31.3, 199.7), (38.3, 192.4)],
                width: 2.0,
                color: p.on_primary,
            });
        } else {
            let border = if cb_hover { p.checkbox_border_hover } else { p.checkbox_border };
            cmds.push(PaintCmd::StrokeRoundRect {
                rect: Rect::new(box_rect.x + 0.875, box_rect.y + 0.875, box_rect.w - 1.75, box_rect.h - 1.75),
                radius: 2.8,
                width: 1.75,
                color: border,
            });
        }
        cmds.push(PaintCmd::DrawText {
            text: "不再询问",
            rect: Rect::new(50.0, 176.0, 74.0, 40.0),
            size: 13.5,
            weight: FontWeight::Regular,
            color: if cb_hover { p.on_surface } else { p.on_surface_variant },
            align: TextAlign::Left,
        });

        // 6. M3 Plain Tooltip above checkbox on hover (fits cleanly inside gutter)
        if cb_hover {
            cmds.push(PaintCmd::FillRoundRect { rect: TOOLTIP_RECT, radius: 4.0, color: p.tooltip_bg });
            cmds.push(PaintCmd::DrawText {
                text: "勾选后将在侧边栏 Settings 设为「不处理」",
                rect: TOOLTIP_RECT,
                size: 12.0,
                weight: FontWeight::Regular,
                color: p.tooltip_fg,
                align: TextAlign::Center,
            });
        }

        // 7. M3 Text Button: "稍后"
        let later_hover = self.hovered == HitZone::Later;
        let later_active = self.pressed == HitZone::Later && later_hover;
        if later_active {
            cmds.push(PaintCmd::FillRoundRect { rect: LATER_BTN, radius: 20.0, color: p.state_primary_active });
        } else if later_hover {
            cmds.push(PaintCmd::FillRoundRect { rect: LATER_BTN, radius: 20.0, color: p.state_primary_hover });
        }
        cmds.push(PaintCmd::DrawText {
            text: "稍后",
            rect: LATER_BTN,
            size: 14.0,
            weight: FontWeight::Medium,
            color: p.primary,
            align: TextAlign::Center,
        });

        // 8. M3 Filled Primary Pill: "立即重启"
        let pri_hover = self.hovered == HitZone::Relaunch;
        let pri_active = self.pressed == HitZone::Relaunch && pri_hover;
        if pri_hover && !pri_active {
            cmds.push(PaintCmd::FillRoundRect {
                rect: Rect::new(RELAUNCH_BTN.x - 1.0, RELAUNCH_BTN.y + 1.0, RELAUNCH_BTN.w + 2.0, RELAUNCH_BTN.h + 2.0),
                radius: 21.0,
                color: p.primary_hover_glow,
            });
        }
        let pri_bg = if pri_active {
            p.primary_active
        } else if pri_hover {
            p.primary_hover
        } else {
            p.primary
        };
        cmds.push(PaintCmd::FillRoundRect { rect: RELAUNCH_BTN, radius: 20.0, color: pri_bg });
        cmds.push(PaintCmd::DrawText {
            text: "立即重启",
            rect: RELAUNCH_BTN,
            size: 14.0,
            weight: FontWeight::Medium,
            color: p.on_primary,
            align: TextAlign::Center,
        });

        cmds
    }
}

/// Runs the native GUI confirmation dialog directly in the current process (used by the
/// `agy-statusline relaunch-prompt` helper subcommand).
pub fn run_prompt_process(theme_override: Option<&str>) -> PromptResult {
    let forced = Theme::parse_override(theme_override);
    #[cfg(target_os = "macos")]
    {
        macos::show_dialog(forced)
    }
    #[cfg(target_os = "windows")]
    {
        windows::show_dialog(forced)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = forced;
        PromptResult::default()
    }
}

/// Launches the custom-drawn confirmation dialog in a short-lived child process of the current
/// binary so macOS AppKit executes on its main thread and the sidecar stays memory-lean.
pub fn ask_relaunch_confirm() -> PromptResult {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let Ok(exe) = std::env::current_exe() else {
            return PromptResult::default();
        };
        let mut cmd = Command::new(exe);
        cmd.arg("relaunch-prompt");
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let Ok(status) = cmd.status() else {
            return PromptResult::default();
        };
        PromptResult::from_exit_code(status.code())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        PromptResult::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_roundtrip_all_prompt_outcomes() {
        for relaunch in [false, true] {
            for dont_ask_again in [false, true] {
                let res = PromptResult { relaunch, dont_ask_again };
                assert_eq!(PromptResult::from_exit_code(Some(res.to_exit_code())), res);
            }
        }
        assert_eq!(PromptResult::from_exit_code(None), PromptResult::default());
        assert_eq!(PromptResult::from_exit_code(Some(1)), PromptResult::default());
    }

    #[test]
    fn hit_test_and_state_machine_cover_all_controls() {
        assert_eq!(PromptState::hit_test(33.0, 196.0), HitZone::Checkbox);
        assert_eq!(PromptState::hit_test(258.0, 196.0), HitZone::Later);
        assert_eq!(PromptState::hit_test(354.0, 196.0), HitZone::Relaunch);
        assert_eq!(PromptState::hit_test(100.0, 60.0), HitZone::DragArea);
        assert_eq!(PromptState::hit_test(-1.0, 10.0), HitZone::None);

        let mut st = PromptState::new(Theme::Light);
        assert_eq!(st.on_mouse_down(100.0, 60.0), (true, false));

        // Toggle "不再询问"
        assert_eq!(st.on_mouse_down(33.0, 196.0), (false, true));
        assert_eq!(st.on_mouse_up(33.0, 196.0), (None, true));
        assert!(st.dont_ask_again);

        // Press and release "立即重启"
        assert_eq!(st.on_mouse_down(354.0, 196.0), (false, true));
        assert_eq!(st.on_mouse_up(354.0, 196.0), (Some(PromptResult { relaunch: true, dont_ask_again: true }), true));
    }

    #[test]
    fn tooltip_renders_only_when_checkbox_hovered_and_fits_inside_dialog() {
        let mut st = PromptState::new(Theme::Dark);
        let idle = st.paint_commands();
        assert!(!idle.iter().any(|c| matches!(c, PaintCmd::DrawText { text, .. } if text.contains("Settings"))));

        st.on_mouse_move(33.0, 196.0);
        let hovered = st.paint_commands();
        assert!(hovered.iter().any(|c| matches!(c, PaintCmd::DrawText { text, .. } if text.contains("Settings"))));
        const {
            assert!(TOOLTIP_RECT.y >= 140.0);
            assert!(TOOLTIP_RECT.y + TOOLTIP_RECT.h <= CHECKBOX_HIT.y);
            assert!(TOOLTIP_RECT.x + TOOLTIP_RECT.w <= DIALOG_W);
        }
    }
}
