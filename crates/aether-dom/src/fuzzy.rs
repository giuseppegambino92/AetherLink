//! Case- and diacritics-insensitive fuzzy subsequence matcher (fzf-v2 style
//! Smith-Waterman scoring with affine gaps, word-boundary, camelCase and
//! consecutive-run bonuses). Indices returned are **char indices into the
//! original string**, so the UI can highlight them directly.

use unicode_normalization::char::{decompose_canonical, is_combining_mark};

const SCORE_MATCH: i32 = 16;
const GAP_START: i32 = 3;
const GAP_EXT: i32 = 1;
const BONUS_BOUNDARY_WHITE: i32 = 10;
const BONUS_BOUNDARY_DELIM: i32 = 9;
const BONUS_BOUNDARY: i32 = 8;
const BONUS_CAMEL: i32 = 7;
const BONUS_CONSECUTIVE: i32 = GAP_START + GAP_EXT;
const FIRST_CHAR_MULT: i32 = 2;
/// Longer haystacks are truncated (labels beyond this are not useful to match).
pub const MAX_HAYSTACK: usize = 512;
const NEG: i32 = i32::MIN / 4;

/// Fold one char to its search form: lowercase, without diacritics.
fn fold_char(c: char, out: &mut Vec<char>) {
    match c {
        'ß' | 'ẞ' => out.extend(['s', 's']),
        'æ' | 'Æ' => out.extend(['a', 'e']),
        'œ' | 'Œ' => out.extend(['o', 'e']),
        'ø' | 'Ø' => out.push('o'),
        'đ' | 'Đ' => out.push('d'),
        'ł' | 'Ł' => out.push('l'),
        'ı' => out.push('i'),
        '’' | '‘' | '`' => out.push('\''),
        '“' | '”' => out.push('"'),
        '\u{2026}' => out.extend(['.', '.', '.']),
        _ if c.is_ascii() => out.push(c.to_ascii_lowercase()),
        _ => decompose_canonical(c, |d| {
            if !is_combining_mark(d) {
                out.extend(d.to_lowercase());
            }
        }),
    }
}

/// Search form of a string (lowercase, no diacritics).
pub fn fold(s: &str) -> String {
    let mut v = Vec::with_capacity(s.len());
    for c in s.chars() {
        fold_char(c, &mut v);
    }
    v.into_iter().collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    White,
    Delim,
    Lower,
    Upper,
    Digit,
    Other,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::White
    } else if c.is_lowercase() {
        Class::Lower
    } else if c.is_uppercase() {
        Class::Upper
    } else if c.is_numeric() {
        Class::Digit
    } else if c.is_alphabetic() {
        Class::Other
    } else {
        Class::Delim
    }
}

fn bonus_for(prev: Option<Class>, cur: Class) -> i32 {
    match (prev, cur) {
        (_, Class::White | Class::Delim) => 0,
        (None, _) | (Some(Class::White), _) => BONUS_BOUNDARY_WHITE,
        (Some(Class::Delim), _) => BONUS_BOUNDARY_DELIM,
        (Some(Class::Lower), Class::Upper) => BONUS_CAMEL,
        (Some(Class::Digit), Class::Lower | Class::Upper | Class::Other) => BONUS_BOUNDARY,
        (Some(Class::Lower | Class::Upper | Class::Other), Class::Digit) => BONUS_BOUNDARY,
        _ => 0,
    }
}

/// A haystack prepared for matching.
#[derive(Debug, Clone)]
pub struct Prepared {
    chars: Vec<char>,
    orig: Vec<usize>,
    bonus: Vec<i32>,
}

impl Prepared {
    pub fn new(s: &str) -> Self {
        let mut chars = Vec::with_capacity(s.len().min(MAX_HAYSTACK));
        let mut orig = Vec::with_capacity(chars.capacity());
        let mut bonus = Vec::with_capacity(chars.capacity());
        let mut prev: Option<Class> = None;
        let mut buf = Vec::with_capacity(4);
        for (i, c) in s.chars().enumerate() {
            if chars.len() >= MAX_HAYSTACK {
                break;
            }
            let cl = class(c);
            let b = bonus_for(prev, cl);
            prev = Some(cl);
            buf.clear();
            fold_char(c, &mut buf);
            for (k, &f) in buf.iter().enumerate() {
                chars.push(f);
                orig.push(i);
                bonus.push(if k == 0 { b } else { 0 });
            }
        }
        Self { chars, orig, bonus }
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn folded(&self) -> &[char] {
        &self.chars
    }
}

/// Result of a successful match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub score: i32,
    /// Sorted, deduplicated char indices into the original string.
    pub indices: Vec<usize>,
}

/// Reusable matcher (keeps its DP buffers between calls).
#[derive(Debug, Default)]
pub struct Matcher {
    h: Vec<i32>,
    cb: Vec<i32>,
    back: Vec<u32>,
}

fn is_subsequence(q: &[char], h: &[char]) -> bool {
    let mut it = h.iter();
    q.iter().all(|c| it.any(|x| x == c))
}

