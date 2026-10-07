//! Telling somebody who is not looking that something finished, and showing
//! a walk's progress where the terminal can show it outside its window.
//!
//! Two things a terminal can do on request, neither of which is drawing:
//!
//! - **A desktop notification** when a long walk or a download finishes, for
//!   somebody who went to another window while it ran. The full-screen views
//!   know whether the terminal has the focus (`browser::input::focused`) and
//!   say it only when it does not; a plain command does not know, and says it
//!   when what finished took longer than [`LONG`].
//! - **The walk's progress on the taskbar or the tab** (`OSC 9;4`), where a
//!   walk of an hour can be glanced at without switching to it.
//!
//! **Sent only to terminals known to understand them.** These are escape
//! sequences, and a terminal that does not know one may act on part of it:
//! iTerm2 shows `OSC 9;4;1;40` as a notification reading "4;1;40", kitty
//! discards the progress form and shows the rest. So each form goes to the
//! terminals documented to implement it (vtdn.dev's survey of each
//! terminal's source, October 2026), recognized by what they put in the
//! environment, and to nothing else. Inside tmux nothing is sent but the bell,
//! which tmux passes on as an alert; the sequences would need its passthrough
//! wrapper and a setting turned on to get through.
//!
//! What the full-screen views fall back to is the bell, which every terminal
//! has some answer to: a sound, a flash, a mark on the tab or the taskbar.
//! A plain command never rings it: an unknown focus is no reason to beep at
//! somebody who may be watching.

use std::io::Write;
use std::time::Duration;

/// How long something must have taken before a plain command, which cannot
/// tell whether anybody is looking, says it finished.
pub const LONG: Duration = Duration::from_secs(2 * 60);

/// What the terminal running this is known to understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Terminal {
    /// `OSC 9 ; text`: iTerm2, WezTerm, Ghostty, kitty, foot.
    pub osc9: bool,
    /// `OSC 777 ; notify ; title ; text`: VTE (GNOME Terminal and its kin),
    /// urxvt.
    pub osc777: bool,
    /// `OSC 9 ; 4 ; state ; percent`: Windows Terminal, ConEmu, WezTerm,
    /// Ghostty, Konsole, VTE, mintty.
    pub progress: bool,
}

impl Terminal {
    /// What the environment says about the terminal. `var` is
    /// `std::env::var` outside the tests.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Self {
        if var("TMUX").is_some() || var("STY").is_some() {
            return Self::default();
        }
        let program = var("TERM_PROGRAM").unwrap_or_default();
        let term = var("TERM").unwrap_or_default();
        let is = |name: &str| program.eq_ignore_ascii_case(name);
        let vte = var("VTE_VERSION").is_some();
        let kitty = term == "xterm-kitty";
        let foot = term.starts_with("foot");
        let wezterm = is("WezTerm");
        let ghostty = is("ghostty");
        let windows_terminal = var("WT_SESSION").is_some();
        let conemu = var("ConEmuANSI").is_some_and(|on| on == "ON");
        let konsole = var("KONSOLE_VERSION").is_some();
        let mintty = is("mintty");
        Self {
            osc9: is("iTerm.app") || wezterm || ghostty || kitty || foot,
            osc777: vte || term.starts_with("rxvt-unicode"),
            progress: windows_terminal || conemu || wezterm || ghostty || konsole || vte || mintty,
        }
    }

    /// The terminal this process writes to, read once.
    pub fn here() -> Self {
        static HERE: std::sync::OnceLock<Terminal> = std::sync::OnceLock::new();
        *HERE.get_or_init(|| Self::from_env(|name| std::env::var(name).ok()))
    }

    /// The notification saying `text`, if this terminal shows one.
    pub fn notification(&self, text: &str) -> Option<String> {
        let text = printable(text);
        if self.osc9 {
            // Never starting with a number and a semicolon, which ConEmu's
            // family reads as one of its other commands.
            Some(format!("\x1b]9;snob: {text}\x07"))
        } else if self.osc777 {
            Some(format!("\x1b]777;notify;snob;{text}\x07"))
        } else {
            None
        }
    }

    /// The progress shown on the taskbar or the tab, if this terminal shows
    /// one.
    pub fn progress(&self, progress: Progress) -> Option<String> {
        if !self.progress {
            return None;
        }
        Some(match progress {
            Progress::Clear => "\x1b]9;4;0;0\x07".to_string(),
            Progress::Unknown => "\x1b]9;4;3;0\x07".to_string(),
            Progress::Percent(p) => format!("\x1b]9;4;1;{}\x07", p.min(100)),
        })
    }
}

