//! Plain-text tables and the small formatting the listings share.

use serde_json::Value;

/// Rows under a header, each column as wide as its widest cell, two spaces
/// apart; the last column is not padded.
pub fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }
    let line = |cells: &[String]| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                out.push_str(cell);
            } else {
                out.push_str(cell);
                let pad = widths[i].saturating_sub(cell.chars().count()) + 2;
                out.extend(std::iter::repeat_n(' ', pad));
            }
        }
        out.trim_end().to_owned() + "\n"
    };
    let mut out = line(&header.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>());
    for row in rows {
        out.push_str(&line(row));
    }
    out
}

/// A string field, or `-`.
pub fn s(v: &Value, key: &str) -> String {
    match &v[key] {
        Value::String(s) => s.clone(),
        Value::Null => "-".to_owned(),
        other => other.to_string(),
    }
}

/// An RFC 3339 time as `YYYY-MM-DD HH:MM`.
pub fn when(v: &Value, key: &str) -> String {
    let text = s(v, key);
    if text.len() >= 16 && text.as_bytes()[10] == b'T' {
        format!("{} {}", &text[..10], &text[11..16])
    } else {
        text
    }
}

/// The first 12 characters (of a hash or an id).
pub fn short(text: &str) -> String {
    text.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn columns_line_up() {
        let t = table(
            &["A", "LONG"],
            &[vec!["xyz".into(), "1".into()], vec!["".into(), "".into()]],
        );
        assert_eq!(t, "A    LONG\nxyz  1\n\n");
    }

    #[test]
    fn fields_format() {
        let v = json!({"t": "2026-11-07T09:28:09.266849Z", "n": null, "k": 3});
        assert_eq!(when(&v, "t"), "2026-11-07 09:28");
        assert_eq!(s(&v, "n"), "-");
        assert_eq!(s(&v, "k"), "3");
        assert_eq!(s(&v, "missing"), "-");
        assert_eq!(short("0123456789abcdef"), "0123456789ab");
    }
}
