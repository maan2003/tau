//! Theme definition and resolution.
//!
//! A [`Theme`] maps [`StyleName`]s to [`ThemeStyle`]s. Resolution
//! takes a [`ThemedText`] and produces [`ResolvedSpan`]s with
//! concrete style attributes.

use std::collections::HashMap;
use std::path::Path;
use std::{fmt, io};

use crate::color::Color;
use crate::text::{SpanTree, StyleIdx, StyleName, ThemedText};

/// Visual attributes for a style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct ThemeStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub underline: bool,
    pub italic: bool,
}

impl ThemeStyle {
    fn override_with(self, inner: Self) -> Self {
        Self {
            fg: inner.fg.or(self.fg),
            bg: inner.bg.or(self.bg),
            bold: self.bold || inner.bold,
            underline: self.underline || inner.underline,
            italic: self.italic || inner.italic,
        }
    }
}

/// A theme: a mapping from style names to visual attributes.
///
/// Styles not present in the map resolve to [`ThemeStyle::default()`]
/// (no formatting).
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct Theme {
    #[serde(default)]
    styles: HashMap<StyleName, ThemeStyle>,
}

const BUILTIN_DARK_THEME: &str = include_str!("../themes/tau.json5");
const BUILTIN_LIGHT_THEME: &str = include_str!("../themes/tau-light.json5");

impl Theme {
    /// Creates an empty theme (everything uses default styling).
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the built-in "tau" dark theme.
    ///
    /// This is the default theme used when no user theme is configured.
    pub fn builtin() -> Self {
        Self::builtin_dark()
    }

    /// Returns the built-in "tau" dark theme.
    pub fn builtin_dark() -> Self {
        // The embedded JSON5 is validated by tests; parsing cannot
        // fail at runtime.
        Self::parse(BUILTIN_DARK_THEME).expect("built-in dark theme is valid JSON5")
    }

    /// Returns the built-in "tau-light" theme.
    pub fn builtin_light() -> Self {
        // The embedded JSON5 is validated by tests; parsing cannot
        // fail at runtime.
        Self::parse(BUILTIN_LIGHT_THEME).expect("built-in light theme is valid JSON5")
    }

    /// Loads a theme from a JSON5 file.
    pub fn load(path: &Path) -> Result<Self, ThemeLoadError> {
        let contents =
            std::fs::read_to_string(path).map_err(|e| ThemeLoadError::Io(path.to_path_buf(), e))?;
        Self::parse(&contents).map_err(|e| ThemeLoadError::Parse(path.to_path_buf(), e))
    }

    /// Parses a theme from a JSON5 string.
    pub fn parse(s: &str) -> Result<Self, json5::Error> {
        json5::from_str(s)
    }

    /// Looks up the style for a name, falling back to the default.
    pub fn resolve_style(&self, name: &StyleName) -> ThemeStyle {
        self.styles.get(name).copied().unwrap_or_default()
    }

    /// Resolves a [`ThemedText`] into spans with concrete styles.
    pub fn resolve<'a>(&self, themed: &'a ThemedText) -> Vec<ResolvedSpan<'a>> {
        let mut out = Vec::new();
        let mut stack = Vec::new();
        self.resolve_tree(themed, themed.spans(), &mut stack, &mut out);
        out
    }

    fn resolve_tree<'a>(
        &self,
        themed: &'a ThemedText,
        span: &'a SpanTree<StyleIdx>,
        stack: &mut Vec<ThemeStyle>,
        out: &mut Vec<ResolvedSpan<'a>>,
    ) {
        match span {
            SpanTree::Text(text) => out.push(ResolvedSpan {
                text,
                style: effective_style(stack),
            }),
            SpanTree::Span { style, text } => {
                stack.push(
                    themed
                        .style_name(*style)
                        .map(|name| self.resolve_style(name))
                        .unwrap_or_default(),
                );
                for child in text {
                    self.resolve_tree(themed, child, stack, out);
                }
                stack.pop();
            }
        }
    }
}

fn effective_style(stack: &[ThemeStyle]) -> ThemeStyle {
    let mut effective = ThemeStyle::default();
    for style in stack {
        effective = effective.override_with(*style);
    }
    effective
}

/// A span of text with resolved style attributes.
pub struct ResolvedSpan<'a> {
    pub text: &'a str,
    pub style: ThemeStyle,
}

/// Errors that can occur when loading a theme file.
#[derive(Debug)]
pub enum ThemeLoadError {
    Io(std::path::PathBuf, io::Error),
    Parse(std::path::PathBuf, json5::Error),
}

impl fmt::Display for ThemeLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, err) => write!(f, "reading {}: {err}", path.display()),
            Self::Parse(path, err) => write!(f, "parsing {}: {err}", path.display()),
        }
    }
}

impl std::error::Error for ThemeLoadError {}

#[cfg(test)]
mod tests;
