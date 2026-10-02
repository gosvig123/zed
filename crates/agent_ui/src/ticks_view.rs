use std::{collections::HashSet, sync::Arc, time::Duration};

use editor::{Editor, MultiBuffer};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, PromptLevel, Render, SharedString, Task,
    WeakEntity, Window,
};
use ui::{
    Checkbox, IconButton, IconName, IconSize, Indicator, Label, LabelSize, ListItem, ToggleState,
    Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::pi_tick::{self, RunRecord, TickDraft, TickHost, TickJob, TickKey};

// Remote hosts have no file watcher, so their jobs and running state are polled.
const REMOTE_POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
enum TickAction {
    Enable,
    Disable,
    Kill,
    Delete,
}

impl TickAction {
    fn description(self) -> &'static str {
        match self {
            TickAction::Enable => "enable",
            TickAction::Disable => "disable",
            TickAction::Kill => "stop",
            TickAction::Delete => "delete",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ScheduleKind {
    Daily,
    Weekly,
    Interval,
}

impl ScheduleKind {
    const ALL: [ScheduleKind; 3] = [
        ScheduleKind::Daily,
        ScheduleKind::Weekly,
        ScheduleKind::Interval,
    ];

    fn from_value(value: &str) -> Self {
        match value {
            "weekly" => ScheduleKind::Weekly,
            "interval" => ScheduleKind::Interval,
            _ => ScheduleKind::Daily,
        }
    }

    fn value(self) -> &'static str {
        match self {
            ScheduleKind::Daily => "daily",
            ScheduleKind::Weekly => "weekly",
            ScheduleKind::Interval => "interval",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ScheduleKind::Daily => "Daily",
            ScheduleKind::Weekly => "Weekly",
            ScheduleKind::Interval => "Interval",
        }
    }
}

/// The create and edit form. pi-tick validates every field on the tick's host.
struct TickForm {
    /// `None` while creating a new tick.
    editing: Option<TickKey>,
    host: TickHost,
    schedule_kind: ScheduleKind,
    enabled: bool,
    saving: bool,
    error: Option<SharedString>,
    id_editor: Entity<Editor>,
    prompt_editor: Entity<Editor>,
    cwd_editor: Entity<Editor>,
    time_editor: Entity<Editor>,
    days_editor: Entity<Editor>,
    minutes_editor: Entity<Editor>,
    model_editor: Entity<Editor>,
}

pub(crate) struct TicksView {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    hosts: Vec<TickHost>,
    ticks: Option<Vec<TickJob>>,
    ticks_error: Option<SharedString>,
    host_errors: Vec<(TickHost, SharedString)>,
    ticks_task: Task<()>,
    selected_tick: Option<TickKey>,
    tick_runs: Vec<RunRecord>,
    tick_runs_error: Option<SharedString>,
    busy_ticks: HashSet<TickKey>,
    form: Option<TickForm>,
    _tick_watch_task: Task<()>,
    _remote_poll_task: Task<()>,
}

impl TicksView {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        let tick_watch_task = cx.spawn({
            let fs = fs.clone();
            let data_directory = pi_tick::data_directory();
            async move |this, cx| {
                let (mut events, _watcher) =
                    fs.watch(&data_directory, Duration::from_millis(200)).await;
                while let Some(events) = events.next().await {
                    // Transcripts and logs change constantly during a run; only catalog,
                    // run history, and active-run records change what the panel shows.
                    let ticks_changed = events.iter().any(|event| {
                        event.path.file_name().is_some_and(|name| {
                            name == pi_tick::JOBS_FILE_NAME
                                || name == pi_tick::RUNS_FILE_NAME
                                || name == pi_tick::HOSTS_FILE_NAME
                        }) || event.path.parent().and_then(|parent| parent.file_name())
                            == Some(pi_tick::ACTIVE_DIRECTORY_NAME.as_ref())
                    });
                    if ticks_changed
                        && this
                            .update(cx, |this, cx| {
                                this.reload_ticks(cx);
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            }
        });
        let remote_poll_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REMOTE_POLL_INTERVAL).await;
                let updated = this.update(cx, |this, cx| {
                    if this.hosts.iter().any(TickHost::is_remote) {
                        this.reload_ticks(cx);
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        Self {
            workspace,
            focus_handle: cx.focus_handle(),
            hosts: Vec::new(),
            ticks: None,
            ticks_error: None,
            host_errors: Vec::new(),
            ticks_task: Task::ready(()),
            selected_tick: None,
            tick_runs: Vec::new(),
            tick_runs_error: None,
            busy_ticks: HashSet::new(),
            form: None,
            _tick_watch_task: tick_watch_task,
            _remote_poll_task: remote_poll_task,
        }
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reload_ticks(cx);
    }

    fn reload_ticks(&mut self, cx: &mut Context<Self>) {
        let selected_tick = self.selected_tick.clone();
        self.ticks_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let selected_tick = selected_tick.clone();
                    async move {
                        let overview = pi_tick::load_jobs().await?;
                        let runs = match &selected_tick {
                            Some(key) => Some(pi_tick::recent_runs(key).await),
                            None => None,
                        };
                        anyhow::Ok((overview, runs))
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((overview, runs)) => {
                        this.hosts = overview.hosts;
                        this.ticks = Some(overview.jobs);
                        this.ticks_error = None;
                        this.host_errors = overview
                            .failures
                            .into_iter()
                            .map(|failure| {
                                let recovery = match &failure.host {
                                    TickHost::Local => "Run `node ~/.pi/agent/tick/pi-tick.mjs list` in a terminal to check pi-tick.".to_string(),
                                    TickHost::Ssh(alias) => format!(
                                        "Check that `ssh {alias}` works without a password prompt and that pi-tick is installed there."
                                    ),
                                };
                                let message =
                                    format!("Could not load ticks: {:#}. {recovery}", failure.error);
                                (failure.host, message.into())
                            })
                            .collect();
                        if this.selected_tick == selected_tick {
                            match runs.transpose() {
                                Ok(runs) => {
                                    this.tick_runs = runs.unwrap_or_default();
                                    this.tick_runs_error = None;
                                }
                                Err(error) => {
                                    this.tick_runs_error = Some(
                                        format!(
                                            "Could not read run history: {error:#}. \
                                             Check that pi-tick's runs.jsonl is readable."
                                        )
                                        .into(),
                                    );
                                }
                            }
                        }
                    }
                    Err(error) => {
                        this.ticks_error = Some(
                            format!(
                                "Could not load ticks: {error:#}. \
                                 Fix ~/.pi/agent/tick/{}, then refresh.",
                                pi_tick::HOSTS_FILE_NAME
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

    fn open_tick(&mut self, key: TickKey, cx: &mut Context<Self>) {
        self.selected_tick = Some(key);
        self.tick_runs.clear();
        self.tick_runs_error = None;
        self.reload_ticks(cx);
        cx.notify();
    }

    fn close_tick(&mut self, cx: &mut Context<Self>) {
        // Back from the form returns to where it was opened: the tick's detail or the list.
        if self.form.take().is_none() {
            self.selected_tick = None;
        }
        cx.notify();
    }

    fn open_form(&mut self, editing: Option<TickKey>, window: &mut Window, cx: &mut Context<Self>) {
        let job = editing.as_ref().and_then(|key| {
            self.ticks
                .as_ref()?
                .iter()
                .find(|job| &job.key() == key)
                .cloned()
        });
        let schedule = job.as_ref().map(|job| &job.schedule);
        let interval_minutes = schedule
            .filter(|schedule| schedule.kind == "interval")
            .map(|schedule| {
                let seconds =
                    schedule.value.minutes.unwrap_or(0) * 60 + schedule.value.seconds.unwrap_or(0);
                (seconds / 60).to_string()
            })
            .unwrap_or_default();
        let form = TickForm {
            host: editing
                .as_ref()
                .map(|key| key.host.clone())
                .unwrap_or_default(),
            schedule_kind: schedule
                .map(|schedule| ScheduleKind::from_value(&schedule.kind))
                .unwrap_or(ScheduleKind::Daily),
            enabled: true,
            saving: false,
            error: None,
            id_editor: single_line_editor(
                job.as_ref().map_or("", |job| job.id.as_str()),
                "daily-planning",
                window,
                cx,
            ),
            prompt_editor: cx.new(|cx| {
                let mut editor = Editor::auto_height(4, 16, window, cx);
                editor.set_placeholder_text("What should Pi do on each run?", window, cx);
                if let Some(job) = &job {
                    editor.set_text(job.prompt.as_str(), window, cx);
                }
                editor
            }),
            cwd_editor: single_line_editor(
                job.as_ref().map_or("", |job| job.cwd.as_str()),
                "Absolute path on the tick's machine",
                window,
                cx,
            ),
            time_editor: single_line_editor(
                schedule
                    .and_then(|schedule| schedule.value.time.as_deref())
                    .unwrap_or("09:00"),
                "HH:MM",
                window,
                cx,
            ),
            days_editor: single_line_editor(
                &schedule
                    .map(|schedule| schedule.value.days.join(", "))
                    .unwrap_or_default(),
                "monday, friday",
                window,
                cx,
            ),
            minutes_editor: single_line_editor(&interval_minutes, "30", window, cx),
            model_editor: single_line_editor(
                job.as_ref()
                    .and_then(|job| job.model.as_deref())
                    .unwrap_or(""),
                "Default model",
                window,
                cx,
            ),
            editing,
        };
        let focused_editor = if form.editing.is_some() {
            &form.prompt_editor
        } else {
            &form.id_editor
        };
        window.focus(&focused_editor.focus_handle(cx), cx);
        self.form = Some(form);
        cx.notify();
    }

    fn save_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.saving {
            return;
        }
        let text = |editor: &Entity<Editor>| editor.read(cx).text(cx).trim().to_string();
        let id = match &form.editing {
            Some(key) => key.job_id.clone(),
            None => text(&form.id_editor),
        };
        if id.is_empty() {
            form.error = Some("Enter an ID for the tick.".into());
            cx.notify();
            return;
        }
        let model = text(&form.model_editor);
        let draft = TickDraft {
            id: id.clone(),
            prompt: text(&form.prompt_editor),
            cwd: text(&form.cwd_editor),
            schedule_kind: form.schedule_kind.value().to_string(),
            time: text(&form.time_editor),
            days: text(&form.days_editor),
            minutes: text(&form.minutes_editor),
            model: (!model.is_empty()).then_some(model),
            enabled: form.enabled,
            create: form.editing.is_none(),
        };
        let host = form.host.clone();
        form.saving = true;
        form.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let host = host.clone();
                    async move { pi_tick::save(&host, &draft).await }
                })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.form = None;
                        this.selected_tick = Some(TickKey { host, job_id: id });
                        this.tick_runs.clear();
                        this.tick_runs_error = None;
                    }
                    Err(error) => {
                        if let Some(form) = this.form.as_mut() {
                            form.saving = false;
                            form.error = Some(
                                format!("Could not save the tick: {error:#}. Fix the field, then save again.")
                                    .into(),
                            );
                        }
                    }
                }
                this.reload_ticks(cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn perform_tick_action(&mut self, key: TickKey, action: TickAction, cx: &mut Context<Self>) {
        if !self.busy_ticks.insert(key.clone()) {
            return;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let key = key.clone();
                    async move {
                        match action {
                            TickAction::Enable => pi_tick::set_enabled(&key, true).await,
                            TickAction::Disable => pi_tick::set_enabled(&key, false).await,
                            TickAction::Kill => pi_tick::kill(&key).await,
                            TickAction::Delete => pi_tick::delete(&key).await,
                        }
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.busy_ticks.remove(&key);
                match result {
                    Ok(()) => {
                        if matches!(action, TickAction::Delete)
                            && this.selected_tick.as_ref() == Some(&key)
                        {
                            this.selected_tick = None;
                        }
                    }
                    Err(error) => show_error(
                        &this.workspace,
                        format!(
                            "Could not {} tick {}: {error:#}. The Ticks list was refreshed; try again.",
                            action.description(),
                            tick_name(&key)
                        ),
                        cx,
                    ),
                }
                this.reload_ticks(cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn confirm_delete_tick(&mut self, key: TickKey, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete tick \"{}\"?", tick_name(&key)),
            Some("This removes its schedule, logs, and transcripts. You cannot undo this."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return Ok(());
            }
            this.update(cx, |this, cx| {
                this.perform_tick_action(key, TickAction::Delete, cx)
            })
        })
        .detach_and_log_err(cx);
    }

    fn run_tick(&mut self, key: TickKey, cx: &mut Context<Self>) {
        // The local CLI returns only when the agent run ends, so the panel learns that the run
        // started from its active-run record, not from this task.
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let key = key.clone();
                    async move { pi_tick::run_now(&key).await }
                })
                .await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    show_error(
                        &this.workspace,
                        format!(
                            "Could not run tick {}: {error:#}. Fix the cause, then use Run Now again.",
                            tick_name(&key)
                        ),
                        cx,
                    );
                }
                this.reload_ticks(cx);
            })
        })
        .detach_and_log_err(cx);
        self.reload_ticks(cx);
    }

    fn open_transcript(
        &mut self,
        key: TickKey,
        run_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        let name = tick_name(&key);
        let title = match &run_id {
            Some(run_id) => format!("Tick {name} · {run_id}"),
            None => format!("Tick {name}"),
        };
        cx.spawn_in(window, async move |_, cx| {
            let transcript = cx
                .background_spawn({
                    let key = key.clone();
                    async move { pi_tick::transcript(&key, run_id.as_deref()).await }
                })
                .await;
            let transcript = match transcript {
                Ok(transcript) => transcript,
                Err(error) => {
                    let job_id = &key.job_id;
                    let place = match &key.host {
                        TickHost::Local => "a terminal".to_string(),
                        TickHost::Ssh(alias) => format!("a terminal on {alias}"),
                    };
                    return cx.update(|_, cx| {
                        show_error(
                            &workspace,
                            format!(
                                "Could not open the transcript for tick {name}: {error:#}. \
                                 Run `pi-tick show {job_id}` in {place} to check it."
                            ),
                            cx,
                        )
                    });
                }
            };
            workspace.update_in(cx, |workspace, window, cx| {
                let project = workspace.project().clone();
                let buffer = project.update(cx, |project, cx| {
                    project.create_local_buffer(&transcript, None, false, cx)
                });
                let multi_buffer =
                    cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title));
                let editor = cx.new(|cx| {
                    let mut editor =
                        Editor::for_multibuffer(multi_buffer, Some(project), window, cx);
                    editor.set_read_only(true);
                    editor
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn render_form(&self, form: &TickForm, cx: &mut Context<Self>) -> AnyElement {
        let field = |label: &'static str, editor: &Entity<Editor>| {
            v_flex()
                .px_3()
                .gap_0p5()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                .child(
                    div()
                        .px_1p5()
                        .py_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .child(editor.clone()),
                )
        };
        let choice_row = |label: &'static str| {
            v_flex()
                .px_3()
                .gap_0p5()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        };
        let machine = if form.editing.is_some() {
            choice_row("Machine")
                .child(Label::new(form.host.label().to_string()).size(LabelSize::Small))
        } else {
            choice_row("Machine").child(h_flex().gap_1().flex_wrap().children(
                self.hosts.iter().enumerate().map(|(index, host)| {
                    let host = host.clone();
                    Button::new(("tick-form-host", index), host.label().to_string())
                        .label_size(LabelSize::Small)
                        .toggle_state(form.host == host)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(form) = this.form.as_mut() {
                                form.host = host.clone();
                                cx.notify();
                            }
                        }))
                }),
            ))
        };
        let schedule = choice_row("Schedule").child(h_flex().gap_1().children(
            ScheduleKind::ALL.into_iter().map(|kind| {
                Button::new(
                    SharedString::from(format!("tick-form-{}", kind.value())),
                    kind.label(),
                )
                .label_size(LabelSize::Small)
                .toggle_state(form.schedule_kind == kind)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(form) = this.form.as_mut() {
                        form.schedule_kind = kind;
                        cx.notify();
                    }
                }))
            }),
        ));
        let kind = form.schedule_kind;
        v_flex()
            .id("chat-tick-form")
            .flex_1()
            .overflow_y_scroll()
            .py_2()
            .gap_2()
            .child(machine)
            .when(form.editing.is_none(), |this| {
                this.child(field("ID", &form.id_editor))
            })
            .child(field("Prompt", &form.prompt_editor))
            .child(field("Directory", &form.cwd_editor))
            .child(schedule)
            .when(kind != ScheduleKind::Interval, |this| {
                this.child(field(
                    "Time (24-hour, machine's time zone)",
                    &form.time_editor,
                ))
            })
            .when(kind == ScheduleKind::Weekly, |this| {
                this.child(field("Days", &form.days_editor))
            })
            .when(kind == ScheduleKind::Interval, |this| {
                this.child(field("Every (minutes)", &form.minutes_editor))
            })
            .child(field("Model (optional)", &form.model_editor))
            .when(form.editing.is_none(), |this| {
                this.child(
                    div().px_3().child(
                        Checkbox::new("tick-form-enabled", ToggleState::from(form.enabled))
                            .label("Enable schedule")
                            .on_click(cx.listener(|this, _: &ToggleState, _, cx| {
                                if let Some(form) = this.form.as_mut() {
                                    form.enabled = !form.enabled;
                                    cx.notify();
                                }
                            })),
                    ),
                )
            })
            .when_some(form.error.clone(), |this, error| {
                this.child(
                    div()
                        .px_3()
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .child(
                h_flex()
                    .px_3()
                    .gap_1()
                    .child(
                        Button::new(
                            "tick-form-save",
                            match (form.saving, form.editing.is_some()) {
                                (true, _) => "Saving…",
                                (false, true) => "Save",
                                (false, false) => "Create Tick",
                            },
                        )
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Filled)
                        .disabled(form.saving)
                        .on_click(cx.listener(|this, _, _, cx| this.save_form(cx))),
                    )
                    .child(
                        Button::new("tick-form-cancel", "Cancel")
                            .label_size(LabelSize::Small)
                            .disabled(form.saving)
                            .on_click(cx.listener(|this, _, _, cx| this.close_tick(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_ticks(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(form) = &self.form {
            return self.render_form(form, cx);
        }
        if let Some(error) = &self.ticks_error {
            return div()
                .p_2()
                .child(Label::new(error.clone()).color(Color::Error))
                .into_any_element();
        }
        let Some(ticks) = &self.ticks else {
            return div()
                .p_2()
                .child(Label::new("Loading ticks…").color(Color::Muted))
                .into_any_element();
        };
        if let Some(key) = &self.selected_tick {
            let Some(job) = ticks.iter().find(|job| &job.key() == key) else {
                let message = match self.host_errors.iter().find(|(host, _)| host == &key.host) {
                    Some((_, error)) => error.clone(),
                    None => "This tick no longer exists.".into(),
                };
                return div()
                    .p_2()
                    .child(Label::new(message).color(Color::Muted))
                    .into_any_element();
            };
            return self.render_tick_detail(job, cx);
        }
        let show_host_headers = self.hosts.iter().any(TickHost::is_remote);
        let mut rows = Vec::new();
        for host in &self.hosts {
            if show_host_headers {
                rows.push(
                    div()
                        .px_3()
                        .pt_2()
                        .pb_1()
                        .child(
                            Label::new(host.label().to_string())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                );
            }
            if let Some((_, error)) = self.host_errors.iter().find(|(failed, _)| failed == host) {
                rows.push(
                    div()
                        .px_3()
                        .child(
                            Label::new(error.clone())
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                        .into_any_element(),
                );
                continue;
            }
            let host_jobs: Vec<_> = ticks.iter().filter(|job| &job.host == host).collect();
            if host_jobs.is_empty() {
                let message = if show_host_headers {
                    "No ticks on this machine."
                } else {
                    "No ticks yet. Ask Pi to schedule one with the tick_create tool."
                };
                rows.push(
                    div()
                        .px_3()
                        .child(
                            Label::new(message)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                );
                continue;
            }
            rows.extend(
                host_jobs
                    .into_iter()
                    .map(|job| self.render_tick_row(job, cx)),
            );
        }
        v_flex()
            .id("chat-ticks-list")
            .flex_1()
            .overflow_y_scroll()
            .py_1()
            .children(rows)
            .into_any_element()
    }

    fn render_tick_row(&self, job: &TickJob, cx: &mut Context<Self>) -> AnyElement {
        let last_run = job
            .last_run
            .as_ref()
            .map(|run| {
                let outcome = match (&run.error, run.exit_code) {
                    (Some(error), _) => error.clone(),
                    (None, Some(0)) => "ok".to_string(),
                    (None, Some(code)) => format!("exit {code}"),
                    (None, None) => "no exit code".to_string(),
                };
                format!("{} · {outcome}", format_utc_minute(&run.finished_at))
            })
            .unwrap_or_else(|| "Not run yet".to_string());
        let key = job.key();
        ListItem::new(SharedString::from(format!(
            "tick-{}-{}",
            job.host.label(),
            job.id
        )))
        .start_slot(Indicator::dot().color(tick_color(job)))
        .child(
            v_flex()
                .min_w_0()
                .child(Label::new(job.id.clone()).truncate())
                .child(
                    Label::new(format!("{} · {last_run}", job.schedule.describe()))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                ),
        )
        .end_slot(
            Label::new(job.status())
                .size(LabelSize::Small)
                .color(tick_color(job)),
        )
        .tooltip(Tooltip::text("Open Tick"))
        .on_click(cx.listener(move |this, _, _, cx| this.open_tick(key.clone(), cx)))
        .into_any_element()
    }

    fn render_tick_detail(&self, job: &TickJob, cx: &mut Context<Self>) -> AnyElement {
        let key = job.key();
        let busy = self.busy_ticks.contains(&key);
        let listener = |action: TickAction, key: TickKey| {
            cx.listener(move |this, _, _, cx| this.perform_tick_action(key.clone(), action, cx))
        };
        let toggle_label = if job.enabled { "Disable" } else { "Enable" };
        let toggle_action = if job.enabled {
            TickAction::Disable
        } else {
            TickAction::Enable
        };
        let actions = h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .flex_wrap()
            .child(
                Button::new("tick-toggle", toggle_label)
                    .label_size(LabelSize::Small)
                    .disabled(busy)
                    .on_click(listener(toggle_action, key.clone())),
            )
            .child(if job.running {
                Button::new("tick-kill", "Stop Run")
                    .label_size(LabelSize::Small)
                    .disabled(busy)
                    .on_click(listener(TickAction::Kill, key.clone()))
            } else {
                let key = key.clone();
                Button::new("tick-run", "Run Now")
                    .label_size(LabelSize::Small)
                    .disabled(busy || !job.enabled)
                    // pi-tick refuses to run disabled jobs.
                    .when(!job.enabled, |button| {
                        button.tooltip(Tooltip::text("Enable the tick to run it"))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.run_tick(key.clone(), cx)))
            })
            .child({
                let key = key.clone();
                Button::new("tick-edit", "Edit…")
                    .label_size(LabelSize::Small)
                    // Saving re-registers the schedule, which would stop a running agent.
                    .disabled(busy || job.running)
                    .when(job.running, |button| {
                        button.tooltip(Tooltip::text(
                            "Wait for the run to finish, or stop it, to edit",
                        ))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_form(Some(key.clone()), window, cx)
                    }))
            })
            .child({
                let key = key.clone();
                Button::new("tick-transcript", "Latest Transcript")
                    .label_size(LabelSize::Small)
                    .disabled(job.last_run.is_none())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_transcript(key.clone(), None, window, cx)
                    }))
            })
            .child({
                let key = key.clone();
                Button::new("tick-delete", "Delete…")
                    .label_size(LabelSize::Small)
                    .color(Color::Error)
                    .disabled(busy || job.running)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_delete_tick(key.clone(), window, cx)
                    }))
            });
        let detail_row = |label: &'static str, value: String| {
            h_flex()
                .px_3()
                .gap_2()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                .child(Label::new(value).size(LabelSize::Small).truncate())
        };
        v_flex()
            .id("chat-tick-detail")
            .flex_1()
            .overflow_y_scroll()
            .pb_2()
            .child(actions)
            .child(
                h_flex()
                    .px_3()
                    .gap_2()
                    .child(Indicator::dot().color(tick_color(job)))
                    .child(
                        Label::new(job.status())
                            .size(LabelSize::Small)
                            .color(tick_color(job)),
                    ),
            )
            .when(job.host.is_remote(), |this| {
                this.child(detail_row("Machine", job.host.label().to_string()))
            })
            .child(detail_row("Schedule", job.schedule.describe()))
            .child(detail_row("Directory", job.cwd.clone()))
            .when_some(job.model.clone(), |this, model| {
                this.child(detail_row("Model", model))
            })
            .when_some(
                job.last_run
                    .as_ref()
                    .and_then(|run| run.final_text_preview.clone()),
                |this, preview| {
                    this.child(section_header("Latest Result", cx)).child(
                        div()
                            .px_3()
                            .child(Label::new(preview).size(LabelSize::Small)),
                    )
                },
            )
            .child(section_header("Prompt", cx))
            .child(
                div().px_3().child(
                    Label::new(job.prompt.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(section_header("Recent Runs", cx))
            .children(self.render_tick_runs(&key, cx))
            .into_any_element()
    }

    fn render_tick_runs(&self, key: &TickKey, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if let Some(error) = &self.tick_runs_error {
            return vec![
                div()
                    .px_3()
                    .child(Label::new(error.clone()).color(Color::Error))
                    .into_any_element(),
            ];
        }
        if self.tick_runs.is_empty() {
            return vec![
                div()
                    .px_3()
                    .child(Label::new("No runs yet.").color(Color::Muted))
                    .into_any_element(),
            ];
        }
        self.tick_runs
            .iter()
            .map(|run| {
                let (outcome, color) = if run.succeeded() {
                    ("Completed".to_string(), Color::Success)
                } else {
                    let outcome = run.error.clone().unwrap_or_else(|| match run.exit_code {
                        Some(code) => format!("exit {code}"),
                        None => "no exit code".to_string(),
                    });
                    (outcome, Color::Error)
                };
                let key = key.clone();
                let run_id = run.run_id.clone();
                ListItem::new(SharedString::from(format!("tick-run-{}", run.run_id)))
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                Label::new(format!(
                                    "{} · {}",
                                    format_utc_minute(&run.started_at),
                                    run.trigger_kind
                                ))
                                .size(LabelSize::Small),
                            )
                            .when_some(run.final_text_preview.clone(), |this, preview| {
                                this.child(
                                    Label::new(preview)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            }),
                    )
                    .end_slot(Label::new(outcome).size(LabelSize::Small).color(color))
                    .tooltip(Tooltip::text("Open Transcript"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_transcript(key.clone(), Some(run_id.clone()), window, cx)
                    }))
                    .into_any_element()
            })
            .collect()
    }
}
fn single_line_editor(
    text: &str,
    placeholder: &str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_text(text, window, cx);
        editor
    })
}

fn tick_name(key: &TickKey) -> String {
    match &key.host {
        TickHost::Local => key.job_id.clone(),
        TickHost::Ssh(alias) => format!("{} on {alias}", key.job_id),
    }
}

fn tick_color(job: &TickJob) -> Color {
    if job.running {
        Color::Accent
    } else if job.enabled {
        Color::Success
    } else {
        Color::Muted
    }
}

fn format_utc_minute(timestamp: &str) -> String {
    timestamp
        .get(..16)
        .map(|minute| format!("{} UTC", minute.replace('T', " ")))
        .unwrap_or_else(|| timestamp.to_string())
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

fn show_error(workspace: &WeakEntity<Workspace>, message: String, cx: &mut App) {
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<TicksView>(), message),
                cx,
            );
        })
        .log_err();
}

