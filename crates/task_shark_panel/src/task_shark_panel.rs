use std::{
    collections::HashSet,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

mod task_board;

use agent_ui::{
    Agent, AgentPanel, AgentThreadSource, ExternalSourcePrompt,
    thread_metadata_store::{ThreadId, ThreadMetadataStore},
};
use anyhow::{Context as _, Result, anyhow, bail};
use db::kvp::KeyValueStore;
use editor::Editor;
use fs::Fs;
use futures::{AsyncWriteExt as _, StreamExt as _};
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Render, SharedString, Task, WeakEntity, Window, actions, px,
};
use serde::{Deserialize, Serialize};
use task_board::{BOARD_FILE_NAME, Board, BoardEntry, ThreadLink};
use ui::{
    Checkbox, ContextMenu, DropdownMenu, DropdownStyle, IconButton, IconName, IconSize, Label,
    LabelSize, ListItem, ToggleState, Tooltip, prelude::*,
};
use util::{
    ResultExt as _,
    command::{Stdio, new_command},
};
use workspace::{
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

const TASK_SHARK_PANEL_KEY: &str = "TaskSharkPanel";
const SCHEMA_VERSION: u32 = 1;

actions!(
    task_shark_panel,
    [
        /// Toggles focus on the Task Shark panel.
        ToggleFocus
    ]
);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("enter", menu::Confirm, Some("TaskSharkCreate")),
        gpui::KeyBinding::new("escape", menu::Cancel, Some("TaskSharkCreate")),
    ]);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TaskSharkPanel>(window, cx);
        });
    })
    .detach();
}

// These types mirror the version 1 machine API in tasks-go (`internal/machine/model.go`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListsSnapshot {
    current_list: String,
    lists: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    day: String,
    revision: String,
    pending_count: usize,
    tasks: Vec<TaskItem>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskItem {
    id: String,
    title: String,
    completed: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    due_date: Option<String>,
    #[serde(default)]
    owner_list: Option<String>,
    #[serde(default)]
    subtasks: Vec<TaskItem>,
    #[serde(default)]
    available_actions: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    schema_version: u32,
    request_id: String,
    operation: String,
    task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    list: Option<String>,
    expected_revision: String,
    changes: Changes,
}

#[derive(Default, Serialize)]
struct Changes {
    #[serde(skip_serializing_if = "Option::is_none")]
    completed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
}

#[derive(Deserialize)]
struct Response {
    success: bool,
    snapshot: Option<Snapshot>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

pub struct TaskSharkPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    position: DockPosition,
    lists: Vec<String>,
    selected_list: Option<String>,
    snapshot: Option<Snapshot>,
    load_error: Option<SharedString>,
    load_task: Task<()>,
    selected_task: Option<String>,
    board: Option<Board>,
    board_error: Option<SharedString>,
    board_filter: Option<String>,
    thread_links: Vec<ThreadLink>,
    board_task: Task<()>,
    note_editor: Entity<Editor>,
    task_editor: Entity<Editor>,
    create_list: Option<String>,
    creating_task: bool,
    create_error: Option<SharedString>,
    collapsed_sections: HashSet<&'static str>,
    expanded_tasks: HashSet<String>,
    _watch_task: Task<()>,
    _board_watch_task: Task<()>,
}

