//! Source labels, suggested edits, and notes for diagnostics.

#![forbid(unsafe_code)]

use rcmake_base::BString;

use crate::SourceId;

/// A range of bytes in one registered source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceSpan {
    /// Identifier of the source that contains the range.
    pub source: SourceId,
    /// Zero-based offset of the first byte.
    pub start: u32,
    /// Zero-based offset after the last byte.
    pub end: u32,
}

/// A marked range with an optional message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    /// Range to mark.
    pub span: SourceSpan,
    /// Text to show next to the marker. An empty message shows only the marker.
    pub message: BString,
    /// Whether this label marks a primary location.
    pub primary: bool,
}

/// One text replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// Range to replace. An empty range marks an insertion.
    pub span: SourceSpan,
    /// Bytes to insert. An empty replacement deletes the range.
    pub replacement: BString,
}

/// Whether a consumer can apply a suggested fix automatically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    /// A consumer can apply the fix automatically.
    MachineApplicable,
    /// The user must review the fix.
    MaybeIncorrect,
}

/// A suggested fix or advice on how to correct a problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fix {
    /// Text that describes the fix.
    pub message: BString,
    /// Edits to apply together. Ranges must not overlap. An empty list gives advice.
    pub edits: Vec<Edit>,
    /// Whether a consumer can apply the fix automatically.
    pub confidence: Confidence,
}

/// Extra information that diagnostic consumers can display.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Annotations {
    /// Source labels in the order supplied by the caller.
    pub labels: Vec<Label>,
    /// Suggested fixes in the order supplied by the caller.
    pub fixes: Vec<Fix>,
    /// Notes in the order supplied by the caller.
    pub notes: Vec<BString>,
}

impl Annotations {
    /// Start a set of annotations with one primary label.
    #[must_use]
    pub fn primary(span: SourceSpan, message: impl Into<BString>) -> Self {
        Self {
            labels: vec![Label {
                span,
                message: message.into(),
                primary: true,
            }],
            ..Self::default()
        }
    }

    /// Add a secondary label.
    #[must_use]
    pub fn with_secondary(mut self, span: SourceSpan, message: impl Into<BString>) -> Self {
        self.labels.push(Label {
            span,
            message: message.into(),
            primary: false,
        });
        self
    }

    /// Add a suggested fix.
    #[must_use]
    pub fn with_fix(mut self, fix: Fix) -> Self {
        self.fixes.push(fix);
        self
    }

    /// Add a note.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<BString>) -> Self {
        self.notes.push(note.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{SourceMap, source::SourceKind};

    use super::*;

    #[test]
    fn builders_preserve_labels_fixes_and_notes() -> Result<(), &'static str> {
        let mut sources = SourceMap::default();
        let source = sources
            .add(
                "CMakeLists.txt".into(),
                SourceKind::File,
                Arc::from(b"abc".as_slice()),
            )
            .ok_or("Cannot register source")?;
        let span = SourceSpan {
            source,
            start: 0,
            end: 3,
        };
        let fix = Fix {
            message: "Replace the name".into(),
            edits: vec![Edit {
                span,
                replacement: "def".into(),
            }],
            confidence: Confidence::MachineApplicable,
        };
        let annotations = Annotations::primary(span, "name")
            .with_secondary(span, "related name")
            .with_fix(fix.clone())
            .with_note("first")
            .with_note("second");

        assert_eq!(
            annotations.labels,
            [
                Label {
                    span,
                    message: "name".into(),
                    primary: true
                },
                Label {
                    span,
                    message: "related name".into(),
                    primary: false
                },
            ],
        );
        assert_eq!(annotations.fixes, [fix]);
        assert_eq!(
            annotations.notes,
            [BString::from("first"), BString::from("second")]
        );
        Ok(())
    }
}
