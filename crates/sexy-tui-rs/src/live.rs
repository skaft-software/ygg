//! Stable retained live regions for mutable status/output nodes.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::rich_text::diff::{DiffRenderOptions, UnifiedDiff};
use crate::rich_text::render::{RenderedLine, RichRenderer};
use crate::rich_text::{Block, Document, Inline};
use crate::tui::Component;

/// Stable application-selected identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

/// A generation-safe update token. Handles from removed/replaced nodes cannot
/// mutate a later node with the same [`NodeId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeHandle {
    pub id: NodeId,
    generation: u64,
}

impl NodeHandle {
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Semantic content accepted by a live node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveContent {
    Plain(String),
    Rich(Document),
    Diff(UnifiedDiff, DiffRenderOptions),
}

impl LiveContent {
    pub fn plain_text(&self) -> String {
        match self {
            Self::Plain(text) => text.clone(),
            Self::Rich(document) => document.plain_text(),
            Self::Diff(diff, _) => diff.plain_text(),
        }
    }
}

impl From<String> for LiveContent {
    fn from(value: String) -> Self {
        Self::Plain(value)
    }
}

impl From<&str> for LiveContent {
    fn from(value: &str) -> Self {
        Self::Plain(value.to_owned())
    }
}

impl From<Document> for LiveContent {
    fn from(value: Document) -> Self {
        Self::Rich(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    Active,
    Committed,
}

/// Deterministically ordered update from an event producer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderUpdate {
    pub sequence: u64,
    pub handle: NodeHandle,
    pub content: LiveContent,
}

/// Chronological fallback event for noninteractive/log frontends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlainEvent {
    pub sequence: u64,
    pub id: NodeId,
    pub kind: PlainEventKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlainEventKind {
    Inserted,
    Updated,
    Committed,
    Removed,
}

struct Entry {
    handle: NodeHandle,
    state: NodeState,
    revision: u64,
    content: LiveContent,
}

#[derive(Clone, Debug, Default)]
struct CachedEntry {
    revision: u64,
    lines: Vec<RenderedLine>,
}

#[derive(Clone, Debug, Default)]
struct RegionCache {
    width: Option<u16>,
    theme_revision: u64,
    entries: HashMap<NodeHandle, CachedEntry>,
}

/// A retained vertical region with stable identity and generation-safe updates.
/// It remains single-owner and lock-free; applications serialize producer
/// events before calling [`apply`](Self::apply).
pub struct LiveRegion {
    renderer: RichRenderer,
    entries: Vec<Entry>,
    generations: HashMap<NodeId, u64>,
    next_id: u64,
    next_sequence: u64,
    last_applied_sequence: u64,
    plain_events: Vec<PlainEvent>,
    cache: RefCell<RegionCache>,
}

impl LiveRegion {
    pub fn new(renderer: RichRenderer) -> Self {
        Self {
            renderer,
            entries: Vec::new(),
            generations: HashMap::new(),
            next_id: 1,
            next_sequence: 1,
            last_applied_sequence: 0,
            plain_events: Vec::new(),
            cache: RefCell::new(RegionCache::default()),
        }
    }

    pub fn renderer(&self) -> &RichRenderer {
        &self.renderer
    }

    pub fn renderer_mut(&mut self) -> &mut RichRenderer {
        self.cache.get_mut().width = None;
        &mut self.renderer
    }

    /// Insert with a generated ID.
    pub fn insert(&mut self, content: impl Into<LiveContent>) -> NodeHandle {
        let id = NodeId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        self.insert_with_id(id, content)
    }

    /// Insert or replace an externally named node. Reuse increments the
    /// generation, invalidating every older handle.
    pub fn insert_with_id(&mut self, id: NodeId, content: impl Into<LiveContent>) -> NodeHandle {
        self.next_id = self.next_id.max(id.0.saturating_add(1));
        if let Some(position) = self.entries.iter().position(|entry| entry.handle.id == id) {
            let old = self.entries.remove(position);
            self.cache.get_mut().entries.remove(&old.handle);
        }
        let generation = self
            .generations
            .entry(id)
            .and_modify(|generation| *generation = generation.saturating_add(1))
            .or_insert(1);
        let handle = NodeHandle {
            id,
            generation: *generation,
        };
        let content = content.into();
        let plain = self
            .renderer
            .capabilities()
            .plain
            .then(|| content.plain_text());
        self.entries.push(Entry {
            handle,
            state: NodeState::Active,
            revision: 1,
            content,
        });
        if let Some(plain) = plain {
            self.record(handle, PlainEventKind::Inserted, plain);
        }
        handle
    }

    /// Update an active node. Returns false for stale handles or committed
    /// nodes.
    pub fn update(&mut self, handle: NodeHandle, content: impl Into<LiveContent>) -> bool {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.handle == handle && entry.state == NodeState::Active)
        else {
            return false;
        };
        let content = content.into();
        let plain = self
            .renderer
            .capabilities()
            .plain
            .then(|| content.plain_text());
        entry.content = content;
        entry.revision = entry.revision.saturating_add(1);
        if let Some(plain) = plain {
            self.record(handle, PlainEventKind::Updated, plain);
        }
        true
    }

    /// Apply a globally ordered update. Duplicate or out-of-order sequences
    /// are ignored, making concurrent producer arbitration deterministic.
    pub fn apply(&mut self, update: RenderUpdate) -> bool {
        if update.sequence <= self.last_applied_sequence {
            return false;
        }
        self.last_applied_sequence = update.sequence;
        self.next_sequence = self.next_sequence.max(update.sequence.saturating_add(1));
        self.update(update.handle, update.content)
    }

