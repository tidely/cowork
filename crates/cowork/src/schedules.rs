//! The Scheduled page: the tasks that submit a saved prompt at a time or on
//! an interval, what each is doing, and the dialog creating and editing them
//! with their run history. See `docs/schedule.md` for what each option
//! means; `scheduler.rs` runs them.

use chrono::{Datelike as _, Duration, Local, NaiveDateTime, NaiveTime, Timelike as _, Weekday};
use gpui::{
    App, Context, Entity, Focusable as _, FontWeight, Hsla, IntoElement, Render, SharedString,
    Subscription, WeakEntity, Window, div, prelude::*, px,
};
use gpui_base::input::{InputEvent, InputState, TextareaState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IndexPath, Selectable as _, Sizable as _,
    WindowExt as _,
    button::{Button, ButtonGroup, ButtonVariants as _},
    date_picker::{DatePicker, DatePickerEvent, DatePickerState},
    dialog::{DialogDescription, DialogFooter, DialogHeader, DialogTitle},
    h_flex,
    input::{Input, Textarea},
    radio::Radio,
    searchable_list::SearchableVec,
    select::{Select, SelectState},
    switch::Switch,
    time_field::{TimeField, TimeFieldState, TimePrecision},
    v_flex,
};
use gpui_kit_assets::IconName as AssetIconName;
use uuid::Uuid;

use std::path::{Path, PathBuf};

use crate::{
    Cowork, MainStage,
    models::ModelRef,
    project_folders::{ProjectFolder, ProjectFolders, add_folders, remove_folder},
    project_mode::{ProjectMode, WRITE_WARNING},
    scheduled_task::{
        IntervalUnit, MissedRuns, RunRecord, RunResult, ScheduledTask, SkipReason, TaskSettings,
        ThreadMode, Trigger, When,
    },
    scheduler::TaskStatus,
};

const WEEK: [Weekday; 7] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
    Weekday::Sat,
    Weekday::Sun,
];

/// Runs listed in a task's dialog, newest first.
const SHOWN_RUNS: usize = 10;

/// A time relative to `now`: "Today 09:00", "Tomorrow 09:00", "Yesterday
/// 09:00", a weekday within the coming week, otherwise the date.
pub(crate) fn format_relative(at: NaiveDateTime, now: NaiveDateTime) -> String {
    let time = at.format("%H:%M");
    match (at.date() - now.date()).num_days() {
        0 => format!("Today {time}"),
        1 => format!("Tomorrow {time}"),
        -1 => format!("Yesterday {time}"),
        2..=6 => format!("{} {time}", at.format("%a")),
        _ if at.year() == now.year() => format!("{} {time}", at.format("%b %-d")),
        _ => format!("{} {time}", at.format("%b %-d, %Y")),
    }
}

fn trigger_label(trigger: Trigger) -> &'static str {
    match trigger {
        Trigger::Scheduled => "Scheduled",
        Trigger::CatchUp => "Catch-up",
        Trigger::Manual => "Run now",
    }
}

/// A run's result in a few words, and whether it is a problem.
fn result_label(result: &RunResult) -> (String, bool) {
    match result {
        RunResult::Running => ("Running".into(), false),
        RunResult::Completed => ("Completed".into(), false),
        RunResult::Stopped => ("Stopped".into(), false),
        RunResult::Failed(error) => (format!("Failed: {error}"), true),
        RunResult::Interrupted => ("Interrupted when Cowork quit".into(), true),
        RunResult::Skipped { count, reason } => {
            let runs = if *count == 1 {
                "Skipped".to_owned()
            } else {
                format!("Skipped {count} runs")
            };
            let why = match reason {
                SkipReason::Missed => "missed while closed or asleep",
                SkipReason::AlreadyQueued => "a run was already waiting",
                SkipReason::Paused => "paused",
            };
            (format!("{runs}: {why}"), false)
        }
    }
}

/// The most recent run that ran, for the row's "last run" line.
fn last_run(task: &ScheduledTask) -> Option<&RunRecord> {
    task.history()
        .iter()
        .rev()
        .find(|record| !matches!(record.result, RunResult::Skipped { .. }))
}

impl Cowork {
    pub(crate) fn open_schedules(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.main_stage = MainStage::Schedules;
        // The composer is hidden, so it must not keep taking keystrokes.
        window.blur(cx);
        cx.notify();
    }

    /// The models a task can pick, by name: the app's catalog, plus `current`
    /// if the catalog no longer offers it.
    fn schedule_models(&self, current: Option<&ModelRef>) -> Vec<(ModelRef, SharedString)> {
        let mut models: Vec<(ModelRef, SharedString)> = self
            .models
            .providers()
            .flat_map(|(provider, models)| {
                models.iter().map(move |(id, info)| {
                    let model = ModelRef {
                        provider,
                        id: id.clone(),
                    };
                    (model, SharedString::from(info.name.clone()))
                })
            })
            .collect();
        if let Some(current) = current
            && !models.iter().any(|(model, _)| model == current)
        {
            models.push((
                current.clone(),
                format!("{} (unavailable)", current.id).into(),
            ));
        }
        models
    }

