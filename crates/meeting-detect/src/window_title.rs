use crate::{ProcessInfo, Source};

/// The browser hosting a meeting, and which meeting it is, found by window title.
pub(crate) fn meeting_process(browsers: &[ProcessInfo]) -> Option<(ProcessInfo, Source)> {
    platform::meeting_process(browsers)
}

#[cfg(target_os = "macos")]
pub(crate) use platform::{request_screen_recording, screen_recording_granted};

pub(crate) fn is_meet_title(title: &str) -> bool {
    let title = title.trim();
    title == "Google Meet" || title.starts_with("Meet – ") || title.starts_with("Meet - ")
}

/// Cal Video titles its meeting page "Cal.com Video"; on Windows the browser appends its own name.
pub(crate) fn is_cal_video_title(title: &str) -> bool {
    title.trim().starts_with("Cal.com Video")
}

fn source_for_title(title: &str) -> Option<Source> {
    if is_meet_title(title) {
        Some(Source::Meet)
    } else if is_cal_video_title(title) {
        Some(Source::CalVideo)
    } else {
        None
    }
}

fn matching_browser(browsers: &[ProcessInfo], pid: u32, title: &str) -> Option<(ProcessInfo, Source)> {
    let source = source_for_title(title)?;
    let browser = browsers.iter().find(|browser| browser.pid == Some(pid)).cloned()?;
    Some((browser, source))
}

