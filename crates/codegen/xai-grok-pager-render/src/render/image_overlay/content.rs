use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::prompt_images::PastedImage;
use crate::render::SafeBuf;

pub(super) fn paint_path_line(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    path: &Path,
    path_label: &str,
    text_fg: Color,
    bg: Color,
) {
    let raw = path.display().to_string();
    let prefix_width = UnicodeWidthStr::width(path_label).saturating_add(1);
    let label = format!(
        "{} {}",
        path_label,
        truncate_path_for_overlay(&raw, width.saturating_sub(prefix_width as u16) as usize)
    );
    let clipped = crate::render::line_utils::truncate_str(&label, width as usize);
    buf.set_span_safe(
        x,
        y,
        &Span::styled(clipped, Style::default().fg(text_fg).bg(bg)),
        width,
    );
}

pub(super) fn build_meta_line(image: &PastedImage, display_path: Option<&Path>) -> String {
    let mut parts = Vec::with_capacity(4);
    parts.push(format_mime(&image.mime_type));
    if let Some((width, height)) = image.preview_dimensions() {
        parts.push(format!("{}x{}", width, height));
    }
    parts.push(format_bytes(image.byte_len));
    if let Some(path) = display_path
        && let Some(name) = path.file_name()
    {
        parts.push(name.to_string_lossy().into_owned());
    }
    parts.join(" \u{00b7} ")
}

pub(super) fn format_mime(mime: &str) -> String {
    match mime {
        "image/png" => "PNG".into(),
        "image/jpeg" => "JPEG".into(),
        "image/tiff" => "TIFF".into(),
        "image/gif" => "GIF".into(),
        "image/webp" => "WebP".into(),
        "image/bmp" => "BMP".into(),
        other => other.into(),
    }
}

pub(super) fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub(super) fn truncate_path_for_overlay(path: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(path) <= max_width {
        return path.to_owned();
    }
    if max_width <= 3 {
        return crate::render::line_utils::truncate_str(path, max_width);
    }
    let keep_width = max_width.saturating_sub(3);
    let head_width = keep_width / 2;
    let tail_width = keep_width - head_width;

    let mut head = String::new();
    let mut used = 0usize;
    for ch in path.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > head_width {
            break;
        }
        used += width;
        head.push(ch);
    }

    let mut tail_chars = Vec::new();
    used = 0;
    for ch in path.chars().rev() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > tail_width {
            break;
        }
        used += width;
        tail_chars.push(ch);
    }
    tail_chars.reverse();
    let tail: String = tail_chars.into_iter().collect();
    format!("{head}...{tail}")
}
