use std::ops::Range;

/// A single text replacement: replace the byte `range` of the old text with `insert`.
///
/// Offsets are UTF-8 byte offsets and are expected to lie on char boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub insert: String,
}

impl TextEdit {
    /// The minimal single edit turning `old` into `new`: longest common prefix and suffix, both on
    /// char boundaries, never overlapping. Returns `None` if `old == new`.
    ///
    /// The prefix is taken greedily first, so for runs of repeated characters (e.g. `"aaa"` ->
    /// `"aaaa"`) the edit lands at the end of the run.
    pub fn diff(old: &str, new: &str) -> Option<TextEdit> {
        if old == new {
            return None;
        }

        // Comparing whole chars keeps both cut points on char boundaries in both strings.
        let prefix = common_len(old.chars(), new.chars());
        let (old_rest, new_rest) = (&old[prefix..], &new[prefix..]);
        // The suffix only looks at what the prefix left over, so the two can't overlap.
        let suffix = common_len(old_rest.chars().rev(), new_rest.chars().rev());

        Some(TextEdit {
            range: prefix..old.len() - suffix,
            insert: new_rest[..new_rest.len() - suffix].to_owned(),
        })
    }

    /// Maps a byte offset in the old text to the corresponding offset in the new text, e.g. to
    /// keep a caret in place across the edit.
    ///
    /// - `offset <= range.start`: unchanged. In particular, text inserted exactly at the offset
    ///   ends up after it.
    /// - `offset >= range.end` (and past `range.start`): shifted by the length difference, so it
    ///   stays attached to the text following the edit.
    /// - Strictly inside the replaced range: moved to the end of the inserted text.
    pub fn map_offset(&self, offset: usize) -> usize {
        if offset <= self.range.start {
            offset
        } else if offset >= self.range.end {
            (offset - self.range.end).saturating_add(self.range.start + self.insert.len())
        } else {
            self.range.start + self.insert.len()
        }
    }

    /// Applies the edit to `text`.
    ///
    /// # Panics
    ///
    /// Like [`String::replace_range`], panics if the range is out of bounds or not on char
    /// boundaries of `text`.
    pub fn apply(&self, text: &mut String) {
        text.replace_range(self.range.clone(), &self.insert);
    }
}

