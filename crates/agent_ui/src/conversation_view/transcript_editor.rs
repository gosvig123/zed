use std::{any::TypeId, ops::Range, sync::Arc};

use acp_thread::{AcpThread, AgentThreadEntry, AssistantMessageChunk};
use agent_client_protocol::schema::v1 as acp_v1;
use collections::{HashMap, HashSet};
use editor::{
    DisplayPoint, Editor, EditorEvent, EditorMode, FoldPlaceholder, MinimapVisibility, MultiBuffer,
    MultiBufferOffset, MultiBufferSnapshot, SelectionEffects, ToOffset as _,
    display_map::{
        BlockPlacement, BlockProperties, BlockStyle, Crease, CustomBlockId, DisplayRow,
        ToDisplayPoint as _,
    },
    scroll::Autoscroll,
};
use editor::HighlightKey;
use gpui::{
    App, Context, Entity, EntityId, Focusable as _, IntoElement, Render, Subscription, WeakEntity,
    Window, prelude::*, px,
};
use gpui::{
    FontStyle, FontWeight, HighlightStyle, Pixels, StrikethroughStyle, TextStyleRefinement,
    UnderlineStyle,
};
use language::LanguageRegistry;
use language::{Bias, Buffer, language_settings::SoftWrap};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownOptions, MarkdownStyle};
use search::{BufferSearchBar, buffer_search, buffer_search::Deploy as DeployBufferSearch};
use settings::CurrentLineHighlight;
use settings::Settings as _;
use text::SelectionGoal;
use theme::ActiveTheme as _;
use theme::ThemeColors;
use theme_settings::ThemeSettings;
use ui::{
    Button, ButtonCommon as _, ButtonStyle, Clickable as _, Color, Icon, IconButton, IconName,
    IconSize, Label, LabelCommon, LabelSize, h_flex, v_flex,
};
use util::ResultExt as _;
use workspace::{ToolbarItemView as _, item::ItemHandle};

use super::thread_view::{ActivityKind, ThreadView, activity_summary};
use super::transcript_comments::{
    CommentReply, CommentText, comments_message, extract_replies, split_user_message,
};
use crate::thread_metadata_store::{ThreadId, ThreadMetadataStore};

/// The `editor` thread layout: the whole transcript as one read-only text editor, so the
/// caret, selections, and every editor motion move across all messages.
pub struct TranscriptEditor {
    editor: Entity<Editor>,
    buffer: Entity<Buffer>,
    thread: Entity<AcpThread>,
    thread_view: WeakEntity<ThreadView>,
    /// This thread, to find the subthreads started from its comments.
    thread_id: ThreadId,
    segments: Vec<Segment>,
    /// The card, table, diagram, and turn-control blocks of each segment, aligned with
    /// `segments`.
    blocks: Vec<Vec<CustomBlockId>>,
    /// How each segment's Markdown shows, aligned with `segments`.
    markup: Vec<SegmentMarkup>,
    /// The composer, as the last block of the transcript.
    composer_block: Option<CustomBlockId>,
    /// Set while the transcript moves its own caret onto the last line, which would
    /// otherwise hand focus to the composer.
    moving_caret_to_end: bool,
    /// After a send with `cursor_after_send: response`, the number of segments at send time.
    /// The caret moves to the first reply that appears after a later user message.
    awaiting_response_after: Option<usize>,
    /// Comments written but not sent; they go with the next message.
    draft_comments: Vec<(CommentText, Range<editor::Anchor>)>,
    comment_input: Option<CommentInput>,
    comment_views: Vec<CommentView>,
    comment_blocks: HashSet<CustomBlockId>,
    /// Collapsed folds as applied, with the summary their placeholder shows.
    folds: HashMap<FoldKey, (Range<editor::Anchor>, String)>,
    /// Folds the user opened. They stay open while the transcript changes.
    expanded_folds: HashSet<FoldKey>,
    /// The thread reveals streamed text by appending to each message's Markdown without
    /// an entry event, so the transcript watches those entities itself.
    markdown_observations: HashMap<EntityId, Subscription>,
    /// The regular find bar, searching the transcript text.
    search_bar: Option<Entity<BufferSearchBar>>,
    /// The entry at the top of the view, for the message rail.
    top_entry_ix: usize,
    _subscriptions: Vec<Subscription>,
}

/// The text of one thread entry in the transcript buffer.
#[derive(Clone, PartialEq)]
struct Segment {
    text: String,
    kind: SegmentKind,
    /// Comments sent with a user message.
    comments: Vec<CommentText>,
    /// Replies to comments, taken out of agent prose.
    replies: Vec<CommentReply>,
}

impl Segment {
    fn new(text: String, kind: SegmentKind) -> Self {
        Self {
            text,
            kind,
            comments: Vec::new(),
            replies: Vec::new(),
        }
    }
}

#[derive(Clone, PartialEq)]
enum SegmentKind {
    UserMessage,
    /// Agent prose. Thinking at the start of the message is folded on its own. The last
    /// prose of a turn gets the turn's controls (copy, feedback) below it.
    Prose {
        thought_len: usize,
        turn_end: bool,
    },
    /// Work that folds into a summary line together with its neighbors.
    Activity {
        kind: ActivityKind,
        tool_call_id: Option<acp_v1::ToolCallId>,
    },
    /// An entry the user may need to act on, shown as its chat card. The text is a title
    /// line so copying across it still says what happened.
    Card,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum FoldKey {
    /// A run of activity, keyed by its first tool call so the key survives rebuilds.
    Activity(acp_v1::ToolCallId),
    /// A run of thinking with no tool call, or the thinking that opens an answer.
    Thought { entry_ix: usize },
}

struct PlannedFold {
    key: FoldKey,
    range: Range<usize>,
    summary: String,
}

const SEGMENT_SEPARATOR: &str = "\n\n";

impl TranscriptEditor {
    pub fn new(
        thread: Entity<AcpThread>,
        thread_view: WeakEntity<ThreadView>,
        thread_id: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The text always ends in an empty line: the composer sits above it, so it is never
        // inside a block that replaces text, and moving onto it hands focus to the composer.
        let buffer = cx.new(|cx| Buffer::local("\n", cx));
        let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(EditorMode::full(), multi_buffer, None, window, cx);
            editor.set_read_only(true);
            // Disabled input makes the editor report typed text, which goes to the composer.
            editor.set_input_enabled(false);
            editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
            editor.set_show_gutter(false, cx);
            editor.set_show_line_numbers(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_minimap_visibility(MinimapVisibility::Disabled, window, cx);
            editor.set_current_line_highlight(Some(CurrentLineHighlight::None));
            editor
        });

        // No Markdown grammar: it colors prose like code. The transcript styles prose itself.
        let mut this = Self {
            editor,
            buffer,
            thread,
            thread_view,
            thread_id,
            segments: Vec::new(),
            blocks: Vec::new(),
            markup: Vec::new(),
            composer_block: None,
            moving_caret_to_end: false,
            awaiting_response_after: None,
            draft_comments: Vec::new(),
            comment_input: None,
            comment_views: Vec::new(),
            comment_blocks: HashSet::default(),
            folds: HashMap::default(),
            expanded_folds: HashSet::default(),
            markdown_observations: HashMap::default(),
            search_bar: None,
            top_entry_ix: 0,
            _subscriptions: Vec::new(),
        };
        let editor_events = cx.subscribe_in(&this.editor, window, |this, _, event, window, cx| {
            match event {
                // Typing in the read-only transcript continues the next message in the composer.
                EditorEvent::InputIgnored { text } => {
                    let text = text.clone();
                    this.thread_view
                        .update(cx, |thread_view, cx| {
                            thread_view.focus_composer(Some(&text), window, cx)
                        })
                        .log_err();
                    this.scroll_to_composer(window, cx);
                }
                EditorEvent::SelectionsChanged { .. } => this.leave_through_last_line(window, cx),
                EditorEvent::ScrollPositionChanged { .. } => {
                    this.update_top_entry(cx);
                    // The gutter marks follow the text.
                    cx.notify();
                }
                _ => {}
            }
        });
        this._subscriptions.push(editor_events);
        // Subthreads and their titles live in the metadata store.
        if let Some(store) = ThreadMetadataStore::try_global(cx) {
            this._subscriptions
                .push(cx.observe(&store, |this, _, cx| this.refresh_comments(cx)));
        }
        this.insert_composer_block(cx);
        this.sync(None, window, cx);
        this
    }

