/// Describes how the main TUI should split itself across columns and rows.
/// This is used to keep the playlist, right panel, and sidebar readable.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub side_by_side: bool,
    pub show_sidebar: bool,
    pub sidebar_width: u16,
    pub playlist_width: u16,
    pub right_width: u16,
}

/// Chooses the best terminal layout for the current width.
/// Wide terminals show a three-panel layout; narrower ones collapse to a stacked view.
pub fn compute_layout(columns: u16) -> Layout {
    let side_by_side = columns >= 72;
    if !side_by_side {
        let full = columns.saturating_sub(2).max(20);
        return Layout {
            side_by_side,
            show_sidebar: false,
            sidebar_width: 0,
            playlist_width: full,
            right_width: full,
        };
    }

    let show_sidebar = columns >= 100;
    let sidebar_width = if show_sidebar { 22 } else { 0 };
    let reserve = if show_sidebar { sidebar_width + 2 } else { 0 };
    let right_width = (columns.saturating_sub(reserve + 44)).clamp(28, 46);
    let playlist_width = columns
        .saturating_sub(reserve + right_width + 4)
        .max(24);

    Layout {
        side_by_side,
        show_sidebar,
        sidebar_width,
        playlist_width,
        right_width,
    }
}

/// Converts a floating-point elapsed time to a compact `m:ss` display.
pub fn format_time(total_seconds: f64) -> String {
    let safe = if total_seconds.is_finite() && total_seconds > 0.0 {
        total_seconds
    } else {
        0.0
    };
    let m = (safe / 60.0).floor() as u64;
    let s = (safe % 60.0).floor() as u64;
    format!("{m}:{s:02}")
}

/// Builds a small progress bar from 0.0 to 1.0 ratio.
/// The filled portion uses block characters, and the remaining portion is a line.
pub fn render_bar(ratio: f64, width: usize) -> String {
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "─".repeat(width.saturating_sub(filled)))
}
