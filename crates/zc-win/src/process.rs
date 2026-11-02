//! Open windows and running processes, for per-app audio tracks (DESIGN §6, D26).
//!
//! Everything here is a read-only query. Nothing is injected, suspended or
//! terminated, and no admin rights are needed for the processes we can open.
//!
//! Windows are listed with [`EnumWindows`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-enumwindows),
//! processes with a Toolhelp snapshot. Each process is opened with only
//! [`PROCESS_QUERY_LIMITED_INFORMATION`](https://learn.microsoft.com/en-us/windows/win32/procthread/process-security-and-access-rights),
//! which is enough for an image path and timestamps and is granted for more processes
//! than full query rights.

use std::ffi::c_void;
use std::path::Path;

use windows::Win32::Foundation::{FILETIME, HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, Owned, PWSTR};

/// Shell and UI host processes whose windows are never an audio source.
const SKIPPED_EXES: [&str; 4] =
    ["explorer.exe", "ApplicationFrameHost.exe", "TextInputHost.exe", "SystemSettings.exe"];

/// A top-level window the user could pick as an audio source.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenApp {
    /// Process id of the window's owner.
    pub pid: u32,
    /// Executable file name, such as `Discord.exe`.
    pub exe: String,
    /// The window's title.
    pub title: String,
}

/// The visible, user-facing top-level windows, one per executable.
///
/// Sorted by executable name, case-insensitively. For an executable with several
/// windows, the first one in Z-order keeps its title and pid. Never panics. A
/// window whose process can't be opened is skipped.
#[must_use]
pub fn list_open_apps() -> Vec<OpenApp> {
    let mut apps: Vec<OpenApp> = Vec::new();
    // SAFETY: `collect_window` runs only during this call, on this thread, and
    // `apps` outlives the call. The LPARAM carries that pointer and nothing else.
    // The callback always returns TRUE, so the only error would be a failed enumeration,
    // and the partial list is still usable.
    let _ = unsafe { EnumWindows(Some(collect_window), LPARAM(&raw mut apps as isize)) };
    apps.sort_by_key(|app| app.exe.to_lowercase());
    apps
}

