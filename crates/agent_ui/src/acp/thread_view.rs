use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use agentic_coding_protocol::{self as acp};
use collections::HashSet;
use editor::{
    ContextMenuOptions, ContextMenuPlacement, Editor, EditorElement, EditorMode, EditorStyle,
    MinimapVisibility, MultiBuffer,
};
use gpui::{
    Animation, AnimationExt, App, BorderStyle, EdgesRefinement, Empty, Entity, Focusable, Hsla,
    ListState, SharedString, StyleRefinement, Subscription, TextStyle, TextStyleRefinement,
    Transformation, UnderlineStyle, Window, div, list, percentage, prelude::*, pulsating_between,
};
use gpui::{FocusHandle, Task};
use language::language_settings::SoftWrap;
use language::{Buffer, Language};
use markdown::{HeadingLevelStyles, Markdown, MarkdownElement, MarkdownStyle};
use project::Project;
use settings::Settings as _;
use theme::ThemeSettings;
use ui::{Disclosure, Tooltip, prelude::*};
use util::{ResultExt, paths};
use zed_actions::agent::Chat;

use ::acp::{
    AcpThread, AcpThreadEvent, AgentThreadEntryContent, AssistantMessage, AssistantMessageChunk,
    Diff, ThreadEntry, ThreadStatus, ToolCall, ToolCallConfirmation, ToolCallContent, ToolCallId,
    ToolCallStatus, UserMessageChunk,
};

use crate::message_editor::ContextCreasesAddon;

pub struct AcpThreadView {
    thread: Entity<AcpThread>,
    thread_state: ThreadState,
    // todo! reconsider structure. currently pretty sparse, but easy to clean up if we need to delete entries.
    thread_entry_views: Vec<Option<ThreadEntryView>>,
    message_editor: Entity<Editor>,
    last_error: Option<Entity<Markdown>>,
    list_state: ListState,
    auth_task: Option<Task<()>>,
    expanded_tool_calls: HashSet<ToolCallId>,
    expanded_thinking_blocks: HashSet<(usize, usize)>,
}

#[derive(Debug)]
enum ThreadEntryView {
    Diff { editor: Entity<Editor> },
}

enum ThreadState {
    Loading {
        _task: Task<()>,
    },
    Ready {
        thread: Entity<AcpThread>,
        _subscription: Subscription,
    },
    LoadError(SharedString),
    Unauthenticated,
}

impl AcpThreadView {
    pub fn new(project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let language = Language::new(
            language::LanguageConfig {
                completion_query_characters: HashSet::from_iter(['.', '-', '_', '@']),
                ..Default::default()
            },
            None,
        );

        let message_editor = cx.new(|cx| {
            let buffer = cx.new(|cx| Buffer::local("", cx).with_language(Arc::new(language), cx));
            let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));

            let mut editor = Editor::new(
                editor::EditorMode::AutoHeight {
                    min_lines: 4,
                    max_lines: None,
                },
                buffer,
                None,
                window,
                cx,
            );
            editor.set_placeholder_text("Message the agent - @ to include files", cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_soft_wrap();
            editor.set_use_modal_editing(true);
            editor.set_context_menu_options(ContextMenuOptions {
                min_entries_visible: 12,
                max_entries_visible: 12,
                placement: Some(ContextMenuPlacement::Above),
            });
            editor.register_addon(ContextCreasesAddon::new());
            editor
        });

        let list_state = ListState::new(
            0,
            gpui::ListAlignment::Bottom,
            px(2048.0),
            cx.processor({
                move |this: &mut Self, index: usize, window, cx| {
                    let Some((entry, len)) = this.thread().and_then(|thread| {
                        let entries = &thread.read(cx).entries();
                        Some((entries.get(index)?, entries.len()))
                    }) else {
                        return Empty.into_any();
                    };
                    this.render_entry(index, len, entry, window, cx)
                }
            }),
        );