    /// Opens the dialog creating a task, or with `task_id` editing one, and
    /// returns its form.
    pub(crate) fn open_schedule_editor(
        &mut self,
        task_id: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<ScheduleEditor>> {
        if window.has_active_dialog(cx) {
            return None;
        }
        let task = task_id.and_then(|id| self.scheduler.task(id).cloned());
        let (title, description) = if task.is_some() {
            ("Edit schedule", "Changes apply from the next run.")
        } else {
            (
                "New schedule",
                "Run a prompt at a set time or on an interval.",
            )
        };
        let models = self.schedule_models(task.as_ref().map(|task| &task.settings().model));
        let default_model = self.new_thread_model.clone();
        let cowork = cx.entity().downgrade();
        let editor = cx.new(|cx| {
            ScheduleEditor::new(cowork, task.as_ref(), models, default_model, window, cx)
        });
        let prompt = editor.read(cx).prompt.clone();
        let form = editor.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let editor = editor.clone();
            dialog
                .w(px(620.))
                .bg(cx.theme().popover)
                .content(move |content, _, _| {
                    // Allowed to be shorter than the form, so that in a small
                    // window its fields scroll and its buttons stay.
                    content
                        .min_h_0()
                        .overflow_hidden()
                        .child(
                            DialogHeader::new()
                                .child(DialogTitle::new().child(title))
                                .child(DialogDescription::new().child(description)),
                        )
                        .child(editor.clone())
                })
        });
        prompt.focus_handle(cx).focus(window, cx);
        Some(form)
    }

    pub(crate) fn render_schedules_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Local::now().naive_local();
        let tasks = &self.scheduler.tasks;
        let active = tasks.iter().filter(|task| task.is_active()).count();
        let summary = match (tasks.len(), active) {
            (0, _) => "No scheduled prompts".to_owned(),
            (total, active) if total == active => format!("{total} active"),
            (total, active) => format!("{active} active, {} paused", total - active),
        };

        let header = h_flex()
            .w_full()
            .items_end()
            .justify_between()
            .gap_3()
            .child(
                v_flex()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().secondary_foreground)
                            .child("Scheduled"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary),
                    ),
            )
            .child(
                Button::new("new-schedule")
                    .primary()
                    .small()
                    .icon(Icon::new(AssetIconName::Plus))
                    .label("New schedule")
                    .debug_selector(|| "new-schedule".to_owned())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_schedule_editor(None, window, cx);
                    })),
            );

        let list = if tasks.is_empty() {
            v_flex()
                .w_full()
                .py_12()
                .items_center()
                .gap_2()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(AssetIconName::CalendarClock).size_8())
                .child(div().text_sm().child("No scheduled prompts"))
                .child(
                    div()
                        .text_xs()
                        .child("Run a prompt every morning, every hour, or once later on."),
                )
                .into_any_element()
        } else {
            let rows = tasks
                .iter()
                .enumerate()
                .map(|(index, task)| self.render_scheduled_task(index, task, now, cx))
                .collect::<Vec<_>>();
            v_flex()
                .w_full()
                .rounded(cx.theme().radius_lg)
                .border_1()
                .border_color(cx.theme().border)
                .overflow_hidden()
                .children(rows)
                .into_any_element()
        };

        div()
            .id("schedules-page")
            .debug_selector(|| "schedules-page".to_owned())
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .rounded_tl(px(12.))
            .border_t_1()
            .border_l_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(
                div()
                    .id("schedules-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .mx_auto()
                            .w_full()
                            .max_w(px(720.))
                            .pt(px(48.))
                            .pb_6()
                            .px_6()
                            .gap_4()
                            .child(header)
                            .child(list),
                    ),
            )
    }

    fn render_scheduled_task(
        &self,
        index: usize,
        task: &ScheduledTask,
        now: NaiveDateTime,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let task_id = task.id;
        let settings = task.settings();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let status = self.task_status(task, cx);
        let (status_label, status_text, status_color): (&str, String, Option<Hsla>) = match status {
            TaskStatus::Running => ("Now", "Running".into(), Some(theme.primary)),
            TaskStatus::WaitingForApproval => ("Now", "Needs approval".into(), Some(theme.warning)),
            TaskStatus::WaitingForThread => ("Next run", "Waiting for its thread".into(), None),
            TaskStatus::WaitingForModels => ("Next run", "Waiting for models".into(), None),
            TaskStatus::Queued => ("Next run", "Queued".into(), None),
            TaskStatus::Due(at) if at <= now => ("Next run", "Due now".into(), None),
            TaskStatus::Due(at) => ("Next run", format_relative(at, now), None),
            TaskStatus::NoMoreRuns => ("Next run", "No more runs".into(), None),
            TaskStatus::Paused => ("Next run", "Paused".into(), None),
        };
        let last = last_run(task).map(|record| {
            let at = record.started_at.unwrap_or(record.scheduled_for);
            let (result, problem) = result_label(&record.result);
            let result = result.split(':').next().unwrap_or_default().to_owned();
            (
                format!("Last run {} \u{b7} {result}", format_relative(at, now)),
                problem,
            )
        });
        // The thread to open: the run's going on now, or the last one's.
        let last_thread = self
            .scheduler
            .run
            .filter(|run| run.task_id == task_id)
            .map(|run| run.thread_id)
            .or_else(|| last_run(task).and_then(|record| record.thread_id));
        let (open_thread, open_tooltip) = {
            let store = self.thread_store.read(cx);
            match last_thread {
                None => (None, "No thread yet. The first run creates one."),
                Some(id) if store.thread(id, cx).is_some() => (Some(id), "Open its thread"),
                Some(id) if store.archived_thread(id, cx).is_some() => {
                    (None, "Its thread is archived.")
                }
                Some(_) => (None, "Its thread was deleted."),
            }
        };
        // The buttons' icons have their own color, which would hide the
        // disabled look.
        let icon_color = |disabled: bool| if disabled { muted.opacity(0.4) } else { muted };
        let icon = match settings.when {
            When::Interval { .. } => AssetIconName::Repeat,
            When::Once(_) => AssetIconName::Clock,
            _ => AssetIconName::CalendarClock,
        };
        let mut folder_names = Vec::new();
        add_folders(&mut folder_names, settings.folders.clone());
        let project = (!folder_names.is_empty()).then(|| {
            let names = folder_names
                .iter()
                .map(|folder| folder.name.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            match settings.project_mode {
                ProjectMode::Write => format!("{names} (write)"),
                ProjectMode::Read => names,
            }
        });
        let details = [
            Some(settings.when.describe()),
            Some(
                self.models
                    .get(&settings.model)
                    .map_or_else(|| settings.model.id.clone(), |info| info.name.clone()),
            ),
            Some(settings.thread.label().to_owned()),
            project,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" \u{b7} ");
        let queued = task.queued().is_some();

        h_flex()
            .id(task_id)
            .debug_selector(move || format!("scheduled-task-{task_id}"))
            .w_full()
            .min_h(px(64.))
            .pl_4()
            .pr_2()
            .py_2()
            .gap_3()
            .cursor_pointer()
            .when(index > 0, |this| {
                this.border_t_1().border_color(theme.border)
            })
            .hover(|this| this.bg(theme.muted.opacity(0.5)))
            .when(!task.is_active(), |this| this.opacity(0.6))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_schedule_editor(Some(task_id), window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .size(px(32.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme.radius)
                    .bg(theme.muted)
                    .child(Icon::new(icon).size_4().text_color(muted)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(settings.title.clone()),
                    )
                    .child(div().truncate().text_xs().text_color(muted).child(details))
                    .children(last.map(|(line, problem)| {
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(if problem { theme.danger } else { muted })
                            .child(line)
                    })),
            )
            .child(
                v_flex()
                    .flex_none()
                    .items_end()
                    .child(div().text_xs().text_color(muted).child(status_label))
                    .child(
                        div()
                            .debug_selector(move || format!("scheduled-task-next-{task_id}"))
                            .text_sm()
                            .when_some(status_color, |this, color| this.text_color(color))
                            .child(status_text),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .child(
                        Button::new("open-scheduled-thread")
                            .ghost()
                            .small()
                            .icon(
                                Icon::new(AssetIconName::MessageSquare)
                                    .size_4()
                                    .text_color(icon_color(open_thread.is_none())),
                            )
                            .disabled(open_thread.is_none())
                            .debug_selector(move || format!("open-scheduled-thread-{task_id}"))
                            .accessibility_label("Open its thread")
                            .tooltip(open_tooltip)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if let Some(thread_id) = open_thread {
                                    this.open_thread(thread_id, window, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("run-scheduled-task")
                            .ghost()
                            .small()
                            .icon(
                                Icon::new(AssetIconName::Play)
                                    .size_4()
                                    .text_color(icon_color(queued)),
                            )
                            .disabled(queued)
                            .debug_selector(move || format!("run-scheduled-task-{task_id}"))
                            .accessibility_label("Run now")
                            .tooltip(if queued { "Already queued" } else { "Run now" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.run_scheduled_task_now(task_id, cx);
                            })),
                    )
                    .child(
                        // Its own element, so toggling doesn't also open the
                        // editor.
                        div()
                            .id("toggle-wrapper")
                            .debug_selector(move || format!("toggle-scheduled-task-{task_id}"))
                            .px_1()
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(
                                Switch::new("toggle-scheduled-task")
                                    .small()
                                    .checked(task.is_active())
                                    .tooltip(if task.is_active() { "Pause" } else { "Resume" })
                                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                        this.set_scheduled_task_active(task_id, *checked, cx);
                                    })),
                            ),
                    )
                    .child(
                        Button::new("delete-scheduled-task")
                            .ghost()
                            .small()
                            .icon(Icon::new(AssetIconName::Trash).size_4().text_color(muted))
                            .debug_selector(move || format!("delete-scheduled-task-{task_id}"))
                            .accessibility_label("Delete schedule")
                            .tooltip("Delete schedule")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.delete_scheduled_task(task_id, cx);
                            })),
                    ),
            )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WhenKind {
    Schedule,
    Interval,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Frequency {
    Once,
    Daily,
    Weekdays,
    Weekly,
    Monthly,
    Custom,
}

impl Frequency {
    const ALL: [Self; 6] = [
        Self::Once,
        Self::Daily,
        Self::Weekdays,
        Self::Weekly,
        Self::Monthly,
        Self::Custom,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Once => "Once",
            Self::Daily => "Daily",
            Self::Weekdays => "Weekdays",
            Self::Weekly => "Weekly",
            Self::Monthly => "Monthly",
            Self::Custom => "Custom",
        }
    }
}