/// Byte length of the longest common run of chars yielded by both iterators.
fn common_len(a: impl Iterator<Item = char>, b: impl Iterator<Item = char>) -> usize {
    a.zip(b)
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(range: Range<usize>, insert: &str) -> TextEdit {
        TextEdit {
            range,
            insert: insert.to_owned(),
        }
    }

    fn roundtrip(old: &str, new: &str) {
        match TextEdit::diff(old, new) {
            None => assert_eq!(old, new),
            Some(e) => {
                assert!(e.range.start <= e.range.end && e.range.end <= old.len());
                assert!(old.is_char_boundary(e.range.start));
                assert!(old.is_char_boundary(e.range.end));
                // Minimal: nothing at the edges of the edit is shared.
                let removed = &old[e.range.clone()];
                if !removed.is_empty() && !e.insert.is_empty() {
                    assert_ne!(removed.chars().next(), e.insert.chars().next());
                    assert_ne!(removed.chars().last(), e.insert.chars().last());
                }
                let mut text = old.to_owned();
                e.apply(&mut text);
                assert_eq!(text, new, "diff({old:?}, {new:?}) = {e:?}");
            }
        }
    }

    #[test]
    fn identical_strings_have_no_diff() {
        assert_eq!(TextEdit::diff("", ""), None);
        assert_eq!(TextEdit::diff("héllo 👋", "héllo 👋"), None);
    }

    #[test]
    fn insert_delete_replace() {
        assert_eq!(
            TextEdit::diff("hello", "hello world"),
            Some(edit(5..5, " world"))
        );
        assert_eq!(
            TextEdit::diff("world", "hello world"),
            Some(edit(0..0, "hello "))
        );
        assert_eq!(TextEdit::diff("helo", "hello"), Some(edit(3..3, "l")));
        assert_eq!(
            TextEdit::diff("hello world", "hello"),
            Some(edit(5..11, ""))
        );
        assert_eq!(TextEdit::diff("hello world", "world"), Some(edit(0..6, "")));
        assert_eq!(
            TextEdit::diff("the cat sat", "the dog sat"),
            Some(edit(4..7, "dog"))
        );
        assert_eq!(TextEdit::diff("", "abc"), Some(edit(0..0, "abc")));
        assert_eq!(TextEdit::diff("abc", ""), Some(edit(0..3, "")));
    }

    #[test]
    fn repeated_characters() {
        assert_eq!(TextEdit::diff("aaa", "aaaa"), Some(edit(3..3, "a")));
        assert_eq!(TextEdit::diff("aaaa", "aaa"), Some(edit(3..4, "")));
        assert_eq!(TextEdit::diff("abab", "ababab"), Some(edit(4..4, "ab")));
        assert_eq!(TextEdit::diff("xaax", "xaaax"), Some(edit(3..3, "a")));
    }

    #[test]
    fn multibyte_boundaries() {
        // "é" (C3 A9) and "è" (C3 A8) share their first byte; the edit must not split it.
        assert_eq!(TextEdit::diff("é", "è"), Some(edit(0..2, "è")));
        assert_eq!(TextEdit::diff("aéb", "aèb"), Some(edit(1..3, "è")));
        // 😀 (F0 9F 98 80) and 😁 (F0 9F 98 81) share three bytes.
        assert_eq!(TextEdit::diff("x😀", "x😁"), Some(edit(1..5, "😁")));
        assert_eq!(TextEdit::diff("😀y", "😁y"), Some(edit(0..4, "😁")));
        // Shared trailing bytes: "é" (C3 A9) vs "©" (C2 A9).
        assert_eq!(TextEdit::diff("é", "©"), Some(edit(0..2, "©")));
        assert_eq!(TextEdit::diff("héllo", "hello"), Some(edit(1..3, "e")));
        assert_eq!(TextEdit::diff("👋", "👋👋"), Some(edit(4..4, "👋")));
    }

    #[test]
    fn apply_diff_roundtrips() {
        let samples = [
            "",
            "a",
            "aa",
            "aaa",
            "aaaa",
            "abc",
            "abcabc",
            "hello world",
            "hello, wörld",
            "héllo",
            "hèllo",
            "é",
            "è",
            "©",
            "😀",
            "😁",
            "😀😀😁",
            "a😀b",
            "a😁b",
            "  \n\t",
            "日本語テキスト",
            "日本テキスト語",
        ];
        for old in samples {
            for new in samples {
                roundtrip(old, new);
            }
        }
    }

    #[test]
    fn apply_edit() {
        let mut text = "hello world".to_owned();
        edit(6..11, "there 👋").apply(&mut text);
        assert_eq!(text, "hello there 👋");
    }

    #[test]
    fn map_offset() {
        // "hello world" -> "hello big world"
        let insert = edit(6..6, "big ");
        assert_eq!(insert.map_offset(0), 0);
        assert_eq!(insert.map_offset(6), 6);
        assert_eq!(insert.map_offset(7), 11);
        assert_eq!(insert.map_offset(11), 15);

        // "hello cruel world" -> "hello world"
        let delete = edit(6..12, "");
        assert_eq!(delete.map_offset(5), 5);
        assert_eq!(delete.map_offset(6), 6);
        assert_eq!(delete.map_offset(9), 6);
        assert_eq!(delete.map_offset(12), 6);
        assert_eq!(delete.map_offset(17), 11);

        // "the cat sat" -> "the tiger sat"
        let replace = edit(4..7, "tiger");
        assert_eq!(replace.map_offset(4), 4);
        assert_eq!(replace.map_offset(5), 9);
        assert_eq!(replace.map_offset(7), 9);
        assert_eq!(replace.map_offset(11), 13);
    }
}
