use std::ffi::c_void;

use windows::core::w;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::WindowsAndMessaging::{CreateIcon, HICON};

/// Pixel width/height of the embedded Noren system-tray alpha mask.
pub const TRAY_ICON_SIZE: usize = 32;

/// 32x32 anti-aliased alpha mask derived from `docs/assets/noren-logo.png`.
pub const TRAY_ICON_ALPHA: &[u8; TRAY_ICON_SIZE * TRAY_ICON_SIZE] =
    include_bytes!("../assets/noren-logo-alpha-32.bin");

/// Returns `true` if Windows is currently using a dark taskbar (`SystemUsesLightTheme == 0`).
pub fn system_taskbar_is_dark() -> bool {
    let mut data: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut data as *mut u32).cast::<c_void>()),
            Some(&mut size),
        )
    };
    if status.is_ok() {
        data == 0
    } else {
        true
    }
}

/// Creates a monochrome-tinted 32x32 Noren tray icon (`HICON`) crisp on dark or light taskbars.
pub fn create_tray_icon(dark_taskbar: bool) -> Option<HICON> {
    let (r, g, b): (u8, u8, u8) = if dark_taskbar {
        (245, 245, 245)
    } else {
        (24, 24, 24)
    };

    let mut xor = vec![0u8; TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4];
    for (idx, &alpha) in TRAY_ICON_ALPHA.iter().enumerate() {
        let i = idx * 4;
        xor[i] = b;
        xor[i + 1] = g;
        xor[i + 2] = r;
        xor[i + 3] = alpha;
    }
    let and = vec![0u8; TRAY_ICON_SIZE * TRAY_ICON_SIZE / 8];
    unsafe {
        CreateIcon(
            None,
            TRAY_ICON_SIZE as i32,
            TRAY_ICON_SIZE as i32,
            1,
            32,
            and.as_ptr(),
            xor.as_ptr(),
        )
        .ok()
    }
}

/// Creates a 32x32 rounded Noren app icon (`HICON`) on a light `#FAFAFA` squircle badge
/// so the silhouette remains sharp on both light and dark titlebars/taskbars.
pub fn create_app_icon() -> Option<HICON> {
    const N: usize = TRAY_ICON_SIZE;
    let rrect_cov = |px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, rad: f32| -> f32 {
        let cx = (x0 + x1) * 0.5;
        let cy = (y0 + y1) * 0.5;
        let hx = (x1 - x0) * 0.5 - rad;
        let hy = (y1 - y0) * 0.5 - rad;
        let qx = (px - cx).abs() - hx;
        let qy = (py - cy).abs() - hy;
        let dist = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - rad;
        (0.5 - dist).clamp(0.0, 1.0)
    };

    let mut xor = vec![0u8; N * N * 4];
    for y in 0..N {
        for x in 0..N {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let bg_cov = rrect_cov(px, py, 1.0, 1.0, 31.0, 31.0, 6.5);
            let ink_a = TRAY_ICON_ALPHA[y * N + x] as f32 / 255.0;
            // Blend #1A1A1A (26) ink over #FAFAFA (250) badge
            let val = (250.0 + (26.0 - 250.0) * ink_a).round() as u8;
            let a = (bg_cov * 255.0).round() as u8;
            let i = (y * N + x) * 4;
            xor[i] = val;
            xor[i + 1] = val;
            xor[i + 2] = val;
            xor[i + 3] = a;
        }
    }
    let and = vec![0u8; N * N / 8];
    unsafe { CreateIcon(None, N as i32, N as i32, 1, 32, and.as_ptr(), xor.as_ptr()).ok() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;

    #[test]
    fn tray_alpha_mask_is_valid_and_non_empty() {
        assert_eq!(TRAY_ICON_ALPHA.len(), 1024);
        assert_eq!(TRAY_ICON_ALPHA[0], 0);
        let strong_ink = TRAY_ICON_ALPHA.iter().filter(|&&a| a >= 180).count();
        assert!(strong_ink >= 100, "expected >=100 ink pixels, got {strong_ink}");
    }

    #[test]
    fn creates_tray_and_app_hicons() {
        let dark = create_tray_icon(true).expect("dark tray icon");
        let light = create_tray_icon(false).expect("light tray icon");
        let app = create_app_icon().expect("app icon");
        unsafe {
            let _ = DestroyIcon(dark);
            let _ = DestroyIcon(light);
            let _ = DestroyIcon(app);
        }
    }
}