        let root_dir = project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path())
            .unwrap_or_else(|| paths::home_dir().as_path().into());

        let cli_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../gemini-cli/packages/cli");

        let child = util::command::new_smol_command("node")
            .arg(cli_path)
            .arg("--acp")
            .current_dir(root_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        let thread = cx.new(|cx| AcpThread::stdio(child, project, cx));

        Self {
            thread_state: Self::initial_state(thread.clone(), window, cx),
            thread,
            message_editor,
            thread_entry_views: Vec::new(),
            list_state: list_state,
            last_error: None,
            auth_task: None,
            expanded_tool_calls: HashSet::default(),
            expanded_thinking_blocks: HashSet::default(),
        }
    }

    fn initial_state(
        thread: Entity<AcpThread>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ThreadState {
        let initialize = thread.read(cx).initialize();
        let load_task = cx.spawn_in(window, async move |this, cx| {
            let result = match initialize.await {
                Err(e) => Err(e),
                Ok(response) => {
                    if !response.is_authenticated {
                        this.update(cx, |this, _| {
                            this.thread_state = ThreadState::Unauthenticated;
                        })
                        .ok();
                        return;
                    };
                    Ok(())
                }
            };

            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        let subscription =
                            cx.subscribe_in(&thread, window, Self::handle_thread_event);
                        this.list_state
                            .splice(0..0, thread.read(cx).entries().len());

                        this.thread_state = ThreadState::Ready {
                            thread,
                            _subscription: subscription,
                        };
                    }
                    Err(e) => {
                        if let Some(exit_status) = thread.read(cx).exit_status() {
                            this.thread_state = ThreadState::LoadError(
                                format!(
                                    "Gemini exited with status {}",
                                    exit_status.code().unwrap_or(-127)
                                )
                                .into(),
                            )
                        } else {
                            this.thread_state = ThreadState::LoadError(e.to_string().into())
                        }
                    }
                };
                cx.notify();
            })
            .log_err();
        });

        ThreadState::Loading { _task: load_task }
    }

    fn thread(&self) -> Option<&Entity<AcpThread>> {
        match &self.thread_state {
            ThreadState::Ready { thread, .. } => Some(thread),
            ThreadState::Loading { .. }
            | ThreadState::LoadError(..)
            | ThreadState::Unauthenticated => None,
        }
    }

    pub fn title(&self, cx: &App) -> SharedString {
        match &self.thread_state {
            ThreadState::Ready { thread, .. } => thread.read(cx).title(),
            ThreadState::Loading { .. } => "Loading...".into(),
            ThreadState::LoadError(_) => "Failed to load".into(),
            ThreadState::Unauthenticated => "Not authenticated".into(),
        }
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.last_error.take();

        if let Some(thread) = self.thread() {
            thread.update(cx, |thread, cx| thread.cancel(cx)).detach();
        }
    }

    fn chat(&mut self, _: &Chat, window: &mut Window, cx: &mut Context<Self>) {
        self.last_error.take();
        let text = self.message_editor.read(cx).text(cx);
        if text.is_empty() {
            return;
        }
        let Some(thread) = self.thread() else { return };

        let task = thread.update(cx, |thread, cx| thread.send(&text, cx));

        cx.spawn(async move |this, cx| {
            let result = task.await;

            this.update(cx, |this, cx| {
                if let Err(err) = result {
                    this.last_error =
                        Some(cx.new(|cx| {
                            Markdown::new(format!("Error: {err}").into(), None, None, cx)
                        }))
                }
            })
        })
        .detach();

        self.message_editor.update(cx, |editor, cx| {
            editor.clear(window, cx);
        });
    }

    fn handle_thread_event(
        &mut self,
        thread: &Entity<AcpThread>,
        event: &AcpThreadEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.list_state.item_count();
        match event {
            AcpThreadEvent::NewEntry => {
                self.sync_thread_entry_view(thread.read(cx).entries().len() - 1, window, cx);
                self.list_state.splice(count..count, 1);
            }
            AcpThreadEvent::EntryUpdated(index) => {
                let index = *index;
                self.sync_thread_entry_view(index, window, cx);
                self.list_state.splice(index..index + 1, 1);
            }
        }
        cx.notify();
    }

    // todo! should we do this on the fly from render?
    fn sync_thread_entry_view(
        &mut self,
        entry_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let multibuffer = match (
            self.entry_diff_multibuffer(entry_ix, cx),
            self.thread_entry_views.get(entry_ix),
        ) {
            (Some(multibuffer), Some(Some(ThreadEntryView::Diff { editor }))) => {
                if editor.read(cx).buffer() == &multibuffer {
                    // same buffer, all synced up
                    return;
                }
                // new buffer, replace editor
                multibuffer
            }
            (Some(multibuffer), _) => multibuffer,
            (None, Some(Some(ThreadEntryView::Diff { .. }))) => {
                // no longer displaying a diff, drop editor
                self.thread_entry_views[entry_ix] = None;
                return;
            }
            (None, _) => return,
        };

        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: false,
                    show_active_line_background: false,
                    sized_by_content: true,
                },
                multibuffer.clone(),
                None,
                window,
                cx,
            );
            editor.set_show_gutter(false, cx);
            editor.disable_inline_diagnostics();
            editor.disable_expand_excerpt_buttons(cx);
            editor.set_show_vertical_scrollbar(false, cx);
            editor.set_minimap_visibility(MinimapVisibility::Disabled, window, cx);
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.scroll_manager.set_forbid_vertical_scroll(true);
            editor.set_show_indent_guides(false, cx);
            editor.set_read_only(true);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_expand_all_diff_hunks(cx);
            editor.set_text_style_refinement(TextStyleRefinement {
                font_size: Some(
                    TextSize::Small
                        .rems(cx)
                        .to_pixels(ThemeSettings::get_global(cx).agent_font_size(cx))
                        .into(),
                ),
                ..Default::default()
            });
            editor
        });

        if entry_ix >= self.thread_entry_views.len() {
            self.thread_entry_views
                .resize_with(entry_ix + 1, Default::default);
        }

        self.thread_entry_views[entry_ix] = Some(ThreadEntryView::Diff {
            editor: editor.clone(),
        });
    }

    fn entry_diff_multibuffer(&self, entry_ix: usize, cx: &App) -> Option<Entity<MultiBuffer>> {
        let entry = self.thread()?.read(cx).entries().get(entry_ix)?;
        if let AgentThreadEntryContent::ToolCall(ToolCall {
            content: Some(ToolCallContent::Diff { diff }),
            ..
        }) = &entry.content
        {
            Some(diff.multibuffer.clone())
        } else {
            None
        }
    }

    fn authenticate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let agent = self.thread.clone();
        self.last_error.take();
        let authenticate = self.thread.read(cx).authenticate();
        self.auth_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = authenticate.await;

            this.update_in(cx, |this, window, cx| {
                if let Err(err) = result {
                    this.last_error =
                        Some(cx.new(|cx| {
                            Markdown::new(format!("Error: {err}").into(), None, None, cx)
                        }))
                } else {
                    this.thread_state = Self::initial_state(agent, window, cx)
                }
                this.auth_task.take()
            })
            .ok();
        }));
    }

    fn authorize_tool_call(
        &mut self,
        id: ToolCallId,
        outcome: acp::ToolCallConfirmationOutcome,
        cx: &mut Context<Self>,
    ) {
        let Some(thread) = self.thread() else {
            return;
        };
        thread.update(cx, |thread, cx| {
            thread.authorize_tool_call(id, outcome, cx);
        });
        cx.notify();
    }

    fn render_entry(
        &self,
        index: usize,
        total_entries: usize,
        entry: &ThreadEntry,
        window: &mut Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        match &entry.content {
            AgentThreadEntryContent::UserMessage(message) => {
                let style = user_message_markdown_style(window, cx);
                let message_body = div().children(message.chunks.iter().map(|chunk| match chunk {
                    UserMessageChunk::Text { chunk } => {
                        // todo!() open link
                        MarkdownElement::new(chunk.clone(), style.clone())
                    }
                    _ => todo!(),
                }));

                div()
                    .py_4()
                    .px_2()
                    .child(
                        div()
                            .p_3()
                            .rounded_lg()
                            .shadow_md()
                            .bg(cx.theme().colors().editor_background)
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .text_xs()
                            .child(message_body),
                    )
                    .into_any()
            }
            AgentThreadEntryContent::AssistantMessage(AssistantMessage { chunks }) => {
                let style = default_markdown_style(window, cx);
                let message_body = v_flex()
                    .w_full()
                    .gap_2p5()
                    .children(
                        chunks
                            .iter()
                            .enumerate()
                            .map(|(chunk_ix, chunk)| match chunk {
                                AssistantMessageChunk::Text { chunk } => {
                                    // todo!() open link
                                    MarkdownElement::new(chunk.clone(), style.clone())
                                        .into_any_element()
                                }
                                AssistantMessageChunk::Thought { chunk } => self
                                    .render_thinking_block(
                                        index,
                                        chunk_ix,
                                        chunk.clone(),
                                        window,
                                        cx,
                                    ),
                            }),
                    )
                    .into_any();

                v_flex()
                    .px_5()
                    .py_1()
                    .when(index + 1 == total_entries, |this| this.pb_4())
                    .w_full()
                    .text_ui(cx)
                    .child(message_body)
                    .into_any()
            }
            AgentThreadEntryContent::ToolCall(tool_call) => div()
                .py_1()
                .px_5()
                .child(self.render_tool_call(index, tool_call, window, cx))
                .into_any(),
        }
    }

    fn render_thinking_block(
        &self,
        entry_ix: usize,
        chunk_ix: usize,
        chunk: Entity<Markdown>,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let header_id = SharedString::from(format!("thinking-block-header-{}", entry_ix));
        let key = (entry_ix, chunk_ix);
        let is_open = self.expanded_thinking_blocks.contains(&key);

        v_flex()
            .child(
                h_flex()
                    .id(header_id)
                    .group("disclosure-header")
                    .w_full()
                    .justify_between()
                    .opacity(0.8)
                    .hover(|style| style.opacity(1.))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::LightBulb)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(Label::new("Thinking").size(LabelSize::Small)),
                    )
                    .child(
                        div().visible_on_hover("disclosure-header").child(
                            Disclosure::new("thinking-disclosure", is_open)
                                .opened_icon(IconName::ChevronUp)
                                .closed_icon(IconName::ChevronDown)
                                .on_click(cx.listener({
                                    move |this, _event, _window, cx| {
                                        if is_open {
                                            this.expanded_thinking_blocks.remove(&key);
                                        } else {
                                            this.expanded_thinking_blocks.insert(key);
                                        }
                                        cx.notify();
                                    }
                                })),
                        ),
                    )
                    .on_click(cx.listener({
                        move |this, _event, _window, cx| {
                            if is_open {
                                this.expanded_thinking_blocks.remove(&key);
                            } else {
                                this.expanded_thinking_blocks.insert(key);
                            }
                            cx.notify();
                        }
                    })),
            )
            .when(is_open, |this| {
                this.child(
                    div()
                        .relative()
                        .mt_1p5()
                        .ml_1p5()
                        .pl_2p5()
                        .border_l_1()
                        .border_color(cx.theme().colors().border_variant)
                        .text_ui_sm(cx)
                        .child(
                            // todo! url click
                            MarkdownElement::new(chunk, default_markdown_style(window, cx)),
                            // .on_url_click({
                            //     let workspace = self.workspace.clone();
                            //     move |text, window, cx| {
                            //         open_markdown_link(text, workspace.clone(), window, cx);
                            //     }
                            // }),
                        ),
                )
            })
            .into_any_element()
    }

    fn tool_card_header_bg(&self, cx: &Context<Self>) -> Hsla {
        cx.theme()
            .colors()
            .element_background
            .blend(cx.theme().colors().editor_foreground.opacity(0.025))
    }

    fn render_tool_call(
        &self,
        entry_ix: usize,
        tool_call: &ToolCall,
        window: &Window,
        cx: &Context<Self>,
    ) -> Div {
        let header_id = SharedString::from(format!("tool-call-header-{}", entry_ix));

        let status_icon = match &tool_call.status {
            ToolCallStatus::WaitingForConfirmation { .. } => None,
            ToolCallStatus::Allowed {
                status: acp::ToolCallStatus::Running,
                ..
            } => Some(
                Icon::new(IconName::ArrowCircle)
                    .color(Color::Accent)
                    .size(IconSize::Small)
                    .with_animation(
                        "running",
                        Animation::new(Duration::from_secs(2)).repeat(),
                        |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
                    )
                    .into_any(),
            ),
            ToolCallStatus::Allowed {
                status: acp::ToolCallStatus::Finished,
                ..
            } => None,
            ToolCallStatus::Rejected
            | ToolCallStatus::Canceled
            | ToolCallStatus::Allowed {
                status: acp::ToolCallStatus::Error,
                ..
            } => Some(
                Icon::new(IconName::X)
                    .color(Color::Error)
                    .size(IconSize::Small)
                    .into_any_element(),
            ),
        };

        let needs_confirmation = match &tool_call.status {
            ToolCallStatus::WaitingForConfirmation { .. } => true,
            _ => tool_call
                .content
                .iter()
                .any(|content| matches!(content, ToolCallContent::Diff { .. })),
        };

        // todo! consider cleaning up these conditions. maybe break it into a few variants?

        let has_content = tool_call.content.is_some();
        let is_collapsible = has_content && !needs_confirmation;
        let is_open = !is_collapsible || self.expanded_tool_calls.contains(&tool_call.id);

        let content = if is_open {
            match &tool_call.status {
                ToolCallStatus::WaitingForConfirmation { confirmation, .. } => {
                    Some(self.render_tool_call_confirmation(
                        entry_ix,
                        tool_call.id,
                        confirmation,
                        tool_call.content.as_ref(),
                        window,
                        cx,
                    ))
                }
                ToolCallStatus::Allowed { .. } | ToolCallStatus::Canceled => {
                    tool_call.content.as_ref().map(|content| {
                        div()
                            .py_1p5()
                            .border_t_1()
                            .border_color(cx.theme().colors().border)
                            .child(self.render_tool_call_content(entry_ix, content, window, cx))
                            .into_any_element()
                    })
                }
                ToolCallStatus::Rejected => None,
            }
        } else {
            None
        };

        v_flex()
            .text_xs()
            .when(needs_confirmation, |this| {
                this.rounded_lg()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .overflow_hidden()
            })
            .child(
                h_flex()
                    .id(header_id)
                    .w_full()
                    .gap_1()
                    .justify_between()
                    .map(|this| {
                        if needs_confirmation {
                            this.px_2()
                                .py_1()
                                .bg(self.tool_card_header_bg(cx))
                                .rounded_t_md()
                        } else {
                            this.opacity(0.8).hover(|style| style.opacity(1.))
                        }
                    })
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(tool_call.icon)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(MarkdownElement::new(
                                tool_call.label.clone(),
                                default_markdown_style(window, cx),
                            )),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .when(is_collapsible, |this| {
                                this.child(
                                    Disclosure::new(("expand", tool_call.id.as_u64()), is_open)
                                        .opened_icon(IconName::ChevronUp)
                                        .closed_icon(IconName::ChevronDown)
                                        .on_click(cx.listener({
                                            let id = tool_call.id;
                                            move |this: &mut Self, _, _, cx: &mut Context<Self>| {
                                                if is_open {
                                                    this.expanded_tool_calls.remove(&id);
                                                } else {
                                                    this.expanded_tool_calls.insert(id);
                                                }
                                                cx.notify();
                                            }
                                        })),
                                )
                            })
                            .children(status_icon),
                    )
                    .on_click(cx.listener({
                        let id = tool_call.id;
                        move |this: &mut Self, _, _, cx: &mut Context<Self>| {
                            if is_open {
                                this.expanded_tool_calls.remove(&id);
                            } else {
                                this.expanded_tool_calls.insert(id);
                            }
                            cx.notify();
                        }
                    })),
            )
            .when(is_open, |this| {
                this.child(
                    div()
                        .when(is_collapsible, |this| {
                            this.mt_1()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .bg(cx.theme().colors().editor_background)
                                .rounded_lg()
                        })
                        .children(content),
                )
            })
    }

    fn render_tool_call_content(
        &self,
        entry_ix: usize,
        content: &ToolCallContent,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        match content {
            ToolCallContent::Markdown { markdown } => {
                MarkdownElement::new(markdown.clone(), default_markdown_style(window, cx))
                    .into_any_element()
            }
            ToolCallContent::Diff {
                diff: Diff { path, .. },
                ..
            } => self.render_diff_editor(entry_ix, path),
        }
    }

    fn render_tool_call_confirmation(
        &self,
        entry_ix: usize,
        tool_call_id: ToolCallId,
        confirmation: &ToolCallConfirmation,
        content: Option<&ToolCallContent>,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let confirmation_container = v_flex()
            // .px_2()
            .py_1p5()
            .border_t_1()
            .border_color(cx.theme().colors().border);

        let button_container = h_flex()
            .pt_1p5()
            .px_1p5()
            .gap_1()
            .justify_end()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant);

        match confirmation {
            ToolCallConfirmation::Edit { description } => confirmation_container
                .child(
                    div()
                        .px_2()
                        .children(description.clone().map(|description| {
                            MarkdownElement::new(description, default_markdown_style(window, cx))
                        })),
                )
                .children(
                    content.map(|content| {
                        self.render_tool_call_content(entry_ix, content, window, cx)
                    }),
                )
                .child(
                    button_container
                        .child(
                            Button::new(
                                ("always_allow", tool_call_id.as_u64()),
                                "Always Allow Edits",
                            )
                            .icon(IconName::CheckDouble)
                            .icon_position(IconPosition::Start)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Success)
                            .on_click(cx.listener({
                                let id = tool_call_id;
                                move |this, _, _, cx| {
                                    this.authorize_tool_call(
                                        id,
                                        acp::ToolCallConfirmationOutcome::AlwaysAllow,
                                        cx,
                                    );
                                }
                            })),
                        )
                        .child(
                            Button::new(("allow", tool_call_id.as_u64()), "Allow")
                                .icon(IconName::Check)
                                .icon_position(IconPosition::Start)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Success)
                                .on_click(cx.listener({
                                    let id = tool_call_id;
                                    move |this, _, _, cx| {
                                        this.authorize_tool_call(
                                            id,
                                            acp::ToolCallConfirmationOutcome::Allow,
                                            cx,
                                        );
                                    }
                                })),
                        )
                        .child(
                            Button::new(("reject", tool_call_id.as_u64()), "Reject")
                                .icon(IconName::X)
                                .icon_position(IconPosition::Start)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Error)
                                .on_click(cx.listener({
                                    let id = tool_call_id;
                                    move |this, _, _, cx| {
                                        this.authorize_tool_call(
                                            id,
                                            acp::ToolCallConfirmationOutcome::Reject,
                                            cx,
                                        );
                                    }
                                })),
                        ),
                )
                .into_any(),
            ToolCallConfirmation::Execute {
                command,
                root_command,
                description,
            } => confirmation_container
                .child(
                    v_flex()
                        .px_2()
                        .pb_1p5()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(command.clone())
                        .children(description.clone().map(|description| {
                            MarkdownElement::new(description, default_markdown_style(window, cx))
                        })),
                )
                .children(
                    content.map(|content| {
                        self.render_tool_call_content(entry_ix, content, window, cx)
                    }),
                )
                .child(
                    button_container
                        .child(
                            Button::new(
                                ("always_allow", tool_call_id.as_u64()),
                                format!("Always Allow {root_command}"),
                            )
                            .icon(IconName::CheckDouble)
                            .icon_position(IconPosition::Start)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Success)
                            .on_click(cx.listener({
                                let id = tool_call_id;
                                move |this, _, _, cx| {
                                    this.authorize_tool_call(
                                        id,
                                        acp::ToolCallConfirmationOutcome::AlwaysAllow,
                                        cx,
                                    );
                                }
                            })),
                        )
                        .child(
                            Button::new(("allow", tool_call_id.as_u64()), "Allow")
                                .icon(IconName::Check)
                                .icon_position(IconPosition::Start)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Success)
                                .on_click(cx.listener({
                                    let id = tool_call_id;
                                    move |this, _, _, cx| {
                                        this.authorize_tool_call(
                                            id,
                                            acp::ToolCallConfirmationOutcome::Allow,
                                            cx,
                                        );
                                    }
                                })),
                        )
                        .child(
                            Button::new(("reject", tool_call_id.as_u64()), "Reject")
                                .icon(IconName::X)
                                .icon_position(IconPosition::Start)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Error)
                                .on_click(cx.listener({
                                    let id = tool_call_id;
                                    move |this, _, _, cx| {
                                        this.authorize_tool_call(
                                            id,
                                            acp::ToolCallConfirmationOutcome::Reject,
                                            cx,
                                        );
                                    }
                                })),
                        ),
                )
                .into_any(),
            ToolCallConfirmation::Mcp {
                server_name,
                tool_name: _,
                tool_display_name,
                description,
            } => confirmation_container
                .child(
                    v_flex()
                        .px_2()
                        .pb_1p5()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(format!("{server_name} - {tool_display_name}"))
                        .children(description.clone().map(|description| {
                            MarkdownElement::new(description, default_markdown_style(window, cx))
                        })),
                )
                .children(
                    content.map(|content| {
                        self.render_tool_call_content(entry_ix, content, window, cx)
                    }),
                )
                .child(
                    button_container
                        .child(
                            Button::new(
                                ("always_allow_server", tool_call_id.as_u64()),
                                format!("Always Allow {server_name}"),
                            )
                            .icon(IconName::CheckDouble)
                            .icon_position(IconPosition::Start)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Success)
                            .on_click(cx.listener({
                                let id = tool_call_id;
                                move |this, _, _, cx| {
                                    this.authorize_tool_call(
                                        id,
                                        acp::ToolCallConfirmationOutcome::AlwaysAllowMcpServer,
                                        cx,
                                    );
                                }
                            })),
                        )
                        .child(
                            Button::new(
                                ("always_allow_tool", tool_call_id.as_u64()),
                                format!("Always Allow {tool_display_name}"),
                            )
                            .icon(IconName::CheckDouble)
                            .icon_position(IconPosition::Start)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Success)
                            .on_click(cx.listener({
                                let id = tool_call_id;
                                move |this, _, _, cx| {
                                    this.authorize_tool_call(
                                        id,
                                        acp::ToolCallConfirmationOutcome::AlwaysAllowTool,
                                        cx,
                                    );
                                }
                            })),
                        )
                        .child(
                            Button::new(("allow", tool_call_id.as_u64()), "Allow")
                                .icon(IconName::Check)
                                .icon_position(IconPosition::Start)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Success)
                                .on_click(cx.listener({
                                    let id = tool_call_id;
                                    move |this, _, _, cx| {
                                        this.authorize_tool_call(
                                            id,
                                            acp::ToolCallConfirmationOutcome::Allow,
                                            cx,
                                        );
                                    }
                                })),
                        )
                        .child(
                            Button::new(("reject", tool_call_id.as_u64()), "Reject")
                                .icon(IconName::X)
                                .icon_position(IconPosition::Start)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Error)
                                .on_click(cx.listener({
                                    let id = tool_call_id;
                                    move |this, _, _, cx| {
                                        this.authorize_tool_call(
                                            id,
                                            acp::ToolCallConfirmationOutcome::Reject,
                                            cx,
                                        );
                                    }
                                })),
                        ),
                )
                .into_any(),
            ToolCallConfirmation::Fetch { description, urls } => confirmation_container
                .child(
                    v_flex()
                        .px_2()
                        .pb_1p5()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .children(urls.clone())
                        .children(description.clone().map(|description| {
                            MarkdownElement::new(description, default_markdown_style(window, cx))
                        })),
                )
                .children(
                    content.map(|content| {
                        self.render_tool_call_content(entry_ix, content, window, cx)
                    }),
                )
                .child(
                    button_container
                        .child(
                            Button::new(("always_allow", tool_call_id.as_u64()), "Always Allow")
                                .icon(IconName::CheckDouble)
                                .icon_position(IconPosition::Start)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Success)
                                .on_click(cx.listener({
                                    let id = tool_call_id;
                                    move |this, _, _, cx| {
                                        this.authorize_tool_call(
                                            id,
                                            acp::ToolCallConfirmationOutcome::AlwaysAllow,
                                            cx,
                                        );
                                    }
                                })),
                        )
                        .child(
                            Button::new(("allow", tool_call_id.as_u64()), "Allow")
                                .icon(IconName::Check)
                                .icon_position(IconPosition::Start)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Success)
                                .on_click(cx.listener({
                                    let id = tool_call_id;
                                    move |this, _, _, cx| {
                                        this.authorize_tool_call(
                                            id,
                                            acp::ToolCallConfirmationOutcome::Allow,
                                            cx,
                                        );
                                    }
                                })),
                        )
                        .child(
                            Button::new(("reject", tool_call_id.as_u64()), "Reject")
                                .icon(IconName::X)
                                .icon_position(IconPosition::Start)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Error)
                                .on_click(cx.listener({
                                    let id = tool_call_id;
                                    move |this, _, _, cx| {
                                        this.authorize_tool_call(
                                            id,
                                            acp::ToolCallConfirmationOutcome::Reject,
                                            cx,
                                        );
                                    }
                                })),
                        ),
                )
                .into_any(),
            ToolCallConfirmation::Other { description } => confirmation_container
                .child(
                    v_flex()
                        .px_2()
                        .pb_1p5()
                        .border_b_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(MarkdownElement::new(
                            description.clone(),
                            default_markdown_style(window, cx),
                        )),
                )
                .children(
                    content.map(|content| {
                        self.render_tool_call_content(entry_ix, content, window, cx)
                    }),
                )
                .child(
                    button_container
                        .child(
                            Button::new(("always_allow", tool_call_id.as_u64()), "Always Allow")
                                .icon(IconName::CheckDouble)
                                .icon_position(IconPosition::Start)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Success)
                                .on_click(cx.listener({
                                    let id = tool_call_id;
                                    move |this, _, _, cx| {
                                        this.authorize_tool_call(
                                            id,
                                            acp::ToolCallConfirmationOutcome::AlwaysAllow,
                                            cx,
                                        );
                                    }
                                })),
                        )
                        .child(
                            Button::new(("allow", tool_call_id.as_u64()), "Allow")
                                .icon(IconName::Check)
                                .icon_position(IconPosition::Start)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Success)
                                .on_click(cx.listener({
                                    let id = tool_call_id;
                                    move |this, _, _, cx| {
                                        this.authorize_tool_call(
                                            id,
                                            acp::ToolCallConfirmationOutcome::Allow,
                                            cx,
                                        );
                                    }
                                })),
                        )
                        .child(
                            Button::new(("reject", tool_call_id.as_u64()), "Reject")
                                .icon(IconName::X)
                                .icon_position(IconPosition::Start)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Error)
                                .on_click(cx.listener({
                                    let id = tool_call_id;
                                    move |this, _, _, cx| {
                                        this.authorize_tool_call(
                                            id,
                                            acp::ToolCallConfirmationOutcome::Reject,
                                            cx,
                                        );
                                    }
                                })),
                        ),
                )
                .into_any(),
        }
    }

    fn render_diff_editor(&self, entry_ix: usize, path: &Path) -> AnyElement {
        v_flex()
            .h_full()
            .child(path.to_string_lossy().to_string())
            .child(
                if let Some(Some(ThreadEntryView::Diff { editor })) =
                    self.thread_entry_views.get(entry_ix)
                {
                    editor.clone().into_any_element()
                } else {
                    Empty.into_any()
                },
            )
            .into_any()
    }

    fn render_gemini_logo(&self) -> AnyElement {
        Icon::new(IconName::AiGemini)
            .color(Color::Muted)
            .size(IconSize::XLarge)
            .into_any_element()
    }

    fn render_empty_state(&self, loading: bool, cx: &App) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                if loading {
                    h_flex()
                        .justify_center()
                        .child(self.render_gemini_logo())
                        .with_animation(
                            "pulsating_icon",
                            Animation::new(Duration::from_secs(2))
                                .repeat()
                                .with_easing(pulsating_between(0.4, 1.0)),
                            |icon, delta| icon.opacity(delta),
                        ).into_any()
                } else {
                    self.render_gemini_logo().into_any_element()
                }
            )
            .child(
                h_flex()
                    .mt_4()
                    .mb_1()
                    .justify_center()
                    .child(Headline::new(if loading {
                        "Connecting to Gemini…"
                    } else {
                        "Welcome to Gemini"
                    }).size(HeadlineSize::Medium)),
            )
            .child(
                div()
                    .max_w_1_2()
                    .text_sm()
                    .text_center()
                    .map(|this| if loading {
                        this.invisible()
                    } else {
                        this.text_color(cx.theme().colors().text_muted)
                    })
                    .child("Ask questions, edit files, run commands.\nBe specific for the best results.")
            )
            .into_any()
    }

    fn render_pending_auth_state(&self) -> AnyElement {
        v_flex()
            .items_center()
            .justify_center()
            .child(self.render_gemini_logo())
            .child(
                h_flex()
                    .mt_4()
                    .mb_1()
                    .justify_center()
                    .child(Headline::new("Not Authenticated").size(HeadlineSize::Medium)),
            )
            .into_any()
    }

    fn render_message_editor(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let settings = ThemeSettings::get_global(cx);
        let font_size = TextSize::Small
            .rems(cx)
            .to_pixels(settings.agent_font_size(cx));
        let line_height = settings.buffer_line_height.value() * font_size;

        let text_style = TextStyle {
            color: cx.theme().colors().text,
            font_family: settings.buffer_font.family.clone(),
            font_fallbacks: settings.buffer_font.fallbacks.clone(),
            font_features: settings.buffer_font.features.clone(),
            font_size: font_size.into(),
            line_height: line_height.into(),
            ..Default::default()
        };

        EditorElement::new(
            &self.message_editor,
            EditorStyle {
                background: cx.theme().colors().editor_background,
                local_player: cx.theme().players().local(),
                text: text_style,
                syntax: cx.theme().syntax().clone(),
                ..Default::default()
            },
        )
        .into_any()
    }
}