    fn insert_composer_block(&mut self, cx: &mut Context<Self>) {
        let thread_view = self.thread_view.clone();
        let block_ids = self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let end = snapshot.anchor_after(snapshot.len());
            editor.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Above(end),
                    height: Some(1),
                    style: BlockStyle::Flex,
                    render: Arc::new(move |block_cx| {
                        let max_width = block_cx.max_width;
                        let composer = thread_view
                            .update(&mut *block_cx.app, |view, cx| {
                                view.render_transcript_composer(cx)
                            })
                            .log_err();
                        gpui::div()
                            .w(max_width)
                            .children(composer)
                            .into_any_element()
                    }),
                    priority: 0,
                }],
                None,
                cx,
            )
        });
        self.composer_block = block_ids.into_iter().next();
    }

    /// The empty last line sits below the composer, so arriving on it means leaving the
    /// transcript for the composer.
    fn leave_through_last_line(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.moving_caret_to_end {
            return;
        }
        let arrived = self.editor.update(cx, |editor, cx| {
            if !editor.focus_handle(cx).is_focused(window) {
                return None;
            }
            let snapshot = editor.display_snapshot(cx);
            let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);
            (selection.is_empty() && selection.head() == snapshot.buffer_snapshot().len())
                .then_some(selection.goal)
        });
        let Some(goal) = arrived else {
            return;
        };
        // Moving down keeps the horizontal position as the selection goal; carry it over.
        let x = match goal {
            SelectionGoal::HorizontalPosition(x) => Some(px(x as f32)),
            _ => None,
        };
        self.thread_view
            .update(cx, |thread_view, cx| match x {
                Some(x) => thread_view.focus_composer_at_x(x, window, cx),
                None => thread_view.focus_composer(None, window, cx),
            })
            .log_err();
        self.scroll_to_composer(window, cx);
    }

    /// Focuses the last line of text with the caret at a horizontal position, arriving from
    /// the composer below.
    pub fn focus_last_text_line_at(
        &mut self,
        x: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let text_end = MultiBufferOffset(snapshot.buffer_snapshot().len().0.saturating_sub(1));
            let row = text_end.to_display_point(&snapshot).row();
            let details = editor.text_layout_details(window, cx);
            let column = snapshot.display_column_for_x(row, x, &details);
            let point = DisplayPoint::new(row, column);
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::fit()),
                window,
                cx,
                |selections| selections.select_display_ranges([point..point]),
            );
            window.focus(&editor.focus_handle(cx), cx);
        });
    }

    /// Keeps the composer in view, for example while it grows or an answer streams in.
    pub fn scroll_to_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.moving_caret_to_end = true;
        self.editor.update(cx, |editor, cx| {
            let end = editor.buffer().read(cx).snapshot(cx).len();
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::bottom()),
                window,
                cx,
                |selections| selections.select_ranges([end..end]),
            );
        });
        self.moving_caret_to_end = false;
    }

    fn caret_is_at_end(&self, cx: &mut Context<Self>) -> bool {
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);
            // The text before the empty last line ends one byte before the buffer.
            selection.is_empty() && selection.head().0 + 1 >= snapshot.buffer_snapshot().len().0
        })
    }

    /// Brings the buffer, blocks, and folds in line with the thread's entries. With
    /// `changed_entry`, only that entry's text is recomputed, which keeps streaming cheap.
    pub fn sync(
        &mut self,
        changed_entry: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries = self.thread.read(cx).entries();
        let segments = match changed_entry {
            Some(entry_ix) if entries.len() == self.segments.len() => {
                let mut segments = self.segments.clone();
                if let (Some(segment), Some(entry)) =
                    (segments.get_mut(entry_ix), entries.get(entry_ix))
                {
                    *segment = segment_for(entry, cx);
                }
                segments
            }
            Some(entry_ix)
                if entries.len() == self.segments.len() + 1 && entry_ix + 1 == entries.len() =>
            {
                let mut segments = self.segments.clone();
                segments.extend(entries.get(entry_ix).map(|entry| segment_for(entry, cx)));
                segments
            }
            _ => entries.iter().map(|entry| segment_for(entry, cx)).collect(),
        };
        let mut segments = segments;
        mark_turn_ends(&mut segments);
        if segments != self.segments {
            self.apply(segments, window, cx);
            self.place_caret_in_response(window, cx);
        }
        self.observe_markdown(changed_entry, window, cx);
    }

    pub fn move_caret_to_next_response(&mut self) {
        self.awaiting_response_after = Some(self.segments.len());
    }

    fn place_caret_in_response(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sent_after) = self.awaiting_response_after else {
            return;
        };
        let Some(user_message_ix) = (sent_after..self.segments.len())
            .find(|&segment_ix| self.segments[segment_ix].kind == SegmentKind::UserMessage)
        else {
            return;
        };
        let Some(response) = segment_offsets(&self.segments)
            .get(user_message_ix + 1)
            .cloned()
        else {
            return;
        };
        self.awaiting_response_after = None;
        self.editor.update(cx, |editor, cx| {
            let start = MultiBufferOffset(response.start);
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::top_relative(2.)),
                window,
                cx,
                |selections| selections.select_ranges([start..start]),
            );
            window.focus(&editor.focus_handle(cx), cx);
        });
    }

    /// Opens a comment input below the selected passage.
    pub fn start_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.comment_input.is_some() {
            return;
        }
        let Some((quote, anchors)) = self.selected_passage(cx) else {
            return;
        };
        let input = cx.new(|cx| {
            let mut input = Editor::auto_height(1, 6, window, cx);
            input.set_placeholder_text(
                "Comment on this passage. Enter saves, Esc cancels.",
                window,
                cx,
            );
            input
        });
        let transcript = cx.entity().downgrade();
        let block = self.editor.update(cx, |editor, cx| {
            let input = input.clone();
            editor.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Below(anchors.end),
                    height: Some(1),
                    style: BlockStyle::Flex,
                    render: Arc::new(move |block_cx| {
                        let confirm_transcript = transcript.clone();
                        let cancel_transcript = transcript.clone();
                        comment_frame(block_cx.max_width, block_cx.app)
                            .key_context("AgentTranscriptComment")
                            .on_action(move |_: &menu::Confirm, window, cx| {
                                confirm_transcript
                                    .update(cx, |transcript, cx| {
                                        transcript.save_comment(window, cx)
                                    })
                                    .log_err();
                            })
                            .on_action(move |_: &menu::Cancel, window, cx| {
                                cancel_transcript
                                    .update(cx, |transcript, cx| {
                                        transcript.close_comment_input(window, cx)
                                    })
                                    .log_err();
                            })
                            .child(input.clone())
                            .into_any_element()
                    }),
                    priority: 0,
                }],
                None,
                cx,
            )
        });
        let Some(block) = block.into_iter().next() else {
            return;
        };
        window.focus(&input.focus_handle(cx), cx);
        self.comment_input = Some(CommentInput {
            editor: input,
            quote,
            anchors,
            block,
        });
        cx.notify();
    }

    /// The selected text, when it can be commented on.
    fn selected_passage(&self, cx: &App) -> Option<(String, Range<editor::Anchor>)> {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let selection = editor.selections.newest_anchor();
        let range = selection.start.to_offset(&snapshot)..selection.end.to_offset(&snapshot);
        let quote: String = snapshot.text_for_range(range.clone()).collect();
        let quote = quote.trim();
        (!quote.is_empty() && !quote.contains(['<', '>'])).then(|| {
            (
                quote.to_string(),
                snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end),
            )
        })
    }

    fn save_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.comment_input.as_ref() else {
            return;
        };
        let text = input.editor.read(cx).text(cx).trim().to_string();
        if !text.is_empty() {
            let comment = CommentText {
                id: self.next_comment_id(),
                quote: input.quote.clone(),
                text,
            };
            self.draft_comments.push((comment, input.anchors.clone()));
        }
        self.close_comment_input(window, cx);
    }

    fn close_comment_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.comment_input.take() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks(HashSet::from_iter([input.block]), None, cx);
                window.focus(&editor.focus_handle(cx), cx);
            });
        }
        self.refresh_comments(cx);
        cx.notify();
    }

    fn next_comment_id(&self) -> u32 {
        self.segments
            .iter()
            .flat_map(|segment| segment.comments.iter().map(|comment| comment.id))
            .chain(self.draft_comments.iter().map(|(comment, _)| comment.id))
            .max()
            .map_or(1, |id| id + 1)
    }

    pub fn has_draft_comments(&self) -> bool {
        !self.draft_comments.is_empty()
    }

    /// The draft comments, formatted to send with the next message.
    pub fn take_draft_comments(&mut self, cx: &mut Context<Self>) -> Option<String> {
        if self.draft_comments.is_empty() {
            return None;
        }
        let comments: Vec<CommentText> = self
            .draft_comments
            .drain(..)
            .map(|(comment, _)| comment)
            .collect();
        self.refresh_comments(cx);
        Some(comments_message(&comments))
    }

    /// Starts a thread from a comment. A draft becomes the thread instead of being sent.
    fn start_subthread(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self
            .draft_comments
            .iter()
            .position(|(comment, _)| comment.id == id);
        let comment = match draft {
            Some(index) => Some(self.draft_comments.remove(index).0),
            None => self
                .segments
                .iter()
                .flat_map(|segment| &segment.comments)
                .find(|comment| comment.id == id)
                .cloned(),
        };
        let Some(comment) = comment else {
            return;
        };
        self.thread_view
            .update(cx, |thread_view, cx| {
                thread_view.start_subthread(comment.quote, comment.text, window, cx)
            })
            .log_err();
        self.refresh_comments(cx);
    }

    fn remove_draft_comment(&mut self, id: u32, cx: &mut Context<Self>) {
        self.draft_comments.retain(|(comment, _)| comment.id != id);
        self.refresh_comments(cx);
    }

    /// Where each comment shows: drafts at their passage, sent comments at the last match of
    /// their quote before the message that carried them, with the agent's reply if any.
    fn current_comment_views(&self, cx: &App) -> Vec<CommentView> {
        let text = self.buffer.read(cx).text();
        let offsets = segment_offsets(&self.segments);
        let mut views = Vec::new();
        for (segment_ix, segment) in self.segments.iter().enumerate() {
            if segment.comments.is_empty() {
                continue;
            }
            let Some(before) = offsets
                .get(segment_ix)
                .and_then(|range| text.get(..range.start))
            else {
                continue;
            };
            let replies: Vec<&CommentReply> = self.segments[segment_ix + 1..]
                .iter()
                .take_while(|later| later.kind != SegmentKind::UserMessage)
                .flat_map(|later| &later.replies)
                .collect();
            for comment in &segment.comments {
                let Some(start) = before.rfind(comment.quote.as_str()) else {
                    continue;
                };
                let state = match replies.iter().find(|reply| reply.id == comment.id) {
                    Some(reply) if reply.complete => CommentState::Answered(reply.text.clone()),
                    Some(reply) => CommentState::Replying(reply.text.clone()),
                    None => CommentState::Waiting,
                };
                views.push(CommentView {
                    id: comment.id,
                    range: start..start + comment.quote.len(),
                    text: comment.text.clone(),
                    state,
                });
            }
        }
        if let Some(store) = ThreadMetadataStore::try_global(cx) {
            let store = store.read(cx);
            for (thread_id, parent) in store.children_of(self.thread_id) {
                let Some(start) = text.rfind(parent.quote.as_str()) else {
                    continue;
                };
                let title = store
                    .entry(thread_id)
                    .map(|metadata| metadata.display_title().to_string())
                    .unwrap_or_default();
                views.push(CommentView {
                    id: 0,
                    range: start..start + parent.quote.len(),
                    text: title,
                    state: CommentState::Subthread(thread_id),
                });
            }
        }
        let snapshot = self.editor.read(cx).buffer().read(cx).snapshot(cx);
        for (comment, anchors) in &self.draft_comments {
            views.push(CommentView {
                id: comment.id,
                range: anchors.start.to_offset(&snapshot).0..anchors.end.to_offset(&snapshot).0,
                text: comment.text.clone(),
                state: CommentState::Draft,
            });
        }
        views
    }

    /// Highlights commented passages and shows each comment, with its reply, below them.
    fn refresh_comments(&mut self, cx: &mut Context<Self>) {
        let views = self.current_comment_views(cx);
        if views == self.comment_views {
            return;
        }
        self.comment_views = views.clone();
        let stale_blocks = std::mem::take(&mut self.comment_blocks);
        let transcript = cx.entity().downgrade();
        let thread_view = self.thread_view.clone();
        let highlight = HighlightStyle {
            background_color: Some(cx.theme().colors().text_accent.opacity(0.15)),
            ..Default::default()
        };
        let block_ids = self.editor.update(cx, |editor, cx| {
            editor.remove_blocks(stale_blocks, None, cx);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let ranges = views
                .iter()
                .map(|view| {
                    snapshot.anchor_after(MultiBufferOffset(view.range.start))
                        ..snapshot.anchor_before(MultiBufferOffset(view.range.end))
                })
                .collect();
            editor.highlight_text(
                HighlightKey::AgentTranscript(COMMENT_HIGHLIGHT),
                ranges,
                highlight,
                cx,
            );
            let blocks: Vec<_> = views
                .into_iter()
                .map(|view| {
                    let transcript = transcript.clone();
                    let thread_view = thread_view.clone();
                    BlockProperties {
                        placement: BlockPlacement::Below(
                            snapshot.anchor_before(MultiBufferOffset(view.range.end)),
                        ),
                        height: Some(1),
                        style: BlockStyle::Flex,
                        render: Arc::new(move |block_cx| {
                            render_comment(&view, &transcript, &thread_view, block_cx)
                        }),
                        priority: 0,
                    }
                })
                .collect();
            editor.insert_blocks(blocks, None, cx)
        });
        self.comment_blocks = block_ids.into_iter().collect();
        cx.notify();
    }

    /// The vertical position of the selection's end, for the floating comment button.
    fn comment_button_position(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Pixels> {
        if self.comment_input.is_some() || self.selected_passage(cx).is_none() {
            return None;
        }
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.snapshot(window, cx);
            let head = editor.selections.newest_anchor().head();
            editor
                .to_pixel_point(head, &snapshot, window, cx)
                .map(|position| position.y)
        })
    }

    /// The thread entry under the caret.
    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.focus_handle(cx)
    }

    pub fn caret_entry_ix(&self, cx: &App) -> Option<usize> {
        let editor = self.editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let caret = editor
            .selections
            .newest_anchor()
            .head()
            .to_offset(&snapshot)
            .0;
        segment_offsets(&self.segments)
            .iter()
            .rposition(|range| range.start <= caret)
    }

    pub fn top_entry_ix(&self) -> usize {
        self.top_entry_ix
    }

    fn update_top_entry(&mut self, cx: &mut Context<Self>) {
        let top_offset = self.editor.update(cx, |editor, cx| {
            let top_row = editor.scroll_position(cx).y.max(0.) as u32;
            let snapshot = editor.display_snapshot(cx);
            DisplayPoint::new(DisplayRow(top_row), 0)
                .to_offset(&snapshot, Bias::Left)
                .0
        });
        let top_entry_ix = segment_offsets(&self.segments)
            .iter()
            .rposition(|range| range.start <= top_offset)
            .unwrap_or(0);
        if top_entry_ix != self.top_entry_ix {
            self.top_entry_ix = top_entry_ix;
            // The rail lives in the thread view.
            self.thread_view.update(cx, |_, cx| cx.notify()).log_err();
        }
    }

    /// Puts the caret at the start of an entry and scrolls it to the top.
    pub fn scroll_to_entry(
        &mut self,
        entry_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(range) = segment_offsets(&self.segments).get(entry_ix).cloned() else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            let start = MultiBufferOffset(range.start);
            // One line of room keeps a message's header block in view.
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::top_relative(1.)),
                window,
                cx,
                |selections| selections.select_ranges([start..start]),
            );
        });
    }

    /// Opens the find bar on the transcript, or closes it when it is open.
    pub fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let search_bar = self
            .search_bar
            .get_or_insert_with(|| cx.new(|cx| BufferSearchBar::new(None, window, cx)))
            .clone();
        let editor = self.editor.clone();
        search_bar.update(cx, |search_bar, cx| {
            if search_bar.is_dismissed() {
                let item: &dyn ItemHandle = &editor;
                search_bar.set_active_pane_item(Some(item), window, cx);
                search_bar.deploy(&DeployBufferSearch::find(), None, window, cx);
            } else {
                search_bar.dismiss(&buffer_search::Dismiss, window, cx);
            }
        });
        cx.notify();
    }

    /// Focuses the transcript with the caret at its end, coming up from the composer.
    pub fn focus_end(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let end = MultiBufferOffset(
                editor
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .len()
                    .0
                    .saturating_sub(1),
            );
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::fit()),
                window,
                cx,
                |selections| selections.select_ranges([end..end]),
            );
            window.focus(&editor.focus_handle(cx), cx);
        });
    }

    fn observe_markdown(
        &mut self,
        changed_entry: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entries = self.thread.read(cx).entries();
        let entry_range = match changed_entry {
            Some(entry_ix) => entry_ix..entry_ix + 1,
            None => 0..entries.len(),
        };
        let mut to_observe = Vec::new();
        for entry_ix in entry_range {
            let Some(AgentThreadEntry::AssistantMessage(message)) = entries.get(entry_ix) else {
                continue;
            };
            for chunk in &message.chunks {
                let (AssistantMessageChunk::Message { block, .. }
                | AssistantMessageChunk::Thought { block, .. }) = chunk;
                for markdown in block.markdowns() {
                    if !self
                        .markdown_observations
                        .contains_key(&markdown.entity_id())
                    {
                        to_observe.push((entry_ix, markdown.clone()));
                    }
                }
            }
        }
        for (entry_ix, markdown) in to_observe {
            let observation = cx.observe_in(&markdown, window, move |this, _, window, cx| {
                this.sync(Some(entry_ix), window, cx);
            });
            self.markdown_observations
                .insert(markdown.entity_id(), observation);
        }
    }

    /// Edits only the text that changed, so anchors, the caret, and the scroll position
    /// survive streaming. Blocks of changed segments are recreated, and folds are redone
    /// only when their range or summary changed.
    fn apply(&mut self, segments: Vec<Segment>, window: &mut Window, cx: &mut Context<Self>) {
        let old_segments = std::mem::replace(&mut self.segments, segments);
        let old_offsets = segment_offsets(&old_segments);
        let old_len = old_offsets.last().map_or(0, |range| range.end);
        let common_len = old_segments.len().min(self.segments.len());

        let mut edits: Vec<(Range<usize>, String)> = Vec::new();
        let mut changed_segments = Vec::new();
        let mut changed_markup_segments = Vec::new();
        for (segment_ix, (old, new)) in old_segments.iter().zip(&self.segments).enumerate() {
            if old == new {
                continue;
            }
            changed_segments.push(segment_ix);
            if old.text != new.text {
                changed_markup_segments.push(segment_ix);
                let range = old_offsets[segment_ix].clone();
                match new.text.strip_prefix(old.text.as_str()) {
                    Some(suffix) => edits.push((range.end..range.end, suffix.to_string())),
                    None => edits.push((range, new.text.clone())),
                }
            }
        }
        if self.segments.len() < old_segments.len() {
            let start = common_len
                .checked_sub(1)
                .map_or(0, |last_kept| old_offsets[last_kept].end);
            edits.push((start..old_len, String::new()));
        }
        if self.segments.len() > old_segments.len() {
            changed_markup_segments.extend(old_segments.len()..self.segments.len());
            let mut appended = String::new();
            for (offset, segment) in self.segments[old_segments.len()..].iter().enumerate() {
                if !(old_segments.is_empty() && offset == 0) {
                    appended.push_str(SEGMENT_SEPARATOR);
                }
                appended.push_str(&segment.text);
            }
            // Streaming into the last segment and appending a new one insert at the same
            // offset; one edit keeps their order.
            match edits.last_mut() {
                Some((range, text)) if range.start == old_len && range.end == old_len => {
                    text.push_str(&appended)
                }
                _ => edits.push((old_len..old_len, appended)),
            }
        }

        let follow_tail = old_segments.is_empty() || self.caret_is_at_end(cx);

        self.buffer
            .update(cx, |buffer, cx| buffer.edit(edits, None, cx));

        let offsets = segment_offsets(&self.segments);
        self.markup
            .resize_with(self.segments.len(), SegmentMarkup::default);
        for &segment_ix in &changed_markup_segments {
            if let (Some(segment), Some(markup)) = (
                self.segments.get(segment_ix),
                self.markup.get_mut(segment_ix),
            ) {
                *markup = segment_markup(segment);
            }
        }

        let languages = self.thread.read(cx).project().read(cx).languages().clone();
        let mut stale_blocks: HashSet<CustomBlockId> = HashSet::default();
        let mut block_specs = Vec::new();
        for segment_ix in changed_segments
            .into_iter()
            .chain(common_len..old_segments.len().max(self.segments.len()))
        {
            if let Some(blocks) = self.blocks.get_mut(segment_ix) {
                stale_blocks.extend(blocks.drain(..));
            }
            block_specs.extend(block_specs_for(
                segment_ix,
                &self.segments,
                &self.markup,
                &offsets,
                &languages,
                cx,
            ));
        }
        self.blocks.resize_with(self.segments.len(), Vec::new);

        let planned_folds = planned_folds(&self.segments, &offsets);
        let thread_view = self.thread_view.clone();
        let transcript = cx.entity().downgrade();

        let (new_blocks, applied_folds) = self.editor.update(cx, |editor, cx| {
            editor.remove_blocks(stale_blocks, None, cx);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let block_owners: Vec<usize> = block_specs.iter().map(|spec| spec.entry_ix).collect();
            let blocks: Vec<_> = block_specs
                .into_iter()
                .map(|spec| block_properties(spec, &snapshot, thread_view.clone()))
                .collect();
            let new_blocks: Vec<(usize, CustomBlockId)> = block_owners
                .into_iter()
                .zip(editor.insert_blocks(blocks, None, cx))
                .collect();

            for &segment_ix in &changed_markup_segments {
                let (Some(markup), Some(range)) =
                    (self.markup.get(segment_ix), offsets.get(segment_ix))
                else {
                    continue;
                };
                // Widened by one so folds that an edit collapsed onto the edges go too.
                let clear_range = MultiBufferOffset(range.start.saturating_sub(1))
                    ..MultiBufferOffset((range.end + 1).min(snapshot.len().0));
                editor.remove_folds_with_type(
                    &[clear_range],
                    TypeId::of::<MarkupFold>(),
                    false,
                    cx,
                );
                let creases = markup
                    .hidden
                    .iter()
                    .map(|(hidden, replacement)| {
                        Crease::simple(
                            MultiBufferOffset(range.start + hidden.start)
                                ..MultiBufferOffset(range.start + hidden.end),
                            markup_placeholder(*replacement),
                        )
                    })
                    .collect();
                editor.fold_creases(creases, false, window, cx);
            }

            let mut applied_folds = Vec::new();
            for fold in planned_folds {
                if self.expanded_folds.contains(&fold.key) {
                    continue;
                }
                let current = self.folds.get(&fold.key).map(|(anchors, summary)| {
                    (
                        anchors.start.to_offset(&snapshot).0..anchors.end.to_offset(&snapshot).0,
                        summary.clone(),
                    )
                });
                if current.as_ref() == Some(&(fold.range.clone(), fold.summary.clone())) {
                    continue;
                }
                if let Some((anchors, _)) = self.folds.get(&fold.key) {
                    editor.remove_folds_with_type(
                        std::slice::from_ref(anchors),
                        TypeId::of::<TranscriptEditor>(),
                        false,
                        cx,
                    );
                }
                // Both ends bias left so text appended after the run stays outside it.
                let anchors = snapshot.anchor_before(MultiBufferOffset(fold.range.start))
                    ..snapshot.anchor_before(MultiBufferOffset(fold.range.end));
                let range = MultiBufferOffset(fold.range.start)..MultiBufferOffset(fold.range.end);
                applied_folds.push((fold.key.clone(), anchors, fold.summary.clone()));
                editor.fold_creases(
                    vec![Crease::simple(
                        range,
                        fold_placeholder(fold, transcript.clone()),
                    )],
                    false,
                    window,
                    cx,
                );
            }
            (new_blocks, applied_folds)
        });

        for (segment_ix, block_id) in new_blocks {
            if let Some(blocks) = self.blocks.get_mut(segment_ix) {
                blocks.push(block_id);
            }
        }
        for (key, anchors, summary) in applied_folds {
            self.folds.insert(key, (anchors, summary));
        }
        self.apply_prose_styles(&offsets, cx);
        self.refresh_comments(cx);
        if follow_tail {
            self.scroll_to_composer(window, cx);
        }
        cx.notify();
    }

    /// Highlights every styled range in the transcript, one highlight key per style.
    fn apply_prose_styles(&mut self, offsets: &[Range<usize>], cx: &mut Context<Self>) {
        let colors = cx.theme().colors();
        let styles = ProseStyle::ALL.map(|style| (style, style.highlight(colors)));
        let markup = &self.markup;
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            for (style, highlight) in styles {
                let ranges = markup
                    .iter()
                    .zip(offsets)
                    .flat_map(|(segment_markup, segment_range)| {
                        segment_markup
                            .styled
                            .iter()
                            .filter(move |(_, segment_style)| *segment_style == style)
                            .map(move |(range, _)| {
                                segment_range.start + range.start..segment_range.start + range.end
                            })
                    })
                    .map(|range| {
                        snapshot.anchor_after(MultiBufferOffset(range.start))
                            ..snapshot.anchor_before(MultiBufferOffset(range.end))
                    })
                    .collect();
                editor.highlight_text(
                    HighlightKey::AgentTranscript(style as usize),
                    ranges,
                    highlight,
                    cx,
                );
            }
        });
    }
}

