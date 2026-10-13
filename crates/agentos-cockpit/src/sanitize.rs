/// Every format (Cf) character, from Unicode 18.0.0 `DerivedGeneralCategory.txt`: bidi marks,
/// embeddings, overrides and isolates, zero-width characters, the BOM and tag characters.
const FORMAT: &[(char, char)] = &[
    ('\u{00AD}', '\u{00AD}'),
    ('\u{0600}', '\u{0605}'),
    ('\u{061C}', '\u{061C}'),
    ('\u{06DD}', '\u{06DD}'),
    ('\u{070F}', '\u{070F}'),
    ('\u{0890}', '\u{0891}'),
    ('\u{08E2}', '\u{08E2}'),
    ('\u{180E}', '\u{180E}'),
    ('\u{200B}', '\u{200F}'),
    ('\u{202A}', '\u{202E}'),
    ('\u{2060}', '\u{2064}'),
    ('\u{2066}', '\u{206F}'),
    ('\u{FEFF}', '\u{FEFF}'),
    ('\u{FFF9}', '\u{FFFB}'),
    ('\u{110BD}', '\u{110BD}'),
    ('\u{110CD}', '\u{110CD}'),
    ('\u{13430}', '\u{1343F}'),
    ('\u{1BCA0}', '\u{1BCA3}'),
    ('\u{1D173}', '\u{1D17A}'),
    ('\u{E0001}', '\u{E0001}'),
    ('\u{E0020}', '\u{E007F}'),
];

fn is_format(c: char) -> bool {
    FORMAT.iter().any(|&(lo, hi)| (lo..=hi).contains(&c))
}

pub fn clean(s: &str, max: usize) -> String {
    let kept: Vec<char> = s
        .chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            // Line and paragraph separators break lines in some terminals.
            '\u{2028}' | '\u{2029}' => None,
            c if c.is_control() || is_format(c) => None,
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

    #[test]
    fn format_characters_and_line_and_paragraph_separators_vanish() {
        assert_eq!(
            clean(
                "a\u{200e}b\u{200f}c\u{61c}d\u{200b}e\u{2028}f\u{2029}g\u{ad}h\u{e0041}i",
                80
            ),
            "abcdefghi"
        );
    }
}
