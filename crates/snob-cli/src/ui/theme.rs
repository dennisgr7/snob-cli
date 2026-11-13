//! The colors the interface names, by what they are for.
//!
//! Five roles and no more, the five the site's terminal blocks are drawn in
//! (`web/DESIGN.md`, "Theme"; the `term-*` tokens of
//! `web/src/styles/global.css`): the accent, which is the brand's magenta,
//! and four states. A view asks for a role and never for a color, so what a
//! warning looks like is decided here and nowhere else.
//!
//! **Each role is one of the terminal's own sixteen colors**, so the user's
//! theme decides the shade, and nothing is drawn in a color their background
//! was not chosen for. The accent is used as the site uses it, sparingly:
//! the prompt, the word a title is about, a link, the keys in the help.
//!
//! **`SNOB_THEME=brand` draws the accent in the site's own pink**,
//! `#f472b6`, and only on a terminal whose `COLORTERM` says it shows 24-bit
//! color. An option and not the default, because it is one fixed color for a
//! background it knows nothing about: it is the site's accent on its dark
//! page, above 6:1 there, and on a light terminal it is hard to read. The
//! four states stay the terminal's own whatever the theme.
//!
//! What is not here: the selection, which is reverse video and no color at
//! all (`tui::selection`), and the bars of a walk, which name the same cyan,
//! green and yellow for the same states in indicatif's own templates
//! (`progress.rs`).

use std::sync::OnceLock;

use ratatui::style::{Color, Style};

/// What a color is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The brand: the prompt, the word a title is about, a link, a key.
    Accent,
    /// Done, and as asked: a file saved, a question answered.
    Ok,
    /// Worth a look: a note that is neither a receipt nor a failure.
    Warn,
    /// It did not happen.
    Error,
    /// An aside that is data: a mention, a hint.
    Info,
}

/// Where the accent comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Palette {
    /// The terminal's sixteen, every role.
    Ansi,
    /// The site's pink for the accent, in 24 bits.
    Brand,
}

/// The site's accent on its dark page (`--accent`, `web/DESIGN.md`).
const BRAND_ACCENT: (u8, u8, u8) = (0xf4, 0x72, 0xb6);

/// The palette `SNOB_THEME` and `COLORTERM` ask for. `brand` is honored only
/// where the terminal says it shows 24-bit color: a terminal that does not
/// would draw an escape sequence it half understands, or the nearest of 256
/// colors, which is nobody's accent.
fn palette_of(theme: Option<&str>, colorterm: Option<&str>) -> Palette {
    let brand = theme.is_some_and(|theme| theme.eq_ignore_ascii_case("brand"));
    let truecolor = colorterm.is_some_and(|said| {
        said.eq_ignore_ascii_case("truecolor") || said.eq_ignore_ascii_case("24bit")
    });
    if brand && truecolor {
        Palette::Brand
    } else {
        Palette::Ansi
    }
}

/// The palette of this run, read from the environment once.
fn palette() -> Palette {
    static PALETTE: OnceLock<Palette> = OnceLock::new();
    *PALETTE.get_or_init(|| {
        palette_of(
            std::env::var("SNOB_THEME").ok().as_deref(),
            std::env::var("COLORTERM").ok().as_deref(),
        )
    })
}

impl Role {
    /// The role as a view draws it: a foreground and nothing else. Passed
    /// through `tui::paint` like every other style, so a run that may not
    /// style draws none of them.
    pub fn style(self) -> Style {
        Style::new().fg(self.color_in(palette()))
    }

    fn color_in(self, palette: Palette) -> Color {
        match (self, palette) {
            (Role::Accent, Palette::Brand) => {
                Color::Rgb(BRAND_ACCENT.0, BRAND_ACCENT.1, BRAND_ACCENT.2)
            }
            (Role::Accent, Palette::Ansi) => Color::Magenta,
            (Role::Ok, _) => Color::Green,
            (Role::Warn, _) => Color::Yellow,
            (Role::Error, _) => Color::Red,
            (Role::Info, _) => Color::Cyan,
        }
    }

