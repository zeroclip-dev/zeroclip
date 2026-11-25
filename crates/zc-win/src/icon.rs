//! A program's icon as pixels, for the game list.

use std::mem::size_of;
use std::path::Path;

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, ReleaseDC,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};
use windows::core::HSTRING;

/// An icon: width, height and RGBA bytes, row by row from the top.
pub struct IconPixels {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Four bytes a pixel: red, green, blue, alpha.
    pub rgba: Vec<u8>,
}

/// The large icon of the program at `path`, or `None` if it has none or can't be read. Read with
/// [`SHGetFileInfoW`](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shgetfileinfow).
#[must_use]
pub fn icon_of(path: &Path) -> Option<IconPixels> {
    let wide = HSTRING::from(path.as_os_str());
    let mut info = SHFILEINFOW::default();
    // SAFETY: `wide` is a nul-terminated path and `info` a valid out-struct of the size given.
    let ok = unsafe {
        SHGetFileInfoW(
            &wide,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&raw mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if ok == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let pixels = read_icon(&info.hIcon);
    // SAFETY: `hIcon` came from SHGetFileInfoW with SHGFI_ICON, so this caller owns and must free it.
    let _ = unsafe { DestroyIcon(info.hIcon) };
    pixels
}

fn read_icon(icon: &windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<IconPixels> {
    let mut parts = ICONINFO::default();
    // SAFETY: `parts` is a valid out-struct. Both bitmaps it gets are freed below.
    unsafe { GetIconInfo(*icon, &raw mut parts) }.ok()?;
    let mut bitmap = BITMAP::default();
    // SAFETY: `hbmColor` is a bitmap handle from GetIconInfo; `bitmap` is BITMAP-sized.
    let got = unsafe {
        GetObjectW(
            parts.hbmColor.into(),
            size_of::<BITMAP>() as i32,
            Some((&raw mut bitmap).cast()),
        )
    };
    let (width, height) = (bitmap.bmWidth, bitmap.bmHeight);
    let mut rgba = Vec::new();
    if got != 0 && width > 0 && height > 0 {
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // negative: top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        rgba = vec![0_u8; width as usize * height as usize * 4];
        // SAFETY: a screen DC for GetDIBits.
        let dc = unsafe { GetDC(None) };
        // SAFETY: `rgba` holds width*height 32-bit pixels, as `header` asks; `dc` is live.
        let lines = unsafe {
            GetDIBits(
                dc,
                parts.hbmColor,
                0,
                height as u32,
                Some(rgba.as_mut_ptr().cast()),
                &raw mut header,
                DIB_RGB_COLORS,
            )
        };
        // SAFETY: `dc` came from GetDC(None) just above.
        unsafe { ReleaseDC(None, dc) };
        if lines == 0 {
            rgba.clear();
        }
    }
    // SAFETY: both bitmaps were created for us by GetIconInfo and are freed once.
    unsafe {
        let _ = DeleteObject(parts.hbmColor.into());
        let _ = DeleteObject(parts.hbmMask.into());
    }
    if rgba.is_empty() {
        return None;
    }
    // BGRA to RGBA. An icon without an alpha channel has all zeros there: make it opaque.
    let opaque = rgba.chunks_exact(4).all(|p| p[3] == 0);
    for p in rgba.chunks_exact_mut(4) {
        p.swap(0, 2);
        if opaque {
            p[3] = 255;
        }
    }
    Some(IconPixels { width: width as u32, height: height as u32, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_system_program_has_a_visible_icon() {
        let icon = icon_of(Path::new(r"C:\Windows\explorer.exe")).expect("explorer has an icon");
        assert_eq!(icon.rgba.len(), (icon.width * icon.height * 4) as usize);
        assert!(icon.rgba.chunks_exact(4).any(|p| p[3] > 0), "all transparent");
    }
}
