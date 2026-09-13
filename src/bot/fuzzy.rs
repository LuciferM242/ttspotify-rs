//! Typo-tolerant matching of typed words against names.

/// Rank `items` by how well their names match `query`, best first. An item is
/// kept only when every query word matches some word of its name.
pub fn rank<'a, T>(query: &str, items: &'a [T], name: impl Fn(&T) -> &str) -> Vec<&'a T> {
    let words = words(query);
    let mut scored: Vec<(u32, usize, &T)> = items
        .iter()
        .filter_map(|item| {
            let n = name(item);
            score(&words, &self::words(n)).map(|s| (s, n.chars().count(), item))
        })
        .collect();
    scored.sort_by_key(|(score, len, _)| (*score, *len));
    scored.into_iter().map(|(_, _, item)| item).collect()
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Lower is better; `None` when some query word matches nothing.
fn score(query: &[String], name: &[String]) -> Option<u32> {
    if query.is_empty() {
        return None;
    }
    query
        .iter()
        .map(|q| name.iter().filter_map(|n| word_cost(q, n)).min())
        .sum()
}

fn word_cost(query: &str, word: &str) -> Option<u32> {
    if query == word {
        return Some(0);
    }
    if word.starts_with(query) {
        return Some(1);
    }
    if query.chars().count() >= 3 && word.contains(query) {
        return Some(2);
    }
    let allowed = match query.chars().count() {
        0..=3 => 0,
        4..=6 => 1,
        _ => 2,
    };
    let distance = edit_distance(query, word, allowed)?;
    Some(2 + distance)
}

/// Levenshtein distance, or `None` once it must exceed `max`.
fn edit_distance(a: &str, b: &str, max: u32) -> Option<u32> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) as u32 > max {
        return None;
    }
    let mut prev: Vec<u32> = (0..=b.len() as u32).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i as u32 + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let substitute = prev[j] + u32::from(ca != cb);
            row[j + 1] = substitute.min(prev[j + 1] + 1).min(row[j] + 1);
        }
        if row.iter().min().copied().unwrap_or(0) > max {
            return None;
        }
        prev = row;
    }
    prev.last().copied().filter(|d| *d <= max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names<'a>(query: &str, items: &'a [&'a str]) -> Vec<&'a str> {
        rank(query, items, |s| s).into_iter().copied().collect()
    }

    #[test]
    fn a_typo_still_finds_the_playlist() {
        let items = ["Fuzzy Feeling", "Road Trip"];
        assert_eq!(names("fealling", &items), vec!["Fuzzy Feeling"]);
        assert_eq!(names("fuzy feeling", &items), vec!["Fuzzy Feeling"]);
    }

    #[test]
    fn every_word_must_match() {
        let items = ["Fuzzy Feeling", "Fuzzy Logic"];
        assert_eq!(names("fuzzy feeling", &items), vec!["Fuzzy Feeling"]);
        assert_eq!(names("fuzzy", &items).len(), 2);
    }

    #[test]
    fn the_start_of_a_word_matches() {
        assert_eq!(names("fuz", &["Fuzzy Feeling"]), vec!["Fuzzy Feeling"]);
    }

    #[test]
    fn an_exact_word_ranks_above_a_typo() {
        let items = ["Chill Mix", "Chil Out"];
        assert_eq!(names("chill", &items), vec!["Chill Mix", "Chil Out"]);
    }

    #[test]
    fn case_and_punctuation_do_not_matter() {
        assert_eq!(names("ROCK n roll", &["Rock'n'Roll!"]), vec!["Rock'n'Roll!"]);
    }

    #[test]
    fn short_words_need_to_be_exact_or_a_prefix() {
        assert!(names("pop", &["Top Hits"]).is_empty());
    }

    #[test]
    fn non_latin_names_match() {
        assert_eq!(names("русский", &["Русский рок"]), vec!["Русский рок"]);
    }

    #[test]
    fn nothing_typed_matches_nothing() {
        assert!(names("  ", &["Anything"]).is_empty());
    }

    #[test]
    fn distance_is_bounded() {
        assert_eq!(edit_distance("feeling", "fealling", 2), Some(2));
        assert_eq!(edit_distance("abcdef", "zzzzzz", 2), None);
    }
}
