use anyhow::{Result, anyhow};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
    tty::IsTty,
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    env,
    io::{self, Stdout},
};

pub(crate) type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

const TUI_KEYBOARD_ENHANCEMENT_PROBE_ENV: &str = "TRUEFLOW_TUI_KEYBOARD_ENHANCEMENT_PROBE";
const TUI_KEYBOARD_ENHANCEMENT_PROBE_AUTO_VALUE: &str = "auto";
const TUI_KEYBOARD_ENHANCEMENT_PROBE_SKIP_VALUE: &str = "skip";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardEnhancementProbeMode {
    Auto,
    Skip,
}

impl KeyboardEnhancementProbeMode {
    fn from_environment() -> Result<Self> {
        match env::var(TUI_KEYBOARD_ENHANCEMENT_PROBE_ENV) {
            Ok(value) => Self::from_env_value(Some(value.as_str())),
            Err(env::VarError::NotPresent) => Self::from_env_value(None),
            Err(env::VarError::NotUnicode(_)) => Err(anyhow!(
                "{TUI_KEYBOARD_ENHANCEMENT_PROBE_ENV} must be valid UTF-8"
            )),
        }
    }

    fn from_env_value(value: Option<&str>) -> Result<Self> {
        match value {
            None => Ok(Self::Auto),
            Some(TUI_KEYBOARD_ENHANCEMENT_PROBE_AUTO_VALUE) => Ok(Self::Auto),
            Some(TUI_KEYBOARD_ENHANCEMENT_PROBE_SKIP_VALUE) => Ok(Self::Skip),
            Some(value) => Err(anyhow!(
                "unsupported {TUI_KEYBOARD_ENHANCEMENT_PROBE_ENV} value {value:?}; expected {TUI_KEYBOARD_ENHANCEMENT_PROBE_AUTO_VALUE:?} or {TUI_KEYBOARD_ENHANCEMENT_PROBE_SKIP_VALUE:?}"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalCapabilities {
    keyboard_enhancement_supported: bool,
}

impl TerminalCapabilities {
    pub(crate) fn detect() -> Result<Self> {
        Ok(Self::detect_with_probe_mode(
            KeyboardEnhancementProbeMode::from_environment()?,
            supports_keyboard_enhancement,
        ))
    }

    fn detect_with_probe_mode<F>(probe_mode: KeyboardEnhancementProbeMode, probe: F) -> Self
    where
        F: FnOnce() -> io::Result<bool>,
    {
        match probe_mode {
            KeyboardEnhancementProbeMode::Auto => {
                Self::from_keyboard_enhancement_support_result(probe())
            }
            KeyboardEnhancementProbeMode::Skip => {
                Self::from_keyboard_enhancement_support_result(Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "keyboard enhancement probe skipped",
                )))
            }
        }
    }

    fn from_keyboard_enhancement_support_result(result: io::Result<bool>) -> Self {
        Self {
            keyboard_enhancement_supported: should_request_keyboard_enhancement(result),
        }
    }

    pub(crate) fn keyboard_enhancement_supported(self) -> bool {
        self.keyboard_enhancement_supported
    }

    #[cfg(test)]
    pub(crate) fn with_keyboard_enhancement_supported(
        keyboard_enhancement_supported: bool,
    ) -> Self {
        Self {
            keyboard_enhancement_supported,
        }
    }
}

pub(crate) fn tui_keyboard_enhancement_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
}

pub(crate) fn enter_tui_mode<W: io::Write>(
    writer: &mut W,
    capabilities: TerminalCapabilities,
) -> Result<()> {
    execute!(
        writer,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    if capabilities.keyboard_enhancement_supported()
        && let Err(error) = execute!(
            writer,
            PushKeyboardEnhancementFlags(tui_keyboard_enhancement_flags())
        )
    {
        let primary = anyhow::Error::new(error);
        if let Err(restore) = leave_base_tui_mode(writer) {
            return Err(merge_primary_and_restore_error(&primary, &restore));
        }
        return Err(primary);
    }
    Ok(())
}

fn leave_base_tui_mode<W: io::Write>(writer: &mut W) -> Result<()> {
    execute!(writer, DisableMouseCapture, DisableBracketedPaste)?;
    execute!(writer, LeaveAlternateScreen)?;
    Ok(())
}

pub(crate) fn leave_tui_mode<W: io::Write>(
    writer: &mut W,
    capabilities: TerminalCapabilities,
) -> Result<()> {
    execute!(writer, DisableMouseCapture, DisableBracketedPaste)?;
    if capabilities.keyboard_enhancement_supported() {
        execute!(writer, PopKeyboardEnhancementFlags)?;
    }
    execute!(writer, LeaveAlternateScreen)?;
    Ok(())
}

fn validate_tty_preflight(stdin_is_tty: bool, stdout_is_tty: bool) -> Result<()> {
    if stdin_is_tty && stdout_is_tty {
        return Ok(());
    }

    let mut missing_streams = Vec::new();
    if !stdin_is_tty {
        missing_streams.push("stdin");
    }
    if !stdout_is_tty {
        missing_streams.push("stdout");
    }

    Err(anyhow!(
        "trueflow tui requires an interactive terminal; {} {} not TTY",
        missing_streams.join(" and "),
        if missing_streams.len() == 1 {
            "is"
        } else {
            "are"
        }
    ))
}

fn ensure_tty_preflight() -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    validate_tty_preflight(stdin.is_tty(), stdout.is_tty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalPhase {
    Active,
    Suspended,
    Restored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalModeState {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalSessionStatus {
    phase: TerminalPhase,
    raw_mode: TerminalModeState,
    tui_mode: TerminalModeState,
}

impl TerminalSessionStatus {
    fn active() -> Self {
        Self {
            phase: TerminalPhase::Active,
            raw_mode: TerminalModeState::Enabled,
            tui_mode: TerminalModeState::Enabled,
        }
    }

    fn is_restored(self) -> bool {
        matches!(self.phase, TerminalPhase::Restored)
    }

    fn raw_mode_enabled(self) -> bool {
        matches!(self.raw_mode, TerminalModeState::Enabled)
    }

    fn tui_mode_enabled(self) -> bool {
        matches!(self.tui_mode, TerminalModeState::Enabled)
    }

    fn mark_raw_mode_disabled(&mut self) {
        self.raw_mode = TerminalModeState::Disabled;
        if self.tui_mode_enabled() {
            return;
        }
        if !self.is_restored() {
            self.phase = TerminalPhase::Suspended;
        }
    }

    fn mark_tui_mode_disabled(&mut self) {
        self.tui_mode = TerminalModeState::Disabled;
        if self.raw_mode_enabled() {
            return;
        }
        if !self.is_restored() {
            self.phase = TerminalPhase::Suspended;
        }
    }

    fn mark_raw_mode_enabled(&mut self) {
        self.raw_mode = TerminalModeState::Enabled;
        self.phase = TerminalPhase::Active;
    }

    fn mark_tui_mode_enabled(&mut self) {
        self.tui_mode = TerminalModeState::Enabled;
        self.phase = TerminalPhase::Active;
    }

    fn mark_restored(&mut self) {
        self.phase = TerminalPhase::Restored;
        self.raw_mode = TerminalModeState::Disabled;
        self.tui_mode = TerminalModeState::Disabled;
    }
}

pub(crate) struct TerminalSession {
    terminal: TuiTerminal,
    capabilities: TerminalCapabilities,
    status: TerminalSessionStatus,
}

impl TerminalSession {
    pub(crate) fn enter() -> Result<Self> {
        ensure_tty_preflight()?;
        let capabilities = TerminalCapabilities::detect()?;
        let mut stdout = io::stdout();
        enter_tui_mode(&mut stdout, capabilities)?;
        if let Err(error) = enable_raw_mode() {
            let _ = leave_tui_mode(&mut stdout, capabilities);
            return Err(error.into());
        }

        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self {
                terminal,
                capabilities,
                status: TerminalSessionStatus::active(),
            }),
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = disable_raw_mode();
                let _ = leave_tui_mode(&mut stdout, capabilities);
                Err(error.into())
            }
        }
    }

    pub(crate) fn terminal_mut(&mut self) -> &mut TuiTerminal {
        &mut self.terminal
    }

    pub(crate) fn suspend<F>(&mut self, action: F) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        self.deactivate()?;
        let action_result = action();
        let restore_result = self.reactivate();

        match (action_result, restore_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(primary), Ok(())) => Err(primary),
            (Ok(()), Err(restore)) => Err(restore),
            (Err(primary), Err(restore)) => {
                Err(merge_primary_and_restore_error(&primary, &restore))
            }
        }
    }

    pub(crate) fn restore(&mut self) -> Result<()> {
        if self.status.is_restored() {
            return Ok(());
        }

        let mut first_error = None;

        if self.status.raw_mode_enabled() {
            if let Err(error) = disable_raw_mode() {
                first_error.get_or_insert_with(|| error.into());
            } else {
                self.status.mark_raw_mode_disabled();
            }
        }

        if self.status.tui_mode_enabled() {
            if let Err(error) = leave_tui_mode(self.terminal.backend_mut(), self.capabilities) {
                first_error.get_or_insert(error);
            } else {
                self.status.mark_tui_mode_disabled();
            }
        }

        if let Err(error) = self.terminal.show_cursor() {
            first_error.get_or_insert_with(|| error.into());
        }

        if let Some(error) = first_error {
            Err(error)
        } else {
            self.status.mark_restored();
            Ok(())
        }
    }

    fn deactivate(&mut self) -> Result<()> {
        if self.status.raw_mode_enabled() {
            disable_raw_mode()?;
            self.status.mark_raw_mode_disabled();
        }
        if self.status.tui_mode_enabled() {
            leave_tui_mode(self.terminal.backend_mut(), self.capabilities)?;
            self.status.mark_tui_mode_disabled();
        }
        Ok(())
    }

    fn reactivate(&mut self) -> Result<()> {
        if !self.status.tui_mode_enabled() {
            enter_tui_mode(self.terminal.backend_mut(), self.capabilities)?;
            self.status.mark_tui_mode_enabled();
        }
        if !self.status.raw_mode_enabled() {
            enable_raw_mode()?;
            self.status.mark_raw_mode_enabled();
        }
        self.terminal.clear()?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if !self.status.is_restored() {
            let _ = self.restore();
        }
    }
}

// On Unix, requesting kitty keyboard enhancement is cheap and terminals that
// do not support it generally ignore the escape sequence. The probe itself is
// the flaky part, so we request it optimistically there.
fn should_request_keyboard_enhancement(result: io::Result<bool>) -> bool {
    if cfg!(windows) {
        result.unwrap_or(false)
    } else {
        let _ = result;
        true
    }
}

fn merge_primary_and_restore_error(
    primary: &anyhow::Error,
    restore: &anyhow::Error,
) -> anyhow::Error {
    anyhow!("{primary:#}\nterminal restore also failed: {restore:#}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_capabilities_enable_keyboard_enhancement_when_probe_succeeds() {
        let capabilities = TerminalCapabilities::from_keyboard_enhancement_support_result(Ok(true));
        assert!(capabilities.keyboard_enhancement_supported());
    }

    #[test]
    fn terminal_capabilities_follow_platform_keyboard_enhancement_fallback_policy() {
        let unsupported = TerminalCapabilities::from_keyboard_enhancement_support_result(Ok(false));
        let error = TerminalCapabilities::from_keyboard_enhancement_support_result(Err(
            io::Error::new(io::ErrorKind::Unsupported, "no keyboard enhancement"),
        ));

        if cfg!(windows) {
            assert!(!unsupported.keyboard_enhancement_supported());
            assert!(!error.keyboard_enhancement_supported());
        } else {
            assert!(unsupported.keyboard_enhancement_supported());
            assert!(error.keyboard_enhancement_supported());
        }
    }

    #[test]
    fn terminal_capabilities_auto_mode_runs_keyboard_enhancement_probe() {
        let mut probe_called = false;

        let capabilities = TerminalCapabilities::detect_with_probe_mode(
            KeyboardEnhancementProbeMode::Auto,
            || {
                probe_called = true;
                Ok(true)
            },
        );

        assert!(probe_called);
        assert!(capabilities.keyboard_enhancement_supported());
    }

    #[test]
    fn terminal_capabilities_skip_mode_avoids_keyboard_enhancement_probe() {
        let capabilities = TerminalCapabilities::detect_with_probe_mode(
            KeyboardEnhancementProbeMode::Skip,
            || panic!("keyboard enhancement probe should not run in skip mode"),
        );

        if cfg!(windows) {
            assert!(!capabilities.keyboard_enhancement_supported());
        } else {
            assert!(capabilities.keyboard_enhancement_supported());
        }
    }

    #[test]
    fn keyboard_enhancement_probe_mode_parses_explicit_env_values() {
        assert_eq!(
            KeyboardEnhancementProbeMode::from_env_value(None).unwrap(),
            KeyboardEnhancementProbeMode::Auto
        );
        assert_eq!(
            KeyboardEnhancementProbeMode::from_env_value(Some("auto")).unwrap(),
            KeyboardEnhancementProbeMode::Auto
        );
        assert_eq!(
            KeyboardEnhancementProbeMode::from_env_value(Some("skip")).unwrap(),
            KeyboardEnhancementProbeMode::Skip
        );
        assert!(KeyboardEnhancementProbeMode::from_env_value(Some("later")).is_err());
    }

    #[test]
    fn validate_tty_preflight_accepts_interactive_stdio() {
        validate_tty_preflight(true, true)
            .unwrap_or_else(|error| panic!("expected tty ok: {error}"));
    }

    #[test]
    fn validate_tty_preflight_mentions_stdin() {
        let error = validate_tty_preflight(false, true).unwrap_err();
        assert!(error.to_string().contains("stdin"));
    }

    #[test]
    fn validate_tty_preflight_mentions_stdout() {
        let error = validate_tty_preflight(true, false).unwrap_err();
        assert!(error.to_string().contains("stdout"));
    }

    #[test]
    fn validate_tty_preflight_mentions_both_streams() {
        let error = validate_tty_preflight(false, false).unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("stdin"));
        assert!(rendered.contains("stdout"));
    }

