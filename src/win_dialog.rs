//! Native Windows `TaskDialogIndirect` confirmation dialog for debug-port relaunch.
//!
//! Uses direct Win32 FFI with zero external GUI crates:
//! - Dynamically loads `TaskDialogIndirect` from `comctl32.dll` (v6 via the embedded MSVC
//!   application manifest, with a runtime SxS `CreateActCtxW` fallback for unmanifested builds).
//! - Applies Per-Monitor DPI v2 on the calling thread for crisp HiDPI text.
//! - Matches the system dark title bar via `DwmSetWindowAttribute` when Windows dark mode is on.
//! - Falls back to native `MessageBoxW` if `TaskDialogIndirect` is unavailable.

#[cfg(any(target_os = "windows", test))]
use std::ffi::c_void;

#[cfg(any(target_os = "windows", test))]
type TaskDialogCallback =
    unsafe extern "system" fn(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize, lp_ref_data: isize) -> i32;

/// Matches `<commctrl.h>` `TASKDIALOG_BUTTON` (`#include <pshpack1.h>`).
#[cfg(any(target_os = "windows", test))]
#[repr(C, packed(1))]
struct TaskDialogButton {
    n_button_id: i32,
    psz_button_text: *const u16,
}

/// Matches `<commctrl.h>` `TASKDIALOGCONFIG` (`#include <pshpack1.h>`).
#[cfg(any(target_os = "windows", test))]
#[repr(C, packed(1))]
struct TaskDialogConfig {
    cb_size: u32,
    hwnd_parent: *mut c_void,
    h_instance: *mut c_void,
    dw_flags: u32,
    dw_common_buttons: u32,
    psz_window_title: *const u16,
    psz_main_icon: *const u16,
    psz_main_instruction: *const u16,
    psz_content: *const u16,
    c_buttons: u32,
    p_buttons: *const TaskDialogButton,
    n_default_button: i32,
    c_radio_buttons: u32,
    p_radio_buttons: *const TaskDialogButton,
    n_default_radio_button: i32,
    psz_verification_text: *const u16,
    psz_expanded_information: *const u16,
    psz_expanded_control_text: *const u16,
    psz_collapsed_control_text: *const u16,
    psz_footer_icon: *const u16,
    psz_footer: *const u16,
    pf_callback: Option<TaskDialogCallback>,
    lp_callback_data: isize,
    cx_width: u32,
}

#[cfg(any(target_os = "windows", test))]
fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(target_os = "windows")]
mod imp {
    use super::{TaskDialogButton, TaskDialogConfig, to_wide_null};
    use std::ffi::{c_char, c_void};
    use std::fs;
    use std::mem::{size_of, transmute};
    use std::path::PathBuf;
    use std::ptr::{null, null_mut};

    const WINDOW_TITLE: &str = "AGY Statusline";
    const MAIN_INSTRUCTION: &str = "需要以调试模式重启 Antigravity";
    const CONTENT: &str = "本次启动未携带 --remote-debugging-port=0，状态栏和中文界面暂时无法注入。\n\n\
        立即重启会保留对话记录，但正在运行的任务会被中断。";
    const FOOTER: &str = "可在侧边栏 Settings →「未带调试端口启动时」修改该行为。";
    const LABEL_RELAUNCH: &str = "立即重启";
    const LABEL_LATER: &str = "稍后";

    const BTN_RELAUNCH: i32 = 100;
    const BTN_LATER: i32 = 101;

    const TDF_ALLOW_DIALOG_CANCELLATION: u32 = 0x0008;
    const TDN_CREATED: u32 = 0;
    const TD_INFORMATION_ICON: *const u16 = (-3i16 as u16) as usize as *const u16;
    const ICC_STANDARD_CLASSES: u32 = 0x0000_4000;
    const S_OK: i32 = 0;

    const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

    const HWND_TOPMOST: *mut c_void = -1isize as *mut c_void;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOACTIVATE: u32 = 0x0010;

    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    const DWMWA_USE_IMMERSIVE_DARK_MODE_BEFORE_20H1: u32 = 19;

    const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
    const RRF_RT_REG_DWORD: u32 = 0x0000_0010;

    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