/// The buffer range of each segment's text, without the separators.
fn segment_offsets(segments: &[Segment]) -> Vec<Range<usize>> {
    let mut start = 0;
    segments
        .iter()
        .map(|segment| {
            let range = start..start + segment.text.len();
            start = range.end + SEGMENT_SEPARATOR.len();
            range
        })
        .collect()
}

fn planned_folds(segments: &[Segment], offsets: &[Range<usize>]) -> Vec<PlannedFold> {
    let mut folds = Vec::new();
    let mut entry_ix = 0;
    while entry_ix < segments.len() {
        match &segments[entry_ix].kind {
            SegmentKind::Activity { .. } => {
                let run_start = entry_ix;
                let mut kinds = Vec::new();
                let mut first_tool_call = None;
                while let Some(SegmentKind::Activity { kind, tool_call_id }) =
                    segments.get(entry_ix).map(|segment| &segment.kind)
                {
                    kinds.push(*kind);
                    if first_tool_call.is_none() {
                        first_tool_call = tool_call_id.clone();
                    }
                    entry_ix += 1;
                }
                let key = match first_tool_call {
                    Some(tool_call_id) => FoldKey::Activity(tool_call_id),
                    None => FoldKey::Thought {
                        entry_ix: run_start,
                    },
                };
                folds.push(PlannedFold {
                    key,
                    range: offsets[run_start].start..offsets[entry_ix - 1].end,
                    summary: activity_summary(kinds),
                });
            }
            SegmentKind::Prose { thought_len, .. } if *thought_len > 0 => {
                let start = offsets[entry_ix].start;
                folds.push(PlannedFold {
                    key: FoldKey::Thought { entry_ix },
                    range: start..start + thought_len,
                    summary: activity_summary([ActivityKind::Thought]),
                });
                entry_ix += 1;
            }
            _ => entry_ix += 1,
        }
    }
    folds
}

