use tray_icon::Icon;

/// Generate a 64x64 RGBA tray icon: dark background with a red record dot.
pub fn create_tray_icon() -> Icon {
    let size: u32 = 64;
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    let bg = [0x2D, 0x2D, 0x2D, 0xFF];
    let red = [0xE0, 0x40, 0x40, 0xFF];
    let white = [0xFF, 0xFF, 0xFF, 0xFF];

    for y in 0..size {
        for x in 0..size {
            let idx = ((y * size + x) * 4) as usize;

            // Background
            rgba[idx..idx + 4].copy_from_slice(&bg);

            // Red circle (center 32,32, radius 24)
            let dx = x as f32 - 32.0;
            let dy = y as f32 - 32.0;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= 24.0 {
                rgba[idx..idx + 4].copy_from_slice(&red);
            }

            // White "S" — simple block letter
            if draw_s(x, y) {
                rgba[idx..idx + 4].copy_from_slice(&white);
            }
        }
    }

    Icon::from_rgba(rgba, size, size).expect("Failed to create tray icon")
}

/// Returns true if pixel (x,y) is part of a block-letter "S" centered in the icon.
fn draw_s(x: u32, y: u32) -> bool {
    // S shape within a ~20x28 box centered at (32, 32)
    let x = x as i32;
    let y = y as i32;

    let left = 22;
    let right = 42;
    let thick = 4;

    // Top horizontal bar
    if y >= 18 && y < 18 + thick && x >= left && x < right {
        return true;
    }
    // Upper left vertical
    if x >= left && x < left + thick && y >= 18 && y < 30 {
        return true;
    }
    // Middle horizontal bar
    if y >= 30 && y < 30 + thick && x >= left && x < right {
        return true;
    }
    // Lower right vertical
    if x >= right - thick && x < right && y >= 30 && y < 42 {
        return true;
    }
    // Bottom horizontal bar
    if y >= 42 && y < 42 + thick && x >= left && x < right {
        return true;
    }
    false
}
