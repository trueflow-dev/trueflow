use crate::block::Block;
use crate::config::{
    TuiSpeedReadConfig, TuiSpeedReadPunctuationDwell, update_speed_read_defaults_in_file,
};
use crate::review_speedread::{
    PlaybackState, PunctuationDwellMode, SpeedReadModel, new_model as build_speed_read_model,
    next_wpm_step_down, next_wpm_step_up, rechunk_preserving_progress, set_wpm, step_next,
    step_prev, tick_interval_with_punctuation_ms,
};
use crate::tree::{TreeNodeId, TreeNodeKind};
use anyhow::Result;
use crossterm::event::KeyCode;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const SPEED_READ_PERSIST_DEBOUNCE: Duration = Duration::from_millis(512);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PersistedSpeedReadDefaults {
    default_wpm: u16,
    default_chunk_words: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingSpeedReadPersist {
    defaults: PersistedSpeedReadDefaults,
    flush_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpeedReadUiState {
    pub node_id: TreeNodeId,
    pub model: SpeedReadModel,
    pub next_tick_at: Option<Instant>,
    pub show_prose_optimization_hint: bool,
}

pub(crate) struct SpeedReadController {
    config: TuiSpeedReadConfig,
    active: Option<SpeedReadUiState>,
    persisted_defaults: PersistedSpeedReadDefaults,
    pending_persist: Option<PendingSpeedReadPersist>,
    config_path: PathBuf,
}

impl SpeedReadController {
    pub(crate) fn new(config: TuiSpeedReadConfig, config_path: PathBuf) -> Self {
        let default_wpm = config.default_wpm.clamp(config.min_wpm, config.max_wpm);
        let default_chunk_words = config
            .default_chunk_words
            .clamp(config.min_chunk_words, config.max_chunk_words);

        Self {
            config,
            active: None,
            persisted_defaults: PersistedSpeedReadDefaults {
                default_wpm,
                default_chunk_words,
            },
            pending_persist: None,
            config_path,
        }
    }

    pub(crate) fn config(&self) -> &TuiSpeedReadConfig {
        &self.config
    }

    #[cfg(test)]
    pub(crate) fn is_none(&self) -> bool {
        self.active.is_none()
    }

    #[cfg(test)]
    pub(crate) fn as_ref(&self) -> Option<&SpeedReadUiState> {
        self.active.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn as_mut(&mut self) -> Option<&mut SpeedReadUiState> {
        self.active.as_mut()
    }

    pub(crate) fn active_for(&self, node_id: TreeNodeId) -> Option<&SpeedReadUiState> {
        self.active.as_ref().filter(|mode| mode.node_id == node_id)
    }

    pub(crate) fn is_active_for(&self, node_id: TreeNodeId) -> bool {
        self.active_for(node_id).is_some()
    }

    pub(crate) fn next_deadline(&self, current_node_id: TreeNodeId) -> Option<Instant> {
        let speed_read_deadline = self
            .active
            .as_ref()
            .and_then(|mode| mode.next_tick_at)
            .filter(|_| self.is_active_for(current_node_id));
        let persist_deadline = self.pending_persist.map(|pending| pending.flush_at);

        match (speed_read_deadline, persist_deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        }
    }

    pub(crate) fn clear_if_not_on_current_node(&mut self, current_node_id: TreeNodeId) {
        if self
            .active
            .as_ref()
            .is_some_and(|mode| mode.node_id != current_node_id)
        {
            self.active = None;
        }
    }

    pub(crate) fn toggle_for_node(
        &mut self,
        current_node_id: TreeNodeId,
        node_kind: TreeNodeKind,
        block: Option<&Block>,
    ) -> bool {
        if !self.config.enabled {
            self.active = None;
            return false;
        }

        if self
            .active
            .as_ref()
            .is_some_and(|mode| mode.node_id == current_node_id)
        {
            self.active = None;
            return false;
        }

        if !matches!(node_kind, TreeNodeKind::Block) {
            self.active = None;
            return false;
        }

        let Some(block) = block else {
            self.active = None;
            return false;
        };

        let default_wpm = self
            .persisted_defaults
            .default_wpm
            .clamp(self.config.min_wpm, self.config.max_wpm);
        let default_chunk_words = self
            .persisted_defaults
            .default_chunk_words
            .clamp(self.config.min_chunk_words, self.config.max_chunk_words);
        let mut model = build_speed_read_model(&block.content, default_wpm, default_chunk_words);
        let show_prose_optimization_hint =
            self.config.show_prose_optimization_hint && is_code_heavy_text(&block.content);

        if !model.phrases.is_empty() {
            model.playback = PlaybackState::Playing;
        }

        let mut active = SpeedReadUiState {
            node_id: current_node_id,
            model,
            next_tick_at: None,
            show_prose_optimization_hint,
        };
        update_next_tick(
            &mut active,
            self.config.punctuation_dwell,
            self.config.punctuation_dwell_multiplier,
        );

        self.active = Some(active);
        true
    }

    pub(crate) fn handle_key_binding(
        &mut self,
        key_code: KeyCode,
        current_node_id: TreeNodeId,
    ) -> bool {
        if !self.is_active_for(current_node_id) {
            return false;
        }

        match key_code {
            KeyCode::Esc => {
                self.active = None;
                true
            }
            KeyCode::Char(' ') => {
                self.toggle_playback();
                true
            }
            KeyCode::Char('j') => {
                self.step_prev();
                true
            }
            KeyCode::Char('l') => {
                self.step_next_or_exit();
                true
            }
            KeyCode::Char('-') => {
                self.adjust_wpm_down();
                true
            }
            KeyCode::Char('=') => {
                self.adjust_wpm_up();
                true
            }
            KeyCode::Char('[') => {
                self.adjust_chunk_down();
                true
            }
            KeyCode::Char(']') => {
                self.adjust_chunk_up();
                true
            }
            KeyCode::Char('0') => {
                self.reset_to_persisted_defaults();
                true
            }
            _ => false,
        }
    }

    pub(crate) fn handle_autoplay_timeout(
        &mut self,
        now: Instant,
        current_node_id: TreeNodeId,
    ) -> bool {
        if !self.is_active_for(current_node_id) {
            self.clear_if_not_on_current_node(current_node_id);
            return false;
        }

        let mut should_exit = false;
        let mut did_update = false;
        let punctuation_dwell = self.config.punctuation_dwell;
        let punctuation_dwell_multiplier = self.config.punctuation_dwell_multiplier;
        if let Some(mode) = self.active.as_mut() {
            let loop_playback = self.config.loop_playback;
            if mode.model.playback != PlaybackState::Playing {
                return false;
            }
            if mode.model.phrases.is_empty() {
                should_exit = true;
            } else if let Some(next_tick_at) = mode.next_tick_at {
                if now >= next_tick_at {
                    if mode.model.cursor + 1 >= mode.model.phrases.len() && !loop_playback {
                        mode.model.playback = PlaybackState::Paused;
                        should_exit = true;
                    } else {
                        step_next(&mut mode.model, loop_playback);
                        mode.next_tick_at = Some(
                            now + tick_interval(
                                mode,
                                punctuation_dwell,
                                punctuation_dwell_multiplier,
                            ),
                        );
                        did_update = true;
                    }
                }
            } else {
                mode.next_tick_at = Some(
                    now + tick_interval(mode, punctuation_dwell, punctuation_dwell_multiplier),
                );
                did_update = true;
            }
        }

        if should_exit {
            self.active = None;
            return true;
        }

        did_update
    }

    pub(crate) fn flush_due_defaults(&mut self, now: Instant) -> Result<bool> {
        let Some(pending) = self.pending_persist else {
            return Ok(false);
        };
        if now < pending.flush_at {
            return Ok(false);
        }

        self.flush_pending_defaults()?;
        Ok(true)
    }

    pub(crate) fn flush_pending_defaults(&mut self) -> Result<()> {
        let Some(pending) = self.pending_persist else {
            return Ok(());
        };

        update_speed_read_defaults_in_file(
            &self.config_path,
            pending.defaults.default_wpm,
            pending.defaults.default_chunk_words,
        )?;
        self.persisted_defaults = pending.defaults;
        self.pending_persist = None;
        Ok(())
    }

    fn step_prev(&mut self) {
        let punctuation_dwell = self.config.punctuation_dwell;
        let punctuation_dwell_multiplier = self.config.punctuation_dwell_multiplier;
        if let Some(mode) = self.active.as_mut() {
            step_prev(&mut mode.model);
            update_next_tick(mode, punctuation_dwell, punctuation_dwell_multiplier);
        }
    }

    fn step_next_or_exit(&mut self) {
        let mut should_exit = false;
        let punctuation_dwell = self.config.punctuation_dwell;
        let punctuation_dwell_multiplier = self.config.punctuation_dwell_multiplier;
        if let Some(mode) = self.active.as_mut() {
            let loop_playback = self.config.loop_playback;
            if mode.model.phrases.is_empty()
                || (mode.model.cursor + 1 >= mode.model.phrases.len() && !loop_playback)
            {
                mode.model.playback = PlaybackState::Paused;
                should_exit = true;
            } else {
                step_next(&mut mode.model, loop_playback);
                update_next_tick(mode, punctuation_dwell, punctuation_dwell_multiplier);
            }
        }
        if should_exit {
            self.active = None;
        }
    }

    fn toggle_playback(&mut self) {
        let punctuation_dwell = self.config.punctuation_dwell;
        let punctuation_dwell_multiplier = self.config.punctuation_dwell_multiplier;
        if let Some(mode) = self.active.as_mut() {
            mode.model.playback = match mode.model.playback {
                PlaybackState::Paused => PlaybackState::Playing,
                PlaybackState::Playing => PlaybackState::Paused,
            };
            update_next_tick(mode, punctuation_dwell, punctuation_dwell_multiplier);
        }
    }

    fn adjust_wpm_up(&mut self) {
        let punctuation_dwell = self.config.punctuation_dwell;
        let punctuation_dwell_multiplier = self.config.punctuation_dwell_multiplier;
        if let Some(mode) = self.active.as_mut() {
            let updated = next_wpm_step_up(mode.model.settings.wpm, self.config.max_wpm);
            set_wpm(
                &mut mode.model,
                updated,
                self.config.min_wpm,
                self.config.max_wpm,
            );
            update_next_tick(mode, punctuation_dwell, punctuation_dwell_multiplier);
            self.schedule_defaults_persist();
        }
    }

    fn adjust_wpm_down(&mut self) {
        let punctuation_dwell = self.config.punctuation_dwell;
        let punctuation_dwell_multiplier = self.config.punctuation_dwell_multiplier;
        if let Some(mode) = self.active.as_mut() {
            let updated = next_wpm_step_down(mode.model.settings.wpm, self.config.min_wpm);
            set_wpm(
                &mut mode.model,
                updated,
                self.config.min_wpm,
                self.config.max_wpm,
            );
            update_next_tick(mode, punctuation_dwell, punctuation_dwell_multiplier);
            self.schedule_defaults_persist();
        }
    }

    fn adjust_chunk_up(&mut self) {
        let punctuation_dwell = self.config.punctuation_dwell;
        let punctuation_dwell_multiplier = self.config.punctuation_dwell_multiplier;
        if let Some(mode) = self.active.as_mut() {
            let updated = mode
                .model
                .settings
                .chunk_words
                .saturating_add(1)
                .min(self.config.max_chunk_words);
            rechunk_preserving_progress(&mut mode.model, updated);
            update_next_tick(mode, punctuation_dwell, punctuation_dwell_multiplier);
            self.schedule_defaults_persist();
        }
    }

    fn adjust_chunk_down(&mut self) {
        let punctuation_dwell = self.config.punctuation_dwell;
        let punctuation_dwell_multiplier = self.config.punctuation_dwell_multiplier;
        if let Some(mode) = self.active.as_mut() {
            let updated = mode
                .model
                .settings
                .chunk_words
                .saturating_sub(1)
                .max(self.config.min_chunk_words);
            rechunk_preserving_progress(&mut mode.model, updated);
            update_next_tick(mode, punctuation_dwell, punctuation_dwell_multiplier);
            self.schedule_defaults_persist();
        }
    }

    fn reset_to_persisted_defaults(&mut self) {
        let punctuation_dwell = self.config.punctuation_dwell;
        let punctuation_dwell_multiplier = self.config.punctuation_dwell_multiplier;
        if let Some(mode) = self.active.as_mut() {
            let default_wpm = self
                .persisted_defaults
                .default_wpm
                .clamp(self.config.min_wpm, self.config.max_wpm);
            let default_chunk_words = self
                .persisted_defaults
                .default_chunk_words
                .clamp(self.config.min_chunk_words, self.config.max_chunk_words);
            set_wpm(
                &mut mode.model,
                default_wpm,
                self.config.min_wpm,
                self.config.max_wpm,
            );
            rechunk_preserving_progress(&mut mode.model, default_chunk_words);
            update_next_tick(mode, punctuation_dwell, punctuation_dwell_multiplier);
            self.pending_persist = None;
        }
    }

    fn schedule_defaults_persist(&mut self) {
        let Some(mode) = self.active.as_ref() else {
            return;
        };

        self.pending_persist = Some(PendingSpeedReadPersist {
            defaults: PersistedSpeedReadDefaults {
                default_wpm: mode.model.settings.wpm,
                default_chunk_words: mode.model.settings.chunk_words,
            },
            flush_at: Instant::now() + SPEED_READ_PERSIST_DEBOUNCE,
        });
    }
}

fn update_next_tick(
    mode: &mut SpeedReadUiState,
    punctuation_dwell: TuiSpeedReadPunctuationDwell,
    punctuation_dwell_multiplier: f64,
) {
    if mode.model.playback == PlaybackState::Playing {
        mode.next_tick_at = Some(
            Instant::now() + tick_interval(mode, punctuation_dwell, punctuation_dwell_multiplier),
        );
    } else {
        mode.next_tick_at = None;
    }
}

fn tick_interval(
    mode: &SpeedReadUiState,
    punctuation_dwell: TuiSpeedReadPunctuationDwell,
    punctuation_dwell_multiplier: f64,
) -> Duration {
    let phrase = mode
        .model
        .phrases
        .get(mode.model.cursor)
        .map(|phrase| phrase.text.as_str())
        .unwrap_or("");

    let dwell_mode = match punctuation_dwell {
        TuiSpeedReadPunctuationDwell::Off => PunctuationDwellMode::Off,
        TuiSpeedReadPunctuationDwell::Light => PunctuationDwellMode::Light,
    };
    let interval_ms = tick_interval_with_punctuation_ms(
        mode.model.settings.wpm,
        mode.model.settings.chunk_words,
        phrase,
        dwell_mode,
        punctuation_dwell_multiplier,
    );
    Duration::from_millis(interval_ms)
}

fn is_code_heavy_text(text: &str) -> bool {
    let total = text.chars().filter(|ch| !ch.is_whitespace()).count();
    if total == 0 {
        return false;
    }

    let code_like = text
        .chars()
        .filter(|ch| {
            matches!(
                ch,
                '{' | '}' | '(' | ')' | ';' | ':' | '=' | '<' | '>' | '[' | ']'
            )
        })
        .count();

    (code_like as f64 / total as f64) > 0.12
}
