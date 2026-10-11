//! Text analysis: Unicode-aware tokenizer, stop words, Porter-style stemmer,
//! edge n-grams. Zero dependencies.

use std::collections::{BTreeMap, HashMap};

/// A small English stop-word list.
pub const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is",
    "it", "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there",
    "these", "they", "this", "to", "was", "will", "with",
];

/// Per-field analyzer configuration.
#[derive(Debug, Clone)]
pub struct Analyzer {
    pub lowercase: bool,
    pub stopwords: bool,
    pub stem: bool,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self { lowercase: true, stopwords: true, stem: true }
    }
}

impl Analyzer {
    pub fn raw() -> Self {
        Self { lowercase: true, stopwords: false, stem: false }
    }
}

/// Split text into word tokens. A token is a maximal run of alphanumeric
/// Unicode characters (letters or numbers). Everything else is a separator.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                cur.push(lc);
            }
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Analyze text into indexed terms (positions collapsed).
pub fn analyze(text: &str, cfg: &Analyzer) -> Vec<String> {
    analyze_with_positions(text, cfg).into_iter().map(|(t, _)| t).collect()
}

/// Analyze text into (term, position) pairs. Positions count every emitted
/// token slot (stop words removed leave gaps), so phrase queries still work.
pub fn analyze_with_positions(text: &str, cfg: &Analyzer) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut pos: u32 = 0;
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<(String, u32)>, pos: &mut u32| {
        if cur.is_empty() {
            return;
        }
        let mut tok: String =
            cur.chars().flat_map(|c| c.to_lowercase()).collect();
        cur.clear();
        if !cfg.lowercase {
            tok = tok.clone();
        }
        if cfg.stopwords && STOPWORDS.contains(&tok.as_str()) {
            *pos += 1;
            return;
        }
        if cfg.stem {
            tok = stem(&tok);
        }
        if !tok.is_empty() {
            out.push((tok, *pos));
        }
        *pos += 1;
    };
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                cur.push(lc);
            }
        } else {
            flush(&mut cur, &mut out, &mut pos);
        }
    }
    flush(&mut cur, &mut out, &mut pos);
    out
}

/// Edge n-grams for autocomplete, e.g. "rust" -> ["r","ru","rus","rust"].
pub fn edge_ngrams(token: &str, min: usize, max: usize) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    let mut out = Vec::new();
    let top = max.min(chars.len());
    for len in min..=top {
        out.push(chars[..len].iter().collect());
    }
    out
}

// ---- Porter-style stemmer (compact, hand-written) ----

fn is_consonant(w: &[char], i: usize) -> bool {
    match w[i] {
        'a' | 'e' | 'i' | 'o' | 'u' => false,
        'y' => i == 0 || !is_consonant(w, i - 1),
        _ => true,
    }
}

fn measure(w: &[char]) -> usize {
    let mut m = 0;
    let mut i = 0;
    let n = w.len();
    while i < n && is_consonant(w, i) {
        i += 1;
    }
    while i < n {
        while i < n && !is_consonant(w, i) {
            i += 1;
        }
        if i >= n {
            break;
        }
        m += 1;
        while i < n && is_consonant(w, i) {
            i += 1;
        }
    }
    m
}

fn has_vowel(w: &[char]) -> bool {
    w.iter().enumerate().any(|(i, _)| !is_consonant(w, i))
}

fn ends_double_consonant(w: &[char]) -> bool {
    w.len() >= 2 && w[w.len() - 1] == w[w.len() - 2] && is_consonant(w, w.len() - 1)
}

fn ends_cvc(w: &[char]) -> bool {
    if w.len() < 3 {
        return false;
    }
    let n = w.len();
    if !is_consonant(w, n - 3) || is_consonant(w, n - 2) || !is_consonant(w, n - 1) {
        return false;
    }
    matches!(w[n - 1], 'w' | 'x' | 'y') == false
}

fn replace_suffix(s: &mut Vec<char>, from: &str, to: &str) -> bool {
    let f: Vec<char> = from.chars().collect();
    if s.len() >= f.len() && s[s.len() - f.len()..] == f[..] {
        s.truncate(s.len() - f.len());
        s.extend(to.chars());
        true
    } else {
        false
    }
}