/// What the taskbar shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Nothing: the walk is over.
    Clear,
    /// Busy, with no total to measure against.
    Unknown,
    /// This far through.
    Percent(u8),
}

/// The text with every control character out of it, `;` included: it ends a
/// field of `OSC 777`, and an escape or a bell would end the sequence early.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() || c == ';' { ' ' } else { c })
        .collect()
}

/// Says that `what` finished, to somebody who may not be looking: a
/// notification where the terminal shows one, and otherwise the bell when
/// `ring` asks for it. Written to standard error, where the views draw, and
/// only when it is a terminal.
pub fn finished(what: &str, ring: bool) {
    if !stderr_is_a_terminal() {
        return;
    }
    let sent = match Terminal::here().notification(what) {
        Some(sequence) => sequence,
        None if ring => "\x07".to_string(),
        None => return,
    };
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(sent.as_bytes());
    let _ = stderr.flush();
}

/// Shows `progress` on the taskbar or the tab, where the terminal does.
pub fn progress(progress: Progress) {
    if !stderr_is_a_terminal() {
        return;
    }
    if let Some(sequence) = Terminal::here().progress(progress) {
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(sequence.as_bytes());
        let _ = stderr.flush();
    }
}

fn stderr_is_a_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal(vars: &[(&str, &str)]) -> Terminal {
        Terminal::from_env(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        })
    }

    #[test]
    fn each_terminal_gets_only_the_forms_it_implements() {
        let iterm = terminal(&[("TERM_PROGRAM", "iTerm.app")]);
        assert!(
            iterm.osc9 && !iterm.progress,
            "iTerm2 would show 9;4 as a notification"
        );

        let kitty = terminal(&[("TERM", "xterm-kitty")]);
        assert!(kitty.osc9 && !kitty.progress, "kitty discards 9;4");

        let windows = terminal(&[("WT_SESSION", "a-guid")]);
        assert!(windows.progress && !windows.osc9 && !windows.osc777);

        let gnome = terminal(&[("VTE_VERSION", "7800"), ("TERM", "xterm-256color")]);
        assert!(gnome.osc777 && gnome.progress && !gnome.osc9);

        let wezterm = terminal(&[("TERM_PROGRAM", "WezTerm")]);
        assert!(wezterm.osc9 && wezterm.progress);

        let unknown = terminal(&[("TERM", "xterm-256color")]);
        assert_eq!(unknown, Terminal::default());
        assert_eq!(unknown.notification("done"), None);
        assert_eq!(unknown.progress(Progress::Percent(40)), None);
    }

    #[test]
    fn inside_tmux_nothing_is_sent() {
        let under_tmux = terminal(&[("TMUX", "/tmp/tmux-1000/default,1,0"), ("WT_SESSION", "x")]);
        assert_eq!(under_tmux, Terminal::default());
    }

    #[test]
    fn the_sequences_are_whole_and_carry_no_control_character() {
        let iterm = terminal(&[("TERM_PROGRAM", "iTerm.app")]);
        assert_eq!(
            iterm.notification("4;1;40 \x1b]evil\x07").unwrap(),
            "\x1b]9;snob: 4 1 40  ]evil \x07"
        );
        let gnome = terminal(&[("VTE_VERSION", "7800")]);
        assert_eq!(
            gnome.notification("the walk; done").unwrap(),
            "\x1b]777;notify;snob;the walk  done\x07"
        );
        let windows = terminal(&[("WT_SESSION", "x")]);
        assert_eq!(
            windows.progress(Progress::Percent(140)).unwrap(),
            "\x1b]9;4;1;100\x07"
        );
        assert_eq!(
            windows.progress(Progress::Clear).unwrap(),
            "\x1b]9;4;0;0\x07"
        );
    }
}
