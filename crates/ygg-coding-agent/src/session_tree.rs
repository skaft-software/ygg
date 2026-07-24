#![allow(missing_docs)]

use std::collections::{HashMap, HashSet};

use ygg_agent::{Entry, EntryValue, Session};

/// Render the durable entry forest in append order while making forks and the
/// selected branch visible. Session replay has already validated parent links,
/// so this formatter can stay presentation-only and never alter persistence.
pub(crate) fn render_session_tree(session: &Session) -> String {
    let entries = session.entries();
    let mut output = String::from("Session branch tree (* = active head, + = active branch):\n");
    if entries.is_empty() {
        output.push_str("  (empty session)");
        return output;
    }

    let by_id = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id.0.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut children = vec![Vec::new(); entries.len()];
    let mut roots = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if let Some(parent) = entry.parent.as_ref() {
            if let Some(parent) = by_id.get(parent.0.as_str()) {
                children[*parent].push(index);
                continue;
            }
        }
        roots.push(index);
    }

    let head = session.head_ref().map(|id| id.0.as_str());
    let active_branch = active_branch_indices(entries, &by_id, head);
    let mut stack = roots
        .iter()
        .enumerate()
        .rev()
        .map(|(position, index)| TreeFrame {
            index: *index,
            ancestor_has_next_sibling: Vec::new(),
            is_last: position + 1 == roots.len(),
        })
        .collect::<Vec<_>>();

    while let Some(frame) = stack.pop() {
        for has_next_sibling in &frame.ancestor_has_next_sibling {
            output.push_str(if *has_next_sibling { "│  " } else { "   " });
        }
        output.push_str(if frame.is_last { "└─" } else { "├─" });

        let entry = &entries[frame.index];
        let marker = if head == Some(entry.id.0.as_str()) {
            '*'
        } else if active_branch.contains(&frame.index) {
            '+'
        } else {
            ' '
        };
        output.push(marker);
        output.push(' ');
        output.push_str(&entry.id.0);
        output.push_str("  ");
        output.push_str(entry_kind(entry));
        output.push('\n');

        let mut next_ancestors = frame.ancestor_has_next_sibling;
        next_ancestors.push(!frame.is_last);
        let node_children = &children[frame.index];
        for (position, child) in node_children.iter().enumerate().rev() {
            stack.push(TreeFrame {
                index: *child,
                ancestor_has_next_sibling: next_ancestors.clone(),
                is_last: position + 1 == node_children.len(),
            });
        }
    }

    output.push_str("\nUse /checkout <entry-id> to select an earlier point and fork from it.");
    output
}

#[derive(Debug)]
struct TreeFrame {
    index: usize,
    ancestor_has_next_sibling: Vec<bool>,
    is_last: bool,
}

fn active_branch_indices(
    entries: &[Entry],
    by_id: &HashMap<&str, usize>,
    head: Option<&str>,
) -> HashSet<usize> {
    let mut active = HashSet::new();
    let mut cursor = head;
    while let Some(id) = cursor {
        let Some(index) = by_id.get(id).copied() else {
            break;
        };
        if !active.insert(index) {
            break;
        }
        cursor = entries[index]
            .parent
            .as_ref()
            .map(|parent| parent.0.as_str());
    }
    active
}

fn entry_kind(entry: &Entry) -> &'static str {
    match &entry.value {
        EntryValue::Message(ygg_ai::Message::User(_)) => "user",
        EntryValue::Message(ygg_ai::Message::Assistant(_)) => "assistant",
        EntryValue::Compaction { .. } => "compaction",
        EntryValue::Config { .. } => "config",
        EntryValue::PromptTemplateSelected { .. } => "prompt-template",
        EntryValue::SkillActivated { .. } => "skill-activated",
        EntryValue::SkillResourceRead { .. } => "skill-resource",
        EntryValue::SkillDeactivated { .. } => "skill-deactivated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(model: &str) -> EntryValue {
        EntryValue::Config {
            model: Some(model.to_owned()),
            reasoning: None,
            reasoning_mode: None,
        }
    }

    #[test]
    fn empty_tree_is_explicit() {
        let directory = tempfile::tempdir().unwrap();
        let session = Session::create(directory.path().join("empty.jsonl")).unwrap();

        assert_eq!(
            render_session_tree(&session),
            "Session branch tree (* = active head, + = active branch):\n  (empty session)"
        );
    }

    #[test]
    fn forks_are_nested_and_active_branch_is_marked() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("tree.jsonl")).unwrap();
        let root = session.append(config("root")).unwrap();
        let abandoned = session.append(config("abandoned")).unwrap();
        let abandoned_leaf = session.append(config("abandoned-leaf")).unwrap();
        session.checkout(root.clone()).unwrap();
        let selected = session.append(config("selected")).unwrap();
        let head = session.append(config("head")).unwrap();

        let tree = render_session_tree(&session);
        assert_eq!(
            tree,
            format!(
                concat!(
                    "Session branch tree (* = active head, + = active branch):\n",
                    "└─+ {}  config\n",
                    "   ├─  {}  config\n",
                    "   │  └─  {}  config\n",
                    "   └─+ {}  config\n",
                    "      └─* {}  config\n",
                    "\nUse /checkout <entry-id> to select an earlier point and fork from it."
                ),
                root.0, abandoned.0, abandoned_leaf.0, selected.0, head.0
            )
        );
        assert_eq!(render_session_tree(&session), tree);
    }

    #[test]
    fn checkout_marks_the_exact_durable_head_not_the_last_entry() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("checkout.jsonl")).unwrap();
        let root = session.append(config("root")).unwrap();
        let old_head = session.append(config("old-head")).unwrap();
        session.checkout(root.clone()).unwrap();

        let tree = render_session_tree(&session);
        assert!(tree.contains(&format!("└─* {}  config", root.0)), "{tree}");
        assert!(
            tree.contains(&format!("└─  {}  config", old_head.0)),
            "{tree}"
        );
        assert!(
            !tree.contains(&format!("* {}  config", old_head.0)),
            "{tree}"
        );
    }
}