/// `EnumWindows` callback: appends `hwnd` to the `Vec<OpenApp>` behind `lparam`.
unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut Vec<OpenApp>` that `list_open_apps` passed to
    // `EnumWindows`. It is valid and unaliased for the whole enumeration.
    let apps = unsafe { &mut *(lparam.0 as *mut Vec<OpenApp>) };
    if let Some(app) = describe_window(hwnd)
        && !apps.iter().any(|known| known.exe.eq_ignore_ascii_case(&app.exe))
    {
        apps.push(app);
    }
    BOOL(1)
}

/// `Some` if `hwnd` is a window the user could pick as an audio source. The filters
/// mirror what Windows leaves out of its Alt+Tab list: hidden, cloaked, tool and owned windows.
/// See [`IsWindowVisible`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-iswindowvisible).
fn describe_window(hwnd: HWND) -> Option<OpenApp> {
    // SAFETY: IsWindowVisible accepts any HWND value and returns FALSE for a bad one.
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return None;
    }
    if is_cloaked(hwnd) || is_tool_window(hwnd) || has_owner(hwnd) {
        return None;
    }
    let title = window_title(hwnd)?;

    let mut pid = 0_u32;
    // SAFETY: `pid` is a valid out-pointer for the process id. The returned thread id is unused.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
    if pid == 0 || pid == std::process::id() {
        return None;
    }
    let exe = exe_name_for_pid(pid)?;
    if SKIPPED_EXES.iter().any(|skipped| exe_matches(&exe, skipped)) {
        return None;
    }
    Some(OpenApp { pid, exe, title })
}

/// Whether DWM has cloaked the window (hidden UWP, another virtual desktop, and so on).
/// A failed query counts as not cloaked, so no window is lost to an API error. See
/// [`DwmGetWindowAttribute`](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/nf-dwmapi-dwmgetwindowattribute)
/// and [`DWMWA_CLOAKED`](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/ne-dwmapi-dwmwindowattribute).
fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0_u32;
    // SAFETY: `cloaked` is a u32 and we pass its size, as DWMWA_CLOAKED requires.
    let queried = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast::<c_void>(),
            size_of::<u32>() as u32,
        )
    };
    queried.is_ok() && cloaked != 0
}

/// Whether the window has the `WS_EX_TOOLWINDOW` style: a floating palette, not an app's
/// main window. See [extended window styles](https://learn.microsoft.com/en-us/windows/win32/winmsg/extended-window-styles)
/// and [`GetWindowLongPtrW`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getwindowlongptrw).
fn is_tool_window(hwnd: HWND) -> bool {
    // SAFETY: GetWindowLongPtrW reads the extended style of `hwnd`. A bad handle gives 0.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    style & WS_EX_TOOLWINDOW.0 != 0
}

/// Whether the window has an owner window. Owned windows are dialogs and secondary
/// windows of an app whose main window is already listed. See
/// [`GetWindow`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getwindow).
fn has_owner(hwnd: HWND) -> bool {
    // SAFETY: GetWindow reads the owner of `hwnd`. A window with no owner gives an error or a null handle.
    unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|owner| !owner.0.is_null())
}

/// The trimmed title, or `None` when the window has none. Read with
/// [`GetWindowTextW`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getwindowtextw),
/// the UTF-16 variant, so non-ASCII titles survive.
fn window_title(hwnd: HWND) -> Option<String> {
    let mut buffer = [0_u16; 512];
    // SAFETY: the buffer is writable and we pass its exact length. The return value is the copied length.
    let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    let copied = usize::try_from(copied).ok()?;
    let title = String::from_utf16_lossy(buffer.get(..copied)?);
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_owned())
}

/// The executable file name (no directory) of process `pid`, or `None` if it can't be opened.
/// The full image path comes from [`QueryFullProcessImageNameW`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-queryfullprocessimagenamew),
/// which works with limited rights, unlike the older module-name APIs.
fn exe_name_for_pid(pid: u32) -> Option<String> {
    let process = open_process_limited(pid)?;
    let mut buffer = [0_u16; 1024];
    let mut len = u32::try_from(buffer.len()).ok()?;
    // SAFETY: `buffer` is writable and `len` holds its length, as the call requires. `*process` is a live handle opened with query rights.
    unsafe {
        QueryFullProcessImageNameW(
            *process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &raw mut len,
        )
    }
    .ok()?;
    let full = String::from_utf16_lossy(buffer.get(..usize::try_from(len).ok()?)?);
    Some(file_name(&full).to_owned())
}

/// Opens `pid` with the least access that gives its image path and creation time. The handle closes when the result drops.
/// See [`OpenProcess`](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocess).
fn open_process_limited(pid: u32) -> Option<Owned<windows::Win32::Foundation::HANDLE>> {
    // SAFETY: OpenProcess only reads its arguments. The handle it returns is wrapped in `Owned` below, which closes it.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    // SAFETY: `handle` is a fresh handle from OpenProcess, and nothing else owns it.
    Some(unsafe { Owned::new(handle) })
}

/// The process's creation time in 100 ns ticks since 1601, or `None` if unavailable. Read
/// with [`GetProcessTimes`](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getprocesstimes),
/// which fills four [`FILETIME`](https://learn.microsoft.com/en-us/windows/win32/api/minwinbase/ns-minwinbase-filetime)s;
/// only the creation one is used.
fn creation_time(pid: u32) -> Option<u64> {
    let process = open_process_limited(pid)?;
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: `*process` is a live handle with query rights. All four out-pointers are valid `FILETIME`s.
    unsafe {
        GetProcessTimes(*process, &raw mut created, &raw mut exited, &raw mut kernel, &raw mut user)
    }
    .ok()?;
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// The root of the process tree for `exe`: its matching process with the earliest creation time.
/// A parent starts before its children, so the earliest-started match is normally the root.
///
/// `None` if no process matches. If no creation time can be read, the lowest pid
/// is used. Matching is per [`exe_matches`].
#[must_use]
pub fn root_pid_for_exe(exe: &str) -> Option<u32> {
    let pids = pids_for_exe(exe);
    let timed =
        pids.iter().filter_map(|&pid| creation_time(pid).map(|created| (created, pid))).min();
    timed.map(|(_, pid)| pid).or_else(|| pids.iter().copied().min())
}

/// Every running process whose executable name matches `exe`. Walks a Toolhelp snapshot:
/// [`CreateToolhelp32Snapshot`](https://learn.microsoft.com/en-us/windows/win32/api/tlhelp32/nf-tlhelp32-createtoolhelp32snapshot)
/// with [`Process32FirstW`](https://learn.microsoft.com/en-us/windows/win32/api/tlhelp32/nf-tlhelp32-process32firstw)
/// and [`Process32NextW`](https://learn.microsoft.com/en-us/windows/win32/api/tlhelp32/nf-tlhelp32-process32nextw).
fn pids_for_exe(exe: &str) -> Vec<u32> {
    // SAFETY: TH32CS_SNAPPROCESS with pid 0 takes a snapshot of all processes. The result is wrapped in `Owned` below.
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    // SAFETY: `snapshot` is a fresh handle from CreateToolhelp32Snapshot, and nothing else owns it.
    let snapshot = unsafe { Owned::new(snapshot) };

    let mut pids = Vec::new();
    let mut entry =
        PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    // SAFETY: `*snapshot` is a live toolhelp snapshot. `entry.dwSize` is set, as the API requires.
    let mut more = unsafe { Process32FirstW(*snapshot, &raw mut entry) }.is_ok();
    while more {
        if exe_matches(&wide_to_string(&entry.szExeFile), exe) {
            pids.push(entry.th32ProcessID);
        }
        // SAFETY: same snapshot and entry as above. An error here means the walk is done.
        more = unsafe { Process32NextW(*snapshot, &raw mut entry) }.is_ok();
    }
    pids
}

/// Whether two executable names refer to the same file: case-insensitive, and any
/// directory prefix is ignored. `"C:\\Apps\\Discord.exe"` matches `"discord.exe"`.
#[must_use]
pub fn exe_matches(a: &str, b: &str) -> bool {
    let (a, b) = (file_name(a), file_name(b));
    !a.is_empty() && a.eq_ignore_ascii_case(b)
}

/// The part after the last `\` or `/`.
fn file_name(path: &str) -> &str {
    Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or(path)
}

/// The UTF-16 string up to its first nul.
fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_matches_ignores_case_and_directories() {
        assert!(exe_matches("Discord.exe", "discord.exe"));
        assert!(exe_matches(r"C:\Apps\Discord.exe", "Discord.exe"));
        assert!(exe_matches("C:/Apps/Discord.exe", r"D:\Other\DISCORD.EXE"));
    }

    #[test]
    fn exe_matches_rejects_other_names_and_empty_input() {
        assert!(!exe_matches("Discord.exe", "Discord"));
        assert!(!exe_matches("Spotify.exe", "Discord.exe"));
        assert!(!exe_matches(r"C:\Apps\", r"D:\Other\"));
        assert!(!exe_matches("", ""));
    }

    #[test]
    fn file_name_takes_the_last_component() {
        assert_eq!(file_name(r"C:\a\b.exe"), "b.exe");
        assert_eq!(file_name("b.exe"), "b.exe");
    }

    #[test]
    fn listing_open_apps_does_not_panic_and_is_clean() {
        let apps = list_open_apps();
        for app in &apps {
            assert!(!SKIPPED_EXES.iter().any(|s| exe_matches(&app.exe, s)));
            assert_ne!(app.pid, std::process::id());
        }
        let mut sorted = apps.clone();
        sorted.sort_by_key(|app| app.exe.to_lowercase());
        assert_eq!(apps, sorted);
        let unique: std::collections::HashSet<_> =
            apps.iter().map(|app| app.exe.to_lowercase()).collect();
        assert_eq!(unique.len(), apps.len());
    }

    #[test]
    fn a_missing_executable_has_no_root_pid() {
        assert_eq!(root_pid_for_exe("zc-no-such-process-xyz.exe"), None);
    }
}