impl Focusable for AcpThreadView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.message_editor.focus_handle(cx)
    }
}

impl Render for AcpThreadView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self.message_editor.read(cx).text(cx);
        let is_editor_empty = text.is_empty();
        let focus_handle = self.message_editor.focus_handle(cx);

        v_flex()
            .size_full()
            .key_context("MessageEditor")
            .on_action(cx.listener(Self::chat))
            .child(match &self.thread_state {
                ThreadState::Unauthenticated => v_flex()
                    .p_2()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child(self.render_pending_auth_state())
                    .child(h_flex().mt_1p5().justify_center().child(
                        Button::new("sign-in", "Sign in to Gemini").on_click(
                            cx.listener(|this, _, window, cx| this.authenticate(window, cx)),
                        ),
                    )),
                ThreadState::Loading { .. } => {
                    v_flex().flex_1().child(self.render_empty_state(true, cx))
                }
                ThreadState::LoadError(e) => div()
                    .p_2()
                    .flex_1()
                    .justify_end()
                    .child(Label::new(format!("Failed to load: {e}")).into_any_element()),
                ThreadState::Ready { thread, .. } => v_flex().flex_1().map(|this| {
                    if self.list_state.item_count() > 0 {
                        this.child(
                            list(self.list_state.clone())
                                .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                                .flex_grow()
                                .into_any(),
                        )
                        .children(match thread.read(cx).status() {
                            ThreadStatus::Idle | ThreadStatus::WaitingForToolConfirmation => None,
                            ThreadStatus::Generating => div()
                                .px_5()
                                .py_2()
                                .child(LoadingLabel::new("").size(LabelSize::Small))
                                .into(),
                        })
                    } else {
                        this.child(self.render_empty_state(false, cx))
                    }
                }),
            })
            .when_some(self.last_error.clone(), |el, error| {
                el.child(
                    div()
                        .p_2()
                        .text_xs()
                        .border_t_1()
                        .border_color(cx.theme().colors().border)
                        .bg(cx.theme().status().error_background)
                        .child(MarkdownElement::new(
                            error,
                            default_markdown_style(window, cx),
                        )),
                )
            })
            .child(
                v_flex()
                    .p_2()
                    .pt_3()
                    .gap_1()
                    .bg(cx.theme().colors().editor_background)
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.render_message_editor(cx))
                    .child({
                        let thread = self.thread();

                        h_flex().justify_end().child(
                            if thread.map_or(true, |thread| {
                                thread.read(cx).status() == ThreadStatus::Idle
                            }) {
                                IconButton::new("send-message", IconName::Send)
                                    .icon_color(Color::Accent)
                                    .style(ButtonStyle::Filled)
                                    .disabled(thread.is_none() || is_editor_empty)
                                    .on_click({
                                        let focus_handle = focus_handle.clone();
                                        move |_event, window, cx| {
                                            focus_handle.dispatch_action(&Chat, window, cx);
                                        }
                                    })
                                    .when(!is_editor_empty, |button| {
                                        button.tooltip(move |window, cx| {
                                            Tooltip::for_action("Send", &Chat, window, cx)
                                        })
                                    })
                                    .when(is_editor_empty, |button| {
                                        button.tooltip(Tooltip::text("Type a message to submit"))
                                    })
                            } else {
                                IconButton::new("stop-generation", IconName::StopFilled)
                                    .icon_color(Color::Error)
                                    .style(ButtonStyle::Tinted(ui::TintColor::Error))
                                    .tooltip(move |window, cx| {
                                        Tooltip::for_action(
                                            "Stop Generation",
                                            &editor::actions::Cancel,
                                            window,
                                            cx,
                                        )
                                    })
                                    .on_click(cx.listener(|this, _event, _, cx| this.cancel(cx)))
                            },
                        )
                    }),
            )
    }
}

