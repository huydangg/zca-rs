//! Outgoing @mentions.

/// Most mentions one message carries.
pub const MENTIONS_MAX: usize = 50;

/// Who a [`Mention`] tags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MentionTarget {
    /// A member, by uid.
    User(String),
    /// Everybody in the group (@All).
    All,
}

/// One tag in an outgoing group message: the `len` chars of the text starting at char
/// `start` (Unicode scalar values, not bytes) name `target`. The crate converts to the UTF-16
/// offsets Zalo wants. Mentions outside the text, empty, overlapping or beyond
/// [`MENTIONS_MAX`] are left out; DMs carry none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mention {
    /// First char of the tag in the text.
    pub start: usize,
    /// Length of the tag in chars.
    pub len: usize,
    /// Who it tags.
    pub target: MentionTarget,
}

impl Mention {
    /// A tag of member `uid`.
    pub fn user(start: usize, len: usize, uid: impl Into<String>) -> Self {
        Mention { start, len, target: MentionTarget::User(uid.into()) }
    }

    /// An @All tag.
    pub fn all(start: usize, len: usize) -> Self {
        Mention { start, len, target: MentionTarget::All }
    }
}

/// The mentions that are safe to encode for `text`: inside the text, not empty, with an id,
/// not overlapping, in order, at most [`MENTIONS_MAX`].
fn clean<'a>(text: &str, mentions: &'a [Mention]) -> Vec<&'a Mention> {
    let n = text.chars().count();
    let mut sorted: Vec<&Mention> = mentions
        .iter()
        .filter(|m| {
            m.len > 0
                && !matches!(&m.target, MentionTarget::User(u) if u.is_empty())
                && m.start.checked_add(m.len).is_some_and(|e| e <= n)
        })
        .collect();
    sorted.sort_by_key(|m| m.start);
    let mut out: Vec<&Mention> = Vec::new();
    for m in sorted {
        if out.last().is_none_or(|p| p.start + p.len <= m.start) && out.len() < MENTIONS_MAX {
            out.push(m);
        }
    }
    out
}

/// [`clean`]ed mentions as `(UTF-16 offset, UTF-16 length, mention)`.
pub(crate) fn utf16_spans<'a>(text: &str, mentions: &'a [Mention]) -> Vec<(usize, usize, &'a Mention)> {
    // units[i] = UTF-16 offset of char i
    let mut units = Vec::with_capacity(text.len() + 1);
    let mut at = 0;
    for c in text.chars() {
        units.push(at);
        at += c.len_utf16();
    }
    units.push(at);
    clean(text, mentions).into_iter().map(|m| (units[m.start], units[m.start + m.len] - units[m.start], m)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_utf16_and_bad_mentions_are_dropped() {
        let text = "@Zoë An xem 😀 @All";
        let ms = [Mention::user(0, 7, "1"), Mention::all(14, 4)];
        let got: Vec<_> = utf16_spans(text, &ms).into_iter().map(|(p, l, _)| (p, l)).collect();
        assert_eq!(got, [(0, 7), (15, 4)], "the emoji counts twice");
        // out of range, empty, no id, overlapping
        let bad = [Mention::user(0, 99, "1"), Mention::user(0, 0, "1"), Mention::user(0, 2, ""), Mention::user(0, 4, "1"), Mention::user(2, 4, "2")];
        let got = utf16_spans(text, &bad);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].2, &Mention::user(0, 4, "1"));
    }
}
