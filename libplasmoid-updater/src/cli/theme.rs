// SPDX-License-Identifier: GPL-3.0-or-later

//! Every colour and glyph the terminal kit draws.
//!
//! Terminal output carries the same text whether or not styling is on; this
//! module only supplies the escape sequences and the glyphs around it. The
//! palette has two capability tiers, picked once per process from
//! `COLORTERM`: 24-bit RGB on a true-colour terminal, the nearest xterm-256
//! index otherwise. Callers decide whether to paint at all by
//! [`styled`](super::styled), so unstyled output stays plain text.

use std::sync::OnceLock;

/// A named foreground colour from the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Token {
    /// Primary content.
    Text,
    /// Secondary content: labels, versions, reasons.
    TextDim,
    /// Tertiary content: bar tracks and placeholders.
    TextFaint,
    /// The interactive accent: spinners, carets, selections.
    Accent,
    /// A component that finished successfully.
    Success,
    /// Something that needs attention.
    #[expect(
        dead_code,
        reason = "the palette is complete; no surface draws a warning yet"
    )]
    Warning,
    /// A failure.
    Danger,
}

/// The reset sequence that ends every painted run.
pub(crate) const RESET: &str = "\x1b[0m";

const BOLD: &str = "\x1b[1m";

/// The braille frames a spinner cycles through, one per tick.
pub(crate) const SPINNER_FRAMES: [char; 10] = [
    '\u{280b}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283c}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280f}',
];

/// A finished component's mark.
pub(crate) const DONE: char = '\u{2713}';

/// A failed component's mark.
pub(crate) const FAILED: char = '\u{2717}';

/// One filled cell of a progress bar.
pub(crate) const BAR_FULL: char = '\u{2588}';

/// One empty cell of a progress bar.
pub(crate) const BAR_TRACK: char = '\u{2591}';

/// The interactive prompt's caret.
pub(crate) const CARET: &str = "\u{276f}";

/// The arrow between an installed and an available version.
pub(crate) const ARROW: &str = "\u{2192}";

/// How deep the palette can go on this terminal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier {
    TrueColor,
    Indexed,
}

/// One palette entry in the tier this terminal can show.
#[derive(Clone, Copy)]
enum Value {
    Rgb(u8, u8, u8),
    Index(u8),
}

/// The tier, chosen once: `COLORTERM=truecolor` means 24-bit colour.
fn tier() -> Tier {
    static TIER: OnceLock<Tier> = OnceLock::new();
    *TIER.get_or_init(|| {
        if std::env::var("COLORTERM").is_ok_and(|value| value == "truecolor") {
            Tier::TrueColor
        } else {
            Tier::Indexed
        }
    })
}

fn value(token: Token) -> Value {
    let (rgb, index) = match token {
        Token::Text => ((205, 214, 244), 189),
        Token::TextDim => ((166, 173, 200), 145),
        Token::TextFaint => ((127, 132, 156), 102),
        Token::Accent => ((67, 171, 229), 75),
        Token::Success => ((166, 227, 161), 151),
        Token::Warning => ((249, 226, 175), 223),
        Token::Danger => ((243, 139, 168), 211),
    };
    match tier() {
        Tier::TrueColor => {
            let (r, g, b) = rgb;
            Value::Rgb(r, g, b)
        }
        Tier::Indexed => Value::Index(index),
    }
}

/// Wraps `text` in `token`'s foreground colour and a reset.
pub(crate) fn paint(token: Token, text: &str) -> String {
    match value(token) {
        Value::Rgb(r, g, b) => format!("\x1b[38;2;{r};{g};{b}m{text}{RESET}"),
        Value::Index(index) => format!("\x1b[38;5;{index}m{text}{RESET}"),
    }
}

/// [`paint`] with the bold attribute as well.
pub(crate) fn paint_bold(token: Token, text: &str) -> String {
    format!("{BOLD}{}", paint(token, text))
}

/// The palette entry as a table cell colour.
pub(crate) fn table_colour(token: Token) -> comfy_table::Color {
    match value(token) {
        Value::Rgb(r, g, b) => comfy_table::Color::Rgb { r, g, b },
        Value::Index(index) => comfy_table::Color::AnsiValue(index),
    }
}

/// The palette entry as a prompt style colour.
pub(crate) fn prompt_colour(token: Token) -> inquire::ui::Color {
    match value(token) {
        Value::Rgb(r, g, b) => inquire::ui::Color::Rgb { r, g, b },
        Value::Index(index) => inquire::ui::Color::AnsiValue(index),
    }
}