impl Matcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Match folded query chars `q` against `hay`.
    pub fn score(&mut self, q: &[char], hay: &Prepared) -> Option<FuzzyMatch> {
        let (m, n) = (q.len(), hay.chars.len());
        if m == 0 {
            return Some(FuzzyMatch { score: 0, indices: Vec::new() });
        }
        if m > n || !is_subsequence(q, &hay.chars) {
            return None;
        }
        let cells = m * n;
        self.h.clear();
        self.h.resize(cells, NEG);
        self.cb.clear();
        self.cb.resize(cells, 0);
        self.back.clear();
        self.back.resize(cells, u32::MAX);
        let hc = &hay.chars;
        for (i, &qc) in q.iter().enumerate() {
            let row = i * n;
            let prow = row.wrapping_sub(n);
            let mut gap_best = NEG;
            let mut gap_k = u32::MAX;
            for j in 0..n {
                if i > 0 && j >= 2 {
                    let k = j - 2;
                    gap_best = if gap_best > NEG { gap_best - GAP_EXT } else { NEG };
                    let cand = self.h[prow + k];
                    if cand > NEG && cand - GAP_START >= gap_best {
                        gap_best = cand - GAP_START;
                        gap_k = k as u32;
                    }
                }
                if hc[j] != qc {
                    continue;
                }
                let b = hay.bonus[j];
                if i == 0 {
                    self.h[row + j] = SCORE_MATCH + b * FIRST_CHAR_MULT;
                    self.cb[row + j] = b;
                    continue;
                }
                let mut best = NEG;
                let mut best_cb = 0;
                let mut best_back = u32::MAX;
                if j >= 1 && self.h[prow + j - 1] > NEG {
                    let pcb = self.cb[prow + j - 1];
                    let cons = b.max(pcb).max(BONUS_CONSECUTIVE);
                    let s = self.h[prow + j - 1] + SCORE_MATCH + cons;
                    if s > best {
                        best = s;
                        best_cb = if b >= BONUS_BOUNDARY { b.max(pcb) } else { pcb };
                        best_back = (j - 1) as u32;
                    }
                }
                if gap_best > NEG {
                    let s = gap_best + SCORE_MATCH + b;
                    if s > best {
                        best = s;
                        best_cb = b;
                        best_back = gap_k;
                    }
                }
                if best > NEG {
                    self.h[row + j] = best;
                    self.cb[row + j] = best_cb;
                    self.back[row + j] = best_back;
                }
            }
        }
        let last = (m - 1) * n;
        let (mut j, score) = (0..n)
            .filter(|&j| self.h[last + j] > NEG)
            .map(|j| (j, self.h[last + j]))
            .max_by_key(|&(j, s)| (s, std::cmp::Reverse(j)))?;
        let mut idx = Vec::with_capacity(m);
        for i in (0..m).rev() {
            idx.push(hay.orig[j]);
            if i > 0 {
                j = self.back[i * n + j] as usize;
            }
        }
        idx.reverse();
        idx.dedup();
        // `j` is now the folded position of the first matched char: prefer
        // matches that start earlier (ties otherwise favour "Make a Copy" as
        // much as "Copy Link").
        let score = score - (j.min(20) as i32) / 2;
        Some(FuzzyMatch { score, indices: idx })
    }
}

/// One-shot convenience wrapper.
pub fn fuzzy_match(query: &str, hay: &str) -> Option<FuzzyMatch> {
    let q: Vec<char> = fold(query).chars().filter(|c| !c.is_whitespace()).collect();
    Matcher::new().score(&q, &Prepared::new(hay))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding() {
        assert_eq!(fold("Perché È Così"), "perche e cosi");
        assert_eq!(fold("Straße Œuvre"), "strasse oeuvre");
        assert_eq!(fold("Salva…"), "salva...");
        assert_eq!(fold("ÀÉÎÕÜ ñ ç"), "aeiou n c");
    }

    #[test]
    fn basic_and_accents() {
        let m = fuzzy_match("salva", "Salva…").unwrap();
        assert_eq!(m.indices, vec![0, 1, 2, 3, 4]);
        let m = fuzzy_match("perche", "Perché no?").unwrap();
        assert_eq!(m.indices, vec![0, 1, 2, 3, 4, 5]);
        let m = fuzzy_match("città", "Seleziona citta").unwrap();
        assert_eq!(m.indices, vec![10, 11, 12, 13, 14]);
        assert!(fuzzy_match("xyz", "Salva").is_none());
        assert!(fuzzy_match("salvaa", "Salva").is_none());
    }

    #[test]
    fn indices_map_to_original_chars() {
        // "ß" folds to two chars; matching "ss" must highlight the single original char.
        let m = fuzzy_match("strasse", "Straße").unwrap();
        assert_eq!(m.indices, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn prefers_word_boundaries_and_consecutive() {
        let a = fuzzy_match("sa", "Save As").unwrap();
        let b = fuzzy_match("sa", "Disable").unwrap();
        assert!(a.score > b.score, "{a:?} vs {b:?}");
        // Boundary alignment: "sa" in "Save As" should pick S(0) + a(1) (consecutive)
        assert_eq!(a.indices, vec![0, 1]);
        let c = fuzzy_match("ns", "New Session").unwrap();
        assert_eq!(c.indices, vec![0, 4]);
        let d = fuzzy_match("ns", "Transparency").unwrap();
        assert!(c.score > d.score);
        // camelCase
        let e = fuzzy_match("gt", "goToLine").unwrap();
        assert_eq!(e.indices, vec![0, 2]);
    }

    #[test]
    fn prefix_beats_middle() {
        let p = fuzzy_match("copy", "Copy Link").unwrap();
        let q = fuzzy_match("copy", "Make a Copy").unwrap();
        let r = fuzzy_match("copy", "cheap old pygmy").unwrap();
        assert!(p.score > q.score && q.score > r.score, "{} {} {}", p.score, q.score, r.score);
    }

    #[test]
    fn long_haystack_is_truncated_not_panicking() {
        let s = "x".repeat(5000) + "needle";
        assert!(fuzzy_match("needle", &s).is_none());
        let s2 = "needle".to_string() + &"y".repeat(5000);
        assert!(fuzzy_match("needle", &s2).is_some());
    }
}
