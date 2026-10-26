//! A small path-aware fuzzy matcher used by the file/buffer pickers.
//!
//! Subsequence matching with bonuses for word boundaries, camel humps and
//! consecutive runs — the behaviour users expect from `Ctrl-P` style pickers.

/// A fuzzy match: a score plus the matched character indices (byte offsets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub score: i64,
    pub indices: Vec<usize>,
}

const MATCH: i64 = 16;
const SEQUENTIAL_BONUS: i64 = 8;
const BOUNDARY_BONUS: i64 = 10;
const CAMEL_BONUS: i64 = 8;
const CASE_BONUS: i64 = 2;
const GAP_PENALTY: i64 = 2;
const FIRST_CHAR_PENALTY: i64 = 1;

fn is_boundary(prev: Option<char>) -> bool {
    matches!(
        prev,
        None | Some('/') | Some('\\') | Some('.') | Some('-') | Some('_') | Some(' ') | Some(':')
    )
}

fn is_camel_hump(prev: Option<char>, current: char) -> bool {
    match prev {
        Some(p) => (p.is_lowercase() || p.is_numeric()) && current.is_uppercase(),
        None => false,
    }
}

/// Match `pattern` against `text`, honouring case only when the pattern has
/// uppercase characters (smart case).
///
/// Matching is greedy left to right, which is the right trade-off for a picker:
/// the bonuses for adjacency, path separators and camel humps make the good
/// alignments win, and the cost stays linear in the item's length.
pub fn fuzzy_match(text: &str, pattern: &str, smart_case: bool) -> Option<FuzzyMatch> {
    if pattern.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            indices: Vec::new(),
        });
    }

    let case_sensitive = !smart_case || pattern.chars().any(|c| c.is_uppercase());
    let score_for = |pattern_c: char, text_c: char| -> bool {
        if case_sensitive {
            pattern_c == text_c
        } else {
            pattern_c.to_lowercase().eq(text_c.to_lowercase())
        }
    };

    let mut indices = Vec::with_capacity(pattern.len());
    let mut score: i64 = 0;
    let mut pattern_iter = pattern.chars().peekable();
    let mut prev_text: Option<char> = None;
    let mut last_match_index: Option<usize> = None;
    let mut run: i64 = 0;

    for (index, text_char) in text.chars().enumerate() {
        let Some(&pattern_char) = pattern_iter.peek() else {
            break;
        };
        if !score_for(pattern_char, text_char) {
            prev_text = Some(text_char);
            continue;
        }

        let mut char_score = MATCH;
        if last_match_index == Some(index.wrapping_sub(1)) {
            run += 1;
            char_score += SEQUENTIAL_BONUS + run.min(4);
        } else {
            run = 0;
            if let Some(gap_start) = last_match_index {
                let gap = index - gap_start - 1;
                char_score -= (gap as i64) / GAP_PENALTY;
            } else {
                char_score -= (index as i64) / FIRST_CHAR_PENALTY;
            }
        }
        if is_boundary(prev_text) {
            char_score += BOUNDARY_BONUS;
        } else if is_camel_hump(prev_text, text_char) {
            char_score += CAMEL_BONUS;
        }
        if case_sensitive && pattern_char == text_char {
            char_score += CASE_BONUS;
        }

        score += char_score;
        indices.push(index);
        last_match_index = Some(index);
        pattern_iter.next();
        prev_text = Some(text_char);
    }

    if pattern_iter.next().is_some() {
        return None; // pattern not fully consumed
    }

    // Prefer short haystacks; matches in the file name beat directory noise.
    score -= (text.chars().count() as i64) / 4;
    if let Some(basename) = text.rsplit('/').next()
        && basename != text
    {
        let basename_start = text.len() - basename.len();
        if indices.iter().all(|i| *i >= basename_start) {
            score += BOUNDARY_BONUS;
        }
    }

    Some(FuzzyMatch { score, indices })
}

/// Score every item, keep the best `limit`, sorted best first.
pub fn filter_and_sort<T, K>(
    items: &[T],
    pattern: &str,
    smart_case: bool,
    limit: usize,
    key: K,
) -> Vec<(usize, FuzzyMatch)>
where
    K: Fn(&T) -> &str,
{
    let mut scored: Vec<(usize, FuzzyMatch)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| fuzzy_match(key(item), pattern, smart_case).map(|m| (index, m)))
        .collect();
    scored.sort_by(|a, b| b.1.score.cmp(&a.1.score).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pattern_matches_everything() {
        assert!(fuzzy_match("anything", "", true).is_some());
    }

    #[test]
    fn subsequence_required() {
        assert!(fuzzy_match("src/main.rs", "xz", true).is_none());
    }

    #[test]
    fn prefers_path_boundaries() {
        // Consecutive letters at the start of the file name beat a scattered match.
        let strong = fuzzy_match("src/module.rs", "mod", true).unwrap();
        let weak = fuzzy_match("src/my/odd/d.rs", "mod", true).unwrap();
        assert!(
            strong.score > weak.score,
            "{} should beat {}",
            strong.score,
            weak.score
        );
        // Matching just after `/` beats matching in the middle of a word.
        let boundary = fuzzy_match("src/redraw.rs", "red", true).unwrap();
        let middle = fuzzy_match("src/ignored.rs", "red", true).unwrap();
        assert!(
            boundary.score > middle.score,
            "{} should beat {}",
            boundary.score,
            middle.score
        );
    }

    #[test]
    fn smart_case_respects_uppercase() {
        assert!(fuzzy_match("FooBar", "FB", true).is_some());
        assert!(fuzzy_match("foobar", "FB", true).is_none());
        assert!(fuzzy_match("foobar", "fb", true).is_some());
    }

    #[test]
    fn indices_point_at_matched_chars() {
        let text = "src/editor.rs";
        let m = fuzzy_match(text, "edo", true).unwrap();
        assert_eq!(m.indices.len(), 3);
        let matched: String = m
            .indices
            .iter()
            .map(|i| text[*i..].chars().next().unwrap())
            .collect();
        assert_eq!(matched.to_lowercase(), "edo");
    }
}