fn mark_turn_ends(segments: &mut [Segment]) {
    for segment_ix in 0..segments.len() {
        let next_is_user_message = segments
            .get(segment_ix + 1)
            .is_none_or(|next| next.kind == SegmentKind::UserMessage);
        if let SegmentKind::Prose { turn_end, .. } = &mut segments[segment_ix].kind {
            *turn_end = next_is_user_message;
        }
    }
}

/// The blocks of a segment: a card in place of a card segment's title line, rendered
/// tables and diagrams in place of their source, and the turn controls below the last prose
/// of a turn.
fn block_specs_for(
    segment_ix: usize,
    segments: &[Segment],
    markup: &[SegmentMarkup],
    offsets: &[Range<usize>],
    languages: &Arc<LanguageRegistry>,
    cx: &mut App,
) -> Vec<BlockSpec> {
    let (Some(segment), Some(range)) = (segments.get(segment_ix), offsets.get(segment_ix)) else {
        return Vec::new();
    };
    let mut specs = Vec::new();
    if segment.kind == SegmentKind::Card {
        specs.push(BlockSpec {
            entry_ix: segment_ix,
            kind: BlockKind::Card,
            range: range.clone(),
        });
    }
    for rich in markup
        .get(segment_ix)
        .map(|markup| markup.rich.as_slice())
        .unwrap_or_default()
    {
        let Some(source) = segment.text.get(rich.clone()) else {
            continue;
        };
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                dedent_nested_block(&segment.text, rich.start, source).into(),
                Some(languages.clone()),
                None,
                MarkdownOptions {
                    render_mermaid_diagrams: true,
                    ..Default::default()
                },
                cx,
            )
        });
        specs.push(BlockSpec {
            entry_ix: segment_ix,
            kind: BlockKind::Rich(markdown),
            range: range.start + rich.start..range.start + rich.end,
        });
    }
    if let SegmentKind::Prose { turn_end: true, .. } = segment.kind {
        specs.push(BlockSpec {
            entry_ix: segment_ix,
            kind: BlockKind::TurnControls,
            range: range.clone(),
        });
    }
    specs
}

