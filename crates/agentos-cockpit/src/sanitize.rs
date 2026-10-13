pub fn clean(s: &str, max: usize) -> String {
    let kept: Vec<char> = s
        .chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}' => None,
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    if kept.len() <= max {
        return kept.into_iter().collect();
    }
    if max == 0 {
        return String::new();
    }
    kept.into_iter()
        .take(max - 1)
        .chain(std::iter::once('…'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn escape_sequences_lose_their_escape() {
        let out = clean("a\x1b[31mred\x1b[0m\x07b", 80);
        assert!(!out.contains('\x1b') && !out.contains('\x07'));
        assert_eq!(out, "a[31mred[0mb");
    }

    #[test]
    fn newlines_and_tabs_become_spaces_and_c1_controls_vanish() {
        assert_eq!(clean("a\nb\tc\r\nd\u{9b}e\u{85}f", 80), "a b c  def");
    }

    #[test]
    fn bidi_overrides_and_bom_vanish() {
        assert_eq!(clean("a\u{202e}b\u{2066}c\u{feff}d", 80), "abcd");
    }

    #[test]
    fn long_text_is_cut_on_a_char_boundary_with_an_ellipsis() {
        assert_eq!(clean("héllo wörld", 5), "héll…");
        assert_eq!(clean("日本語日本語", 4), "日本語…");
        assert_eq!(clean("abc", 3), "abc");
        assert_eq!(clean("abc", 0), "");
        assert_eq!(clean("abc", 1), "…");
    }
}
