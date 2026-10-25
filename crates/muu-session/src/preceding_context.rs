use std::collections::VecDeque;

pub(crate) const MAX_PRECEDING_COMMIT_UNITS: usize = 8;
pub(crate) const MAX_PRECEDING_COMMIT_UTF8_BYTES: usize = 2 * 1024;

/// Exact commit-response units retained only for the lifetime of one Session.
///
/// These bounds are transport/resource guards, not the eventual model window.
/// Research decides later how (or whether) the units are serialized for a
/// context-aware model.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PrecedingCommitContext {
    units: VecDeque<String>,
    utf8_bytes: usize,
}

impl PrecedingCommitContext {
    // Fed every commit by `Session::handle`, and cleared by the
    // `clear_preceding_context` command Swift sends when the client changes.
    // A single commit over the byte bound clears everything: keeping older
    // units would leave a gap right before the next conversion.
    pub(crate) fn observe(&mut self, committed: &str) {
        if committed.is_empty() {
            return;
        }
        let committed_bytes = committed.len();
        if committed_bytes > MAX_PRECEDING_COMMIT_UTF8_BYTES {
            self.clear();
            return;
        }

        self.units.push_back(committed.to_owned());
        self.utf8_bytes += committed_bytes;
        while self.units.len() > MAX_PRECEDING_COMMIT_UNITS
            || self.utf8_bytes > MAX_PRECEDING_COMMIT_UTF8_BYTES
        {
            let evicted = self.units.pop_front().expect("non-empty context");
            self.utf8_bytes -= evicted.len();
        }
    }

    pub(crate) fn clear(&mut self) {
        self.units.clear();
        self.utf8_bytes = 0;
    }

    pub(crate) fn snapshot(&self) -> Vec<String> {
        self.units.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_exact_commit_units_oldest_to_newest() {
        let mut context = PrecedingCommitContext::default();
        context.observe("最初。");
        context.observe("次です");
        assert_eq!(context.snapshot(), ["最初。", "次です"]);
    }

    #[test]
    fn evicts_oldest_units_at_the_count_bound() {
        let mut context = PrecedingCommitContext::default();
        for index in 0..=MAX_PRECEDING_COMMIT_UNITS {
            context.observe(&format!("unit-{index}"));
        }
        assert_eq!(context.snapshot().len(), MAX_PRECEDING_COMMIT_UNITS);
        assert_eq!(
            context.snapshot().first().map(String::as_str),
            Some("unit-1")
        );
    }

    #[test]
    fn evicts_oldest_units_at_the_byte_bound() {
        let mut context = PrecedingCommitContext::default();
        let half = "x".repeat(MAX_PRECEDING_COMMIT_UTF8_BYTES / 2);
        context.observe("old");
        context.observe(&half);
        context.observe(&half);
        assert_eq!(context.snapshot(), [half.clone(), half]);
    }

    #[test]
    fn oversized_immediate_commit_clears_stale_history() {
        let mut context = PrecedingCommitContext::default();
        context.observe("old");
        context.observe(&"x".repeat(MAX_PRECEDING_COMMIT_UTF8_BYTES + 1));
        assert!(context.snapshot().is_empty());
    }

    #[test]
    fn clear_discards_all_units() {
        let mut context = PrecedingCommitContext::default();
        context.observe("old");
        context.clear();
        assert!(context.snapshot().is_empty());
    }
}