    /// Commit an active node to retained history.
    pub fn commit(&mut self, handle: NodeHandle) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.handle == handle) else {
            return false;
        };
        if entry.state == NodeState::Committed {
            return false;
        }
        entry.state = NodeState::Committed;
        entry.revision = entry.revision.saturating_add(1);
        let plain = self
            .renderer
            .capabilities()
            .plain
            .then(|| entry.content.plain_text());
        if let Some(plain) = plain {
            self.record(handle, PlainEventKind::Committed, plain);
        }
        true
    }

    /// Remove a node. Later stale updates using its handle are ignored.
    pub fn remove(&mut self, handle: NodeHandle) -> bool {
        let Some(position) = self.entries.iter().position(|entry| entry.handle == handle) else {
            return false;
        };
        let entry = self.entries.remove(position);
        self.cache.get_mut().entries.remove(&handle);
        if self.renderer.capabilities().plain {
            self.record(handle, PlainEventKind::Removed, entry.content.plain_text());
        }
        true
    }

    pub fn state(&self, handle: NodeHandle) -> Option<NodeState> {
        self.entries
            .iter()
            .find(|entry| entry.handle == handle)
            .map(|entry| entry.state)
    }

    /// Drain chronological plain-mode events. A log frontend prints these in
    /// order instead of attempting cursor rewrites.
    pub fn drain_plain_events(&mut self) -> Vec<PlainEvent> {
        std::mem::take(&mut self.plain_events)
    }

    fn record(&mut self, handle: NodeHandle, kind: PlainEventKind, text: String) {
        let text = self.renderer.sanitize_copy(&text);
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        if kind == PlainEventKind::Updated {
            if let Some(previous) = self
                .plain_events
                .last_mut()
                .filter(|event| event.id == handle.id && event.kind == PlainEventKind::Updated)
            {
                previous.sequence = sequence;
                previous.text = text;
                return;
            }
        }
        self.plain_events.push(PlainEvent {
            sequence,
            id: handle.id,
            kind,
            text,
        });
    }

    fn rendered_lines(&self, width: u16) -> Vec<String> {
        let mut cache = self.cache.borrow_mut();
        let reflow =
            cache.width != Some(width) || cache.theme_revision != self.renderer.theme().revision();
        if reflow {
            cache.entries.clear();
            cache.width = Some(width);
            cache.theme_revision = self.renderer.theme().revision();
        }

        let mut output = Vec::new();
        for entry in &self.entries {
            let stale = cache
                .entries
                .get(&entry.handle)
                .is_none_or(|cached| cached.revision != entry.revision);
            if stale {
                let lines = match &entry.content {
                    LiveContent::Plain(text) => {
                        let document =
                            Document::new(vec![Block::Paragraph(vec![Inline::Text(text.clone())])]);
                        self.renderer.render(&document, width).lines
                    }
                    LiveContent::Rich(document) => self.renderer.render(document, width).lines,
                    LiveContent::Diff(diff, options) => {
                        self.renderer.render_diff(diff, width, *options).lines
                    }
                };
                cache.entries.insert(
                    entry.handle,
                    CachedEntry {
                        revision: entry.revision,
                        lines,
                    },
                );
            }
            if let Some(cached) = cache.entries.get(&entry.handle) {
                output.extend(cached.lines.iter().map(|line| line.styled.clone()));
            }
        }
        output
    }
}

impl Component for LiveRegion {
    fn render(&self, width: u16) -> Vec<String> {
        self.rendered_lines(width)
    }

    fn invalidate(&mut self) {
        self.cache.get_mut().width = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_update_commit_remove_and_stale_handles_are_safe() {
        let mut region = LiveRegion::new(RichRenderer::plain());
        let handle = region.insert_with_id(NodeId(7), "active");
        assert!(region.update(handle, "working"));
        assert!(region.commit(handle));
        assert!(!region.update(handle, "late"));
        assert!(region.remove(handle));
        assert!(!region.update(handle, "stale"));

        let replacement = region.insert_with_id(NodeId(7), "replacement");
        assert_ne!(replacement.generation(), handle.generation());
        assert!(!region.update(handle, "older generation"));
        assert!(region.update(replacement, "new generation"));
    }

    #[test]
    fn generated_ids_do_not_replace_explicit_ids_and_plain_updates_coalesce() {
        let mut region = LiveRegion::new(RichRenderer::plain());
        let explicit = region.insert_with_id(NodeId(1), "explicit");
        let generated = region.insert("generated");
        assert_ne!(explicit.id, generated.id);
        region.drain_plain_events();
        region.update(generated, "token 1");
        region.update(generated, "token 2\x1b]52;c;bad\x07");
        let events = region.drain_plain_events();
        assert_eq!(events.len(), 1);
        assert!(events[0].text.starts_with("token 2"));
        assert!(!events[0].text.contains('\x1b'));
        assert!(!events[0].text.contains('\x07'));
    }

    #[test]
    fn ordered_updates_reject_duplicates_and_reordering() {
        let mut region = LiveRegion::new(RichRenderer::plain());
        let handle = region.insert("zero");
        assert!(region.apply(RenderUpdate {
            sequence: 10,
            handle,
            content: "ten".into(),
        }));
        assert!(!region.apply(RenderUpdate {
            sequence: 9,
            handle,
            content: "nine".into(),
        }));
        assert!(!region.apply(RenderUpdate {
            sequence: 10,
            handle,
            content: "duplicate".into(),
        }));
    }

    #[test]
    fn plain_fallback_records_chronology_without_spinner_frame_append_rules() {
        let mut region = LiveRegion::new(RichRenderer::plain());
        let handle = region.insert("starting");
        region.update(handle, "running");
        region.commit(handle);
        let events = region.drain_plain_events();
        assert_eq!(events.len(), 3);
        assert!(events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence));
        assert_eq!(events.last().unwrap().kind, PlainEventKind::Committed);
    }
}
