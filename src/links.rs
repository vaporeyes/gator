// ABOUTME: URL detection in row text and OS-default browser launch. Used
// ABOUTME: by modifier-click handlers to open hyperlinks (OSC 8 or plain).

/// Bytes that delimit URLs in plain text. Common surrounding punctuation is
/// stripped from the trailing end so `(https://foo)` resolves cleanly.
const URL_DELIMS: &[char] = &[
    ' ', '\t', '\n', '\r', '"', '\'', '<', '>', '`', '[', ']', '{', '}',
];

/// Return a URL containing column `col` in `row_text`, if any. Recognizes
/// http(s) and ftp explicit schemes. Trailing punctuation is trimmed.
pub fn detect_url_at(row_text: &str, col: usize) -> Option<String> {
    // Convert col to a byte index by walking chars.
    let mut byte_col = 0usize;
    for (i, _) in row_text.char_indices().take(col) {
        byte_col = i + row_text[i..].chars().next()?.len_utf8();
    }
    // Find every scheme occurrence; pick one that contains byte_col.
    for scheme in ["https://", "http://", "ftp://"] {
        let mut start = 0;
        while let Some(s) = row_text[start..].find(scheme) {
            let s_abs = start + s;
            // Find end: first delim, or end of string.
            let after = &row_text[s_abs..];
            let end_rel = after.find(URL_DELIMS).unwrap_or(after.len());
            let mut e_abs = s_abs + end_rel;
            // Trim trailing punctuation that commonly hugs URLs.
            let trailing = [".", ",", ")", "]", ";", ":", "!", "?"];
            while e_abs > s_abs + scheme.len() {
                let last = &row_text[e_abs - 1..e_abs];
                if trailing.contains(&last) {
                    e_abs -= 1;
                } else {
                    break;
                }
            }
            if byte_col >= s_abs && byte_col < e_abs {
                return Some(row_text[s_abs..e_abs].to_string());
            }
            start = e_abs.max(s_abs + scheme.len());
        }
    }
    None
}

/// Open a URL in the OS's default handler. Best-effort; failures are silent.
pub fn open(url: &str) {
    use std::process::Command;
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_plain_https_url() {
        let row = "see https://example.com/docs for details";
        let url = detect_url_at(row, 10).unwrap();
        assert_eq!(url, "https://example.com/docs");
    }

    #[test]
    fn trims_trailing_punctuation() {
        let row = "(see https://example.com.)";
        // Click somewhere inside the URL.
        let url = detect_url_at(row, 8).unwrap();
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn returns_none_outside_url() {
        let row = "no urls here";
        assert!(detect_url_at(row, 3).is_none());
    }
}