    const MB_OKCANCEL: u32 = 0x0000_0001;
    const MB_ICONINFORMATION: u32 = 0x0000_0040;
    const MB_SETFOREGROUND: u32 = 0x0001_0000;
    const MB_TOPMOST: u32 = 0x0004_0000;
    const IDOK: i32 = 1;

    const MANIFEST_XML: &str = include_str!("win_dialog.manifest");

    #[repr(C)]
    struct InitCommonControlsEx {
        dw_size: u32,
        dw_icc: u32,
    }

    #[repr(C)]
    struct ActCtxW {
        cb_size: u32,
        dw_flags: u32,
        lp_source: *const u16,
        w_processor_architecture: u16,
        w_lang_id: u16,
        lp_assembly_directory: *const u16,
        lp_resource_name: *const u16,
        lp_application_name: *const u16,
        h_module: *mut c_void,
    }

    type SetThreadDpiAwarenessContextFn = unsafe extern "system" fn(isize) -> isize;
    type InitCommonControlsExFn = unsafe extern "system" fn(*const InitCommonControlsEx) -> i32;
    type TaskDialogIndirectFn = unsafe extern "system" fn(*const TaskDialogConfig, *mut i32, *mut i32, *mut i32) -> i32;
    type DwmSetWindowAttributeFn = unsafe extern "system" fn(*mut c_void, u32, *const c_void, u32) -> i32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(lp_lib_file_name: *const u16) -> *mut c_void;
        fn GetModuleHandleW(lp_module_name: *const u16) -> *mut c_void;
        fn GetProcAddress(h_module: *mut c_void, lp_proc_name: *const c_char) -> *const c_void;
        fn FreeLibrary(h_lib_module: *mut c_void) -> i32;
        fn CreateActCtxW(p_act_ctx: *const ActCtxW) -> *mut c_void;
        fn ActivateActCtx(h_act_ctx: *mut c_void, lp_cookie: *mut usize) -> i32;
        fn DeactivateActCtx(dw_flags: u32, ul_cookie: usize) -> i32;
        fn ReleaseActCtx(h_act_ctx: *mut c_void);
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowPos(
            h_wnd: *mut c_void,
            h_wnd_insert_after: *mut c_void,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            u_flags: u32,
        ) -> i32;
        fn SetForegroundWindow(h_wnd: *mut c_void) -> i32;
        fn MessageBoxW(h_wnd: *mut c_void, lp_text: *const u16, lp_caption: *const u16, u_type: u32) -> i32;
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

    struct LibGuard(*mut c_void);