/// A block inside a list item starts at its list indent, and its later lines still carry
/// that indent, which would read as part of the code once the block is parsed alone.
fn dedent_nested_block(text: &str, start: usize, source: &str) -> String {
    let line_start = text
        .get(..start)
        .and_then(|before| before.rfind('\n'))
        .map_or(0, |newline| newline + 1);
    let indent = start.saturating_sub(line_start);
    let mut lines = source.split('\n');
    let mut dedented = lines.next().unwrap_or_default().to_string();
    for line in lines {
        let spaces = line.bytes().take(indent).take_while(|&byte| byte == b' ').count();
        dedented.push('\n');
        dedented.push_str(&line[spaces..]);
    }
    dedented
}

struct BlockSpec {
    entry_ix: usize,
    kind: BlockKind,
    range: Range<usize>,
}

enum BlockKind {
    Card,
    Rich(Entity<Markdown>),
    TurnControls,
}

fn block_properties(
    spec: BlockSpec,
    snapshot: &MultiBufferSnapshot,
    thread_view: WeakEntity<ThreadView>,
) -> BlockProperties<editor::Anchor> {
    let entry_ix = spec.entry_ix;
    let start = snapshot.anchor_after(MultiBufferOffset(spec.range.start));
    let end = snapshot.anchor_before(MultiBufferOffset(spec.range.end));
    let (placement, render): (_, editor::display_map::RenderBlock) = match spec.kind {
        BlockKind::Card => (
            BlockPlacement::Replace(start..=end),
            Arc::new(move |block_cx| {
                let max_width = block_cx.max_width;
                let card = thread_view
                    .update(&mut *block_cx.app, |view, cx| {
                        view.render_transcript_card(entry_ix, &*block_cx.window, cx)
                    })
                    .log_err();
                gpui::div().w(max_width).children(card).into_any_element()
            }),
        ),
        BlockKind::Rich(markdown) => (
            BlockPlacement::Replace(start..=end),
            Arc::new(move |block_cx| {
                let style =
                    MarkdownStyle::themed(MarkdownFont::Agent, block_cx.window, block_cx.app);
                gpui::div()
                    .w(block_cx.max_width)
                    .py_1()
                    .child(MarkdownElement::new(markdown.clone(), style))
                    .into_any_element()
            }),
        ),
        BlockKind::TurnControls => (
            BlockPlacement::Below(end),
            Arc::new(move |block_cx| {
                let max_width = block_cx.max_width;
                let controls = thread_view
                    .update(&mut *block_cx.app, |view, cx| {
                        view.render_transcript_turn_controls(entry_ix, cx)
                    })
                    .log_err()
                    .flatten();
                gpui::div()
                    .w(max_width)
                    .children(controls)
                    .into_any_element()
            }),
        ),
    };
    BlockProperties {
        placement,
        height: Some(1),
        style: BlockStyle::Flex,
        render,
        priority: 0,
    }
}

