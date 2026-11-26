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
                let remainder = strip_trailing_vocative(&transcript[end_byte..]);
                if remainder.is_empty() {
                    return WakeMatch::WakeOnly;
                }
                return WakeMatch::WakeWithCommand(remainder.to_string());
            }
        }

        // Fuzzy wake variants for common Whisper STT transcriptions of "Hey Jarvis" / "Jarvis"
        let has_jarvis_phrase = self
            .phrases
            .iter()
            .any(|pw| pw.iter().any(|w| w == "jarvis" || w == "javis"));
        if has_jarvis_phrase {
            if let Some(matched_tokens) = match_fuzzy_jarvis_prefix(&tokens) {
                let end_byte = tokens[matched_tokens - 1].1;
                let remainder = strip_trailing_vocative(&transcript[end_byte..]);
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

fn is_jarvis_variant(word: &str) -> bool {
    matches!(word, "jarvis" | "javis" | "jarviz" | "javiz")
}

fn is_greeting_prefix(word: &str) -> bool {
    matches!(
        word,
        "hey" | "hi" | "hello" | "ok" | "okay" | "yo" | "ê" | "hây" | "hay" | "chào" | "này" | "alo" | "ơi" | "à"
    )
}

fn match_fuzzy_jarvis_prefix(tokens: &[(String, usize)]) -> Option<usize> {
    if tokens.is_empty() {
        return None;
    }
    if is_jarvis_variant(&tokens[0].0) {
        return Some(1);
    }
    if tokens.len() >= 2 {
        let w0 = tokens[0].0.as_str();
        let w1 = tokens[1].0.as_str();
        if (is_greeting_prefix(w0) && is_jarvis_variant(w1))
            || (w0 == "hey" && matches!(w1, "david" | "travis"))
        {
            return Some(2);
        }
    }
    None
}

/// Normalize text for stop-speaking command matching.
pub fn normalize_text(text: &str) -> String {
    let lower = text.to_lowercase();
    let cleaned: String = lower
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Returns true if the transcribed text matches a stop/silence speaking command.
pub fn is_stop_speaking_command(text: &str) -> bool {
    let mut normalized = normalize_text(text);
    if normalized.is_empty() {
        return false;
    }

    // Strip optional leading wake prefixes in a loop
    loop {
        let mut stripped = false;
        let prefixes = [
            "hey jarvis ",
            "hay jarvis ",
            "ok jarvis ",
            "okay jarvis ",
            "yo jarvis ",
            "ê jarvis ",
            "e jarvis ",
            "jarvis ơi ",
            "jarvis oi ",
            "jarvis ",
        ];
        for prefix in &prefixes {
            if normalized.starts_with(prefix) {
                normalized = normalized[prefix.len()..].trim().to_string();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }

    // Strip optional trailing vocatives in a loop
    loop {
        let mut stripped = false;
        let suffixes = [
            " jarvis",
            " đi",
            " di",
            " nhé",
            " nhe",
            " ngay",
            " luôn",
            " luon",
            " please",
            " now",
        ];
        for suffix in &suffixes {
            if normalized.ends_with(suffix) {
                normalized = normalized[..normalized.len() - suffix.len()].trim().to_string();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }

    let normalized = normalized.trim().to_string();
    if normalized.is_empty() || normalized.split_whitespace().count() > 5 {
        return false;
    }

    let stop_phrases = [
        "stop talking",
        "shut up",
        "shutup",
        "stop speaking",
        "stop",
        "be quiet",
        "quiet",
        "silence",
        "mute",
        "enough",
        "hush",
        "im",
        "im đi",
        "im di",
        "im lặng",
        "im lang",
        "câm",
        "câm mồm",
        "cam mom",
        "câm miệng",
        "cam mieng",
        "dừng nói",
        "dung noi",
        "ngừng nói",
        "ngung noi",
        "tắt tiếng",
        "tat tieng",
        "thôi",
        "thôi đi",
        "thoi di",
        "dừng lại",
        "dung lai",
        "nín",
        "nín đi",
        "nin di",
    ];

    stop_phrases.contains(&normalized.as_str())
}
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

fn strip_trailing_vocative(s: &str) -> &str {
    let mut cur = strip_leading_filler(s).trim();
    loop {
        let tokens = tokenize_words(cur);
        let Some((first, end_idx)) = tokens.first() else {
            break;
        };
        if matches!(first.as_str(), "ơi" | "à" | "ạ" | "nhé" | "nha" | "nhỉ" | "please") {
            cur = strip_leading_filler(&cur[*end_idx..]).trim();
        } else {
            break;
        }
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_wake_word_matches_vietnamese_and_whisper_variants() {
        let matcher = WakeWordMatcher::default();
        assert_eq!(matcher.match_transcript("Hey Javis"), WakeMatch::WakeOnly);
        assert_eq!(matcher.match_transcript("Jarvis ơi"), WakeMatch::WakeOnly);
        assert_eq!(
            matcher.match_transcript("Ê Jarvis mở nhạc"),
            WakeMatch::WakeWithCommand("mở nhạc".to_string())
        );
        assert_eq!(matcher.match_transcript("Ok Jarvis!"), WakeMatch::WakeOnly);
        assert_eq!(matcher.match_transcript("tôi đang nói về jarvis"), WakeMatch::NoMatch);
        assert_eq!(matcher.match_transcript("jarvison status"), WakeMatch::NoMatch);
    }

    #[test]
    fn test_is_stop_speaking_command() {
        assert!(is_stop_speaking_command("stop talking"));
        assert!(is_stop_speaking_command("Hey Jarvis, shut up!"));
        assert!(is_stop_speaking_command("shutup"));
        assert!(is_stop_speaking_command("Jarvis im đi"));
        assert!(is_stop_speaking_command("dừng nói ngay"));
        assert!(is_stop_speaking_command("tắt tiếng"));

        assert!(!is_stop_speaking_command("Jarvis stop the music"));
        assert!(!is_stop_speaking_command("stop the build server"));
    }
}