    impl Drop for LibGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    FreeLibrary(self.0);
                }
            }
        }
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

    struct ActCtxGuard {
        handle: *mut c_void,
        cookie: usize,
        temp_path: PathBuf,
    }

    impl ActCtxGuard {
        fn activate_comctl_v6() -> Option<Self> {
            let temp_path = std::env::temp_dir().join(format!("agy-statusline-comctl-{}.manifest", std::process::id()));
            fs::write(&temp_path, MANIFEST_XML).ok()?;
            let source = to_wide_null(&temp_path.to_string_lossy());
            let ctx = ActCtxW {
                cb_size: size_of::<ActCtxW>() as u32,
                dw_flags: 0,
                lp_source: source.as_ptr(),
                w_processor_architecture: 0,
                w_lang_id: 0,
                lp_assembly_directory: null(),
                lp_resource_name: null(),
                lp_application_name: null(),
                h_module: null_mut(),
            };
            let handle = unsafe { CreateActCtxW(&ctx) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                let _ = fs::remove_file(&temp_path);
                return None;
            }
            let mut cookie: usize = 0;
            if unsafe { ActivateActCtx(handle, &mut cookie) } == 0 {
                unsafe { ReleaseActCtx(handle) };
                let _ = fs::remove_file(&temp_path);
                return None;
            }
            Some(Self { handle, cookie, temp_path })
        }
    }

    impl Drop for ActCtxGuard {
        fn drop(&mut self) {
            unsafe {
                DeactivateActCtx(0, self.cookie);
                ReleaseActCtx(self.handle);
            }
            let _ = fs::remove_file(&self.temp_path);
        }
    }

    fn is_windows_dark_mode() -> bool {
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
        status == 0 && data == 0
    }

    unsafe fn apply_dark_title_bar(hwnd: *mut c_void) {
        let dwm_name = to_wide_null("dwmapi.dll");
        let lib = LibGuard(unsafe { LoadLibraryW(dwm_name.as_ptr()) });
        if lib.0.is_null() {
            return;
        }
        let proc = unsafe { GetProcAddress(lib.0, c"DwmSetWindowAttribute".as_ptr()) };
        if proc.is_null() {
            return;
        }
        let set_attr = unsafe { transmute::<*const c_void, DwmSetWindowAttributeFn>(proc) };
        let dark: i32 = 1;
        let ptr = (&dark as *const i32).cast::<c_void>();
        let len = size_of::<i32>() as u32;
        if unsafe { set_attr(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, ptr, len) } < 0 {
            let _ = unsafe { set_attr(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE_BEFORE_20H1, ptr, len) };
        }
    }

    unsafe extern "system" fn task_dialog_callback(
        hwnd: *mut c_void,
        msg: u32,
        _wparam: usize,
        _lparam: isize,
        lp_ref_data: isize,
    ) -> i32 {
        if msg == TDN_CREATED {
            if lp_ref_data != 0 {
                unsafe { apply_dark_title_bar(hwnd) };
            }
            unsafe {
                SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                SetForegroundWindow(hwnd);
            }
        }
        S_OK
    }

    fn load_comctl_v6() -> Option<(Option<ActCtxGuard>, LibGuard, TaskDialogIndirectFn)> {
        let name = to_wide_null("comctl32.dll");
        let lib = LibGuard(unsafe { LoadLibraryW(name.as_ptr()) });
        if !lib.0.is_null() {
            let proc = unsafe { GetProcAddress(lib.0, c"TaskDialogIndirect".as_ptr()) };
            if !proc.is_null() {
                let func = unsafe { transmute::<*const c_void, TaskDialogIndirectFn>(proc) };
                return Some((None, lib, func));
            }
        }
        drop(lib);

        // Fallback for binaries built without the embedded ComCtl32 v6 manifest.
        let act_ctx = ActCtxGuard::activate_comctl_v6()?;
        let lib = LibGuard(unsafe { LoadLibraryW(name.as_ptr()) });
        if lib.0.is_null() {
            return None;
        }
        let proc = unsafe { GetProcAddress(lib.0, c"TaskDialogIndirect".as_ptr()) };
        if proc.is_null() {
            return None;
        }
        let func = unsafe { transmute::<*const c_void, TaskDialogIndirectFn>(proc) };
        Some((Some(act_ctx), lib, func))
    }

    fn show_task_dialog() -> Option<bool> {
        let (_act_ctx, comctl, task_dialog_indirect) = load_comctl_v6()?;

        let init_proc = unsafe { GetProcAddress(comctl.0, c"InitCommonControlsEx".as_ptr()) };
        if !init_proc.is_null() {
            let init_fn = unsafe { transmute::<*const c_void, InitCommonControlsExFn>(init_proc) };
            let icc = InitCommonControlsEx {
                dw_size: size_of::<InitCommonControlsEx>() as u32,
                dw_icc: ICC_STANDARD_CLASSES,
            };
            unsafe {
                init_fn(&icc);
            }
        }

        let title = to_wide_null(WINDOW_TITLE);
        let instruction = to_wide_null(MAIN_INSTRUCTION);
        let content = to_wide_null(CONTENT);
        let footer = to_wide_null(FOOTER);
        let btn_relaunch_txt = to_wide_null(LABEL_RELAUNCH);
        let btn_later_txt = to_wide_null(LABEL_LATER);

        let buttons = [
            TaskDialogButton { n_button_id: BTN_RELAUNCH, psz_button_text: btn_relaunch_txt.as_ptr() },
            TaskDialogButton { n_button_id: BTN_LATER, psz_button_text: btn_later_txt.as_ptr() },
        ];

        let config = TaskDialogConfig {
            cb_size: size_of::<TaskDialogConfig>() as u32,
            hwnd_parent: null_mut(),
            h_instance: null_mut(),
            dw_flags: TDF_ALLOW_DIALOG_CANCELLATION,
            dw_common_buttons: 0,
            psz_window_title: title.as_ptr(),
            psz_main_icon: TD_INFORMATION_ICON,
            psz_main_instruction: instruction.as_ptr(),
            psz_content: content.as_ptr(),
            c_buttons: buttons.len() as u32,
            p_buttons: buttons.as_ptr(),
            n_default_button: BTN_RELAUNCH,
            c_radio_buttons: 0,
            p_radio_buttons: null(),
            n_default_radio_button: 0,
            psz_verification_text: null(),
            psz_expanded_information: null(),
            psz_expanded_control_text: null(),
            psz_collapsed_control_text: null(),
            psz_footer_icon: TD_INFORMATION_ICON,
            psz_footer: footer.as_ptr(),
            pf_callback: Some(task_dialog_callback),
            lp_callback_data: isize::from(is_windows_dark_mode()),
            cx_width: 0,
        };

        let mut selected: i32 = 0;
        let hr = unsafe { task_dialog_indirect(&config, &mut selected, null_mut(), null_mut()) };
        if hr < 0 {
            return None;
        }
        Some(selected == BTN_RELAUNCH)
    }

    fn show_fallback_message_box() -> bool {
        let title = to_wide_null(WINDOW_TITLE);
        let text = to_wide_null(&format!("{MAIN_INSTRUCTION}\n\n{CONTENT}\n\n（{FOOTER}）"));
        let ret = unsafe {
            MessageBoxW(
                null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OKCANCEL | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
            )
        };
        ret == IDOK
    }

    pub fn ask_relaunch_confirm() -> bool {
        let _dpi = DpiGuard::enter_per_monitor_v2();
        show_task_dialog().unwrap_or_else(show_fallback_message_box)
    }
}