/// Stem a single lowercase word. Implements the classic Porter steps 1-5 in
/// compact form; output matches the reference Porter stemmer on common words.
pub fn stem(word: &str) -> String {
    if word.len() <= 2 {
        return word.to_string();
    }
    let mut w: Vec<char> = word.chars().collect();

    // Step 1a
    if replace_suffix(&mut w, "sses", "ss") {
    } else if replace_suffix(&mut w, "ies", "i") {
    } else if replace_suffix(&mut w, "ss", "ss") {
    } else if replace_suffix(&mut w, "s", "") {
    }
    // Step 1b
    let mut flag = false;
    if replace_suffix(&mut w, "eed", "ee") {
        if measure(&w) == 0 {
            replace_suffix(&mut w, "ee", "eed");
        }
    } else if (w.windows(2).any(|p| p == ['e', 'd']) && w.ends_with(&['e', 'd']))
        || (w.windows(3).any(|p| p == ['i', 'n', 'g']) && ends_with(&w, "ing"))
    {
        let suf = if w.ends_with(&['e', 'd']) { "ed" } else { "ing" };
        let stem_part: Vec<char> = w[..w.len() - suf.len()].to_vec();
        if has_vowel(&stem_part) {
            w = stem_part;
            flag = true;
            if replace_suffix(&mut w, "at", "ate")
                || replace_suffix(&mut w, "bl", "ble")
                || replace_suffix(&mut w, "iz", "ize")
            {
            } else if ends_double_consonant(&w)
                && !matches!(w.last(), Some('l') | Some('s') | Some('z'))
            {
                w.pop();
            } else if measure(&w) == 1 && ends_cvc(&w) {
                w.push('e');
            }
        }
    }
    if flag || has_vowel(&w) {
        // Step 1c
        if w.ends_with(&['y']) && has_vowel(&w[..w.len() - 1].to_vec()) {
            w.pop();
            w.push('i');
        }
    }
    // Step 2
    let step2: &[(&str, &str)] = &[
        ("ational", "ate"),
        ("tional", "tion"),
        ("enci", "ence"),
        ("anci", "ance"),
        ("izer", "ize"),
        ("abli", "able"),
        ("alli", "al"),
        ("entli", "ent"),
        ("eli", "e"),
        ("ousli", "ous"),
        ("ization", "ize"),
        ("ation", "ate"),
        ("ator", "ate"),
        ("alism", "al"),
        ("iveness", "ive"),
        ("fulness", "ful"),
        ("ousness", "ous"),
        ("aliti", "al"),
        ("iviti", "ive"),
        ("biliti", "ble"),
    ];
    for (a, b) in step2 {
        let before: Vec<char> = w.clone();
        if replace_suffix(&mut w, a, b) {
            if measure(&w) == 0 {
                w = before;
            }
            break;
        }
    }
    // Step 3
    let step3: &[(&str, &str)] = &[
        ("icate", "ic"),
        ("ative", ""),
        ("alize", "al"),
        ("iciti", "ic"),
        ("ical", "ic"),
        ("ful", ""),
        ("ness", ""),
    ];
    for (a, b) in step3 {
        let before = w.clone();
        if replace_suffix(&mut w, a, b) {
            if measure(&w) == 0 {
                w = before;
            }
            break;
        }
    }
    // Step 4
    let step4 = [
        "al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent", "ion",
        "ou", "ism", "ate", "iti", "ous", "ive", "ize",
    ];
    for suf in step4 {
        if ends_with(&w, suf) {
            let base: Vec<char> = w[..w.len() - suf.len()].to_vec();
            if measure(&base) > 1 {
                if suf == "ion" {
                    if let Some(&c) = base.last() {
                        if c == 's' || c == 't' {
                            w = base;
                        }
                    }
                } else {
                    w = base;
                }
            }
            break;
        }
    }
    // Step 5a/b
    if w.ends_with(&['e']) {
        let base: Vec<char> = w[..w.len() - 1].to_vec();
        let m = measure(&base);
        if m > 1 || (m == 1 && !ends_cvc(&base)) {
            w = base;
        }
    }
    if ends_double_consonant(&w) && w.ends_with(&['l']) && measure(&w) > 1 {
        w.pop();
    }
    w.iter().collect()
}

fn ends_with(w: &[char], s: &str) -> bool {
    let f: Vec<char> = s.chars().collect();
    w.len() >= f.len() && w[w.len() - f.len()..] == f[..]
}

/// Field configuration registry: analyzer per field plus index-time boost.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    pub analyzers: HashMap<String, Analyzer>,
    pub boosts: HashMap<String, f32>,
    pub default_analyzer: Analyzer,
}

impl Schema {
    pub fn analyzer_for(&self, field: &str) -> &Analyzer {
        self.analyzers.get(field).unwrap_or(&self.default_analyzer)
    }
    pub fn boost_for(&self, field: &str) -> f32 {
        self.boosts.get(field).copied().unwrap_or(1.0)
    }
    /// Term frequencies of one field value: term -> (freq, positions).
    pub fn field_terms(&self, field: &str, text: &str) -> BTreeMap<String, (u32, Vec<u32>)> {
        let cfg = self.analyzer_for(field);
        let mut map: BTreeMap<String, (u32, Vec<u32>)> = BTreeMap::new();
        for (tok, pos) in analyze_with_positions(text, cfg) {
            let e = map.entry(tok).or_insert((0, Vec::new()));
            e.0 += 1;
            e.1.push(pos);
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_splits_unicode() {
        assert_eq!(tokenize("Hello,  world! 123"), vec!["hello", "world", "123"]);
        assert_eq!(tokenize("crème brûlée"), vec!["crème", "brûlée"]);
        assert_eq!(tokenize(""), Vec::<String>::new());
        assert_eq!(tokenize("a-b_c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn stopwords_removed_positions_gap() {
        let v = analyze_with_positions("the cat sat", &Analyzer::default());
        // "the" removed but position 0 consumed
        assert!(v.iter().all(|(t, _)| t != "the"));
        assert_eq!(v[0].1, 1);
    }

    #[test]
    fn stemmer_spot_checks() {
        // Classic Porter reference outputs.
        let cases = [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("running", "run"),
            ("relational", "relat"),
            ("conditional", "condit"),
            ("studies", "studi"),
            ("agreed", "agre"),
            ("cats", "cat"),
            ("foxes", "fox"),
        ];
        for (inp, want) in cases {
            assert_eq!(stem(inp), want, "stem({inp})");
        }
    }

    #[test]
    fn ngrams_basic() {
        assert_eq!(edge_ngrams("rust", 1, 4), vec!["r", "ru", "rus", "rust"]);
        assert_eq!(edge_ngrams("ab", 1, 5), vec!["a", "ab"]);
    }
}