    #[test]
    fn enter_tui_mode_skips_keyboard_enhancement_when_unsupported() {
        let mut output = Vec::new();
        enter_tui_mode(
            &mut output,
            TerminalCapabilities::with_keyboard_enhancement_supported(false),
        )
        .unwrap_or_else(|error| panic!("enter tui mode: {error}"));

        let rendered =
            String::from_utf8(output).unwrap_or_else(|error| panic!("invalid ansi bytes: {error}"));
        assert!(rendered.contains("\u{1b}[?2004h"));
        assert!(!rendered.contains("\u{1b}[>11u"));
    }

    #[test]
    fn enter_tui_mode_rolls_back_base_modes_when_keyboard_enhancement_fails() {
        struct FailOnceOnKeyboardPush {
            output: Vec<u8>,
            failed: bool,
        }

        impl io::Write for FailOnceOnKeyboardPush {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let base_modes_entered = self
                    .output
                    .windows(b"\x1b[?2004h".len())
                    .any(|part| part == b"\x1b[?2004h");
                if !self.failed && base_modes_entered {
                    self.failed = true;
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "keyboard enhancement write failed",
                    ));
                }
                self.output.extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut writer = FailOnceOnKeyboardPush {
            output: Vec::new(),
            failed: false,
        };
        let error = enter_tui_mode(
            &mut writer,
            TerminalCapabilities::with_keyboard_enhancement_supported(true),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("keyboard enhancement write failed")
        );
        let rendered = String::from_utf8(writer.output)
            .unwrap_or_else(|error| panic!("invalid ansi bytes: {error}"));
        assert!(rendered.contains("\u{1b}[?2004h"));
        assert!(rendered.contains("\u{1b}[?2004l"));
        assert!(rendered.contains("\u{1b}[?1049l"));
    }

    #[test]
    fn leave_tui_mode_skips_keyboard_enhancement_when_unsupported() {
        let mut output = Vec::new();
        leave_tui_mode(
            &mut output,
            TerminalCapabilities::with_keyboard_enhancement_supported(false),
        )
        .unwrap_or_else(|error| panic!("leave tui mode: {error}"));

        let rendered =
            String::from_utf8(output).unwrap_or_else(|error| panic!("invalid ansi bytes: {error}"));
        assert!(rendered.contains("\u{1b}[?2004l"));
        assert!(!rendered.contains("\u{1b}[<1u"));
    }

    #[test]
    fn terminal_session_status_starts_active_with_both_modes_enabled() {
        let status = TerminalSessionStatus::active();

        assert_eq!(status.phase, TerminalPhase::Active);
        assert!(status.raw_mode_enabled());
        assert!(status.tui_mode_enabled());
        assert!(!status.is_restored());
    }

    #[test]
    fn terminal_session_status_becomes_suspended_once_both_modes_are_disabled() {
        let mut status = TerminalSessionStatus::active();

        status.mark_raw_mode_disabled();
        assert_eq!(status.phase, TerminalPhase::Active);
        status.mark_tui_mode_disabled();

        assert_eq!(status.phase, TerminalPhase::Suspended);
        assert!(!status.raw_mode_enabled());
        assert!(!status.tui_mode_enabled());
    }

    #[test]
    fn terminal_session_status_marks_restored_explicitly() {
        let mut status = TerminalSessionStatus::active();

        status.mark_raw_mode_disabled();
        status.mark_tui_mode_disabled();
        status.mark_restored();

        assert_eq!(status.phase, TerminalPhase::Restored);
        assert!(status.is_restored());
        assert!(!status.raw_mode_enabled());
        assert!(!status.tui_mode_enabled());
    }

    #[test]
    fn merge_primary_and_restore_error_mentions_both_failures() {
        let primary = anyhow!("primary action failed");
        let restore = anyhow!("restore failed");
        let error = merge_primary_and_restore_error(&primary, &restore);
        let rendered = error.to_string();
        assert!(rendered.contains("primary action failed"));
        assert!(rendered.contains("restore failed"));
    }
}