#[cfg(target_os = "windows")]
pub use imp::ask_relaunch_confirm;

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn task_dialog_abi_layout_matches_windows_commctrl() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(size_of::<TaskDialogButton>(), 12);
            assert_eq!(offset_of!(TaskDialogButton, n_button_id), 0);
            assert_eq!(offset_of!(TaskDialogButton, psz_button_text), 4);

            assert_eq!(size_of::<TaskDialogConfig>(), 160);
            assert_eq!(offset_of!(TaskDialogConfig, cb_size), 0);
            assert_eq!(offset_of!(TaskDialogConfig, hwnd_parent), 4);
            assert_eq!(offset_of!(TaskDialogConfig, h_instance), 12);
            assert_eq!(offset_of!(TaskDialogConfig, dw_flags), 20);
            assert_eq!(offset_of!(TaskDialogConfig, dw_common_buttons), 24);
            assert_eq!(offset_of!(TaskDialogConfig, psz_window_title), 28);
            assert_eq!(offset_of!(TaskDialogConfig, psz_main_icon), 36);
            assert_eq!(offset_of!(TaskDialogConfig, psz_main_instruction), 44);
            assert_eq!(offset_of!(TaskDialogConfig, psz_content), 52);
            assert_eq!(offset_of!(TaskDialogConfig, c_buttons), 60);
            assert_eq!(offset_of!(TaskDialogConfig, p_buttons), 64);
            assert_eq!(offset_of!(TaskDialogConfig, n_default_button), 72);
            assert_eq!(offset_of!(TaskDialogConfig, psz_footer_icon), 124);
            assert_eq!(offset_of!(TaskDialogConfig, psz_footer), 132);
            assert_eq!(offset_of!(TaskDialogConfig, pf_callback), 140);
            assert_eq!(offset_of!(TaskDialogConfig, lp_callback_data), 148);
            assert_eq!(offset_of!(TaskDialogConfig, cx_width), 156);
        }
        #[cfg(target_pointer_width = "32")]
        {
            assert_eq!(size_of::<TaskDialogButton>(), 8);
            assert_eq!(size_of::<TaskDialogConfig>(), 96);
        }
    }

    #[test]
    fn wide_null_encoding_terminates_with_zero() {
        let w = to_wide_null("重启 AGY");
        assert_eq!(w.last(), Some(&0));
        assert_eq!(String::from_utf16(&w[..w.len() - 1]).unwrap(), "重启 AGY");
    }
}
