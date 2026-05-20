use tau_config::settings::CliTheme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalShade {
    Dark,
    Light,
}

pub(crate) fn select_theme(mode: CliTheme) -> tau_themes::Theme {
    match mode {
        CliTheme::Dark => tau_themes::Theme::builtin_dark(),
        CliTheme::Light => tau_themes::Theme::builtin_light(),
        CliTheme::Auto => match detect_terminal_shade() {
            Some(TerminalShade::Light) => tau_themes::Theme::builtin_light(),
            Some(TerminalShade::Dark) | None => tau_themes::Theme::builtin_dark(),
        },
    }
}

fn detect_terminal_shade() -> Option<TerminalShade> {
    colorfgbg_terminal_shade()
}

fn colorfgbg_terminal_shade() -> Option<TerminalShade> {
    let value = std::env::var("COLORFGBG").ok()?;
    colorfgbg_terminal_shade_from(&value)
}

fn colorfgbg_terminal_shade_from(value: &str) -> Option<TerminalShade> {
    let bg = value.rsplit([';', ':']).next()?.parse::<u8>().ok()?;
    match bg {
        7 | 15 => Some(TerminalShade::Light),
        _ => Some(TerminalShade::Dark),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorfgbg_detects_light_background() {
        assert_eq!(
            colorfgbg_terminal_shade_from("0;15"),
            Some(TerminalShade::Light)
        );
        assert_eq!(
            colorfgbg_terminal_shade_from("0;7"),
            Some(TerminalShade::Light)
        );
    }

    #[test]
    fn colorfgbg_detects_dark_background() {
        assert_eq!(
            colorfgbg_terminal_shade_from("15;0"),
            Some(TerminalShade::Dark)
        );
        assert_eq!(
            colorfgbg_terminal_shade_from("7;8"),
            Some(TerminalShade::Dark)
        );
    }

    #[test]
    fn colorfgbg_ignores_malformed_values() {
        assert_eq!(colorfgbg_terminal_shade_from(""), None);
        assert_eq!(colorfgbg_terminal_shade_from("0;wat"), None);
    }
}
