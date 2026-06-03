pub fn render_to_terminal(data: &str) -> Option<String> {
    let code = qrcode::QrCode::with_error_correction_level(data.as_bytes(), qrcode::EcLevel::L)
        .ok()?;
    let modules = code.to_colors();
    let width = code.width();
    let total_rows = modules.len() / width;
    let mut out = String::new();

    // Two modules per row keeps the output compact while preserving contrast.
    let mut row = 0;
    while row < total_rows {
        out.push(' ');
        for col in 0..width {
            let top_dark = modules[row * width + col] == qrcode::Color::Dark;
            let bottom_dark = if row + 1 < total_rows {
                modules[(row + 1) * width + col] == qrcode::Color::Dark
            } else {
                false
            };
            let ch = match (top_dark, bottom_dark) {
                (false, false) => ' ',
                (true, false) => '\u{2580}',
                (false, true) => '\u{2584}',
                (true, true) => '\u{2588}',
            };
            out.push(ch);
        }
        out.push('\n');
        row += 2;
    }

    Some(out)
}