fn user_message_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = default_markdown_style(window, cx);
    let mut text_style = window.text_style();
    let theme_settings = ThemeSettings::get_global(cx);

    let buffer_font = theme_settings.buffer_font.family.clone();
    let buffer_font_size = TextSize::Small.rems(cx);

    text_style.refine(&TextStyleRefinement {
        font_family: Some(buffer_font),
        font_size: Some(buffer_font_size.into()),
        ..Default::default()
    });

    style.base_text_style = text_style;
    style
}

fn default_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let theme_settings = ThemeSettings::get_global(cx);
    let colors = cx.theme().colors();
    let ui_font_size = TextSize::Default.rems(cx);
    let buffer_font_size = TextSize::Small.rems(cx);
    let mut text_style = window.text_style();
    let line_height = buffer_font_size * 1.75;

    text_style.refine(&TextStyleRefinement {
        font_family: Some(theme_settings.ui_font.family.clone()),
        font_fallbacks: theme_settings.ui_font.fallbacks.clone(),
        font_features: Some(theme_settings.ui_font.features.clone()),
        font_size: Some(ui_font_size.into()),
        line_height: Some(line_height.into()),
        color: Some(cx.theme().colors().text),
        ..Default::default()
    });

    MarkdownStyle {
        base_text_style: text_style.clone(),
        syntax: cx.theme().syntax().clone(),
        selection_background_color: cx.theme().colors().element_selection_background,
        code_block_overflow_x_scroll: true,
        table_overflow_x_scroll: true,
        heading_level_styles: Some(HeadingLevelStyles {
            h1: Some(TextStyleRefinement {
                font_size: Some(rems(1.15).into()),
                ..Default::default()
            }),
            h2: Some(TextStyleRefinement {
                font_size: Some(rems(1.1).into()),
                ..Default::default()
            }),
            h3: Some(TextStyleRefinement {
                font_size: Some(rems(1.05).into()),
                ..Default::default()
            }),
            h4: Some(TextStyleRefinement {
                font_size: Some(rems(1.).into()),
                ..Default::default()
            }),
            h5: Some(TextStyleRefinement {
                font_size: Some(rems(0.95).into()),
                ..Default::default()
            }),
            h6: Some(TextStyleRefinement {
                font_size: Some(rems(0.875).into()),
                ..Default::default()
            }),
        }),
        code_block: StyleRefinement {
            padding: EdgesRefinement {
                top: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(Pixels(8.)))),
                left: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(Pixels(8.)))),
                right: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(Pixels(8.)))),
                bottom: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(Pixels(8.)))),
            },
            background: Some(colors.editor_background.into()),
            text: Some(TextStyleRefinement {
                font_family: Some(theme_settings.buffer_font.family.clone()),
                font_fallbacks: theme_settings.buffer_font.fallbacks.clone(),
                font_features: Some(theme_settings.buffer_font.features.clone()),
                font_size: Some(buffer_font_size.into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        inline_code: TextStyleRefinement {
            font_family: Some(theme_settings.buffer_font.family.clone()),
            font_fallbacks: theme_settings.buffer_font.fallbacks.clone(),
            font_features: Some(theme_settings.buffer_font.features.clone()),
            font_size: Some(buffer_font_size.into()),
            background_color: Some(colors.editor_foreground.opacity(0.08)),
            ..Default::default()
        },
        link: TextStyleRefinement {
            background_color: Some(colors.editor_foreground.opacity(0.025)),
            underline: Some(UnderlineStyle {
                color: Some(colors.text_accent.opacity(0.5)),
                thickness: px(1.),
                ..Default::default()
            }),
            ..Default::default()
        },
        link_callback: Some(Rc::new(move |_url, _cx| {
            // todo!()
            // if MentionLink::is_valid(url) {
            //     let colors = cx.theme().colors();
            //     Some(TextStyleRefinement {
            //         background_color: Some(colors.element_background),
            //         ..Default::default()
            //     })
            // } else {
            None
            // }
        })),
        ..Default::default()
    }
}
