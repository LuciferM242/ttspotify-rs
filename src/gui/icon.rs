//! Tray icon generation.

use wxdragon::prelude::*;

/// Generate a 16x16 green circle icon for the system tray.
pub fn create_icon() -> Bitmap {
    let size = 16u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let center = size as f32 / 2.0;
    let radius = center - 2.0;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let offset = ((y * size + x) * 4) as usize;
            if dx * dx + dy * dy <= radius * radius {
                rgba[offset] = 0;       // R
                rgba[offset + 1] = 150; // G
                rgba[offset + 2] = 0;   // B
                rgba[offset + 3] = 255; // A
            }
        }
    }

    Bitmap::from_rgba(&rgba, size, size).expect("Failed to create tray icon bitmap")
}
