use tau_config::settings::CliTheme;
use tau_themes::{SpanTree, ThemedText};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalShade {
    Dark,
    Light,
}

const THEME_ENV: &str = "TAU_THEME";

pub(crate) fn active_prompt_marker(
    theme: &tau_themes::Theme,
    prompt_symbol: &str,
    role: Option<&str>,
) -> tau_cli_term::StyledText {
    let mut text = ThemedText::new();
    let base_style = text.add_style(tau_themes::names::PROMPT_MARKER);
    let marker = format!("{prompt_symbol} ");

    let marker = if let Some(role) = role {
        let role_style = text.add_style(prompt_marker_role_style(role));
        SpanTree::span(
            base_style,
            vec![SpanTree::span(role_style, vec![SpanTree::text(marker)])],
        )
    } else {
        SpanTree::span(base_style, vec![SpanTree::text(marker)])
    };

    text.push_tree(marker);
    tau_cli_term::resolve::themed_text(theme, &text)
}

fn prompt_marker_role_style(role: &str) -> String {
    format!("{}.{}", tau_themes::names::PROMPT_MARKER, role)
}

pub(crate) fn select_theme(mode: CliTheme) -> tau_themes::Theme {
    let mode = env_theme_override().unwrap_or(mode);

    match mode {
        CliTheme::Dark => tau_themes::Theme::builtin_dark(),
        CliTheme::Light => tau_themes::Theme::builtin_light(),
        CliTheme::Auto => match detect_terminal_shade() {
            Some(TerminalShade::Light) => tau_themes::Theme::builtin_light(),
            Some(TerminalShade::Dark) | None => tau_themes::Theme::builtin_dark(),
        },
    }
}

fn env_theme_override() -> Option<CliTheme> {
    let value = std::env::var(THEME_ENV).ok()?;
    parse_theme_name(&value)
}

fn parse_theme_name(value: &str) -> Option<CliTheme> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(CliTheme::Auto),
        "dark" => Some(CliTheme::Dark),
        "light" => Some(CliTheme::Light),
        _ => None,
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
    fn parses_theme_env_values() {
        assert_eq!(parse_theme_name("auto"), Some(CliTheme::Auto));
        assert_eq!(parse_theme_name("DARK"), Some(CliTheme::Dark));
        assert_eq!(parse_theme_name(" light "), Some(CliTheme::Light));
        assert_eq!(parse_theme_name("solarized"), None);
    }

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
