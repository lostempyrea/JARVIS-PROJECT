//! Wake word detection and inline command splitting for hands-free voice activation.

use crate::config::VoiceConfig;

/// Result of matching a transcribed utterance against configured wake phrases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeMatch {
    /// Wake phrase detected alone (with optional trailing punctuation/whitespace).
    WakeOnly,
    /// Wake phrase detected followed immediately by an inline command.
    WakeWithCommand(String),
    /// Utterance did not start with any configured wake phrase.
    NoMatch,
}

/// Configurable Unicode- and punctuation-tolerant wake word matcher.
#[derive(Debug, Clone)]
pub struct WakeWordMatcher {
    /// Wake phrases decomposed into lowercase words, sorted longest-first.
    phrases: Vec<Vec<String>>,
}

impl Default for WakeWordMatcher {
    fn default() -> Self {
        Self::new(["hey jarvis", "jarvis"])
    }
}

impl From<&VoiceConfig> for WakeWordMatcher {
    fn from(cfg: &VoiceConfig) -> Self {
        if cfg.wake_phrases.is_empty() {
            Self::default()
        } else {
            Self::new(cfg.wake_phrases.iter().map(String::as_str))
        }
    }
}

impl WakeWordMatcher {
    /// Construct a matcher from a list of wake phrases (e.g. `["hey jarvis", "jarvis"]`).
    ///
    /// Phrases are normalized to lowercase word sequences and sorted longest-first
    /// so `"hey jarvis"` is always checked before `"jarvis"`.
    pub fn new<I, S>(phrases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parsed: Vec<Vec<String>> = phrases
            .into_iter()
            .map(|p| tokenize_words(p.as_ref()).into_iter().map(|(w, _)| w).collect::<Vec<_>>())
            .filter(|words| !words.is_empty())
            .collect();

        // Longest word count first, then longest total character length.
        parsed.sort_by(|a, b| {
            let len_cmp = b.len().cmp(&a.len());
            if len_cmp != std::cmp::Ordering::Equal {
                return len_cmp;
            }
            let a_chars: usize = a.iter().map(|w| w.chars().count()).sum();
            let b_chars: usize = b.iter().map(|w| w.chars().count()).sum();
            b_chars.cmp(&a_chars)
        });
        parsed.dedup();

        Self { phrases: parsed }
    }

    /// Match a transcript against configured wake phrases and extract any inline command.
    pub fn match_transcript(&self, transcript: &str) -> WakeMatch {
        if self.phrases.is_empty() || transcript.trim().is_empty() {
            return WakeMatch::NoMatch;
        }

        let tokens = tokenize_words(transcript);
        if tokens.is_empty() {
            return WakeMatch::NoMatch;
        }

        for phrase_words in &self.phrases {
            if tokens.len() < phrase_words.len() {
                continue;
            }
            let matches = phrase_words
                .iter()
                .zip(tokens.iter())
                .all(|(pw, (tw, _))| pw == tw);

            if matches {
                let end_byte = tokens[phrase_words.len() - 1].1;
                let remainder = strip_leading_filler(&transcript[end_byte..]).trim();
                if remainder.is_empty() {
                    return WakeMatch::WakeOnly;
                }
                return WakeMatch::WakeWithCommand(remainder.to_string());
            }
        }

        WakeMatch::NoMatch
    }

    /// Returns the normalized wake phrases (each as a slice of lowercase words).
    pub fn phrases(&self) -> &[Vec<String>] {
        &self.phrases
    }
}

/// Tokenize `input` into `(lowercase_word, byte_end_in_input)` pairs where words are
/// contiguous sequences of alphanumeric Unicode characters.
fn tokenize_words(input: &str) -> Vec<(String, usize)> {
    let mut tokens = Vec::new();
    let mut word_start: Option<usize> = None;

    for (idx, ch) in input.char_indices() {
        if ch.is_alphanumeric() {
            if word_start.is_none() {
                word_start = Some(idx);
            }
        } else if let Some(start) = word_start.take() {
            tokens.push((input[start..idx].to_lowercase(), idx));
        }
    }

    if let Some(start) = word_start {
        tokens.push((input[start..].to_lowercase(), input.len()));
    }

    tokens
}

/// Strip leading whitespace and punctuation/filler characters after a wake phrase.
fn strip_leading_filler(s: &str) -> &str {
    s.trim_start_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                ',' | ':' | ';' | '.' | '!' | '?' | '-' | '—' | '–' | '…' | '、' | '。'
            )
    })
}