type ModelSelect = SelectState<SearchableVec<SharedString>>;

/// The form inside the schedule dialog. Its own view, so that it redraws as
/// options change, including whether the task can be saved yet.
pub(crate) struct ScheduleEditor {
    cowork: WeakEntity<Cowork>,
    /// The task being edited; `None` while creating one.
    editing: Option<Uuid>,
    name: Entity<InputState>,
    prompt: Entity<TextareaState>,
    kind: WhenKind,
    frequency: Frequency,
    /// The date and time of a one-time task.
    once: Entity<DatePickerState>,
    /// The time of day of the other presets.
    time: Entity<TimeFieldState>,
    weekly_days: Vec<Weekday>,
    month_day: Entity<InputState>,
    rule: Entity<InputState>,
    /// The custom rule being edited and its start, kept while the rule is
    /// unchanged so that editing other options doesn't move it.
    custom: Option<(String, NaiveDateTime)>,
    every: Entity<InputState>,
    unit: IntervalUnit,
    interval_start: Entity<DatePickerState>,
    /// The models the select offers, in its order.
    models: Vec<ModelRef>,
    model: Entity<ModelSelect>,
    thread: ThreadMode,
    missed: MissedRuns,
    /// Named as each run's thread will name them.
    folders: Vec<ProjectFolder>,
    project_mode: ProjectMode,
    _subscriptions: Vec<Subscription>,
}

