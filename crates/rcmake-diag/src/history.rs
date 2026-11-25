use std::mem::size_of_val;

use crate::{Diagnostic, Effect, Location};

const MAX_ENTRIES: usize = 1024;
const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Default)]
pub(crate) struct History {
    diagnostics: Vec<Diagnostic>,
    entries: Vec<(Effect, usize)>,
    bytes: usize,
}

impl History {
    pub(crate) fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub(crate) fn push(&mut self, diagnostic: &Diagnostic, effect: Effect) {
        let cost = retention_cost(diagnostic);
        if cost > MAX_BYTES {
            return;
        }
        while self.entries.len() >= MAX_ENTRIES || self.bytes + cost > MAX_BYTES {
            let index = self
                .entries
                .iter()
                .position(|(effect, _)| *effect == Effect::None);
            let index = match index {
                Some(index) => index,
                None if effect != Effect::None => 0,
                None => return,
            };
            let (_, cost) = self.entries.remove(index);
            self.diagnostics.remove(index);
            self.bytes -= cost;
        }
        self.entries.push((effect, cost));
        self.diagnostics.push(diagnostic.clone());
        self.bytes += cost;
    }
}

fn retention_cost(diagnostic: &Diagnostic) -> usize {
    let mut cost = size_of_val(diagnostic).saturating_add(diagnostic.body.len());
    match &diagnostic.location {
        Location::None => {}
        Location::FileLine { file, .. } => cost = cost.saturating_add(file.len()),
        Location::Backtrace(trace) => {
            for frame in trace.frames() {
                cost = cost
                    .saturating_add(size_of_val(frame))
                    .saturating_add(3 * size_of_val(&0usize))
                    .saturating_add(frame.file.len())
                    .saturating_add(frame.name.as_ref().map_or(0, |name| name.len()));
            }
        }
    }
    if let Some(annotations) = diagnostic.annotations.as_deref() {
        cost = cost
            .saturating_add(size_of_val(annotations))
            .saturating_add(size_of_val(annotations.labels.as_slice()))
            .saturating_add(size_of_val(annotations.fixes.as_slice()))
            .saturating_add(size_of_val(annotations.notes.as_slice()));
        for label in &annotations.labels {
            cost = cost.saturating_add(label.message.len());
        }
        for fix in &annotations.fixes {
            cost = cost
                .saturating_add(fix.message.len())
                .saturating_add(size_of_val(fix.edits.as_slice()));
            for edit in &fix.edits {
                cost = cost.saturating_add(edit.replacement.len());
            }
        }
        for note in &annotations.notes {
            cost = cost.saturating_add(note.len());
        }
    }
    cost
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MessageKind, annotate::Annotations};

    fn warning() -> Diagnostic {
        Diagnostic {
            kind: MessageKind::Warning,
            effect: Effect::None,
            category: None,
            policy: None,
            location: Location::None,
            body: "warning".into(),
            annotations: None,
        }
    }

    #[test]
    fn entry_limit_keeps_errors_before_warnings() {
        let mut history = History::default();
        let error = Diagnostic {
            kind: MessageKind::Error,
            effect: Effect::SetError,
            body: "error".into(),
            ..warning()
        };
        history.push(&error, error.effect);
        for _ in 0..MAX_ENTRIES {
            let warning = warning();
            history.push(&warning, warning.effect);
        }

        assert_eq!(history.diagnostics().len(), MAX_ENTRIES);
        assert!(
            history
                .diagnostics()
                .first()
                .is_some_and(|item| item.body == "error")
        );
    }

    #[test]
    fn byte_limit_charges_annotation_content() {
        let mut history = History::default();
        let warning = Diagnostic {
            annotations: Some(Box::new(
                Annotations::default().with_note(vec![b'x'; MAX_BYTES]),
            )),
            ..warning()
        };
        history.push(&warning, warning.effect);
        assert!(history.diagnostics().is_empty());
    }
}