impl Focusable for TicksView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TicksView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("chat-ticks")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .gap_1()
                            .min_w_0()
                            .when(
                                self.selected_tick.is_some() || self.form.is_some(),
                                |this| {
                                    this.child(
                                        IconButton::new("tick-back", IconName::ArrowLeft)
                                            .icon_size(IconSize::Small)
                                            .tooltip(Tooltip::text("Back to Ticks"))
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.close_tick(cx)),
                                            ),
                                    )
                                },
                            )
                            .child(
                                Label::new(match &self.form {
                                    Some(TickForm {
                                        editing: Some(key), ..
                                    }) => format!("Edit {}", tick_name(key)),
                                    Some(_) => "New Tick".to_string(),
                                    None => self
                                        .selected_tick
                                        .as_ref()
                                        .map(tick_name)
                                        .unwrap_or_else(|| "Ticks".into()),
                                })
                                .size(LabelSize::Small)
                                .truncate(),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .when_some(self.ticks.as_ref(), |this, ticks| {
                                this.child(
                                    Label::new(format!(
                                        "{} active",
                                        ticks.iter().filter(|job| job.enabled).count()
                                    ))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                )
                            })
                            .when(self.form.is_none(), |this| {
                                this.child(
                                    IconButton::new("ticks-new", IconName::Plus)
                                        .icon_size(IconSize::Small)
                                        .tooltip(Tooltip::text("New Tick"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_form(None, window, cx)
                                        })),
                                )
                            })
                            .child(
                                IconButton::new("ticks-refresh", IconName::RotateCw)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Refresh Ticks"))
                                    .on_click(cx.listener(|this, _, _, cx| this.reload_ticks(cx))),
                            ),
                    ),
            )
            .child(self.render_ticks(cx))
    }
}