impl ScheduleEditor {
    fn new(
        cowork: WeakEntity<Cowork>,
        task: Option<&ScheduledTask>,
        models: Vec<(ModelRef, SharedString)>,
        default_model: Option<ModelRef>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let now = Local::now().naive_local();
        let nine = NaiveTime::from_hms_opt(9, 0, 0).expect("a valid time");
        let in_an_hour = now + Duration::hours(1);
        let next_hour = in_an_hour
            .date()
            .and_hms_opt(in_an_hour.hour(), 0, 0)
            .unwrap_or(now);
        let tomorrow_nine = (now.date() + Duration::days(1)).and_time(nine);
        let settings = task.map(ScheduledTask::settings);
        let when = settings.map(|settings| settings.when.clone());

        let (kind, frequency) = match &when {
            None | Some(When::Daily(_)) => (WhenKind::Schedule, Frequency::Daily),
            Some(When::Interval { .. }) => (WhenKind::Interval, Frequency::Daily),
            Some(When::Once(_)) => (WhenKind::Schedule, Frequency::Once),
            Some(When::Weekdays(_)) => (WhenKind::Schedule, Frequency::Weekdays),
            Some(When::Weekly { .. }) => (WhenKind::Schedule, Frequency::Weekly),
            Some(When::Monthly { .. }) => (WhenKind::Schedule, Frequency::Monthly),
            Some(When::Custom { .. }) => (WhenKind::Schedule, Frequency::Custom),
        };
        let time_of_day = match &when {
            Some(
                When::Daily(time)
                | When::Weekdays(time)
                | When::Weekly { time, .. }
                | When::Monthly { time, .. },
            ) => *time,
            _ => nine,
        };
        let once_at = match &when {
            Some(When::Once(at)) => *at,
            _ => tomorrow_nine,
        };
        let weekly_days = match &when {
            Some(When::Weekly { days, .. }) => days.clone(),
            _ => vec![Weekday::Mon],
        };
        let month_day = match &when {
            Some(When::Monthly { day, .. }) => day.to_string(),
            _ => "1".to_owned(),
        };
        let custom = match &when {
            Some(When::Custom { rule, start }) => Some((rule.clone(), *start)),
            _ => None,
        };
        let (every, unit, interval_start) = match &when {
            Some(When::Interval { every, unit, start }) => (every.to_string(), *unit, *start),
            _ => ("1".to_owned(), IntervalUnit::Hours, next_hour),
        };

        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Defaults to the start of the prompt")
                .default_value(settings.map(|s| s.title.clone()).unwrap_or_default())
        });
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder("What should the agent do on each run?")
                .default_value(settings.map(|s| s.prompt.clone()).unwrap_or_default())
        });
        let date_time_picker = |at: NaiveDateTime, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut picker =
                    DatePickerState::new(window, cx).time_precision(TimePrecision::Minute);
                picker.set_date_time(at, window, cx);
                picker
            })
        };
        let once = date_time_picker(once_at, window, cx);
        let interval_start = date_time_picker(interval_start, window, cx);
        let time = cx.new(|cx| {
            let mut field = TimeFieldState::new(window, cx).precision(TimePrecision::Minute);
            field.set_time(time_of_day, window, cx);
            field
        });
        let month_day = cx.new(|cx| InputState::new(window, cx).default_value(month_day));
        let rule = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("FREQ=MONTHLY;BYDAY=1MO;BYHOUR=9;BYMINUTE=0")
                .default_value(
                    custom
                        .as_ref()
                        .map(|(rule, _)| rule.clone())
                        .unwrap_or_default(),
                )
        });
        let every = cx.new(|cx| InputState::new(window, cx).default_value(every));
        let preferred = settings.map(|s| s.model.clone()).or(default_model);
        let selected_model = preferred
            .and_then(|preferred| models.iter().position(|(model, _)| *model == preferred))
            .or((!models.is_empty()).then_some(0));
        let (models, names): (Vec<ModelRef>, Vec<SharedString>) = models.into_iter().unzip();
        let model = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(names),
                selected_model.map(IndexPath::new),
                window,
                cx,
            )
        });

        let redraw_on_input = |input: &Entity<InputState>, cx: &mut Context<Self>| {
            cx.subscribe(input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            })
        };
        let redraw_on_date = |picker: &Entity<DatePickerState>, cx: &mut Context<Self>| {
            cx.subscribe(picker, |_, _, _: &DatePickerEvent, cx| cx.notify())
        };
        let subscriptions = vec![
            redraw_on_input(&name, cx),
            redraw_on_input(&month_day, cx),
            redraw_on_input(&rule, cx),
            redraw_on_input(&every, cx),
            cx.subscribe(&prompt, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            redraw_on_date(&once, cx),
            redraw_on_date(&interval_start, cx),
            // Re-renders the save button once a model is picked.
            cx.observe(&model, |_, _, cx| cx.notify()),
        ];

        Self {
            cowork,
            editing: task.map(|task| task.id),
            name,
            prompt,
            kind,
            frequency,
            once,
            time,
            weekly_days,
            month_day,
            rule,
            custom,
            every,
            unit,
            interval_start,
            models,
            model,
            thread: settings.map_or(ThreadMode::NewEachRun, |s| s.thread),
            missed: settings.map_or(MissedRuns::RunOnce, |s| s.missed),
            folders: {
                let mut folders = Vec::new();
                add_folders(
                    &mut folders,
                    settings.map(|s| s.folders.clone()).unwrap_or_default(),
                );
                folders
            },
            project_mode: settings.map_or(ProjectMode::Read, |s| s.project_mode),
            _subscriptions: subscriptions,
        }
    }

    fn pick_folders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Add to project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let paths = match selected.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    eprintln!("could not choose project folders: {error:#}");
                    return;
                }
            };
            _ = this.update(cx, |this, cx| this.add_folders(paths, cx));
        })
        .detach();
    }

    pub(crate) fn add_folders(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if add_folders(&mut self.folders, paths) {
            cx.notify();
        }
    }

    fn remove_folder(&mut self, path: &Path, cx: &mut Context<Self>) {
        if remove_folder(&mut self.folders, path) {
            cx.notify();
        }
    }

    fn render_project(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let folders = ProjectFolders::new("schedule-folders", self.folders.clone()).editable(
            cx.listener(|this, _: &gpui::ClickEvent, window, cx| this.pick_folders(window, cx)),
            |path, _, cx| cx.open_with_system(path),
            cx.listener(|this, path: &Path, _, cx| this.remove_folder(path, cx)),
        );
        let modes = ButtonGroup::new("schedule-project-mode")
            .outline()
            .small()
            .children(ProjectMode::ALL.into_iter().map(|mode| {
                Button::new(mode.label())
                    .label(mode.label())
                    .selected(self.project_mode == mode)
                    .debug_selector(move || format!("schedule-project-{}", mode.label()))
            }))
            .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                if let Some(mode) = clicked.first().and_then(|ix| ProjectMode::ALL.get(*ix)) {
                    this.project_mode = *mode;
                    cx.notify();
                }
            }));
        let hint = if self.folders.is_empty() {
            "Folders each run's agent can work in, in its sandbox at /projects.".to_owned()
        } else if self.project_mode == ProjectMode::Write {
            format!("Runs can change these folders. {WRITE_WARNING}")
        } else {
            "Runs can read these folders, but not change them.".to_owned()
        };
        Self::field("Project", cx)
            .child(
                h_flex()
                    .gap_3()
                    .justify_between()
                    .child(div().min_w_0().flex_1().child(folders))
                    .when(!self.folders.is_empty(), |this| this.child(modes)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(
                        if self.project_mode == ProjectMode::Write && !self.folders.is_empty() {
                            cx.theme().warning
                        } else {
                            cx.theme().muted_foreground
                        },
                    )
                    .child(hint),
            )
    }

    /// The schedule the form describes, or what to fix in it.
    fn when(&self, cx: &App) -> Result<When, String> {
        let time = self.time.read(cx).time();
        let when = match (self.kind, self.frequency) {
            (WhenKind::Interval, _) => When::Interval {
                every: self
                    .every
                    .read(cx)
                    .value()
                    .trim()
                    .parse()
                    .map_err(|_| "Enter a whole number above zero.")?,
                unit: self.unit,
                start: self
                    .interval_start
                    .read(cx)
                    .date_time()
                    .start()
                    .ok_or("Pick when to start.")?,
            },
            (WhenKind::Schedule, Frequency::Once) => When::Once(
                self.once
                    .read(cx)
                    .date_time()
                    .start()
                    .ok_or("Pick a date and time.")?,
            ),
            (WhenKind::Schedule, Frequency::Daily) => When::Daily(time),
            (WhenKind::Schedule, Frequency::Weekdays) => When::Weekdays(time),
            (WhenKind::Schedule, Frequency::Weekly) => When::Weekly {
                days: self.weekly_days.clone(),
                time,
            },
            (WhenKind::Schedule, Frequency::Monthly) => When::Monthly {
                day: self
                    .month_day
                    .read(cx)
                    .value()
                    .trim()
                    .parse()
                    .map_err(|_| "Enter a day from 1 to 31.")?,
                time,
            },
            (WhenKind::Schedule, Frequency::Custom) => {
                let rule = self.rule.read(cx).value().trim().to_owned();
                let start = match &self.custom {
                    Some((original, start)) if *original == rule => *start,
                    _ => {
                        let now = Local::now().naive_local();
                        now.with_second(0)
                            .unwrap_or(now)
                            .with_nanosecond(0)
                            .unwrap_or(now)
                    }
                };
                When::Custom { rule, start }
            }
        };
        when.validate().map(|()| when)
    }

    /// The settings the form describes, or what keeps them from being saved.
    fn settings(&self, cx: &App) -> Result<TaskSettings, String> {
        let prompt = self.prompt.read(cx).value().trim().to_owned();
        let when = self.when(cx);
        let model = self
            .model
            .read(cx)
            .selected_index(cx)
            .and_then(|index| self.models.get(index.row).cloned());
        let model = match (model, self.models.is_empty()) {
            (Some(model), _) => model,
            (None, true) => return Err("No models found. Set up a provider first.".into()),
            (None, false) => return Err("Pick a model.".into()),
        };
        let when = when?;
        if prompt.is_empty() {
            return Err(String::new());
        }
        let name = self.name.read(cx).value();
        let title = match name.trim() {
            "" => Cowork::thread_title(&prompt),
            name => name.to_owned(),
        };
        Ok(TaskSettings {
            title,
            prompt,
            when,
            model,
            thread: self.thread,
            missed: self.missed,
            folders: self
                .folders
                .iter()
                .filter_map(|folder| folder.path.clone())
                .collect(),
            project_mode: self.project_mode,
        })
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(settings) = self.settings(cx) else {
            return;
        };
        let task_id = self.editing;
        if self
            .cowork
            .update(cx, |cowork, cx| {
                cowork.save_scheduled_task(task_id, settings, cx)
            })
            .is_ok()
        {
            window.close_dialog(cx);
        }
    }

    fn toggle_weekday(&mut self, day: Weekday, cx: &mut Context<Self>) {
        if let Some(index) = self.weekly_days.iter().position(|chosen| *chosen == day) {
            self.weekly_days.remove(index);
        } else {
            self.weekly_days.push(day);
            self.weekly_days
                .sort_by_key(|day| day.num_days_from_monday());
        }
        cx.notify();
    }

    fn field(label: &'static str, cx: &App) -> gpui::Div {
        v_flex().gap_1p5().child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(cx.theme().secondary_foreground)
                .child(label),
        )
    }

    fn hint(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text.into())
    }

    fn error(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
        div()
            .debug_selector(|| "schedule-error".to_owned())
            .text_xs()
            .text_color(cx.theme().danger)
            .child(text.into())
    }

    fn render_when(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = ButtonGroup::new("when-kind")
            .outline()
            .small()
            .child(
                Button::new("when-schedule")
                    .label("On a schedule")
                    .selected(self.kind == WhenKind::Schedule)
                    .debug_selector(|| "when-schedule".to_owned()),
            )
            .child(
                Button::new("when-interval")
                    .label("On an interval")
                    .selected(self.kind == WhenKind::Interval)
                    .debug_selector(|| "when-interval".to_owned()),
            )
            .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                this.kind = if clicked.contains(&1) {
                    WhenKind::Interval
                } else {
                    WhenKind::Schedule
                };
                cx.notify();
            }));

        let details = match self.kind {
            WhenKind::Interval => self.render_interval(cx).into_any_element(),
            WhenKind::Schedule => self.render_schedule(cx).into_any_element(),
        };
        let problem = self.when(cx).err();
        Self::field("When", cx)
            .child(kind)
            .child(details)
            .children(problem.map(|problem| Self::error(problem, cx)))
    }

    fn render_interval(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let units = [
            IntervalUnit::Minutes,
            IntervalUnit::Hours,
            IntervalUnit::Days,
        ];
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().child("Every"))
                    .child(
                        Input::new(&self.every)
                            .id("interval-every")
                            .small()
                            .w(px(64.)),
                    )
                    .child(
                        ButtonGroup::new("interval-unit")
                            .outline()
                            .small()
                            .children(units.into_iter().map(|unit| {
                                Button::new(unit.name(2))
                                    .label(unit.name(2))
                                    .selected(self.unit == unit)
                            }))
                            .on_click(cx.listener(move |this, clicked: &Vec<usize>, _, cx| {
                                if let Some(unit) = clicked.first().and_then(|ix| units.get(*ix)) {
                                    this.unit = *unit;
                                    cx.notify();
                                }
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().child("Starting"))
                    .child(DatePicker::new(&self.interval_start).small().w(px(220.))),
            )
            .child(Self::hint(
                "Counted from the start, so a slow run doesn't push the next ones back.",
                cx,
            ))
    }

    fn render_schedule(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let frequencies = ButtonGroup::new("frequency")
            .outline()
            .small()
            .children(Frequency::ALL.into_iter().map(|frequency| {
                Button::new(frequency.label())
                    .label(frequency.label())
                    .selected(self.frequency == frequency)
                    .debug_selector(move || format!("frequency-{}", frequency.label()))
            }))
            .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                if let Some(frequency) = clicked.first().and_then(|ix| Frequency::ALL.get(*ix)) {
                    this.frequency = *frequency;
                    cx.notify();
                }
            }));

        let at_time = || {
            h_flex()
                .gap_2()
                .child(div().text_sm().child("At"))
                .child(TimeField::new(&self.time).small())
        };
        let details = match self.frequency {
            Frequency::Once => h_flex()
                .gap_2()
                .child(div().text_sm().child("On"))
                .child(DatePicker::new(&self.once).small().w(px(220.)))
                .into_any_element(),
            Frequency::Daily | Frequency::Weekdays => at_time().into_any_element(),
            Frequency::Weekly => v_flex()
                .gap_2()
                .child(h_flex().gap_1().children(WEEK.into_iter().map(|day| {
                    Button::new(SharedString::from(format!("weekday-{day}")))
                        .outline()
                        .small()
                        .label(day.to_string())
                        .selected(self.weekly_days.contains(&day))
                        .debug_selector(move || format!("weekday-{day}"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_weekday(day, cx);
                        }))
                })))
                .child(at_time())
                .into_any_element(),
            Frequency::Monthly => v_flex()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().text_sm().child("On day"))
                        .child(
                            Input::new(&self.month_day)
                                .id("month-day")
                                .small()
                                .w(px(64.)),
                        )
                        .child(at_time()),
                )
                .child(Self::hint("Months without this day are skipped.", cx))
                .into_any_element(),
            Frequency::Custom => v_flex()
                .gap_2()
                .child(Input::new(&self.rule).id("custom-rule").small())
                .child(Self::hint(
                    "An RFC 5545 recurrence rule, counted from when you save it. Times are local.",
                    cx,
                ))
                .into_any_element(),
        };
        v_flex().gap_2().child(frequencies).child(details)
    }

    fn render_choice<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        current: T,
        choices: [(T, &'static str, &'static str); 2],
        set: fn(&mut Self, T),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .gap_2()
            .children(
                choices
                    .into_iter()
                    .enumerate()
                    .map(|(index, (value, label, hint))| {
                        h_flex()
                            .items_start()
                            .gap_2()
                            .child(
                                Radio::new(SharedString::from(format!("{id}-{index}")))
                                    .checked(current == value)
                                    .debug_selector(move || format!("{id}-{index}"))
                                    .on_click(cx.listener(move |this, _: &bool, _, cx| {
                                        set(this, value);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(div().text_sm().line_height(px(16.)).child(label))
                                    .child(Self::hint(hint, cx)),
                            )
                    }),
            )
    }

    /// The task's latest runs, newest first, each opening its thread while
    /// that is in the sidebar.
    fn render_history(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let task_id = self.editing?;
        let cowork = self.cowork.upgrade()?;
        let now = Local::now().naive_local();
        let (records, open): (Vec<RunRecord>, Vec<bool>) = {
            let cowork = cowork.read(cx);
            let task = cowork.scheduler.task(task_id)?;
            let store = cowork.thread_store.read(cx);
            task.history()
                .iter()
                .rev()
                .take(SHOWN_RUNS)
                .map(|record| {
                    let open = record
                        .thread_id
                        .is_some_and(|thread_id| store.thread(thread_id, cx).is_some());
                    (record.clone(), open)
                })
                .unzip()
        };
        let theme = cx.theme();
        let list = if records.is_empty() {
            Self::hint("No runs yet.", cx).into_any_element()
        } else {
            v_flex()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .children(records.into_iter().zip(open).enumerate().map(
                    |(index, (record, open))| {
                        let at = record.started_at.unwrap_or(record.scheduled_for);
                        let (result, problem) = result_label(&record.result);
                        let thread_id = record.thread_id;
                        let cowork = self.cowork.clone();
                        h_flex()
                            .id(SharedString::from(format!("run-{}", record.id)))
                            .debug_selector(move || format!("run-record-{index}"))
                            .min_h(px(32.))
                            .px_2()
                            .gap_2()
                            .text_xs()
                            .when(index > 0, |this| {
                                this.border_t_1().border_color(theme.border)
                            })
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(110.))
                                    .child(format_relative(at, now)),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(64.))
                                    .text_color(theme.muted_foreground)
                                    .child(trigger_label(record.trigger)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .when(problem, |this| this.text_color(theme.danger))
                                    .child(result),
                            )
                            .when(open, |this| {
                                this.child(
                                    Button::new("open-run-thread")
                                        .ghost()
                                        .xsmall()
                                        .label("Open")
                                        .on_click(move |_, window, cx| {
                                            let Some(thread_id) = thread_id else {
                                                return;
                                            };
                                            window.close_dialog(cx);
                                            _ = cowork.update(cx, |cowork, cx| {
                                                cowork.open_thread(thread_id, window, cx);
                                            });
                                        }),
                                )
                            })
                    },
                ))
                .into_any_element()
        };
        Some(Self::field("Recent runs", cx).child(list))
    }
}

