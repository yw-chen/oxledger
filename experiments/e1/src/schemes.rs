// SPDX-License-Identifier: GPL-2.0-or-later
//! Tokenization schemes under test. a/b/c/a2 need nothing beyond M0;
//! d (ICU4X) and e (lindera + CC-CEDICT) sit behind cargo features because
//! they pull in dictionary data.

use anyhow::Result;
use oxledger::search::{tokenize, tokenize_query};
use std::sync::Arc;

pub type TokenFn = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

pub struct Scheme {
    pub key: &'static str,
    pub label: &'static str,
    pub index: TokenFn,
    pub query: TokenFn,
}

/// Same ranges as `search.rs` (private there).
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF |
        0xF900..=0xFAFF | 0x20000..=0x2FFFF)
}

fn is_cjk_bigram(t: &str) -> bool {
    t.chars().count() == 2 && t.chars().all(is_cjk)
}

/// b: drop the CJK bigrams from M0's output → CJK unigrams + Latin words.
fn unigram(text: &str) -> Vec<String> {
    tokenize(text).into_iter().filter(|t| !is_cjk_bigram(t)).collect()
}

pub fn all() -> Result<Vec<Scheme>> {
    #[allow(unused_mut)] // mutated only with the icu/lindera features
    let mut v = vec![
        Scheme {
            key: "a",
            label: "M0: index unigram+bigram, query bigram only (single CJK char → unigram)",
            index: Arc::new(tokenize),
            query: Arc::new(tokenize_query),
        },
        Scheme {
            key: "a2",
            label: "index unigram+bigram, query unigram+bigram (no query-side filtering)",
            index: Arc::new(tokenize),
            query: Arc::new(tokenize),
        },
        Scheme {
            key: "b",
            label: "unigram only (index and query)",
            index: Arc::new(unigram),
            query: Arc::new(unigram),
        },
        Scheme {
            key: "c",
            label: "bigram only (index and query; an isolated CJK char stays a unigram)",
            // tokenize_query keeps a unigram only when its CJK run has length 1,
            // which is exactly the pure-bigram rule.
            index: Arc::new(tokenize_query),
            query: Arc::new(tokenize_query),
        },
    ];
    #[cfg(feature = "icu")]
    {
        let f: TokenFn = Arc::new(icu::words);
        v.push(Scheme {
            key: "d",
            label: "ICU4X icu_segmenter word segmentation (dictionary for CJ), Latin lower-cased",
            index: f.clone(),
            query: f,
        });
    }
    #[cfg(feature = "lindera")]
    {
        let f: TokenFn = Arc::new(lindera_cc::words()?);
        v.push(Scheme {
            key: "e",
            label: "lindera + CC-CEDICT (embedded), Latin lower-cased",
            index: f.clone(),
            query: f,
        });
    }
    Ok(v)
}

/// Post-processing shared by the word segmenters: drop whitespace and
/// punctuation-only pieces, lower-case.
#[allow(dead_code)]
fn keep_word(w: &str) -> Option<String> {
    if w.chars().any(|c| c.is_alphanumeric() || is_cjk(c)) {
        Some(w.to_lowercase())
    } else {
        None
    }
}

#[cfg(feature = "icu")]
mod icu {
    use icu_segmenter::WordSegmenter;
    use icu_segmenter::options::WordBreakInvariantOptions;

    pub fn words(text: &str) -> Vec<String> {
        let seg = WordSegmenter::new_auto(WordBreakInvariantOptions::default());
        let breaks: Vec<usize> = seg.segment_str(text).collect();
        breaks
            .windows(2)
            .filter_map(|w| super::keep_word(&text[w[0]..w[1]]))
            .collect()
    }
}

#[cfg(feature = "lindera")]
mod lindera_cc {
    use anyhow::Result;
    use lindera::dictionary::load_dictionary;
    use lindera::mode::Mode;
    use lindera::segmenter::Segmenter;
    use std::borrow::Cow;

    pub fn words() -> Result<impl Fn(&str) -> Vec<String> + Send + Sync + 'static> {
        let dict = load_dictionary("embedded://cc-cedict")?;
        let seg = Segmenter::new(Mode::Normal, dict, None);
        Ok(move |text: &str| {
            seg.segment(Cow::Borrowed(text))
                .map(|ts| ts.iter().filter_map(|t| super::keep_word(t.surface.as_ref())).collect())
                .unwrap_or_default()
        })
    }
}