#[derive(Clone, Copy)]
enum GutterMark {
    User { entry_ix: usize },
    Agent,
    Composer,
}

impl TranscriptEditor {
    /// The vertical position of each gutter mark in view: the user's avatar beside each of
    /// their messages, and the agent's icon beside the start of each reply.
    fn gutter_marks(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<(Pixels, GutterMark)> {
        let offsets = segment_offsets(&self.segments);
        let mut wanted = Vec::new();
        let mut reply_starts_next = true;
        for (entry_ix, (segment, range)) in self.segments.iter().zip(&offsets).enumerate() {
            if segment.kind == SegmentKind::UserMessage {
                wanted.push((range.start, GutterMark::User { entry_ix }));
                reply_starts_next = true;
            } else if reply_starts_next {
                wanted.push((range.start, GutterMark::Agent));
                reply_starts_next = false;
            }
        }
        let viewport_height = window.viewport_size().height;
        let composer_block = self.composer_block;
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.snapshot(window, cx);
            let mut marks: Vec<(Pixels, GutterMark)> = wanted
                .into_iter()
                .filter_map(|(offset, mark)| {
                    let anchor = snapshot
                        .buffer_snapshot()
                        .anchor_after(MultiBufferOffset(offset));
                    let position = editor.to_pixel_point(anchor, &snapshot, window, cx)?;
                    (position.y < viewport_height).then_some((position.y, mark))
                })
                .collect();
            if let Some(row) = composer_block.and_then(|block| editor.row_for_block(block, cx))
                && let Some(position) =
                    editor.display_to_pixel_point(DisplayPoint::new(row, 0), &snapshot, window, cx)
                && position.y < viewport_height
            {
                marks.push((position.y, GutterMark::Composer));
            }
            marks
        })
    }
}

struct CommentInput {
    editor: Entity<Editor>,
    quote: String,
    anchors: Range<editor::Anchor>,
    block: CustomBlockId,
}

#[derive(Clone, PartialEq)]
struct CommentView {
    id: u32,
    /// The commented passage, as buffer offsets.
    range: Range<usize>,
    text: String,
    state: CommentState,
}

#[derive(Clone, PartialEq)]
enum CommentState {
    /// Saved, and sent with the next message.
    Draft,
    Waiting,
    Replying(String),
    Answered(String),
    /// A thread started from the comment; `text` is its title.
    Subthread(ThreadId),
}

/// Prose styles use keys from 0; commented passages use the next one.
const COMMENT_HIGHLIGHT: usize = ProseStyle::ALL.len();

fn comment_frame(max_width: Pixels, cx: &App) -> gpui::Div {
    gpui::div()
        .w(max_width - px(24.))
        .my_1()
        .ml_2()
        .pl_2()
        .py_1()
        .border_l_2()
        .border_color(cx.theme().colors().text_accent.opacity(0.6))
}

fn render_comment(
    view: &CommentView,
    transcript: &WeakEntity<TranscriptEditor>,
    thread_view: &WeakEntity<ThreadView>,
    block_cx: &mut editor::display_map::BlockContext,
) -> gpui::AnyElement {
    let id = view.id;
    let transcript = transcript.clone();
    if let CommentState::Subthread(thread_id) = view.state {
        let status = thread_view.upgrade().and_then(|thread_view| {
            thread_view
                .read(block_cx.app)
                .subthread_status(thread_id, block_cx.app)
        });
        let thread_view = thread_view.clone();
        return comment_frame(block_cx.max_width, block_cx.app)
            .child(
                h_flex()
                    .id(gpui::SharedString::from(format!(
                        "subthread-reference-{thread_id:?}"
                    )))
                    .gap_1p5()
                    .cursor_pointer()
                    .child(
                        Icon::new(IconName::Thread)
                            .size(IconSize::XSmall)
                            .color(Color::Accent),
                    )
                    .child(Label::new(view.text.clone()).size(LabelSize::Small))
                    .children(status.map(|status| {
                        Label::new(status)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                    }))
                    .child(
                        Icon::new(IconName::ArrowUpRight)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(move |_, window, cx| {
                        thread_view
                            .update(cx, |thread_view, cx| {
                                thread_view.open_subthread(thread_id, window, cx)
                            })
                            .log_err();
                    }),
            )
            .into_any_element();
    }
    let start_thread_transcript = transcript.clone();
    let start_thread = Button::new(("start-subthread", id as usize), "Start thread")
        .start_icon(Icon::new(IconName::Thread).size(IconSize::XSmall))
        .label_size(LabelSize::XSmall)
        .color(Color::Muted)
        .on_click(move |_, window, cx| {
            start_thread_transcript
                .update(cx, |transcript, cx| {
                    transcript.start_subthread(id, window, cx)
                })
                .log_err();
        });
    let agent_mark = thread_view.upgrade().map(|thread_view| {
        thread_view
            .read(block_cx.app)
            .render_transcript_agent_mark()
    });
    let reply = match &view.state {
        CommentState::Draft => h_flex()
            .gap_1()
            .child(
                Label::new("Sent with your next message")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                IconButton::new(("remove-draft-comment", id as usize), IconName::Close)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .on_click(move |_, _window, cx| {
                        transcript
                            .update(cx, |transcript, cx| transcript.remove_draft_comment(id, cx))
                            .log_err();
                    }),
            )
            .into_any_element(),
        CommentState::Waiting => Label::new("Waiting for a reply")
            .size(LabelSize::XSmall)
            .color(Color::Muted)
            .into_any_element(),
        CommentState::Subthread(_) => gpui::Empty.into_any_element(),
        CommentState::Replying(text) | CommentState::Answered(text) => h_flex()
            .items_start()
            .gap_1p5()
            .children(agent_mark)
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(text.clone()).size(LabelSize::Small)),
            )
            .into_any_element(),
    };
    comment_frame(block_cx.max_width, block_cx.app)
        .child(
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .items_start()
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::Chat)
                                .size(IconSize::XSmall)
                                .color(Color::Accent),
                        )
                        .child(
                            gpui::div()
                                .flex_1()
                                .min_w_0()
                                .child(Label::new(view.text.clone()).size(LabelSize::Small)),
                        ),
                )
                .child(reply)
                .child(start_thread),
        )
        .into_any_element()
}

/// Room left of the text for the avatar and agent icon.
const GUTTER_WIDTH: f32 = 28.;