impl Render for ScheduleEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.settings(cx);
        let can_save = settings.is_ok();
        let model_problem = settings
            .err()
            .filter(|problem| problem.contains("model") || problem.contains("provider"));
        let save_label = if self.editing.is_some() {
            "Save"
        } else {
            "Create schedule"
        };

        let fields = v_flex()
            .id("schedule-editor-fields")
            .debug_selector(|| "schedule-editor-fields".to_owned())
            // Shrinks below its tallest in a short window, scrolling instead.
            .max_h(px(560.))
            .min_h_0()
            .flex_shrink(1.)
            .overflow_y_scroll()
            .gap_4()
            .pr_1()
            .child(
                Self::field("Prompt", cx).child(Textarea::new(&self.prompt).aria_label("Prompt")),
            )
            .child(
                h_flex()
                    .items_start()
                    .gap_4()
                    .child(
                        Self::field("Name", cx)
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.name).id("schedule-name").small()),
                    )
                    .child(
                        Self::field("Model", cx)
                            .w(px(200.))
                            .flex_none()
                            .child(
                                Select::new(&self.model)
                                    .id("schedule-model")
                                    .small()
                                    .placeholder("No models")
                                    .accessibility_label("Model"),
                            )
                            .children(model_problem.map(|problem| Self::error(problem, cx))),
                    ),
            )
            .child(self.render_when(cx))
            .child(self.render_project(cx))
            .child(
                h_flex()
                    .items_start()
                    .gap_4()
                    .child(
                        Self::field("Thread", cx)
                            .flex_1()
                            .min_w_0()
                            .child(self.render_choice(
                                "thread-mode",
                                self.thread,
                                [
                                    (
                                        ThreadMode::NewEachRun,
                                        "New thread each run",
                                        "Starts fresh with only the prompt.",
                                    ),
                                    (
                                        ThreadMode::ContinueLast,
                                        "Continue the last thread",
                                        "Adds to one thread, seeing its history.",
                                    ),
                                ],
                                |this, thread| this.thread = thread,
                                cx,
                            )),
                    )
                    .child(
                        Self::field("Missed runs", cx)
                            .flex_1()
                            .min_w_0()
                            .child(self.render_choice(
                                "missed-runs",
                                self.missed,
                                [
                                    (
                                        MissedRuns::RunOnce,
                                        "Run once when available",
                                        "Catches up with one run after sleep.",
                                    ),
                                    (
                                        MissedRuns::Skip,
                                        "Skip",
                                        "Waits for the next scheduled time.",
                                    ),
                                ],
                                |this, missed| this.missed = missed,
                                cx,
                            )),
                    ),
            )
            .child(Self::field("Tool calls", cx).child(Self::hint(
                "Calls that need permission fail the first time, as nobody may be watching. If the agent makes one again, you're notified and asked.",
                cx,
            )))
            .children(self.render_history(cx));

        v_flex().flex_1().min_h_0().gap_4().child(fields).child(
            DialogFooter::new()
                .flex_none()
                .child(
                    Button::new("cancel-schedule")
                        .outline()
                        .label("Cancel")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    Button::new("save-schedule")
                        .primary()
                        .label(save_label)
                        .disabled(!can_save)
                        .debug_selector(|| "save-schedule".to_owned())
                        .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduled_task::tests::{SATURDAY, at};

    #[test]
    fn times_read_relative_to_now() {
        let now = at(SATURDAY, 10, 0);
        assert_eq!(format_relative(at(SATURDAY, 11, 0), now), "Today 11:00");
        assert_eq!(
            format_relative(at((2026, 10, 11), 9, 0), now),
            "Tomorrow 09:00"
        );
        assert_eq!(
            format_relative(at((2026, 10, 9), 9, 0), now),
            "Yesterday 09:00"
        );
        assert_eq!(format_relative(at((2026, 10, 14), 9, 0), now), "Wed 09:00");
        assert_eq!(format_relative(at((2026, 11, 1), 9, 0), now), "Nov 1 09:00");
        assert_eq!(
            format_relative(at((2027, 1, 1), 9, 0), now),
            "Jan 1, 2027 09:00"
        );
    }

    #[test]
    fn run_results_read_in_a_few_words() {
        assert_eq!(
            result_label(&RunResult::Skipped {
                count: 3,
                reason: SkipReason::Missed
            }),
            (
                "Skipped 3 runs: missed while closed or asleep".into(),
                false
            )
        );
        assert_eq!(
            result_label(&RunResult::Failed("no model".into())),
            ("Failed: no model".into(), true)
        );
    }
}
