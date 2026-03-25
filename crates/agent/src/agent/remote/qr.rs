//! Terminal QR code rendering for invite tokens.
//!
//! Renders a string as a QR code using Unicode half-block characters (`\u{2580}`,
//! `\u{2584}`, `\u{2588}`, ` `) so the output is compact (each character cell
//! encodes two vertical modules).
//!
//! The resulting string is suitable for printing to stderr.

/// Render a string as a QR code using Unicode half-block characters.
///
/// Returns a multi-line string suitable for printing to stderr.
/// Returns `None` if the data is too long for a QR code.
#[cfg(feature = "remote-internet")]
pub fn render_to_terminal(data: &str) -> Option<String> {
    use qrcode::QrCode;

    let code = QrCode::with_error_correction_level(data.as_bytes(), qrcode::EcLevel::L).ok()?;
    let modules = code.to_colors();
    let width = code.width();

    // Each row of the QR code is `width` modules wide.
    // We process two rows at a time using Unicode half-block characters:
    //   top=dark, bot=dark  → '█' (full block)
    //   top=dark, bot=light → '▀' (upper half)
    //   top=light, bot=dark → '▄' (lower half)
    //   top=light, bot=light → ' ' (space)

    let mut output = String::new();

    // Add a quiet zone (1 char) around the QR code.
    let quiet = 1;

    // Process rows in pairs.
    let total_rows = modules.len() / width;
    let mut row = 0;
    while row < total_rows {
        // Quiet zone left margin.
        for _ in 0..quiet {
            output.push(' ');
        }

        for col in 0..width {
            let top_dark = modules[row * width + col] == qrcode::Color::Dark;
            let bot_dark = if row + 1 < total_rows {
                modules[(row + 1) * width + col] == qrcode::Color::Dark
            } else {
                false
            };

            let ch = match (top_dark, bot_dark) {
                (true, true) => '\u{2588}',  // █
                (true, false) => '\u{2580}', // ▀
                (false, true) => '\u{2584}', // ▄
                (false, false) => ' ',
            };
            output.push(ch);
        }

        output.push('\n');
        row += 2;
    }

    Some(output)
}