#[cfg(target_os = "windows")]
mod platform {
    use super::matching_browser;
    use crate::{ProcessInfo, Source};
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible};

    struct Search<'a> {
        browsers: &'a [ProcessInfo],
        found: Option<(ProcessInfo, Source)>,
    }

    unsafe extern "system" fn inspect_window(window: HWND, parameter: LPARAM) -> BOOL {
        let search = unsafe { &mut *(parameter.0 as *mut Search<'_>) };
        if !unsafe { IsWindowVisible(window) }.as_bool() {
            return BOOL(1);
        }

        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        if !search.browsers.iter().any(|browser| browser.pid == Some(pid)) {
            return BOOL(1);
        }

        let mut buffer = [0_u16; 1024];
        let length = unsafe { GetWindowTextW(window, &mut buffer) };
        if length <= 0 {
            return BOOL(1);
        }

        let title = String::from_utf16_lossy(&buffer[..length as usize]);
        search.found = matching_browser(search.browsers, pid, &title);
        if search.found.is_some() {
            BOOL(0)
        } else {
            BOOL(1)
        }
    }

    pub(super) fn meeting_process(browsers: &[ProcessInfo]) -> Option<(ProcessInfo, Source)> {
        let mut search = Search { browsers, found: None };
        let parameter = LPARAM((&mut search as *mut Search<'_>) as isize);
        // Stopping enumeration after a match reports an error, so the result itself is intentionally
        // ignored. A genuine API failure and an empty search both safely produce `None`.
        let _ = unsafe { EnumWindows(Some(inspect_window), parameter) };
        search.found
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::matching_browser;
    use crate::{ProcessInfo, Source};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt, Window};

    fn environment_is_wayland() -> bool {
        let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
        if session_type.eq_ignore_ascii_case("wayland") {
            return true;
        }
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
        let x11 = std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
        wayland && !x11
    }

    fn atom(connection: &impl Connection, name: &[u8]) -> Option<Atom> {
        connection.intern_atom(false, name).ok()?.reply().ok().map(|reply| reply.atom)
    }

    fn property(
        connection: &impl Connection,
        window: Window,
        property: Atom,
        property_type: Atom,
    ) -> Option<x11rb::protocol::xproto::GetPropertyReply> {
        connection
            .get_property(false, window, property, property_type, 0, u32::MAX)
            .ok()?
            .reply()
            .ok()
    }

    pub(super) fn first_cardinal(reply: &x11rb::protocol::xproto::GetPropertyReply) -> Option<u32> {
        if reply.format != 32 {
            return None;
        }
        reply.value32()?.next()
    }

    pub(super) fn utf8_title(reply: x11rb::protocol::xproto::GetPropertyReply) -> Option<String> {
        if reply.format != 8 {
            return None;
        }
        String::from_utf8(reply.value).ok()
    }

    fn legacy_title(reply: x11rb::protocol::xproto::GetPropertyReply) -> Option<String> {
        if reply.format != 8 {
            return None;
        }
        Some(reply.value.into_iter().map(char::from).collect())
    }

    pub(super) fn meeting_process(browsers: &[ProcessInfo]) -> Option<(ProcessInfo, Source)> {
        if browsers.is_empty() || environment_is_wayland() {
            return None;
        }

        let (connection, screen_index) = x11rb::connect(None).ok()?;
        let root = connection.setup().roots.get(screen_index)?.root;
        let client_list = atom(&connection, b"_NET_CLIENT_LIST")?;
        let pid_atom = atom(&connection, b"_NET_WM_PID")?;
        let name_atom = atom(&connection, b"_NET_WM_NAME")?;
        let utf8_string = atom(&connection, b"UTF8_STRING")?;
        let windows = property(&connection, root, client_list, AtomEnum::WINDOW.into())?;

        for window in windows.value32()? {
            let pid = property(&connection, window, pid_atom, AtomEnum::CARDINAL.into())
                .as_ref()
                .and_then(first_cardinal);
            let Some(pid) = pid else { continue };
            if !browsers.iter().any(|browser| browser.pid == Some(pid)) {
                continue;
            }

            let title = property(&connection, window, name_atom, utf8_string)
                .and_then(utf8_title)
                .or_else(|| property(&connection, window, AtomEnum::WM_NAME.into(), AtomEnum::ANY.into()).and_then(legacy_title));
            if let Some(found) = title.and_then(|title| matching_browser(browsers, pid, &title)) {
                return Some(found);
            }
        }
        None
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::matching_browser;
    use crate::{ProcessInfo, Source};
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use core_graphics::access::ScreenCaptureAccess;
    use core_graphics::window::{
        copy_window_info, kCGNullWindowID, kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly, kCGWindowName,
        kCGWindowOwnerPID,
    };
    use std::ffi::c_void;

    fn value(dictionary: &CFDictionary, key: CFStringRef) -> Option<CFType> {
        let raw = *dictionary.find(key.cast::<c_void>())?;
        Some(unsafe { CFType::wrap_under_get_rule(raw.cast()) })
    }

    fn window_identity(dictionary: &CFDictionary) -> Option<(u32, String)> {
        let pid = value(dictionary, unsafe { kCGWindowOwnerPID })?
            .downcast::<CFNumber>()?
            .to_i64()?;
        let pid = u32::try_from(pid).ok()?;
        let title = value(dictionary, unsafe { kCGWindowName })?
            .downcast::<CFString>()?
            .to_string();
        Some((pid, title))
    }

    pub(crate) fn screen_recording_granted() -> bool {
        ScreenCaptureAccess.preflight()
    }

    /// Shows the system prompt, and registers the app in System Settings so the user can grant it
    /// later. macOS only ever asks once per app identity; after that this returns the standing answer.
    pub(crate) fn request_screen_recording() -> bool {
        ScreenCaptureAccess.request()
    }

    pub(super) fn meeting_process(browsers: &[ProcessInfo]) -> Option<(ProcessInfo, Source)> {
        if browsers.is_empty() {
            return None;
        }
        if !ScreenCaptureAccess.preflight() {
            // Without this permission the window list comes back with no titles at all, which is
            // indistinguishable from "no meeting" — so say it out loud rather than return silence.
            tracing::warn!("screen recording permission missing; Google Meet and Cal Video cannot be detected");
            return None;
        }

        let windows = copy_window_info(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            kCGNullWindowID,
        )?;
        for raw in windows.get_all_values() {
            let object = unsafe { CFType::wrap_under_get_rule(raw.cast()) };
            let Some(dictionary) = object.downcast::<CFDictionary>() else {
                continue;
            };
            let Some((pid, title)) = window_identity(&dictionary) else {
                continue;
            };
            if let Some(found) = matching_browser(browsers, pid, &title) {
                return Some(found);
            }
        }
        None
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
mod platform {
    use crate::{ProcessInfo, Source};

    pub(super) fn meeting_process(_browsers: &[ProcessInfo]) -> Option<(ProcessInfo, Source)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser(pid: Option<u32>, name: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            name: name.into(),
            executable: None,
            bundle_id: None,
        }
    }

    #[test]
    fn recognizes_only_meet_window_titles() {
        assert!(is_meet_title("Meet – Daily standup"));
        assert!(is_meet_title("Meet - Daily standup"));
        assert!(is_meet_title("  Google Meet  "));
        assert!(!is_meet_title("Google Meet - Google Chrome"));
        assert!(!is_meet_title("Inbox - Google Chrome"));
    }

    #[test]
    fn recognizes_cal_video_titles_with_or_without_the_browser_suffix() {
        assert!(is_cal_video_title("Cal.com Video"));
        assert!(is_cal_video_title("Cal.com Video - Google Chrome"));
        assert!(is_cal_video_title("Cal.com Video – Brave"));
        assert!(!is_cal_video_title("No meeting found | Cal.com"));
        assert!(!is_cal_video_title("Cal.com | Scheduling"));
        assert_eq!(source_for_title("Cal.com Video"), Some(Source::CalVideo));
        assert_eq!(source_for_title("Meet – Daily standup"), Some(Source::Meet));
        assert_eq!(source_for_title("Inbox"), None);
    }

    #[test]
    fn matches_only_a_supplied_browser_pid() {
        let browsers = [browser(Some(42), "chrome"), browser(None, "firefox")];
        assert_eq!(
            matching_browser(&browsers, 42, "Meet – Daily standup"),
            Some((browsers[0].clone(), Source::Meet))
        );
        assert_eq!(
            matching_browser(&browsers, 42, "Cal.com Video - Google Chrome"),
            Some((browsers[0].clone(), Source::CalVideo))
        );
        assert_eq!(matching_browser(&browsers, 7, "Google Meet"), None);
        assert_eq!(matching_browser(&browsers, 42, "Inbox"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn x11_property_parsers_reject_wrong_formats() {
        use x11rb::protocol::xproto::GetPropertyReply;

        let reply = GetPropertyReply {
            format: 8,
            sequence: 0,
            length: 0,
            type_: 0,
            bytes_after: 0,
            value_len: 1,
            value: vec![42],
        };
        assert_eq!(platform::first_cardinal(&reply), None);
        assert_eq!(platform::utf8_title(reply).as_deref(), Some("*"));
    }
}
