//! Small text helpers shared across modules.

/// Collapse all whitespace (incl. nbsp) into single spaces, then trim.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for ch in s.chars() {
        let c = if ch == '\u{a0}' { ' ' } else { ch };
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// German-aware ASCII slug: "Grüne suchen Gründe!" -> "gruene-suchen-gruende".
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'ä' | 'Ä' => out.push_str("ae"),
            'ö' | 'Ö' => out.push_str("oe"),
            'ü' | 'Ü' => out.push_str("ue"),
            'ß' => out.push_str("ss"),
            'é' | 'è' | 'ê' => out.push('e'),
            'à' | 'â' => out.push('a'),
            c if c.is_ascii_alphanumeric() => out.push(c.to_ascii_lowercase()),
            c if c.is_alphanumeric() => {
                for l in c.to_lowercase() {
                    out.push(l);
                }
            }
            _ => {
                if !out.ends_with('-') {
                    out.push('-');
                }
            }
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed.chars().take(80).collect()
}

/// `2026-10-03T11:41:00+02:00` -> `2026-10-03`
pub fn date_prefix(s: &str) -> String {
    let d: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    if d.len() == 10 { d } else { "undated".to_string() }
}

/// Keep only characters that are safe in a file name.
pub fn safe_file_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if ch.is_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_ascii_and_dashed() {
        assert_eq!(slugify("Grüne suchen Gründe für den Erfolg der AfD – auch bei sich"),
                   "gruene-suchen-gruende-fuer-den-erfolg-der-afd-auch-bei-sich");
        assert_eq!(slugify("  ...  "), "");
    }

    #[test]
    fn clean_collapses_whitespace_and_nbsp() {
        assert_eq!(clean("a\u{a0}\u{a0} b\n\n c "), "a b c");
    }

    #[test]
    fn dates() {
        assert_eq!(date_prefix("2026-10-03T11:41:00+02:00"), "2026-10-03");
        assert_eq!(date_prefix(""), "undated");
    }
}