impl TaskSharkPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let fs = workspace.app_state().fs.clone();
            let workspace = cx.weak_entity();
            cx.new(|cx| Self::new(workspace, fs, window, cx))
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tasks_directory = util::paths::home_dir().join("tasks-lists");
        let board_watch_task = cx.spawn({
            let fs = fs.clone();
            let board_root = task_board::board_root();
            async move |this, cx| {
                let (mut events, _watcher) =
                    fs.watch(&board_root, Duration::from_millis(200)).await;
                while let Some(events) = events.next().await {
                    let board_changed = events.iter().any(|event| {
                        event
                            .path
                            .file_name()
                            .is_some_and(|name| name == BOARD_FILE_NAME)
                    });
                    if board_changed && this.update(cx, |this, cx| this.reload_board(cx)).is_err() {
                        break;
                    }
                }
            }
        });
        let note_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Add a note to the board…", window, cx);
            editor
        });
        let task_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Add a task…", window, cx);
            editor
        });
        let watch_task = cx.spawn(async move |this, cx| {
            let (mut events, _watcher) =
                fs.watch(&tasks_directory, Duration::from_millis(200)).await;
            while let Some(events) = events.next().await {
                // tasks-go keeps lock and probe files in the same directory; only list files matter.
                let lists_changed = events.iter().any(|event| {
                    event
                        .path
                        .extension()
                        .is_some_and(|extension| extension == "md")
                });
                if lists_changed && this.update(cx, |this, cx| this.reload(cx)).is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            lists: Vec::new(),
            selected_list: None,
            snapshot: None,
            load_error: None,
            load_task: Task::ready(()),
            selected_task: None,
            board: None,
            board_error: None,
            board_filter: None,
            thread_links: Vec::new(),
            board_task: Task::ready(()),
            note_editor,
            task_editor,
            create_list: None,
            creating_task: false,
            create_error: None,
            collapsed_sections: HashSet::from(["Completed"]),
            expanded_tasks: HashSet::new(),
            _watch_task: watch_task,
            _board_watch_task: board_watch_task,
        };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let selected_list = self.selected_list.clone();
        self.load_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let executable = find_tasks_executable()?;
                    let lists: ListsSnapshot = serde_json::from_slice(
                        &run_command(&executable, &["api", "lists"], &[], None).await?,
                    )
                    .context("parsing `tasks api lists` output")?;
                    let list = selected_list
                        .filter(|list| lists.lists.contains(list))
                        .unwrap_or(lists.current_list);
                    let arguments = ["api", "snapshot", "--list", list.as_str()];
                    let snapshot: Snapshot = serde_json::from_slice(
                        &run_command(&executable, &arguments, &[], None).await?,
                    )
                    .context("parsing `tasks api snapshot` output")?;
                    anyhow::Ok((lists.lists, list, snapshot))
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((lists, list, snapshot)) => {
                        this.lists = lists;
                        this.selected_list = Some(list);
                        this.snapshot = Some(snapshot);
                        this.load_error = None;
                    }
                    Err(error) => {
                        this.load_error = Some(
                            format!(
                                "Could not load tasks: {error:#}. \
                                 Run `tasks api lists` in a terminal to check the tasks CLI."
                            )
                            .into(),
                        );
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn open_task(&mut self, task_id: String, cx: &mut Context<Self>) {
        self.selected_task = Some(task_id);
        self.board = None;
        self.board_error = None;
        self.board_filter = None;
        self.thread_links.clear();
        self.reload_board(cx);
        cx.notify();
    }

    fn close_task(&mut self, cx: &mut Context<Self>) {
        self.selected_task = None;
        self.board_task = Task::ready(());
        cx.notify();
    }

    fn reload_board(&mut self, cx: &mut Context<Self>) {
        let Some(task_id) = self.selected_task.clone() else {
            return;
        };
        let store = KeyValueStore::global(cx);
        self.board_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let links = task_board::read_links(&store, &task_id)?;
                    let board = task_board::read_board(&task_id).await?;
                    anyhow::Ok((board, links))
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((board, links)) => {
                        this.board = Some(board);
                        this.thread_links = links;
                        this.board_error = None;
                    }
                    Err(error) => {
                        this.board_error = Some(
                            format!(
                                "Could not load Board Updates: {error:#}. \
                                 Check that `node TaskBoardMCP/cli.mjs board-read` runs in a terminal."
                            )
                            .into(),
                        );
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn post_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task_id) = self.selected_task.clone() else {
            return;
        };
        let body = self.note_editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { task_board::post_note(&task_id, body).await })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => this
                        .note_editor
                        .update(cx, |editor, cx| editor.clear(window, cx)),
                    Err(error) => show_error(
                        &this.workspace,
                        format!(
                            "Task Shark could not post the note: {error:#}. \
                             Your text is still in the note box; try again."
                        ),
                        cx,
                    ),
                }
                this.reload_board(cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn select_list(&mut self, list: String, cx: &mut Context<Self>) {
        self.selected_list = Some(list);
        self.snapshot = None;
        self.reload(cx);
        cx.notify();
    }

    fn execute(
        &mut self,
        operation: String,
        task_id: String,
        changes: Changes,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let request = Request {
            schema_version: SCHEMA_VERSION,
            request_id: uuid::Uuid::new_v4().to_string(),
            operation: operation.clone(),
            task_id,
            list: None,
            expected_revision: snapshot.revision.clone(),
            changes,
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    execute_request(&request).await

                })
                .await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    show_error(
                        &this.workspace,
                        format!("Task Shark could not run {operation}: {error:#}. The panel was refreshed; try again."),
                        cx,
                    );
                }
                this.reload(cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn create_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating_task {
            return;
        }
        let title = self.task_editor.read(cx).text(cx);
        let title = title.trim().to_string();
        if title.is_empty() {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(selected_list) = self.selected_list.clone() else {
            return;
        };
        let add_to_today = selected_list == "today";
        let list = if add_to_today {
            let Some(list) = self
                .create_list
                .clone()
                .filter(|list| self.lists.contains(list))
            else {
                self.create_error = Some("Choose a list for the new task.".into());
                cx.notify();
                return;
            };
            list
        } else {
            selected_list
        };
        let request = Request {
            schema_version: SCHEMA_VERSION,
            request_id: uuid::Uuid::new_v4().to_string(),
            operation: "task.create".into(),
            task_id: String::new(),
            list: Some(list.clone()),
            expected_revision: snapshot.revision.clone(),
            changes: Changes {
                title: Some(title),
                ..Changes::default()
            },
        };
        self.creating_task = true;
        self.create_error = None;
        self.task_editor
            .update(cx, |editor, _| editor.set_read_only(true));
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move {
                let before = if add_to_today && list != "today" {
                    Some(read_list_snapshot(&list).await?)
                } else {
                    None
                };
                let created = execute_request(&request).await?;
                if let Some(before) = before {
                    if let Err(error) = add_created_task_to_today(&list, before, created).await {
                        return Ok(Some(format!(
                            "Task created in {list}, but could not add it to Today: {error:#}. Open {list} and use Add to Today; do not create it again."
                        )));
                    }
                }
                anyhow::Ok(None)
            }).await;
            this.update_in(cx, |this, window, cx| {
                this.creating_task = false;
                this.task_editor.update(cx, |editor, _| editor.set_read_only(false));
                match result {
                    Ok(warning) => {
                        this.task_editor.update(cx, |editor, cx| editor.clear(window, cx));
                        this.create_error = warning.map(Into::into);
                    }
                    Err(error) => {
                        this.create_error = Some(format!(
                            "Could not add task: {error:#}. Your title is kept. Refresh and try again."
                        ).into());
                    }
                }
                this.reload(cx);
                cx.notify();
            })
        }).detach_and_log_err(cx);
    }

    fn render_task_composer(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .key_context("TaskSharkCreate")
            .px_2()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .on_action(
                cx.listener(|this, _: &menu::Confirm, window, cx| this.create_task(window, cx)),
            )
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                if !this.creating_task {
                    this.task_editor
                        .update(cx, |editor, cx| editor.clear(window, cx));
                    this.create_error = None;
                    this.focus_handle.focus(window, cx);
                    cx.notify();
                }
            }))
            .child(
                h_flex()
                    .gap_1()
                    .child(div().flex_1().min_w_0().child(self.task_editor.clone()))
                    .child(
                        Button::new(
                            "create-task",
                            if self.creating_task {
                                "Adding…"
                            } else {
                                "Add"
                            },
                        )
                        .label_size(LabelSize::Small)
                        .disabled(self.creating_task || self.snapshot.is_none())
                        .on_click(cx.listener(|this, _, window, cx| this.create_task(window, cx))),
                    ),
            )
            .when(self.selected_list.as_deref() == Some("today"), |composer| {
                let lists = self.lists.clone();
                let panel = cx.weak_entity();
                let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for list in lists {
                        let panel = panel.clone();
                        menu = menu.entry(list.clone(), None, move |_, cx| {
                            panel
                                .update(cx, |this, cx| {
                                    this.create_list = Some(list.clone());
                                    cx.notify();
                                })
                                .log_err();
                        });
                    }
                    menu
                });
                composer.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Label::new("Create in")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            DropdownMenu::new(
                                "create-task-list",
                                self.create_list
                                    .clone()
                                    .unwrap_or_else(|| "Choose list…".into()),
                                menu,
                            )
                            .style(DropdownStyle::Subtle)
                            .disabled(self.creating_task),
                        )
                        .child(
                            Label::new("+ Today")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
            })
            .when_some(self.create_error.clone(), |composer, error| {
                composer.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
    }

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        let lists = self.lists.clone();
        let list_menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for list in lists {
                let this = this.clone();
                menu = menu.entry(list.clone(), None, move |_, cx| {
                    this.update(cx, |this, cx| this.select_list(list.clone(), cx))
                        .log_err();
                });
            }
            menu
        });
        let selected_label = self
            .selected_list
            .clone()
            .unwrap_or_else(|| "Task Shark".to_string());
        let navigation = if self.selected_task.is_some() {
            h_flex()
                .gap_1()
                .child(
                    IconButton::new("task-shark-back", IconName::ArrowLeft)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Back to Tasks"))
                        .on_click(cx.listener(|this, _, _, cx| this.close_task(cx))),
                )
                .child(Label::new(selected_label).size(LabelSize::Small))
                .into_any_element()
        } else {
            h_flex()
                .gap_0p5()
                .child(
                    DropdownMenu::new("task-shark-list", selected_label, list_menu)
                        .style(DropdownStyle::Subtle)
                        .disabled(self.lists.is_empty()),
                )
                .into_any_element()
        };
        h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(navigation)
            .child(
                h_flex()
                    .gap_1()
                    .when_some(self.snapshot.as_ref(), |this, snapshot| {
                        this.child(
                            Label::new(format!("{} pending", snapshot.pending_count))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        IconButton::new("task-shark-refresh", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh Tasks"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.reload(cx);
                                this.reload_board(cx);
                            })),
                    ),
            )
    }

    fn render_task(
        &self,
        task: &TaskItem,
        depth: usize,
        list: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let this = cx.weak_entity();
        let set_completed = task
            .available_actions
            .iter()
            .find(|action| action.ends_with(".setCompleted"))
            .cloned();
        let can_add_to_today = task
            .available_actions
            .iter()
            .any(|action| action == "task.addToToday");

        let checkbox = Checkbox::new(
            SharedString::from(format!("task-complete-{}", task.id)),
            ToggleState::from(task.completed),
        )
        .disabled(set_completed.is_none())
        .on_click({
            let this = this.clone();
            let task_id = task.id.clone();
            let completed = !task.completed;
            move |_, _, cx| {
                cx.stop_propagation();
                let Some(operation) = set_completed.clone() else {
                    return;
                };
                let changes = Changes {
                    completed: Some(completed),
                    ..Changes::default()
                };
                this.update(cx, |this, cx| {
                    this.execute(operation, task_id.clone(), changes, cx)
                })
                .log_err();
            }
        });

        let title = Label::new(task.title.clone())
            .truncate()
            .when(task.completed, |label| {
                label.strikethrough().color(Color::Muted)
            });

        let actions = h_flex()
            .gap_0p5()
            .when(can_add_to_today, |actions| {
                actions.child(
                    IconButton::new(
                        SharedString::from(format!("task-today-{}", task.id)),
                        IconName::Plus,
                    )
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Add to Today"))
                    .on_click({
                        let this = this.clone();
                        let task_id = task.id.clone();
                        move |_, _, cx| {
                            this.update(cx, |this, cx| {
                                this.execute(
                                    "task.addToToday".to_string(),
                                    task_id.clone(),
                                    Changes::default(),
                                    cx,
                                )
                            })
                            .log_err();
                        }
                    }),
                )
            })
            .when(depth == 0, |actions| {
                let task = task.clone();
                let list = task.owner_list.clone().unwrap_or_else(|| list.to_string());
                let workspace = self.workspace.clone();
                let panel = cx.weak_entity();
                actions.child(
                    IconButton::new(
                        SharedString::from(format!("task-agent-{}", task.id)),
                        IconName::ZedAgent,
                    )
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Start Agent Thread"))
                    // Not a `cx.listener`: focusing the Agent Panel can deactivate this panel,
                    // which updates it and would panic while it is already being updated.
                    .on_click(move |_, window, cx| {
                        start_agent_thread(&workspace, &panel, &list, &task, window, cx)
                    }),
                )
            });

        ListItem::new(SharedString::from(format!("task-{}", task.id)))
            .indent_level(depth)
            .indent_step_size(px(16.))
            .start_slot(checkbox)
            .child(v_flex().min_w_0().child(title).when_some(
                task.due_date.clone(),
                |this, due_date| {
                    this.child(
                        Label::new(format!("Due {due_date}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                },
            ))
            .end_slot(actions)
            .when(depth == 0 && self.selected_task.is_none(), |item| {
                let task_id = task.id.clone();
                item.tooltip(Tooltip::text("Open Task Board")).on_click(
                    cx.listener(move |this, _, _, cx| this.open_task(task_id.clone(), cx)),
                )
            })
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(error) = &self.load_error {
            return div()
                .p_2()
                .child(Label::new(error.clone()).color(Color::Error))
                .into_any_element();
        }
        let Some(snapshot) = &self.snapshot else {
            return div()
                .p_2()
                .child(Label::new("Loading tasks…").color(Color::Muted))
                .into_any_element();
        };
        if snapshot.tasks.is_empty() {
            return div()
                .p_2()
                .child(Label::new("No tasks yet. Add one above.").color(Color::Muted))
                .into_any_element();
        }
        let list = self.selected_list.clone().unwrap_or_default();
        if let Some(task_id) = &self.selected_task {
            let Some(task) = snapshot
                .tasks
                .iter()
                .find(|task| &task.id == task_id)
                .cloned()
            else {
                return div()
                    .p_2()
                    .child(Label::new("This task is no longer in the list.").color(Color::Muted))
                    .into_any_element();
            };
            return self.render_detail(&task, &list, cx);
        }
        let mut rows = Vec::new();
        for section in ["Overdue", "Today", "Upcoming", "No due date", "Completed"] {
            let tasks: Vec<_> = snapshot
                .tasks
                .iter()
                .filter(|task| task_section(task, &snapshot.day) == section)
                .collect();
            if tasks.is_empty() {
                continue;
            }
            let collapsed = self.collapsed_sections.contains(section);
            rows.push(
                ListItem::new(section)
                    .start_slot(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::Small),
                    )
                    .child(
                        Label::new(section)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .end_slot(
                        Label::new(tasks.len().to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed_sections.remove(section) {
                            this.collapsed_sections.insert(section);
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            if collapsed {
                continue;
            }
            for task in tasks {
                rows.push(self.render_task(task, 0, &list, cx).into_any_element());
                if task.subtasks.is_empty() {
                    continue;
                }
                let expanded = self.expanded_tasks.contains(&task.id);
                let completed = task.subtasks.iter().filter(|task| task.completed).count();
                let task_id = task.id.clone();
                rows.push(
                    ListItem::new(SharedString::from(format!("subtasks-{}", task.id)))
                        .indent_level(1)
                        .indent_step_size(px(16.))
                        .start_slot(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(IconSize::Small),
                        )
                        .child(
                            Label::new(format!(
                                "{completed} of {} subtasks done",
                                task.subtasks.len()
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.expanded_tasks.remove(&task_id) {
                                this.expanded_tasks.insert(task_id.clone());
                            }
                            cx.notify();
                        }))
                        .into_any_element(),
                );
                if expanded {
                    for subtask in &task.subtasks {
                        rows.push(self.render_task(subtask, 1, &list, cx).into_any_element());
                    }
                }
            }
        }
        v_flex()
            .id("task-shark-tasks")
            .flex_1()
            .overflow_y_scroll()
            .py_1()
            .children(rows)
            .into_any_element()
    }
}

impl TaskSharkPanel {
    fn render_detail(&self, task: &TaskItem, list: &str, cx: &mut Context<Self>) -> AnyElement {
        let mut task_rows = vec![self.render_task(task, 0, list, cx).into_any_element()];
        for subtask in &task.subtasks {
            task_rows.push(self.render_task(subtask, 1, list, cx).into_any_element());
        }
        let description = task
            .description
            .clone()
            .filter(|description| !description.trim().is_empty());
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                v_flex()
                    .id("task-shark-detail")
                    .flex_1()
                    .overflow_y_scroll()
                    .py_1()
                    .children(task_rows)
                    .when_some(description, |this, description| {
                        this.child(
                            div().px_3().py_1().child(
                                Label::new(description)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                    })
                    .child(section_header("Conversations", cx))
                    .children(self.render_conversations(cx))
                    .child(section_header("Board Updates", cx))
                    .child(self.render_board()),
            )
            .child(self.render_note_composer(cx))
            .into_any_element()
    }

    fn render_conversations(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(board) = &self.board else {
            return Vec::new();
        };
        let updates_for = |conversation_id: &str| {
            board
                .entries
                .iter()
                .filter(|entry| entry.thread_id.as_deref() == Some(conversation_id))
                .count()
        };
        let mut rows = vec![self.render_conversation_row(
            "all",
            None,
            "All updates".into(),
            update_count(board.entries.len()),
            None,
            cx,
        )];
        let metadata_store = ThreadMetadataStore::try_global(cx);
        let mut shown_conversations = Vec::new();
        for link in &self.thread_links {
            // Drafts that were never sent disappear from Zed, so their links are skipped.
            let Some(metadata) = metadata_store
                .as_ref()
                .and_then(|store| store.read(cx).entry(link.thread_id).cloned())
            else {
                continue;
            };
            shown_conversations.push(link.conversation_id.clone());
            rows.push(self.render_conversation_row(
                &link.conversation_id,
                Some(link.conversation_id.clone()),
                metadata.display_title(),
                format!(
                    "Zed thread · {} updates",
                    updates_for(&link.conversation_id)
                ),
                Some(link.thread_id),
                cx,
            ));
        }
        let mut other_conversations: Vec<(String, Option<String>)> = Vec::new();
        for entry in &board.entries {
            if let Some(conversation_id) = &entry.thread_id
                && !shown_conversations.contains(conversation_id)
                && !other_conversations
                    .iter()
                    .any(|(id, _)| id == conversation_id)
            {
                other_conversations.push((conversation_id.clone(), entry.source.clone()));
            }
        }
        for (conversation_id, source) in other_conversations {
            let short_id: String = conversation_id.chars().take(8).collect();
            let source = source.unwrap_or_else(|| "Task Shark".to_string());
            rows.push(self.render_conversation_row(
                &conversation_id,
                Some(conversation_id.clone()),
                format!("{source} conversation {short_id}").into(),
                update_count(updates_for(&conversation_id)),
                None,
                cx,
            ));
        }
        rows
    }

    fn render_conversation_row(
        &self,
        key: &str,
        filter: Option<String>,
        title: SharedString,
        detail: String,
        thread_id: Option<ThreadId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.board_filter == filter;
        let workspace = self.workspace.clone();
        ListItem::new(SharedString::from(format!("task-conversation-{key}")))
            .toggle_state(selected)
            .child(
                v_flex()
                    .min_w_0()
                    .child(Label::new(title).truncate())
                    .child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.board_filter = filter.clone();
                cx.notify();
            }))
            .when_some(thread_id, |item, thread_id| {
                item.end_slot(
                    IconButton::new(
                        SharedString::from(format!("task-open-thread-{key}")),
                        IconName::ArrowUpRight,
                    )
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Open Agent Thread"))
                    .on_click(move |_, window, cx| {
                        open_linked_thread(&workspace, thread_id, window, cx)
                    }),
                )
            })
            .into_any_element()
    }

    fn render_board(&self) -> AnyElement {
        if let Some(error) = &self.board_error {
            return div()
                .px_3()
                .child(Label::new(error.clone()).color(Color::Error))
                .into_any_element();
        }
        let Some(board) = &self.board else {
            return div()
                .px_3()
                .child(Label::new("Loading Board Updates…").color(Color::Muted))
                .into_any_element();
        };
        let entries: Vec<&BoardEntry> = board
            .entries
            .iter()
            .rev()
            .filter(|entry| self.board_filter.is_none() || entry.thread_id == self.board_filter)
            .collect();
        if entries.is_empty() {
            return div()
                .px_3()
                .child(Label::new("No Board Updates yet.").color(Color::Muted))
                .into_any_element();
        }
        v_flex()
            .px_3()
            .pb_2()
            .gap_2()
            .when(board.has_more, |this| {
                this.child(
                    Label::new("Showing the latest 100 updates.")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .children(entries.into_iter().map(render_board_entry))
            .into_any_element()
    }

    fn render_note_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .key_context("TaskSharkNote")
            .px_2()
            .py_1()
            .gap_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .on_action(
                cx.listener(|this, _: &menu::Confirm, window, cx| this.post_note(window, cx)),
            )
            .child(div().flex_1().min_w_0().child(self.note_editor.clone()))
            .child(
                Button::new("task-shark-post-note", "Post Note")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _, window, cx| this.post_note(window, cx))),
            )
    }
}

// Board timestamps are UTC ISO 8601 (`2026-08-25T07:52:49Z`); show them to the minute.
fn format_utc_minute(timestamp: &str) -> String {
    timestamp
        .get(..16)
        .map(|minute| format!("{} UTC", minute.replace('T', " ")))
        .unwrap_or_else(|| timestamp.to_string())
}

fn task_section(task: &TaskItem, today: &str) -> &'static str {
    if task.completed {
        return "Completed";
    }
    match task.due_date.as_deref().filter(|date| !date.is_empty()) {
        Some(date) if date < today => "Overdue",
        Some(date) if date == today => "Today",
        Some(_) => "Upcoming",
        None => "No due date",
    }
}

fn update_count(count: usize) -> String {
    match count {
        1 => "1 update".to_string(),
        count => format!("{count} updates"),
    }
}

fn section_header(title: &'static str, cx: &App) -> impl IntoElement {
    div()
        .mt_2()
        .px_3()
        .py_1()
        .border_t_1()
        .border_color(cx.theme().colors().border_variant)
        .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
}

fn render_board_entry(entry: &BoardEntry) -> impl IntoElement {
    let kind_color = match entry.kind.as_str() {
        task_board::KIND_BLOCKER => Color::Error,
        task_board::KIND_DECISION => Color::Accent,
        task_board::KIND_PROGRESS => Color::Success,
        task_board::KIND_HANDOFF => Color::Warning,
        _ => Color::Muted,
    };
    let author = [Some(entry.actor_kind.as_str()), entry.source.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    let created_at = format_utc_minute(&entry.created_at);
    v_flex()
        .gap_0p5()
        .child(
            h_flex()
                .gap_2()
                .child(
                    Label::new(entry.kind.to_uppercase())
                        .size(LabelSize::XSmall)
                        .color(kind_color),
                )
                .child(
                    Label::new(format!("{author} · {created_at} · #{}", entry.sequence))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
        .child(Label::new(entry.body.clone()).size(LabelSize::Small))
}

async fn read_list_snapshot(list: &str) -> Result<Snapshot> {
    let executable = find_tasks_executable()?;
    serde_json::from_slice(
        &run_command(&executable, &["api", "snapshot", "--list", list], &[], None).await?,
    )
    .context("parsing task list snapshot")
}

async fn execute_request(request: &Request) -> Result<Option<Snapshot>> {
    let executable = find_tasks_executable()?;
    let output = run_command(
        &executable,
        &["api", "exec"],
        &[],
        Some(serde_json::to_vec(request)?),
    )
    .await?;
    let response: Response =
        serde_json::from_slice(&output).context("parsing `tasks api exec` output")?;
    match (response.success, response.error) {
        (true, _) => Ok(response.snapshot),
        (false, Some(error)) => Err(anyhow!("{}: {}", error.code, error.message)),
        (false, None) => Err(anyhow!("tasks CLI reported failure without details")),
    }
}

async fn add_created_task_to_today(
    list: &str,
    before: Snapshot,
    created: Option<Snapshot>,
) -> Result<()> {
    // The API returns Today's snapshot, not the created task ID. Match IDs only
    // while the revision is unchanged so another writer cannot select the wrong task.
    let created = created.context("tasks CLI omitted the creation snapshot")?;
    let after = read_list_snapshot(list).await?;
    if after.revision != created.revision {
        bail!("task data changed after creation");
    }
    let mut added = after
        .tasks
        .iter()
        .filter(|task| !before.tasks.iter().any(|old| old.id == task.id));
    let task = added.next().context("created task was not found")?;
    if added.next().is_some() {
        bail!("more than one new task was found");
    }
    execute_request(&Request {
        schema_version: SCHEMA_VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        operation: "task.addToToday".into(),
        task_id: task.id.clone(),
        list: None,
        expected_revision: after.revision,
        changes: Changes::default(),
    })
    .await?;
    Ok(())
}

fn find_tasks_executable() -> Result<PathBuf> {
    // GUI launches do not inherit the login shell PATH, so probe the install locations
    // Task Shark itself uses before falling back to PATH.
    let home = util::paths::home_dir();
    [
        home.join(".local/bin/tasks"),
        PathBuf::from("/usr/local/bin/tasks"),
        PathBuf::from("/opt/homebrew/bin/tasks"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .or_else(|| which::which("tasks").ok())
    .context(
        "`tasks` executable not found in ~/.local/bin, /usr/local/bin, /opt/homebrew/bin, or PATH",
    )
}

pub(crate) async fn run_command(
    executable: &Path,
    arguments: &[&str],
    environment: &[(&str, &str)],
    input: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut command = new_command(executable);
    command
        .args(arguments)
        .envs(environment.iter().copied())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", executable.display()))?;
    if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .with_context(|| format!("{} stdin was not captured", executable.display()))?;
        stdin.write_all(&input).await?;
        // Dropping stdin closes the pipe so the child sees end of input.
        drop(stdin);
    }
    let output = child.output().await?;
    if !output.status.success() && output.stdout.is_empty() {
        bail!(
            "`{} {}` failed: {}",
            executable.display(),
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn agent_prompt(list: &str, task: &TaskItem, conversation_id: &str) -> String {
    let mut prompt = format!(
        "Work on this task from my Task Shark list \"{list}\".\n\nTask: {}\nTask ID: {}\n",
        task.title, task.id
    );
    if let Some(description) = task
        .description
        .as_deref()
        .filter(|description| !description.trim().is_empty())
    {
        write!(prompt, "\nNotes:\n{description}\n").log_err();
    }
    if !task.subtasks.is_empty() {
        prompt.push_str("\nSubtasks:\n");
        for subtask in &task.subtasks {
            let mark = if subtask.completed { "x" } else { " " };
            writeln!(prompt, "- [{mark}] {}", subtask.title).log_err();
        }
    }
    write!(
        prompt,
        "\nTask board: use the `taskshark` MCP tools with taskId \"{}\". \
         Call board_read first to see earlier progress. \
         Post meaningful progress, decisions, blockers, and handoffs with board_post, \
         using threadId \"{conversation_id}\" and source \"{}\".\n",
        task.id,
        task_board::BOARD_SOURCE
    )
    .log_err();
    prompt
}

fn start_agent_thread(
    workspace: &WeakEntity<Workspace>,
    panel: &WeakEntity<TaskSharkPanel>,
    list: &str,
    task: &TaskItem,
    window: &mut Window,
    cx: &mut App,
) {
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let prompt = agent_prompt(list, task, &conversation_id);
    let thread_id = workspace
        .update(cx, |workspace, cx| {
            let Some(agent_panel) = workspace.focus_panel::<AgentPanel>(window, cx) else {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<TaskSharkPanel>(),
                        "Could not start an agent thread: the Agent Panel is not available. \
                         Check that `disable_ai` is off.",
                    ),
                    cx,
                );
                return None;
            };
            agent_panel.update(cx, |agent_panel, cx| {
                let previous_thread_id = agent_panel.active_thread_id(cx);
                agent_panel.new_agent_thread_with_external_source_prompt(
                    ExternalSourcePrompt::new(&prompt),
                    window,
                    cx,
                );
                // The Agent Panel skips creating a thread when no project is open, which would
                // leave the previous thread active; never link that one to this task.
                agent_panel
                    .active_thread_id(cx)
                    .filter(|thread_id| Some(*thread_id) != previous_thread_id)
            })
        })
        .log_err()
        .flatten();
    let Some(thread_id) = thread_id else {
        return;
    };
    let link = ThreadLink {
        conversation_id,
        thread_id,
    };
    let task_id = task.id.clone();
    let store = KeyValueStore::global(cx);
    panel
        .update(cx, |_, cx| {
            cx.spawn(async move |panel, cx| {
                cx.background_spawn(task_board::add_link(store, task_id, link))
                    .await?;
                panel.update(cx, |panel, cx| panel.reload_board(cx))
            })
            .detach_and_log_err(cx);
        })
        .log_err();
}

fn open_linked_thread(
    workspace: &WeakEntity<Workspace>,
    thread_id: ThreadId,
    window: &mut Window,
    cx: &mut App,
) {
    let metadata = ThreadMetadataStore::try_global(cx)
        .and_then(|store| store.read(cx).entry(thread_id).cloned());
    let Some(metadata) = metadata else {
        show_error(
            workspace,
            "Could not open the agent thread: Zed no longer has it. \
             Unsent drafts are removed when they are closed."
                .to_string(),
            cx,
        );
        return;
    };
    workspace
        .update(cx, |workspace, cx| {
            let Some(agent_panel) = workspace.focus_panel::<AgentPanel>(window, cx) else {
                return;
            };
            agent_panel.update(cx, |agent_panel, cx| {
                agent_panel.load_agent_thread(
                    Agent::from(metadata.agent_id.clone()),
                    metadata.thread_id,
                    Some(metadata.folder_paths().clone()),
                    metadata.title.clone(),
                    true,
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                );
            });
        })
        .log_err();
}

fn show_error(workspace: &WeakEntity<Workspace>, message: String, cx: &mut App) {
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<TaskSharkPanel>(), message),
                cx,
            );
        })
        .log_err();
}

impl Panel for TaskSharkPanel {
    fn persistent_name() -> &'static str {
        "Task Shark Panel"
    }

    fn panel_key() -> &'static str {
        TASK_SHARK_PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Task Shark")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

impl Focusable for TaskSharkPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for TaskSharkPanel {}

impl Render for TaskSharkPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("task-shark-panel")
            .key_context("TaskSharkPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(window, cx))
            .when(self.selected_task.is_none(), |panel| {
                panel.child(self.render_task_composer(window, cx))
            })
            .child(self.render_body(cx))
    }
}
