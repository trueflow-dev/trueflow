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
use std::io::{self, Stdout};

pub(crate) type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalCapabilities {
    keyboard_enhancement_supported: bool,
}

impl TerminalCapabilities {
    pub(crate) fn detect() -> Self {
        Self::from_keyboard_enhancement_support_result(supports_keyboard_enhancement())
    }

    fn from_keyboard_enhancement_support_result(result: io::Result<bool>) -> Self {
        Self {
            keyboard_enhancement_supported: result.unwrap_or(false),
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
    if capabilities.keyboard_enhancement_supported() {
        execute!(
            writer,
            PushKeyboardEnhancementFlags(tui_keyboard_enhancement_flags())
        )?;
    }
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

pub(crate) struct TerminalSession {
    terminal: TuiTerminal,
    capabilities: TerminalCapabilities,
    raw_mode_enabled: bool,
    tui_mode_enabled: bool,
    restored: bool,
}

impl TerminalSession {
    pub(crate) fn enter() -> Result<Self> {
        ensure_tty_preflight()?;
        let capabilities = TerminalCapabilities::detect();
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
                raw_mode_enabled: true,
                tui_mode_enabled: true,
                restored: false,
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
        if self.restored {
            return Ok(());
        }

        let mut first_error = None;

        if self.raw_mode_enabled {
            if let Err(error) = disable_raw_mode() {
                first_error.get_or_insert_with(|| error.into());
            } else {
                self.raw_mode_enabled = false;
            }
        }

        if self.tui_mode_enabled {
            if let Err(error) = leave_tui_mode(self.terminal.backend_mut(), self.capabilities) {
                first_error.get_or_insert(error);
            } else {
                self.tui_mode_enabled = false;
            }
        }

        if let Err(error) = self.terminal.show_cursor() {
            first_error.get_or_insert_with(|| error.into());
        }

        if let Some(error) = first_error {
            Err(error)
        } else {
            self.restored = true;
            Ok(())
        }
    }

    fn deactivate(&mut self) -> Result<()> {
        if self.raw_mode_enabled {
            disable_raw_mode()?;
            self.raw_mode_enabled = false;
        }
        if self.tui_mode_enabled {
            leave_tui_mode(self.terminal.backend_mut(), self.capabilities)?;
            self.tui_mode_enabled = false;
        }
        Ok(())
    }

    fn reactivate(&mut self) -> Result<()> {
        if !self.tui_mode_enabled {
            enter_tui_mode(self.terminal.backend_mut(), self.capabilities)?;
            self.tui_mode_enabled = true;
        }
        if !self.raw_mode_enabled {
            enable_raw_mode()?;
            self.raw_mode_enabled = true;
        }
        self.terminal.clear()?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.restore();
        }
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
    fn terminal_capabilities_disable_keyboard_enhancement_when_probe_is_unsupported() {
        let capabilities =
            TerminalCapabilities::from_keyboard_enhancement_support_result(Ok(false));
        assert!(!capabilities.keyboard_enhancement_supported());
    }

    #[test]
    fn terminal_capabilities_disable_keyboard_enhancement_when_probe_errors() {
        let capabilities = TerminalCapabilities::from_keyboard_enhancement_support_result(Err(
            io::Error::new(io::ErrorKind::Unsupported, "no keyboard enhancement"),
        ));
        assert!(!capabilities.keyboard_enhancement_supported());
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
    fn merge_primary_and_restore_error_mentions_both_failures() {
        let primary = anyhow!("primary action failed");
        let restore = anyhow!("restore failed");
        let error = merge_primary_and_restore_error(&primary, &restore);
        let rendered = error.to_string();
        assert!(rendered.contains("primary action failed"));
        assert!(rendered.contains("restore failed"));
    }
}
