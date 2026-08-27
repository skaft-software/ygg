//! Bounded visual-row windows for compact command output.

/// Select a tail of already-wrapped physical rows. When either retained rows
/// or upstream capture loss must be disclosed, one physical row is reserved
/// for caller-rendered metadata. Short output stays short; `budget` is a hard
/// maximum, not a minimum height.
pub(super) fn bounded_tail_rows<F>(
    content: Vec<String>,
    budget: usize,
    force_metadata: bool,
    metadata: F,
) -> Vec<String>
where
    F: FnOnce(usize) -> String,
{
    if budget == 0 {
        return Vec::new();
    }

    let needs_metadata = force_metadata || content.len() > budget;
    let content_budget = budget.saturating_sub(usize::from(needs_metadata));
    let hidden_rows = content.len().saturating_sub(content_budget);
    let start = content.len().saturating_sub(content_budget);

    let mut rows = Vec::with_capacity(budget.min(content.len() + usize::from(needs_metadata)));
    if needs_metadata {
        rows.push(metadata(hidden_rows));
    }
    rows.extend(content.into_iter().skip(start));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_tail_reserves_metadata_inside_the_budget() {
        let rows = (1..=8).map(|row| format!("row {row}")).collect();
        let rendered = bounded_tail_rows(rows, 5, false, |hidden| format!("{hidden} hidden"));
        assert_eq!(rendered, ["4 hidden", "row 5", "row 6", "row 7", "row 8"]);
    }

    #[test]
    fn compact_tail_does_not_pad_short_output() {
        let rendered = bounded_tail_rows(vec!["one".into()], 5, false, |_| unreachable!());
        assert_eq!(rendered, ["one"]);
    }

    #[test]
    fn upstream_loss_forces_one_metadata_row_without_padding() {
        let rendered = bounded_tail_rows(vec!["one".into(), "two".into()], 5, true, |hidden| {
            format!("loss; {hidden} hidden")
        });
        assert_eq!(rendered, ["loss; 0 hidden", "one", "two"]);
    }
}
