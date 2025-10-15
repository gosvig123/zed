use editor::{Editor, EditorEvent, InlayId, display_map::Inlay};
use gpui::{Context, Entity, Render, Subscription, Task, Window};
use language::language_settings::all_language_settings;
use project::Project;
use std::time::Duration;
use ui::prelude::*;

use language::{ToOffset, ToPoint};

use workspace::{ItemHandle, StatusItemView, Workspace};

/// Adds inline reference-count hints next to symbols in the active editor and logs counts.
pub struct SymbolRefHints {
    pub enabled: bool,
    project: Entity<Project>,
    _observe_active_editor: Option<Subscription>,
    _observe_settings: Option<Subscription>,
    ongoing_task: Task<()>,
    refresh_rev: u64,
}

const HINT_BASE_ID: usize = 900_000_000; // avoid collisions with other inlays
const MAX_REMOVE: usize = 1024; // remove up to this many old hints each refresh

impl SymbolRefHints {
    pub fn new(workspace: &Workspace) -> Self {
        Self {
            enabled: false,
            project: workspace.project().clone(),
            _observe_active_editor: None,
            _observe_settings: None,
            ongoing_task: Task::ready(()),
            refresh_rev: 0,
        }
    }

    fn cancel_task(&mut self) {
        self.ongoing_task = Task::ready(());
    }

    fn removal_ids() -> Vec<InlayId> {
        (0..MAX_REMOVE)
            .map(|i| InlayId::SymbolRefHint(HINT_BASE_ID + i))
            .collect()
    }

    fn bump_and_clear(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        self.refresh_rev = self.refresh_rev.wrapping_add(1);
        editor.update(cx, |editor, cx| {
            editor.splice_inlays(&Self::removal_ids(), Vec::new(), cx)
        });
    }

    fn is_singleton(editor: &Entity<Editor>, cx: &mut Context<Self>) -> bool {
        editor.read_with(cx, |editor, app| {
            editor.buffer().read(app).as_singleton().is_some()
        })
    }

    fn inlays_enabled(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) -> bool {
        self.enabled && editor.read(cx).inlay_hints_enabled()
    }

    fn edit_debounce(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) -> Duration {
        editor.read_with(cx, |_editor, app| {
            let all_settings = all_language_settings(None, app);
            let settings = &all_settings.defaults.inlay_hints;
            Duration::from_millis(settings.edit_debounce_ms)
        })
    }

    fn flatten_document_symbols(
        mut doc_symbols: Vec<project::DocumentSymbol>,
    ) -> Vec<project::DocumentSymbol> {
        let mut flat_symbols: Vec<project::DocumentSymbol> = Vec::new();
        let mut stack: Vec<project::DocumentSymbol> = Vec::new();
        stack.append(&mut doc_symbols);
        while let Some(mut symbol) = stack.pop() {
            if !symbol.children.is_empty() {
                for child in symbol.children.iter().cloned() {
                    stack.push(child);
                }
            }
            symbol.children.clear();
            flat_symbols.push(symbol);
        }
        flat_symbols
    }

    fn on_symbols_changed(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
        event: &EditorEvent,
    ) {
        if let EditorEvent::InlayHintsToggled { enabled } = event {
            if !enabled {
                self.bump_and_clear(editor, cx);
                return;
            }
        }

        if !self.inlays_enabled(editor, cx) {
            self.bump_and_clear(editor, cx);
            return;
        }

        if !Self::is_singleton(editor, cx) {
            self.bump_and_clear(editor, cx);
            return;
        }

        let debounce = self.edit_debounce(editor, cx);
        self.refresh_symbol_ref_hints(editor, window, cx, debounce);
    }