    /// The role for a line printed through `console`, outside the views:
    /// the error and its hint, the answer a menu leaves behind.
    pub fn console(self) -> console::Color {
        self.console_in(palette())
    }

    fn console_in(self, palette: Palette) -> console::Color {
        match (self, palette) {
            (Role::Accent, Palette::Brand) => {
                console::Color::TrueColor(BRAND_ACCENT.0, BRAND_ACCENT.1, BRAND_ACCENT.2)
            }
            (Role::Accent, Palette::Ansi) => console::Color::Magenta,
            (Role::Ok, _) => console::Color::Green,
            (Role::Warn, _) => console::Color::Yellow,
            (Role::Error, _) => console::Color::Red,
            (Role::Info, _) => console::Color::Cyan,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: [Role; 5] = [Role::Accent, Role::Ok, Role::Warn, Role::Error, Role::Info];

    /// With nothing asked for, every role is one of the terminal's sixteen,
    /// in a view and in a printed line alike.
    #[test]
    fn every_role_is_an_ansi_color_unless_the_brand_theme_was_asked_for() {
        for role in ROLES {
            assert!(
                matches!(
                    role.color_in(Palette::Ansi),
                    Color::Magenta | Color::Green | Color::Yellow | Color::Red | Color::Cyan
                ),
                "{role:?}"
            );
            assert!(
                !matches!(
                    role.console_in(Palette::Ansi),
                    console::Color::TrueColor(..) | console::Color::Color256(_)
                ),
                "{role:?}"
            );
        }
        assert_eq!(Role::Accent.color_in(Palette::Ansi), Color::Magenta);
    }

    /// The brand theme changes the accent and only the accent: the states
    /// stay the terminal's own.
    #[test]
    fn the_brand_theme_draws_the_accent_alone_in_the_sites_pink() {
        assert_eq!(
            Role::Accent.color_in(Palette::Brand),
            Color::Rgb(0xf4, 0x72, 0xb6)
        );
        assert_eq!(
            Role::Accent.console_in(Palette::Brand),
            console::Color::TrueColor(0xf4, 0x72, 0xb6)
        );
        for role in [Role::Ok, Role::Warn, Role::Error, Role::Info] {
            assert_eq!(
                role.color_in(Palette::Brand),
                role.color_in(Palette::Ansi),
                "{role:?}"
            );
        }
    }

    /// `brand` is honored only where the terminal says it shows 24-bit
    /// color; asked for anywhere else, or not asked for, the accent is the
    /// terminal's magenta.
    #[test]
    fn the_brand_accent_needs_a_terminal_that_says_truecolor() {
        assert_eq!(palette_of(Some("brand"), Some("truecolor")), Palette::Brand);
        assert_eq!(palette_of(Some("BRAND"), Some("24bit")), Palette::Brand);
        assert_eq!(palette_of(Some("brand"), None), Palette::Ansi);
        assert_eq!(palette_of(Some("brand"), Some("")), Palette::Ansi);
        assert_eq!(palette_of(Some("brand"), Some("256color")), Palette::Ansi);
        assert_eq!(palette_of(None, Some("truecolor")), Palette::Ansi);
        assert_eq!(palette_of(Some("dark"), Some("truecolor")), Palette::Ansi);
    }

    /// A printed line in the brand accent carries the 24-bit sequence for
    /// that pink, which is what `console` has to write for it.
    #[test]
    fn a_printed_line_in_the_brand_accent_is_that_pink() {
        let styled = console::Style::new()
            .fg(Role::Accent.console_in(Palette::Brand))
            .force_styling(true)
            .apply_to("?")
            .to_string();
        assert!(styled.contains("38;2;244;114;182"), "{styled:?}");
    }
}
