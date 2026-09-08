/// Longest prefix within a byte budget, preserving complete UTF-8 characters.
pub(super) fn byte_prefix(text: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_complete_characters_at_every_byte_budget() {
        for text in ["", "ascii", "中文", "🙂🚀", "aé中🙂z", "a\u{301}"] {
            for budget in 0..=text.len() + 1 {
                let expected: String = text
                    .chars()
                    .scan(0, |bytes, ch| {
                        *bytes += ch.len_utf8();
                        (*bytes <= budget).then_some(ch)
                    })
                    .collect();
                assert_eq!(byte_prefix(text, budget), expected);
            }
            assert_eq!(byte_prefix(text, usize::MAX), text);
        }
    }
}