    fn refresh_symbol_ref_hints(
        &mut self,
        editor: &Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
        debounce: Duration,
    ) {
        if !Self::is_singleton(editor, cx) {
            self.bump_and_clear(editor, cx);
            self.cancel_task();
            return;
        }

        let maybe_data = editor
            .read(cx)
            .active_excerpt(cx)
            .map(|(excerpt_id, buffer, _)| {
                let items = buffer.read(cx).snapshot().outline(None).items;
                (excerpt_id, buffer, items)
            });
        let Some((excerpt_id, buffer, items)) = maybe_data else {
            return;
        };
        let project = self.project.clone();
        let editor_handle = editor.clone();

        let rev = self.refresh_rev;
        self.ongoing_task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(debounce).await;

            let inlay_enabled = editor_handle
                .read_with(cx, |editor, _| editor.inlay_hints_enabled())
                .unwrap_or(false);
            let our_enabled = this.update(cx, |this, _| this.enabled).unwrap_or(true);
            if !(our_enabled && inlay_enabled) {
                return;
            }
            let invalidated = this
                .update(cx, |this, _| this.refresh_rev != rev)
                .unwrap_or(true);
            if invalidated {
                return;
            }

            let doc_symbols = if let Some(task) = project
                .update(cx, |project, cx| project.document_symbols(&buffer, cx))
                .ok()
            {
                (task.await).unwrap_or_default()
            } else {
                Vec::new()
            };

            let flat_symbols = Self::flatten_document_symbols(doc_symbols);

            let positions = editor_handle
                .read_with(cx, |_, app| {
                    let snapshot = buffer.read(app).snapshot();
                    items
                        .iter()
                        .map(|item| {
                            let item_offset = item.range.start.to_offset(&snapshot);
                            let mut best_symbol: Option<&project::DocumentSymbol> = None;
                            for symbol in &flat_symbols {
                                let range_start = symbol.range.start.to_offset(&snapshot);
                                let range_end = symbol.range.end.to_offset(&snapshot);
                                if range_start <= item_offset && item_offset < range_end {
                                    match &best_symbol {
                                        None => best_symbol = Some(symbol),
                                        Some(prev) => {
                                            let prev_span = prev.range.end.to_offset(&snapshot)
                                                - prev.range.start.to_offset(&snapshot);
                                            let this_span = range_end - range_start;
                                            if this_span <= prev_span {
                                                best_symbol = Some(symbol);
                                            }
                                        }
                                    }
                                }
                            }
                            match best_symbol {
                                Some(symbol) => symbol.selection_range.start.to_point(&snapshot),
                                None => item.range.start.to_point(&snapshot),
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            let mut counts: Vec<usize> = Vec::with_capacity(items.len());
            for position in &positions {
                let count = if let Some(task) = project
                    .update(cx, |project, cx| project.references(&buffer, *position, cx))
                    .ok()
                {
                    match task.await {
                        Ok(Some(locations)) => locations.len(),
                        Ok(None) => 0,
                        Err(_) => 0,
                    }
                } else {
                    0
                };
                counts.push(count);
            }

            let inlays = editor_handle
                .read_with(cx, |editor, app| {
                    let multi_buffer_snapshot = editor.buffer().read(app).snapshot(app);
                    items
                        .into_iter()
                        .enumerate()
                        .filter_map(|(i, item)| {
                            let position = multi_buffer_snapshot
                                .anchor_in_excerpt(excerpt_id, item.range.start)?;
                            let text = format!("{} ", counts[i]);
                            Some(Inlay::symbol_ref_hint(HINT_BASE_ID + i, position, text))
                        })
                        .collect::<Vec<Inlay>>()
                })
                .unwrap_or_default();

            let inlay_enabled = editor_handle
                .read_with(cx, |editor, _| editor.inlay_hints_enabled())
                .unwrap_or(false);
            let our_enabled = this.update(cx, |this, _| this.enabled).unwrap_or(true);
            if inlays.is_empty() || !(our_enabled && inlay_enabled) {
                return;
            }
            let invalidated = this
                .update(cx, |this, _| this.refresh_rev != rev)
                .unwrap_or(true);
            if invalidated {
                return;
            }

            let _ = editor_handle.update(cx, |editor, cx| {
                editor.splice_inlays(&Self::removal_ids(), inlays, cx)
            });
        });
    }
}

impl Render for SymbolRefHints {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w_0().invisible()
    }
}

impl StatusItemView for SymbolRefHints {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_task();
        if let Some(editor) = active_pane_item.and_then(|item| item.act_as::<Editor>(cx)) {
            self._observe_active_editor = Some(cx.subscribe_in(
                &editor,
                window,
                |this, editor, event: &EditorEvent, window, cx| match event {
                    EditorEvent::Reparsed(_)
                    | EditorEvent::ExcerptsEdited { .. }
                    | EditorEvent::Edited { .. }
                    | EditorEvent::BufferEdited
                    | EditorEvent::Saved
                    | EditorEvent::InlayHintsToggled { .. } => {
                        this.on_symbols_changed(&editor, window, cx, event);
                    }
                    _ => {}
                },
            ));

            let editor_for_settings = editor.clone();
            self._observe_settings = Some(cx.observe_global_in::<settings::SettingsStore>(
                window,
                move |this, window, cx| {
                    let our_enabled = this.enabled;
                    let inlay_enabled = editor_for_settings.read(cx).inlay_hints_enabled();
                    let is_singleton = editor_for_settings.read_with(cx, |editor, app| {
                        editor.buffer().read(app).as_singleton().is_some()
                    });
                    if !(our_enabled && inlay_enabled) || !is_singleton {
                        this.bump_and_clear(&editor_for_settings, cx);
                        this.cancel_task();
                    } else {
                        let debounce = this.edit_debounce(&editor_for_settings, cx);
                        this.refresh_symbol_ref_hints(&editor_for_settings, window, cx, debounce);
                    }
                },
            ));

            let debounce = self.edit_debounce(&editor, cx);
            self.refresh_symbol_ref_hints(&editor, window, cx, debounce);
        } else {
            self._observe_active_editor = None;
            self._observe_settings = None;
            self.cancel_task();
        }
        cx.notify();
    }
}
