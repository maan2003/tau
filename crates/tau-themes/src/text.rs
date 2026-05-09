//! Themed text representation.
//!
//! [`ThemedText`] pairs style *names* with a tree of text spans. The
//! actual visual attributes are resolved later via a [`Theme`](crate::Theme).

use std::fmt;

/// A semantic style name (e.g. `"prompt"`, `"error"`, `"muted"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(transparent)]
pub struct StyleName(String);

impl StyleName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StyleName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for StyleName {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for StyleName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Index into [`ThemedText::styles`]. Values beyond the styles
/// array (including [`StyleIdx::DEFAULT`]) resolve to the default
/// (no formatting) style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StyleIdx(u16);

impl StyleIdx {
    /// Sentinel value that always resolves to the default style.
    pub const DEFAULT: Self = Self(u16::MAX);

    pub fn raw(self) -> u16 {
        self.0
    }
}

/// A tree of text and styled child spans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpanTree<S> {
    Text(String),
    Span { style: S, text: Vec<Self> },
}

impl<S> SpanTree<S> {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    pub fn span(style: S, text: Vec<Self>) -> Self {
        Self::Span { style, text }
    }
}

/// Backwards-compatible alias for a styled tree node.
pub type ThemedSpan = SpanTree<StyleIdx>;

/// Themed text: a list of style names plus spans that reference them
/// by index.
///
/// The indirection (`StyleIdx` → `StyleName`) avoids repeating
/// style-name strings in every span. The spans form a tree so inner
/// styles can refine outer styles.
#[derive(Clone, Debug)]
pub struct ThemedText {
    styles: Vec<StyleName>,
    spans: SpanTree<StyleIdx>,
}

impl Default for ThemedText {
    fn default() -> Self {
        Self {
            styles: Vec::new(),
            spans: SpanTree::Span {
                style: StyleIdx::DEFAULT,
                text: Vec::new(),
            },
        }
    }
}

impl ThemedText {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_spans(spans: SpanTree<StyleIdx>) -> Self {
        Self {
            styles: Vec::new(),
            spans,
        }
    }

    /// Registers a style name and returns its index.
    ///
    /// Duplicate names are allowed — each call allocates a new slot.
    pub fn add_style(&mut self, name: impl Into<StyleName>) -> StyleIdx {
        let idx = self.styles.len();
        self.styles.push(name.into());
        // Truncate to u16; styles beyond u16::MAX - 1 are
        // unreachable since DEFAULT is u16::MAX.
        StyleIdx(idx as u16)
    }

    /// Appends a span with the given style index at the root level.
    pub fn push(&mut self, idx: StyleIdx, text: impl Into<String>) {
        self.push_tree(SpanTree::Span {
            style: idx,
            text: vec![SpanTree::Text(text.into())],
        });
    }

    /// Appends a tree at the root level.
    pub fn push_tree(&mut self, span: SpanTree<StyleIdx>) {
        match &mut self.spans {
            SpanTree::Span { text, .. } => text.push(span),
            SpanTree::Text(_) => unreachable!("ThemedText root is always a span"),
        }
    }

    /// Appends a span with the default (no formatting) style.
    pub fn push_default(&mut self, text: impl Into<String>) {
        self.push(StyleIdx::DEFAULT, text);
    }

    /// Returns the registered style names.
    pub fn styles(&self) -> &[StyleName] {
        &self.styles
    }

    /// Returns the span tree.
    pub fn spans(&self) -> &SpanTree<StyleIdx> {
        &self.spans
    }

    /// Looks up the [`StyleName`] for a span's index, or `None` if
    /// the index is out of bounds (default style).
    pub fn style_name(&self, idx: StyleIdx) -> Option<&StyleName> {
        self.styles.get(idx.0 as usize)
    }
}
