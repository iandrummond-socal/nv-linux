use std::collections::HashSet;

use crate::model::{NoteId, SearchFields};

/// Cached filtering state. Force a rescan when note contents or membership change.
#[derive(Debug, Default)]
pub struct SearchState {
    last_query: Option<String>,
    phrase_mode: bool,
    matched_ids: HashSet<NoteId>,
}

impl SearchState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Filter Unicode-lowercased fields in input order, optionally bypassing the cache.
    pub fn filter(
        &mut self,
        query: &str,
        notes: &[(NoteId, &SearchFields)],
        force: bool,
    ) -> Vec<NoteId> {
        let query = query.to_lowercase();
        let phrase_mode = query.contains('"');
        let narrow = !force
            && phrase_mode == self.phrase_mode
            && self
                .last_query
                .as_ref()
                .is_some_and(|previous| query.starts_with(previous));
        let tokens: Vec<&[u8]> = query
            .split(|ch| {
                if phrase_mode {
                    ch == '"'
                } else {
                    matches!(ch, ' ' | ':' | '\t' | '\r' | '\n')
                }
            })
            .filter(|token| !token.is_empty())
            .map(str::as_bytes)
            .collect();
        let matches: Vec<NoteId> = notes
            .iter()
            .filter(|(id, fields)| {
                tokens.is_empty()
                    || ((!narrow || self.matched_ids.contains(id))
                        && matches_tokens(fields, &tokens))
            })
            .map(|(id, _)| *id)
            .collect();

        self.last_query = Some(query);
        self.phrase_mode = phrase_mode;
        self.matched_ids = matches.iter().copied().collect();
        matches
    }

    /// Complete from the first title prefix in display order and its prefix parents.
    pub fn autocomplete(
        &self,
        query: &str,
        filtered: &[(NoteId, &SearchFields)],
    ) -> Option<NoteId> {
        let query = query.to_lowercase();
        if query.is_empty() {
            return None;
        }
        let (_, candidate) = filtered
            .iter()
            .find(|(_, fields)| fields.title.starts_with(&query))?;
        filtered
            .iter()
            .filter(|(_, fields)| {
                fields.title.starts_with(&query) && candidate.title.starts_with(&fields.title)
            })
            .min_by(|(_, left), (_, right)| {
                left.title
                    .len()
                    .cmp(&right.title.len())
                    .then_with(|| left.title.cmp(&right.title))
            })
            .map(|(id, _)| *id)
    }
}

fn matches_tokens(fields: &SearchFields, tokens: &[&[u8]]) -> bool {
    let fields = [
        fields.title.as_bytes(),
        fields.body.as_bytes(),
        fields.labels.as_bytes(),
    ];
    let mut cursors = [Some(0); 3];
    for token in tokens {
        for (field, cursor) in fields.iter().zip(&mut cursors) {
            // Retain the start, so repeated or overlapping tokens may share bytes.
            *cursor = cursor.and_then(|offset| {
                memchr::memmem::find(&field[offset..], token).map(|position| offset + position)
            });
        }
        if cursors.iter().all(Option::is_none) {
            return false;
        }
    }
    true
}