impl Render for TranscriptEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Prose, not code: the same font as agent answers in the other layouts.
        let settings = ThemeSettings::get_global(cx);
        let line_height = settings.agent_buffer_font_size(cx) * 1.75;
        let text_style = TextStyleRefinement {
            font_family: Some(settings.agent_ui_font_family().clone()),
            font_fallbacks: settings.ui_font.fallbacks.clone(),
            font_features: Some(settings.ui_font.features.clone()),
            font_size: Some(settings.agent_ui_font_size(cx).into()),
            font_weight: Some(settings.ui_font.weight),
            line_height: Some(line_height.into()),
            ..Default::default()
        };
        self.editor.update(cx, |editor, _| {
            editor.set_text_style_refinement(text_style);
            // Vim turns input back on when its mode changes; typing here must keep reaching
            // the composer instead of a read-only buffer.
            editor.set_input_enabled(false);
        });

        let thread_view = self.thread_view.clone();
        let marks: Vec<_> = self
            .gutter_marks(window, cx)
            .into_iter()
            .filter_map(|(top, mark)| {
                let element = thread_view
                    .update(cx, |thread_view, cx| match mark {
                        GutterMark::User { entry_ix } => {
                            thread_view.render_transcript_user_mark(entry_ix, cx)
                        }
                        GutterMark::Agent => thread_view.render_transcript_agent_mark(),
                        GutterMark::Composer => thread_view.render_user_avatar(cx),
                    })
                    .log_err()?;
                Some(
                    gpui::div()
                        .absolute()
                        .left(px(4.))
                        .top(top)
                        .h(line_height)
                        .flex()
                        .items_center()
                        .child(element),
                )
            })
            .collect();

        let comment_button = self.comment_button_position(window, cx).map(|top| {
            gpui::div().absolute().right(px(22.)).top(top).child(
                Button::new("transcript-add-comment", "Comment")
                    .start_icon(Icon::new(IconName::Chat).size(IconSize::XSmall))
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Filled)
                    .on_click(cx.listener(|this, _, window, cx| this.start_comment(window, cx))),
            )
        });

        gpui::div()
            .key_context("AgentTranscript")
            .size_full()
            .on_action(
                cx.listener(|this, action: &crate::ContinueInComposer, window, cx| {
                    let focus_handle = this
                        .thread_view
                        .update(cx, |thread_view, cx| {
                            thread_view.focus_composer(None, window, cx);
                            thread_view.message_editor.focus_handle(cx)
                        })
                        .log_err();
                    this.scroll_to_composer(window, cx);
                    if let (Some(name), Some(focus_handle)) = (&action.action, focus_handle) {
                        crate::dispatch_named_action(name, &focus_handle, window, cx);
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::AddComment, window, cx| {
                    this.start_comment(window, cx)
                }),
            )
            // The editor passes `MoveDown` on only when the caret is already at the end.
            .on_action(
                cx.listener(|this, _: &zed_actions::editor::MoveDown, window, cx| {
                    this.thread_view
                        .update(cx, |thread_view, cx| {
                            thread_view.focus_composer(None, window, cx)
                        })
                        .log_err();
                    this.scroll_to_composer(window, cx);
                }),
            )
            .flex()
            .flex_col()
            .bg(cx.theme().colors().editor_background)
            .children(
                self.search_bar
                    .clone()
                    .filter(|search_bar| !search_bar.read(cx).is_dismissed())
                    .map(|search_bar| {
                        gpui::div()
                            .border_b_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(search_bar)
                    }),
            )
            .child(
                gpui::div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .pl(px(GUTTER_WIDTH))
                    // Leaves the right edge to the message rail, clear of the editor's scrollbar.
                    .pr(px(18.))
                    .child(self.editor.clone())
                    .children(marks)
                    .children(comment_button),
            )
    }
}

/// Tags the folds that hide Markdown syntax, so they can be replaced without touching the
/// activity folds.
struct MarkupFold;

fn markup_placeholder(replacement: Option<&'static str>) -> FoldPlaceholder {
    FoldPlaceholder {
        render: Arc::new(move |_fold_id, _range, _cx| match replacement {
            Some(glyph) => Label::new(glyph).color(Color::Muted).into_any_element(),
            None => gpui::Empty.into_any_element(),
        }),
        constrain_width: false,
        merge_adjacent: false,
        type_tag: Some(TypeId::of::<MarkupFold>()),
        collapsed_text: None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProseStyle {
    Strong,
    Emphasis,
    Strikethrough,
    Code,
    Heading,
    Link,
}

impl ProseStyle {
    const ALL: [Self; 6] = [
        Self::Strong,
        Self::Emphasis,
        Self::Strikethrough,
        Self::Code,
        Self::Heading,
        Self::Link,
    ];

    fn highlight(self, colors: &ThemeColors) -> HighlightStyle {
        match self {
            Self::Strong | Self::Heading => HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
            Self::Emphasis => HighlightStyle {
                font_style: Some(FontStyle::Italic),
                ..Default::default()
            },
            Self::Strikethrough => HighlightStyle {
                strikethrough: Some(StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(colors.text_muted),
                }),
                color: Some(colors.text_muted),
                ..Default::default()
            },
            Self::Code => HighlightStyle {
                background_color: Some(colors.editor_foreground.opacity(0.08)),
                ..Default::default()
            },
            Self::Link => HighlightStyle {
                color: Some(colors.text_accent),
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(colors.text_accent.opacity(0.5)),
                    wavy: false,
                }),
                ..Default::default()
            },
        }
    }
}

/// How a segment's Markdown shows, with ranges relative to the segment start.
#[derive(Default)]
struct SegmentMarkup {
    /// Markdown syntax to hide, each with the glyph shown in its place, if any.
    hidden: Vec<(Range<usize>, Option<&'static str>)>,
    styled: Vec<(Range<usize>, ProseStyle)>,
    /// Tables and code blocks, rendered as blocks in place of their source.
    rich: Vec<Range<usize>>,
}

fn segment_markup(segment: &Segment) -> SegmentMarkup {
    let prose_start = match segment.kind {
        SegmentKind::UserMessage => 0,
        // Thinking is folded as a whole, and fold ranges must not overlap.
        SegmentKind::Prose { thought_len, .. } if thought_len > 0 => {
            thought_len + SEGMENT_SEPARATOR.len()
        }
        SegmentKind::Prose { .. } => 0,
        SegmentKind::Activity { .. } | SegmentKind::Card => return SegmentMarkup::default(),
    };
    let Some(prose) = segment.text.get(prose_start..) else {
        return SegmentMarkup::default();
    };
    let shift = |range: Range<usize>| range.start + prose_start..range.end + prose_start;
    let markup = prose_markup(prose);
    SegmentMarkup {
        hidden: markup
            .hidden
            .into_iter()
            .map(|(range, glyph)| (shift(range), glyph))
            .collect(),
        styled: markup
            .styled
            .into_iter()
            .map(|(range, style)| (shift(range), style))
            .collect(),
        rich: markup.rich.into_iter().map(shift).collect(),
    }
}

/// How to show a passage of Markdown prose as text: which syntax to hide, which spans to
/// style, and which parts to render as blocks.
fn prose_markup(text: &str) -> SegmentMarkup {
    use pulldown_cmark::{CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};

    let bytes = text.as_bytes();
    let run_length = |start: usize, byte: u8| {
        bytes.get(start..).map_or(0, |rest| {
            rest.iter().take_while(|&&next| next == byte).count()
        })
    };
    let mut hidden = Vec::new();
    let mut styled = Vec::new();
    let mut hide_delimiters = |range: &Range<usize>, length: usize, style: Option<ProseStyle>| {
        if range.len() > length * 2 {
            hidden.push((range.start..range.start + length, None));
            hidden.push((range.end - length..range.end, None));
            if let Some(style) = style {
                styled.push((range.start + length..range.end - length, style));
            }
        }
    };
    // Open inline links: their start, and the end of their text so far.
    let mut open_links: Vec<(usize, usize)> = Vec::new();
    let mut extra_hidden = Vec::new();
    let mut extra_styled = Vec::new();
    let mut rich: Vec<Range<usize>> = Vec::new();

    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        // Everything inside a rendered table or diagram belongs to its block.
        if rich.last().is_some_and(|rich| range.start < rich.end) {
            continue;
        }
        if let Some((_, text_end)) = open_links.last_mut()
            && !matches!(event, Event::End(TagEnd::Link))
        {
            *text_end = (*text_end).max(range.end);
        }
        match event {
            Event::Start(Tag::Strong) => {
                let delimiter = bytes.get(range.start).copied().unwrap_or(b'*');
                hide_delimiters(
                    &range,
                    run_length(range.start, delimiter).min(2),
                    Some(ProseStyle::Strong),
                );
            }
            Event::Start(Tag::Emphasis) => hide_delimiters(&range, 1, Some(ProseStyle::Emphasis)),
            Event::Start(Tag::Strikethrough) => hide_delimiters(
                &range,
                run_length(range.start, b'~').min(2),
                Some(ProseStyle::Strikethrough),
            ),
            Event::Code(_) => hide_delimiters(
                &range,
                run_length(range.start, b'`'),
                Some(ProseStyle::Code),
            ),
            Event::Start(Tag::Heading { .. }) if bytes.get(range.start) == Some(&b'#') => {
                let hashes = run_length(range.start, b'#');
                let spaces = run_length(range.start + hashes, b' ');
                extra_hidden.push((range.start..range.start + hashes + spaces, None));
                let line_end = text[range.start..range.end]
                    .find('\n')
                    .map_or(range.end, |newline| range.start + newline);
                extra_styled.push((range.start + hashes + spaces..line_end, ProseStyle::Heading));
            }
            // Code needs the buffer font to keep its columns, and the editor has one font.
            Event::Start(Tag::Table(_) | Tag::CodeBlock(CodeBlockKind::Fenced(_))) => {
                rich.push(trim_trailing_newlines(text, range))
            }
            Event::Start(Tag::Item)
                if matches!(bytes.get(range.start), Some(b'-' | b'*' | b'+'))
                    && bytes.get(range.start + 1) == Some(&b' ') =>
            {
                extra_hidden.push((range.start..range.start + 1, Some("\u{2022}")));
            }
            Event::Start(Tag::Link {
                link_type: LinkType::Inline,
                ..
            }) => open_links.push((range.start, range.start + 1)),
            Event::Start(Tag::Link {
                link_type: LinkType::Autolink,
                ..
            }) => hide_delimiters(&range, 1, Some(ProseStyle::Link)),
            Event::End(TagEnd::Link) => {
                if let Some((start, text_end)) = open_links.pop()
                    && start == range.start
                    && text_end < range.end
                {
                    extra_hidden.push((start..start + 1, None));
                    extra_hidden.push((text_end..range.end, None));
                    extra_styled.push((start + 1..text_end, ProseStyle::Link));
                }
            }
            _ => {}
        }
    }
    hidden.extend(extra_hidden);
    hidden.sort_by_key(|(range, _)| range.start);
    hidden.dedup_by(|next, previous| next.0.start < previous.0.end);
    styled.extend(extra_styled);
    SegmentMarkup {
        hidden,
        styled,
        rich,
    }
}

