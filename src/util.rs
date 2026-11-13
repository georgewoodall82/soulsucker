//! Small formatting, path and filename helpers shared by the app.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use unicode_width::UnicodeWidthChar;

/// Format a byte count in binary units.
pub fn fmt_bytes(n: u64) -> String {
    let mut value = n as f64;
    for unit in ["B", "KiB", "MiB", "GiB"] {
        if value < 1024.0 || unit == "GiB" {
            return if unit == "B" {
                format!("{n} B")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{value:.1} TiB")
}

/// Format a transfer rate. Zero or negative means "unknown".
pub fn fmt_speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec <= 0.5 {
        "--".to_string()
    } else {
        format!("{}/s", fmt_bytes(bytes_per_sec as u64))
    }
}

/// Format a track length as `m:ss` or `h:mm:ss`.
pub fn fmt_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--".to_string();
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Format an elapsed duration as `mm:ss` (or `h:mm:ss` when long).
pub fn fmt_elapsed(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Control or format characters that should never be rendered literally.
fn is_display_control(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

/// Terminal display width of a string.
pub fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Truncate to `width` terminal cells, appending an ellipsis when cut.
pub fn truncate(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    // Terminal-rendered text can come from peers. Never let a control
    // character reach the terminal driver.
    let s: Cow<'_, str> = if s.chars().any(is_display_control) {
        Cow::Owned(
            s.chars()
                .map(|ch| if is_display_control(ch) { ' ' } else { ch })
                .collect(),
        )
    } else {
        Cow::Borrowed(s)
    };
    if display_width(&s) <= width {
        return s.into_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Pad with spaces to `width` terminal cells, truncating if necessary.
pub fn fit(s: &str, width: usize) -> String {
    let text = truncate(s, width);
    let pad = width.saturating_sub(display_width(&text));
    let mut out = text;
    out.push_str(&" ".repeat(pad));
    out
}

/// A smooth Unicode progress bar.
pub fn progress_bar(ratio: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = ratio * width as f64;
    let full = filled.floor() as usize;
    let mut out = "█".repeat(full.min(width));

    let partial = filled - full as f64;
    let mut used = full;
    if used < width {
        let eighth = ((partial * 8.0).round() as usize).min(7);
        if eighth > 0 {
            const PARTIALS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
            out.push_str(PARTIALS[eighth]);
            used += 1;
        }
    }
    out.push_str(&"░".repeat(width.saturating_sub(used)));
    out
}

/// Strip path-hostile and control characters from a filename component.
pub fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_space = false;
    for ch in name.chars() {
        let ch = if ch.is_whitespace() {
            ' '
        } else if is_display_control(ch)
            || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        {
            '_'
        } else {
            ch
        };
        if ch == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
            out.push(' ');
        } else {
            last_space = false;
            out.push(ch);
        }
    }
    let trimmed = out.trim().trim_matches('.').trim();
    let trimmed = if trimmed.is_empty() {
        "untitled"
    } else {
        trimmed
    };
    let mut result = String::new();
    for ch in trimmed.chars() {
        if result.len() + ch.len_utf8() > 180 {
            break;
        }
        result.push(ch);
    }
    let result = result.trim_end_matches('.').trim();
    if result.is_empty() {
        "untitled".to_string()
    } else {
        result.to_string()
    }
}

/// The last component of a remote Soulseek path (handles both separators).
pub fn remote_basename(path: &str) -> &str {
    path.split(['/', '\\'])
        .next_back()
        .filter(|part| !part.is_empty())
        .unwrap_or(path)
}

/// The extension of a filename, lowercased.
pub fn extension_of(filename: &str) -> &str {
    filename.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("")
}

/// The filename without its last extension.
pub fn stem(filename: &str) -> &str {
    filename
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(filename)
}

/// Expand a leading `~` using the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    let path = path.trim();
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from("."));
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

pub fn home_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

pub fn default_music_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.audio_dir().map(Path::to_path_buf))
        .or_else(|| home_dir().map(|home| home.join("Music")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Best-effort open a directory in the platform file manager.
pub fn open_in_file_manager(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = std::process::Command::new("explorer");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");

    let mut child = command
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Percent-encode a value for an application/x-www-form-urlencoded body.
pub fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_and_speeds_are_human_readable() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(2048), "2.0 KiB");
        assert_eq!(fmt_speed(0.0), "--");
        assert_eq!(fmt_speed(1024.0), "1.0 KiB/s");
    }

    #[test]
    fn durations_round_and_include_hours() {
        assert_eq!(fmt_duration(285.4), "4:45");
        assert_eq!(fmt_duration(3725.0), "1:02:05");
        assert_eq!(fmt_duration(-1.0), "--:--");
    }

    #[test]
    fn truncation_respects_width_and_adds_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        assert_eq!(fit("hi", 5), "hi   ");
        assert_eq!(truncate("a\u{1b}[31mred", 20), "a [31mred");
        assert_eq!(truncate("safe\u{202E}txet", 20), "safe txet");
    }

    #[test]
    fn progress_bar_fills_to_ratio() {
        assert_eq!(progress_bar(0.0, 4), "░░░░");
        assert_eq!(progress_bar(1.0, 4), "████");
        assert_eq!(progress_bar(0.5, 4), "██░░");
        assert_eq!(progress_bar(0.99, 4), "███▉");
    }

    #[test]
    fn filenames_are_sanitized() {
        assert_eq!(sanitize_filename("AC/DC: Live?"), "AC_DC_ Live_");
        assert_eq!(sanitize_filename("  ..  "), "untitled");
        assert_eq!(sanitize_filename("a\t\tb"), "a b");
        assert_eq!(sanitize_filename("a*b"), "a_b");
    }

    #[test]
    fn remote_paths_handle_both_separators() {
        assert_eq!(remote_basename("a\\b\\song.flac"), "song.flac");
        assert_eq!(remote_basename("a/b/song.flac"), "song.flac");
        assert_eq!(stem("01 - Song.flac"), "01 - Song");
        assert_eq!(extension_of("Song.FLAC"), "FLAC");
    }
}