/// Return the original-case suffix after a Unicode scalar-value (char) offset.
pub fn completion_suffix(original_title: &str, query_chars: usize) -> Option<String> {
    original_title
        .char_indices()
        .nth(query_chars)
        .map(|(offset, _)| original_title[offset..].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn fields(title: &str, body: &str, labels: &str) -> SearchFields {
        SearchFields {
            title: title.to_lowercase(),
            body: body.to_lowercase(),
            labels: labels.to_lowercase(),
        }
    }

    #[test]
    fn nv_search_contract() {
        const CASES: &[((&str, &str, &str), &str, bool)] = &[
            (("a", "", ""), "a", true),
            (("", "a", ""), "a", true),
            (("", "", "a"), "a", true),
            (("", "", ""), "a", false),
            (("a b", "", ""), "a b", true),
            (("", "a ... b", ""), "a b", true),
            (("", "", "a b"), "a b", true),
            (("a", "b", ""), "a b", false),
            (("a", "", "b"), "a b", false),
            (("", "b ... a", ""), "a b", false),
            (("a", "", ""), "a a", true),
            (("ababa", "", ""), "aba bab", true),
            (("a b", "", ""), "a:b", true),
            (("a b c d e", "", ""), "a\tb\rc\nd:e", true),
            (("a b", "", ""), "  a  b  ", true),
            (("a b", "", ""), "\"a b\"", true),
            (("a ... b", "", ""), "\"a b\"", false),
            (("a:b", "", ""), "\"a:b\"", true),
            (("a b", "", ""), "\"a:b\"", false),
            (("a b c", "", ""), "a b\"c", true),
            (("a ... b c", "", ""), "a b\"c", false),
            (("a b", "", ""), "\"a\" \"b\"", true),
            (("ab", "", ""), "\"a\" \"b\"", false),
            (("ÄPFEL", "", ""), "ÄpFeL", true),
            (("İstanbul", "", ""), "İST", true),
            (("café", "", ""), "cafe", false),
            (("", "", ""), "", true),
            (("anything", "", ""), " :\t\r\n", true),
            (("", "", ""), "\"\"", true),
            (("", "", ""), "\" \"", false),
        ];
        for &((title, body, labels), query, expected) in CASES {
            let fields = fields(title, body, labels);
            let actual = SearchState::new().filter(query, &[(7, &fields)], false);
            assert_eq!(
                !actual.is_empty(),
                expected,
                "fields={fields:?}, query={query:?}"
            );
        }
    }

    #[test]
    fn narrowing_preserves_current_order_and_rescans_when_required() {
        let alpha = fields("alpha", "", "");
        let alpine = fields("alpine", "", "");
        let beta = fields("beta", "", "");
        let notes = [(9, &alpha), (2, &beta), (5, &alpine)];
        let mut state = SearchState::new();
        assert_eq!(state.filter("AL", &notes, false), [9, 5]);
        let reordered = [(5, &alpine), (2, &beta), (9, &alpha)];
        assert_eq!(state.filter("alp", &reordered, false), [5, 9]);
        assert_eq!(state.filter("alph", &notes, false), [9]);
        assert_eq!(state.filter("al", &notes, false), [9, 5]);
        assert_eq!(state.filter("beta", &notes, false), [2]);
        assert_eq!(state.filter("", &notes, false), [9, 2, 5]);
        assert_eq!(state.filter("missing", &notes, false), []);
        assert_eq!(state.filter(" : ", &notes, false), [9, 2, 5]);
        assert_eq!(state.filter("al", &notes, true), [9, 5]);
        let changed = [(9, &alpha), (2, &alpine), (5, &alpine)];
        assert_eq!(state.filter("alp", &changed, false), [9, 5]);
        assert_eq!(state.filter("alp", &changed, true), [9, 2, 5]);
    }

    #[test]
    fn quote_switch_rescans() {
        let fields = fields("a b", "", "");
        let mut state = SearchState::new();
        assert_eq!(state.filter("a b", &[(1, &fields)], false), [1]);
        // Adding an ID makes a full scan observable even for a query extension.
        let notes = [(2, &fields), (1, &fields)];
        assert_eq!(state.filter("a b\"", &notes, false), [2, 1]);
        assert_eq!(state.filter("a b", &[(3, &fields)], false), [3]);
    }

    #[test]
    fn autocomplete_contract() {
        let dogs = fields("dogs", "", "");
        let dog_food = fields("dog food", "", "");
        let dog = fields("dog", "", "");
        let dot = fields("dot", "", "");
        let notes = [(1, &dogs), (2, &dog_food), (3, &dog)];
        let state = SearchState::new();
        assert_eq!(state.autocomplete("DO", &notes), Some(3));
        assert_eq!(state.autocomplete("dog ", &notes), Some(2));
        assert_eq!(state.autocomplete("og", &notes), None);
        assert_eq!(state.autocomplete("", &notes), None);
        assert_eq!(state.autocomplete("do", &[]), None);
        assert_eq!(state.autocomplete("do", &[(4, &dot), (3, &dog)]), Some(4));
        assert_eq!(
            state.autocomplete("do", &[(1, &dogs), (2, &dog_food)]),
            Some(1)
        );
        assert_eq!(state.autocomplete("dog", &[(3, &dog), (5, &dog)]), Some(3));
        let unicode = fields("Äpfel", "", "");
        assert_eq!(state.autocomplete("ÄP", &[(6, &unicode)]), Some(6));
    }

    #[test]
    fn completion_suffix_uses_chars_and_original_case() {
        for (title, offset, expected) in [
            ("Dog Food", 3, Some(" Food")),
            ("Ä🍎Tree", 2, Some("Tree")),
            ("İstanbul", 1, Some("stanbul")),
            ("Dog", 0, Some("Dog")),
            ("Dog", 3, None),
            ("Dog", 4, None),
            ("", 0, None),
        ] {
            assert_eq!(completion_suffix(title, offset).as_deref(), expected);
        }
    }

    proptest! {
        #[test]
        fn narrowed_equals_from_scratch(
            raw_notes in prop::collection::vec(
                ("[a-c A-C:\t\r\n\"]{0,24}", "[a-c A-C:\t\r\n\"]{0,24}", "[a-c A-C:\t\r\n\"]{0,12}"),
                0..16,
            ),
            query in "[a-c A-C:\t\r\n\"]{0,8}",
            extension in "[a-c A-C:\t\r\n\"]{0,8}",
        ) {
            let owned: Vec<SearchFields> = raw_notes
                .iter()
                .map(|(title, body, labels)| fields(title, body, labels))
                .collect();
            let notes: Vec<(NoteId, &SearchFields)> = owned
                .iter()
                .enumerate()
                .map(|(index, fields)| (index as NoteId, fields))
                .collect();
            for extended in [format!("{query}{extension}"), format!("{query}\"{extension}")] {
                let mut state = SearchState::new();
                state.filter(&query, &notes, false);
                let narrowed = state.filter(&extended, &notes, false);
                let scratch = SearchState::new().filter(&extended, &notes, true);
                prop_assert_eq!(narrowed, scratch);
            }
        }
    }
}