fn trim_trailing_newlines(text: &str, range: Range<usize>) -> Range<usize> {
    let trimmed = text[range.clone()].trim_end_matches('\n').len();
    range.start..range.start + trimmed
}

fn fold_placeholder(
    fold: PlannedFold,
    transcript: WeakEntity<TranscriptEditor>,
) -> FoldPlaceholder {
    let PlannedFold { key, summary, .. } = fold;
    FoldPlaceholder {
        render: Arc::new(move |fold_id, fold_range, cx| {
            let transcript = transcript.clone();
            let key = key.clone();
            FoldPlaceholder::fold_element(fold_id, cx)
                .cursor_pointer()
                .child(
                    h_flex()
                        .gap_1()
                        .px_1()
                        .child(
                            Label::new(summary.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Icon::new(IconName::ChevronRight)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, _window, cx| {
                    transcript
                        .update(cx, |transcript, cx| {
                            transcript.expanded_folds.insert(key.clone());
                            transcript.folds.remove(&key);
                            transcript.editor.update(cx, |editor, cx| {
                                editor.remove_folds_with_type(
                                    &[fold_range.start..fold_range.end],
                                    TypeId::of::<TranscriptEditor>(),
                                    false,
                                    cx,
                                );
                            });
                            cx.stop_propagation();
                        })
                        .log_err();
                })
                .into_any_element()
        }),
        constrain_width: false,
        merge_adjacent: false,
        type_tag: Some(TypeId::of::<TranscriptEditor>()),
        collapsed_text: None,
    }
}

fn segment_for(entry: &AgentThreadEntry, cx: &App) -> Segment {
    if let Some(kind) = ActivityKind::of(entry, cx) {
        let (text, tool_call_id) = match entry {
            AgentThreadEntry::ToolCall(tool_call) => {
                let mut text = unescape_markdown(tool_call.label.read(cx).source().trim());
                for content in tool_call.content() {
                    let content = content.to_markdown(cx);
                    if !content.trim().is_empty() {
                        text.push_str(SEGMENT_SEPARATOR);
                        text.push_str(content.trim_end());
                    }
                }
                (text, Some(tool_call.id.clone()))
            }
            AgentThreadEntry::AssistantMessage(message) => {
                (thought_text(&message.chunks, cx), None)
            }
            _ => (String::new(), None),
        };
        return Segment::new(text, SegmentKind::Activity { kind, tool_call_id });
    }

    match entry {
        AgentThreadEntry::UserMessage(message) => {
            let markdown = message.content.to_markdown(cx);
            let (typed, comments) = split_user_message(&markdown);
            let text = match (typed.trim_end(), comments.len()) {
                ("", 1) => "Sent a comment.".to_string(),
                ("", count) => format!("Sent {count} comments."),
                (typed, _) => typed.to_string(),
            };
            Segment {
                comments,
                ..Segment::new(text, SegmentKind::UserMessage)
            }
        }
        AgentThreadEntry::AssistantMessage(message) => {
            let thoughts = thought_text(&message.chunks, cx);
            let prose = message
                .chunks
                .iter()
                .filter_map(|chunk| match chunk {
                    AssistantMessageChunk::Message { block, .. } => {
                        let markdown = block.to_markdown(cx);
                        (!markdown.trim().is_empty()).then(|| markdown.trim_end().to_string())
                    }
                    AssistantMessageChunk::Thought { .. } => None,
                })
                .collect::<Vec<_>>()
                .join(SEGMENT_SEPARATOR);
            let (prose, replies) = if prose.contains("<comment-reply") {
                extract_replies(&prose)
            } else {
                (prose, Vec::new())
            };
            let thought_len = thoughts.len();
            let text = if thoughts.is_empty() {
                prose
            } else {
                format!("{thoughts}{SEGMENT_SEPARATOR}{prose}")
            };
            Segment {
                replies,
                ..Segment::new(
                    text,
                    SegmentKind::Prose {
                        thought_len,
                        turn_end: false,
                    },
                )
            }
        }
        AgentThreadEntry::ToolCall(tool_call) => Segment::new(
            unescape_markdown(tool_call.label.read(cx).source().trim()),
            SegmentKind::Card,
        ),
        AgentThreadEntry::Elicitation(_) => {
            Segment::new("Question from the agent".to_string(), SegmentKind::Card)
        }
        AgentThreadEntry::ContextCompaction(_) => {
            Segment::new("Context compaction".to_string(), SegmentKind::Card)
        }
    }
}

/// Tool titles escape Markdown punctuation for rendering; the transcript shows them as text.
fn unescape_markdown(text: &str) -> String {
    let mut unescaped = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\'
            && let Some(next) = characters.peek()
            && next.is_ascii_punctuation()
        {
            continue;
        }
        unescaped.push(character);
    }
    unescaped
}

fn thought_text(chunks: &[AssistantMessageChunk], cx: &App) -> String {
    chunks
        .iter()
        .filter_map(|chunk| match chunk {
            AssistantMessageChunk::Thought { block, .. } => {
                let markdown = block.to_markdown(cx);
                (!markdown.trim().is_empty()).then(|| markdown.trim_end().to_string())
            }
            AssistantMessageChunk::Message { .. } => None,
        })
        .collect::<Vec<_>>()
        .join(SEGMENT_SEPARATOR)
}
