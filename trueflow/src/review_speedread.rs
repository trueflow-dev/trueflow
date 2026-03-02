#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Paused,
    Playing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PunctuationDwellMode {
    Off,
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub is_word: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phrase {
    pub text: String,
    pub start_word_index: usize,
    pub end_word_index: usize,
    pub anchor_char_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeedReadSettings {
    pub wpm: u16,
    pub chunk_words: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeedReadModel {
    pub tokens: Vec<Token>,
    pub phrases: Vec<Phrase>,
    pub cursor: usize,
    pub playback: PlaybackState,
    pub settings: SpeedReadSettings,
}

pub fn tokenize_prose(text: &str) -> Vec<Token> {
    text.split_whitespace()
        .map(|word| Token {
            text: word.to_string(),
            is_word: true,
        })
        .collect()
}

pub fn build_phrases(tokens: &[Token], chunk_words: u8) -> Vec<Phrase> {
    let mut phrases = Vec::new();
    let chunk_words = chunk_words.max(1) as usize;
    let words = tokens
        .iter()
        .filter(|token| token.is_word)
        .collect::<Vec<_>>();
    for (chunk_index, chunk) in words.chunks(chunk_words).enumerate() {
        let start = chunk_index * chunk_words;
        let end = start + chunk.len();
        let text = chunk
            .iter()
            .map(|token| token.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let anchor_char_index = text.chars().count() / 2;
        phrases.push(Phrase {
            text,
            start_word_index: start,
            end_word_index: end,
            anchor_char_index,
        });
    }
    phrases
}

pub fn new_model(text: &str, wpm: u16, chunk_words: u8) -> SpeedReadModel {
    let tokens = tokenize_prose(text);
    let phrases = build_phrases(&tokens, chunk_words);
    SpeedReadModel {
        tokens,
        phrases,
        cursor: 0,
        playback: PlaybackState::Paused,
        settings: SpeedReadSettings { wpm, chunk_words },
    }
}

pub fn step_next(model: &mut SpeedReadModel, loop_playback: bool) {
    if model.phrases.is_empty() {
        return;
    }
    if model.cursor + 1 < model.phrases.len() {
        model.cursor += 1;
    } else if loop_playback {
        model.cursor = 0;
    }
}

pub fn step_prev(model: &mut SpeedReadModel) {
    model.cursor = model.cursor.saturating_sub(1);
}

pub fn set_wpm(model: &mut SpeedReadModel, new_wpm: u16, min: u16, max: u16) {
    model.settings.wpm = new_wpm.clamp(min, max);
}

pub fn rechunk_preserving_progress(model: &mut SpeedReadModel, new_chunk_words: u8) {
    model.settings.chunk_words = new_chunk_words.max(1);
    model.phrases = build_phrases(&model.tokens, model.settings.chunk_words);
    if model.cursor >= model.phrases.len() {
        model.cursor = model.phrases.len().saturating_sub(1);
    }
}

pub fn tick_interval_ms(wpm: u16, chunk_words: u8) -> u64 {
    let wpm = u64::from(wpm.max(1));
    let chunk_words = u64::from(chunk_words.max(1));
    ((60_000 * chunk_words) / wpm).clamp(30, 2_000)
}

pub fn tick_interval_with_punctuation_ms(
    wpm: u16,
    chunk_words: u8,
    phrase: &str,
    punctuation_dwell_mode: PunctuationDwellMode,
    punctuation_multiplier: f64,
) -> u64 {
    let base = tick_interval_ms(wpm, chunk_words);
    match punctuation_dwell_mode {
        PunctuationDwellMode::Off => base,
        PunctuationDwellMode::Light => {
            let last = phrase.trim_end().chars().last();
            if matches!(last, Some(',' | ';' | ':' | '.' | '!' | '?')) {
                scale_interval_by_multiplier(base, punctuation_multiplier)
            } else {
                base
            }
        }
    }
}

fn scale_interval_by_multiplier(base: u64, multiplier: f64) -> u64 {
    let normalized = if multiplier.is_finite() {
        multiplier.clamp(0.0, 10.0)
    } else {
        1.0
    };
    let basis_points = format!("{:.0}", normalized * 1000.0)
        .parse::<u64>()
        .unwrap_or(1000);
    base.saturating_mul(basis_points).saturating_add(500) / 1000
}

const WPM_STEP_NUM: u64 = 1_122_462;
const WPM_STEP_DEN: u64 = 1_000_000;

pub fn next_wpm_step_up(current_wpm: u16, max_wpm: u16) -> u16 {
    let scaled = u64::from(current_wpm)
        .saturating_mul(WPM_STEP_NUM)
        .saturating_add(WPM_STEP_DEN / 2)
        / WPM_STEP_DEN;
    let scaled_u16 = u16::try_from(scaled).unwrap_or(u16::MAX);
    scaled_u16.min(max_wpm)
}

pub fn next_wpm_step_down(current_wpm: u16, min_wpm: u16) -> u16 {
    let scaled = u64::from(current_wpm)
        .saturating_mul(WPM_STEP_DEN)
        .saturating_add(WPM_STEP_NUM / 2)
        / WPM_STEP_NUM;
    let scaled_u16 = u16::try_from(scaled).unwrap_or(u16::MAX);
    scaled_u16.max(min_wpm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_prose_splits_unicode_whitespace_and_newlines() {
        let tokens = tokenize_prose("Hello,\n\nworld   from\ttrueflow 🚀");
        let words = tokens
            .iter()
            .map(|token| token.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(words, vec!["Hello,", "world", "from", "trueflow", "🚀"]);
    }

    #[test]
    fn build_phrases_chunks_words_by_chunk_size() {
        let tokens = tokenize_prose("alpha beta gamma delta epsilon");
        let phrases = build_phrases(&tokens, 2);
        assert_eq!(phrases.len(), 3);
        assert_eq!(phrases[0].text, "alpha beta");
        assert_eq!(phrases[1].text, "gamma delta");
        assert_eq!(phrases[2].text, "epsilon");
        assert_eq!(phrases[1].start_word_index, 2);
        assert_eq!(phrases[1].end_word_index, 4);
    }

    #[test]
    fn step_next_and_prev_handle_boundaries_and_looping() {
        let mut model = new_model("a b c d", 320, 1);
        step_prev(&mut model);
        assert_eq!(model.cursor, 0);
        step_next(&mut model, false);
        step_next(&mut model, false);
        step_next(&mut model, false);
        step_next(&mut model, false);
        assert_eq!(model.cursor, 3);
        step_next(&mut model, true);
        assert_eq!(model.cursor, 0);
    }

    #[test]
    fn rechunk_preserves_current_word_progress() {
        let mut model = new_model("one two three four five six", 320, 2);
        model.cursor = 1; // "three four"
        rechunk_preserving_progress(&mut model, 3);
        assert_eq!(model.phrases.len(), 2);
        assert_eq!(model.cursor, 1);
        assert_eq!(model.phrases[1].text, "four five six");
    }

    #[test]
    fn tick_interval_clamps_and_scales() {
        assert_eq!(tick_interval_ms(320, 2), 375);
        assert!(tick_interval_ms(900, 1) >= 30);
        assert!(tick_interval_ms(120, 5) <= 2_000);
    }

    #[test]
    fn punctuation_dwell_light_adds_multiplier_for_terminal_punctuation() {
        let base = tick_interval_ms(320, 2);
        let with_punct =
            tick_interval_with_punctuation_ms(320, 2, "hello,", PunctuationDwellMode::Light, 1.15);
        assert_eq!(with_punct, (base * 1150 + 500) / 1000);
    }

    #[test]
    fn wpm_steps_use_geometric_ratio() {
        let up = next_wpm_step_up(320, 900);
        let down = next_wpm_step_down(320, 120);
        assert_eq!(up, 359);
        assert_eq!(down, 285);
    }
}
