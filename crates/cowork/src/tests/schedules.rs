//! The Scheduled page, the dialog creating and editing its tasks, and
//! running them.

use chrono::{NaiveDateTime, NaiveTime, Weekday};
use gpui_component::WindowExt as _;

use super::*;
use crate::{
    project_mode::ProjectMode,
    scheduled_task::{
        IntervalUnit, MissedRuns, RunResult, ScheduledTask, TaskSettings, ThreadMode, Trigger,
        When,
        tests::{SATURDAY, at, settings, time},
    },
    scheduler::TaskStatus,
    submission::RunTotals,
};

/// Folders that exist, named `names`, under a fresh temporary directory.
fn temp_folders(names: &[&str]) -> Vec<PathBuf> {
    let root = std::env::temp_dir().join(format!("cowork-schedule-{}", Uuid::new_v4()));
    names
        .iter()
        .map(|name| {
            let path = root.join(name);
            std::fs::create_dir_all(&path).expect("a temporary folder");
            path
        })
        .collect()
}

/// The local paths of `thread_id`'s project, its mode, and what its sandbox
/// would mount.
fn thread_project(
    cowork: &Entity<Cowork>,
    thread_id: Uuid,
    cx: &mut gpui::VisualTestContext,
) -> (Vec<PathBuf>, ProjectMode, sandbox::Project) {
    cowork.read_with(cx, |cowork, cx| {
        let thread = cowork.thread_store.read(cx).thread(thread_id, cx).unwrap();
        let thread = thread.read(cx);
        (
            thread
                .project_folders()
                .iter()
                .filter_map(|folder| folder.path.clone())
                .collect(),
            thread.project_mode(),
            cowork.sandboxes.project(&thread_id.to_string()),
        )
    })
}

fn selector(name: &str, id: Uuid) -> &'static str {
    Box::leak(format!("{name}-{id}").into_boxed_str())
}

/// Draws a frame once everything settled, including the dialog's opening
/// animation, which otherwise moves buttons between frames.
fn draw(cx: &mut gpui::VisualTestContext) {
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}

fn click(selector: &'static str, cx: &mut gpui::VisualTestContext) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} should be drawn"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    draw(cx);
}

fn dialog_open(cx: &mut gpui::VisualTestContext) -> bool {
    cx.update(|window, cx| window.has_active_dialog(cx))
}

fn tasks(cowork: &Entity<Cowork>, cx: &mut gpui::VisualTestContext) -> Vec<ScheduledTask> {
    cowork.read_with(cx, |cowork, _| cowork.scheduler.tasks.clone())
}

fn task(cowork: &Entity<Cowork>, task_id: Uuid, cx: &mut gpui::VisualTestContext) -> ScheduledTask {
    cowork.read_with(cx, |cowork, _| {
        cowork.scheduler.task(task_id).expect("the task").clone()
    })
}

/// Adds a task created at `created`, so occurrences after it are due.
fn add_task(
    cowork: &Entity<Cowork>,
    settings: TaskSettings,
    created: NaiveDateTime,
    cx: &mut gpui::VisualTestContext,
) -> Uuid {
    cowork.update(cx, |cowork, cx| {
        let task = ScheduledTask::new(settings, created);
        let id = task.id;
        cowork.scheduler.tasks.push(task);
        cx.notify();
        id
    })
}

fn daily(title: &str, hour: u32, minute: u32) -> TaskSettings {
    TaskSettings {
        title: title.into(),
        prompt: format!("{title} prompt"),
        ..settings(When::Daily(time(hour, minute)), MissedRuns::RunOnce)
    }
}

fn tick(cowork: &Entity<Cowork>, now: NaiveDateTime, cx: &mut gpui::VisualTestContext) {
    cowork.update(cx, |cowork, cx| cowork.tick_schedules(now, cx));
    draw(cx);
}

/// The thread the run going on now is in.
fn running_thread(cowork: &Entity<Cowork>, cx: &mut gpui::VisualTestContext) -> Option<Uuid> {
    cowork.read_with(cx, |cowork, _| {
        cowork.scheduler.run.map(|run| run.thread_id)
    })
}

/// Ends the agent run of `thread_id` the way a finished run does.
fn finish(
    cowork: &Entity<Cowork>,
    thread_id: Uuid,
    outcome: protocol::RunOutcome,
    cx: &mut gpui::VisualTestContext,
) {
    cowork.update(cx, |cowork, cx| {
        let thread = cowork.thread_store.read(cx).thread(thread_id, cx).unwrap();
        let message_id = cowork.active_generations[&thread_id].message_id();
        cowork.finish_generation(
            &thread,
            message_id,
            outcome,
            RunTotals {
                started_at: SystemTime::now(),
                duration: Duration::from_secs(1),
                usage: Usage::new(),
            },
            cx,
        );
    });
    draw(cx);
}

/// The prompts submitted to `thread_id`, in order.
fn prompts(
    cowork: &Entity<Cowork>,
    thread_id: Uuid,
    cx: &mut gpui::VisualTestContext,
) -> Vec<String> {
    cowork.read_with(cx, |cowork, cx| {
        let thread = cowork.thread_store.read(cx).thread(thread_id, cx).unwrap();
        thread
            .read(cx)
            .timeline
            .iter()
            .filter_map(|message| match message {
                TimelineMessage::User(group) => Some(group.blocks[0].text.clone()),
                TimelineMessage::Agent(_) => None,
            })
            .collect()
    })
}

fn open_schedules_page(cowork: &Entity<Cowork>, cx: &mut gpui::VisualTestContext) {
    cx.update(|window, cx| cowork.update(cx, |cowork, cx| cowork.open_schedules(window, cx)));
    draw(cx);
}

#[gpui::test]
fn the_scheduled_page_lists_every_task(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let now = chrono::Local::now().naive_local();
    for title in ["Morning", "Evening"] {
        add_task(&cowork, daily(title, 9, 0), now, cx);
    }
    open_schedules_page(&cowork, cx);
    assert_eq!(
        cowork.read_with(cx, |cowork, _| cowork.main_stage),
        MainStage::Schedules
    );
    for task in tasks(&cowork, cx) {
        let row = cx
            .debug_bounds(selector("scheduled-task", task.id))
            .expect("every task has a row");
        let next = cx
            .debug_bounds(selector("scheduled-task-next", task.id))
            .expect("every row shows its next run");
        assert!(next.right() <= row.right() && next.left() > row.center().x);
    }
}

#[gpui::test]
fn a_new_schedule_is_created_from_the_dialog(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    open_schedules_page(&cowork, cx);

    click("new-schedule", cx);
    assert!(dialog_open(cx));
    // Nothing to run yet, so it can't be saved.
    click("save-schedule", cx);
    assert!(dialog_open(cx));
    assert!(tasks(&cowork, cx).is_empty());

    // The prompt has focus as the dialog opens.
    cx.simulate_input("Summarize   my inbox");
    draw(cx);
    click("frequency-Weekdays", cx);
    // Every option fits in the dialog without scrolling, above its buttons.
    let save = cx.debug_bounds("save-schedule").unwrap();
    for option in ["thread-mode-1", "missed-runs-1"] {
        let bounds = cx.debug_bounds(option).unwrap();
        assert!(
            bounds.bottom() < save.top(),
            "{option} {bounds:?} is cut off"
        );
    }
    click("thread-mode-1", cx);
    click("missed-runs-1", cx);
    click("save-schedule", cx);

    assert!(!dialog_open(cx));
    let [task] = tasks(&cowork, cx).try_into().expect("one task");
    let settings = task.settings();
    assert_eq!(settings.title, "Summarize my inbox");
    assert_eq!(settings.prompt, "Summarize   my inbox");
    assert_eq!(
        settings.when,
        When::Weekdays(NaiveTime::from_hms_opt(9, 0, 0).unwrap())
    );
    // The model new threads start with.
    assert_eq!(settings.model, ollama_qwen());
    assert_eq!(settings.thread, ThreadMode::ContinueLast);
    assert_eq!(settings.missed, MissedRuns::Skip);
    assert!(task.is_active());
    assert!(
        cx.debug_bounds(selector("scheduled-task", task.id))
            .is_some()
    );
}

#[gpui::test]
fn the_dialog_says_what_keeps_a_schedule_from_saving(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    open_schedules_page(&cowork, cx);
    click("new-schedule", cx);
    cx.simulate_input("Weekly review");
    draw(cx);

    click("frequency-Weekly", cx);
    // Monday is picked to start with; without it nothing is.
    click("weekday-Mon", cx);
    assert!(cx.debug_bounds("schedule-error").is_some());
    click("save-schedule", cx);
    assert!(dialog_open(cx));

    click("weekday-Fri", cx);
    click("weekday-Tue", cx);
    assert!(cx.debug_bounds("schedule-error").is_none());
    click("save-schedule", cx);
    assert!(!dialog_open(cx));
    let [task] = tasks(&cowork, cx).try_into().expect("one task");
    assert_eq!(
        task.settings().when,
        When::Weekly {
            days: vec![Weekday::Tue, Weekday::Fri],
            time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        }
    );
}

#[gpui::test]
fn tasks_are_edited_paused_and_deleted_from_their_rows(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let now = chrono::Local::now().naive_local();
    let [edited, paused, deleted, kept] = ["Edited", "Paused", "Deleted", "Kept"]
        .map(|title| add_task(&cowork, daily(title, 9, 0), now, cx));
    open_schedules_page(&cowork, cx);

    // Clicking a row edits its task, starting from what it is.
    click(selector("scheduled-task", edited), cx);
    assert!(dialog_open(cx));
    click("when-interval", cx);
    click("save-schedule", cx);
    assert!(!dialog_open(cx));
    let saved = task(&cowork, edited, cx);
    assert_eq!(saved.settings().title, "Edited");
    assert_eq!(saved.settings().prompt, "Edited prompt");
    assert!(matches!(
        saved.settings().when,
        When::Interval {
            every: 1,
            unit: IntervalUnit::Hours,
            ..
        }
    ));

    // Toggling and deleting don't open the editor.
    click(selector("toggle-scheduled-task", paused), cx);
    click(selector("delete-scheduled-task", deleted), cx);
    assert!(!dialog_open(cx));
    let ids: Vec<_> = tasks(&cowork, cx).iter().map(|task| task.id).collect();
    assert_eq!(ids, [edited, paused, kept]);
    assert!(!task(&cowork, paused, cx).is_active());
    assert!(
        cx.debug_bounds(selector("scheduled-task", deleted))
            .is_none()
    );
}

#[gpui::test]
fn a_due_run_starts_in_a_new_thread_and_records_how_it_ended(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let task_id = add_task(
        &cowork,
        daily("Morning briefing", 9, 0),
        at(SATURDAY, 8, 0),
        cx,
    );

    tick(&cowork, at(SATURDAY, 8, 59), cx);
    assert_eq!(running_thread(&cowork, cx), None);

    tick(&cowork, at(SATURDAY, 9, 0), cx);
    let thread_id = running_thread(&cowork, cx).expect("the run started");
    cowork.read_with(cx, |cowork, cx| {
        let thread = cowork.thread_store.read(cx).thread(thread_id, cx).unwrap();
        let thread = thread.read(cx);
        assert_eq!(thread.summary.title, "Morning briefing");
        assert_eq!(thread.model(), Some(&ollama_qwen()));
        assert!(thread.generating);
        // The user's own new-thread draft is untouched, and stays open.
        assert_eq!(cowork.active_thread_id, None);
        let task = cowork.scheduler.task(task_id).unwrap();
        assert_eq!(cowork.task_status(task, cx), TaskStatus::Running);
    });
    assert_eq!(prompts(&cowork, thread_id, cx), ["Morning briefing prompt"]);
    let record = task(&cowork, task_id, cx).history()[0].clone();
    assert_eq!(record.trigger, Trigger::Scheduled);
    assert_eq!(record.result, RunResult::Running);
    assert_eq!(record.thread_id, Some(thread_id));

    finish(&cowork, thread_id, protocol::RunOutcome::Completed, cx);
    assert_eq!(running_thread(&cowork, cx), None);
    let task = task(&cowork, task_id, cx);
    assert_eq!(task.history()[0].result, RunResult::Completed);
    cowork.read_with(cx, |cowork, cx| {
        assert_eq!(
            cowork.task_status(&task, cx),
            TaskStatus::Due(at((2026, 10, 11), 9, 0))
        );
    });
}

#[gpui::test]
fn runs_go_one_at_a_time_in_the_order_they_were_due(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let created = at(SATURDAY, 8, 0);
    // Created first, but due last.
    let later = add_task(&cowork, daily("Later", 9, 1), created, cx);
    let earlier = add_task(&cowork, daily("Earlier", 9, 0), created, cx);

    tick(&cowork, at(SATURDAY, 9, 1), cx);
    let first = running_thread(&cowork, cx).unwrap();
    assert_eq!(prompts(&cowork, first, cx), ["Earlier prompt"]);
    assert!(task(&cowork, later, cx).queued().is_some());
    assert!(task(&cowork, earlier, cx).queued().is_none());

    // The next starts once the first ends, however it ends.
    finish(
        &cowork,
        first,
        protocol::RunOutcome::Failed("offline".into()),
        cx,
    );
    let second = running_thread(&cowork, cx).unwrap();
    assert_ne!(second, first);
    assert_eq!(prompts(&cowork, second, cx), ["Later prompt"]);
    assert_eq!(
        task(&cowork, earlier, cx).history()[0].result,
        RunResult::Failed("offline".into())
    );
}

#[gpui::test]
fn continuing_runs_wait_for_their_thread_and_bring_it_back(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let task_id = add_task(
        &cowork,
        TaskSettings {
            thread: ThreadMode::ContinueLast,
            ..daily("Triage", 9, 0)
        },
        at(SATURDAY, 8, 0),
        cx,
    );
    tick(&cowork, at(SATURDAY, 9, 0), cx);
    let thread_id = running_thread(&cowork, cx).unwrap();
    finish(&cowork, thread_id, protocol::RunOutcome::Completed, cx);

    // Someone is chatting in the thread, so the next run waits for them.
    cowork.update(cx, |cowork, cx| {
        let thread = cowork.thread_store.read(cx).thread(thread_id, cx).unwrap();
        thread.update(cx, |thread, _| thread.generating = true);
        cowork.run_scheduled_task_now(task_id, cx);
        let task = cowork.scheduler.task(task_id).unwrap();
        assert_eq!(cowork.task_status(task, cx), TaskStatus::WaitingForThread);
    });
    assert_eq!(running_thread(&cowork, cx), None);
    cowork.update(cx, |cowork, cx| {
        let thread = cowork.thread_store.read(cx).thread(thread_id, cx).unwrap();
        thread.update(cx, |thread, _| thread.generating = false);
        cowork.run_next_scheduled(at(SATURDAY, 9, 5), cx);
    });
    assert_eq!(running_thread(&cowork, cx), Some(thread_id));
    assert_eq!(
        prompts(&cowork, thread_id, cx),
        ["Triage prompt", "Triage prompt"]
    );
    finish(&cowork, thread_id, protocol::RunOutcome::Completed, cx);

    // An archived thread comes back for the next run.
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.archive_thread(thread_id, window, cx)
        });
    });
    cowork.update(cx, |cowork, cx| cowork.run_scheduled_task_now(task_id, cx));
    assert_eq!(running_thread(&cowork, cx), Some(thread_id));
    cowork.read_with(cx, |cowork, cx| {
        let store = cowork.thread_store.read(cx);
        assert_eq!(store.threads[0].read(cx).instance_id, thread_id);
        assert!(store.archived.is_empty());
    });
    assert_eq!(prompts(&cowork, thread_id, cx).len(), 3);
}

#[gpui::test]
fn a_run_without_its_model_fails_and_the_queue_goes_on(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let created = at(SATURDAY, 8, 0);
    let missing = add_task(
        &cowork,
        TaskSettings {
            model: ollama_model("gone"),
            ..daily("Missing model", 9, 0)
        },
        created,
        cx,
    );
    add_task(&cowork, daily("Fine", 9, 0), created, cx);

    tick(&cowork, at(SATURDAY, 9, 0), cx);
    let thread_id = running_thread(&cowork, cx).expect("the next run started");
    assert_eq!(prompts(&cowork, thread_id, cx), ["Fine prompt"]);
    assert_eq!(
        task(&cowork, missing, cx).history()[0].result,
        RunResult::Failed("The model gone isn't available.".into())
    );
}

#[gpui::test]
fn runs_wait_while_no_models_are_known(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    cowork.update(cx, |cowork, cx| {
        cowork.set_models(crate::models::ModelCatalog::default(), cx)
    });
    let task_id = add_task(&cowork, daily("Morning", 9, 0), at(SATURDAY, 8, 0), cx);
    tick(&cowork, at(SATURDAY, 9, 0), cx);
    assert_eq!(running_thread(&cowork, cx), None);
    cowork.read_with(cx, |cowork, cx| {
        let task = cowork.scheduler.task(task_id).unwrap();
        assert_eq!(cowork.task_status(task, cx), TaskStatus::WaitingForModels);
    });

    // Starts once models are found.
    cowork.update(cx, |cowork, cx| cowork.set_models(test_catalog(), cx));
    assert!(running_thread(&cowork, cx).is_some());
}

#[gpui::test]
fn runs_now_and_shows_the_history_in_the_dialog(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let task_id = add_task(
        &cowork,
        daily("Report", 9, 0),
        chrono::Local::now().naive_local(),
        cx,
    );
    open_schedules_page(&cowork, cx);

    click(selector("run-scheduled-task", task_id), cx);
    assert!(!dialog_open(cx), "running doesn't open the editor");
    let thread_id = running_thread(&cowork, cx).expect("the run started");
    assert_eq!(
        task(&cowork, task_id, cx).history()[0].trigger,
        Trigger::Manual
    );
    finish(&cowork, thread_id, protocol::RunOutcome::Completed, cx);

    click(selector("scheduled-task", task_id), cx);
    assert!(cx.debug_bounds("run-record-0").is_some());
    assert!(cx.debug_bounds("run-record-1").is_none());

    // The row opens the run's thread.
    cx.update(|window, cx| window.close_dialog(cx));
    draw(cx);
    click(selector("open-scheduled-thread", task_id), cx);
    cowork.read_with(cx, |cowork, _| {
        assert_eq!(cowork.main_stage, MainStage::Thread);
        assert_eq!(cowork.active_thread_id, Some(thread_id));
    });
}

#[gpui::test]
fn runs_work_in_the_project_folders_chosen_in_the_dialog(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let folders = temp_folders(&["app", "docs"]);
    open_schedules_page(&cowork, cx);

    let editor = cx
        .update(|window, cx| {
            cowork.update(cx, |cowork, cx| {
                cowork.open_schedule_editor(None, window, cx)
            })
        })
        .expect("the dialog opens");
    draw(cx);
    // The mode only shows once there are folders.
    assert!(cx.debug_bounds("schedule-folders-add").is_some());
    assert!(cx.debug_bounds("schedule-project-Write").is_none());
    // As picking them in the folder picker does.
    editor.update(cx, |editor, cx| editor.add_folders(folders.clone(), cx));
    draw(cx);
    assert!(cx.debug_bounds("schedule-folders-folder-1").is_some());
    cx.simulate_input("Review the code");
    click("thread-mode-1", cx);
    click("schedule-project-Write", cx);
    click("save-schedule", cx);
    assert!(!dialog_open(cx));

    let [saved] = tasks(&cowork, cx).try_into().expect("one task");
    assert_eq!(saved.settings().folders, folders);
    assert_eq!(saved.settings().project_mode, ProjectMode::Write);

    // The run's thread has the project, and its sandbox mounts it writable.
    cowork.update(cx, |cowork, cx| cowork.run_scheduled_task_now(saved.id, cx));
    let thread_id = running_thread(&cowork, cx).unwrap();
    let (paths, mode, mounted) = thread_project(&cowork, thread_id, cx);
    assert_eq!(paths, folders);
    assert_eq!(mode, ProjectMode::Write);
    assert!(mounted.writable);
    assert_eq!(mounted.folders.len(), 2);
    // The user's own next thread keeps its own project.
    cowork.read_with(cx, |cowork, _| {
        assert!(cowork.new_thread_project_folders.is_empty());
    });
    finish(&cowork, thread_id, protocol::RunOutcome::Completed, cx);

    // Edits apply to the continued thread from its next run.
    cowork.update(cx, |cowork, cx| {
        let settings = TaskSettings {
            folders: vec![folders[1].clone()],
            project_mode: ProjectMode::Read,
            ..saved.settings().clone()
        };
        cowork.save_scheduled_task(Some(saved.id), settings, cx);
        cowork.run_scheduled_task_now(saved.id, cx);
    });
    assert_eq!(running_thread(&cowork, cx), Some(thread_id));
    let (paths, mode, mounted) = thread_project(&cowork, thread_id, cx);
    assert_eq!(paths, [folders[1].clone()]);
    assert_eq!(mode, ProjectMode::Read);
    assert!(!mounted.writable);
    _ = std::fs::remove_dir_all(folders[0].parent().unwrap());
}

#[gpui::test]
fn a_run_whose_folder_is_gone_fails(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let folders = temp_folders(&["app"]);
    let task_id = add_task(
        &cowork,
        TaskSettings {
            folders: folders.clone(),
            ..daily("Build", 9, 0)
        },
        at(SATURDAY, 8, 0),
        cx,
    );
    std::fs::remove_dir_all(folders[0].parent().unwrap()).unwrap();

    cowork.update(cx, |cowork, cx| cowork.run_scheduled_task_now(task_id, cx));
    assert_eq!(running_thread(&cowork, cx), None);
    let RunResult::Failed(error) = task(&cowork, task_id, cx).history()[0].result.clone() else {
        panic!("the run should fail");
    };
    assert!(error.contains("is gone"), "{error}");
    // No thread was made for it.
    cowork.read_with(cx, |cowork, cx| {
        assert!(cowork.thread_store.read(cx).threads.is_empty());
    });
}

#[gpui::test]
fn the_dialog_keeps_its_buttons_in_a_short_window(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let task_id = add_task(&cowork, daily("Report", 9, 0), at(SATURDAY, 8, 0), cx);
    // A full history, so the form is at its tallest.
    for _ in 0..12 {
        cowork.update(cx, |cowork, cx| cowork.run_scheduled_task_now(task_id, cx));
        let thread_id = running_thread(&cowork, cx).unwrap();
        finish(&cowork, thread_id, protocol::RunOutcome::Completed, cx);
    }
    open_schedules_page(&cowork, cx);

    for height in [700., 520.] {
        cx.simulate_resize(gpui::size(px(1200.), px(height)));
        draw(cx);
        click(selector("scheduled-task", task_id), cx);
        let save = cx.debug_bounds("save-schedule").expect("the save button");
        let fields = cx.debug_bounds("schedule-editor-fields").unwrap();
        assert!(
            save.bottom() <= px(height),
            "save {save:?} runs past a {height}px window"
        );
        assert!(
            fields.bottom() <= save.top(),
            "the fields {fields:?} cover the buttons"
        );
        cx.update(|window, cx| window.close_dialog(cx));
        draw(cx);
    }
}

#[gpui::test]
fn a_task_opens_its_thread_only_while_it_is_in_the_sidebar(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let task_id = add_task(&cowork, daily("Report", 9, 0), at(SATURDAY, 8, 0), cx);
    open_schedules_page(&cowork, cx);
    let open = selector("open-scheduled-thread", task_id);
    let stays_on_the_page = |cx: &mut gpui::VisualTestContext| {
        assert!(
            !dialog_open(cx),
            "the disabled button doesn't open the editor"
        );
        cowork.read_with(cx, |cowork, _| {
            assert_eq!(cowork.main_stage, MainStage::Schedules)
        });
    };

    // Shown, but disabled, before the first run.
    click(open, cx);
    stays_on_the_page(cx);

    click(selector("run-scheduled-task", task_id), cx);
    let thread_id = running_thread(&cowork, cx).unwrap();
    finish(&cowork, thread_id, protocol::RunOutcome::Completed, cx);

    // Disabled again once its thread is archived.
    cx.update(|window, cx| {
        cowork.update(cx, |cowork, cx| {
            cowork.archive_thread(thread_id, window, cx)
        });
        cowork.update(cx, |cowork, cx| cowork.open_schedules(window, cx));
    });
    draw(cx);
    click(open, cx);
    stays_on_the_page(cx);

    cowork.update(cx, |cowork, cx| cowork.restore_thread(thread_id, cx));
    draw(cx);
    click(open, cx);
    cowork.read_with(cx, |cowork, _| {
        assert_eq!(cowork.main_stage, MainStage::Thread);
        assert_eq!(cowork.active_thread_id, Some(thread_id));
    });
}

#[gpui::test]
fn deleting_a_continued_thread_warns_and_can_delete_its_schedule(cx: &mut gpui::TestAppContext) {
    let (cowork, _runtime, cx) = composer_test_cowork(cx);
    let continuing = |title| TaskSettings {
        thread: ThreadMode::ContinueLast,
        ..daily(title, 9, 0)
    };
    let created = at(SATURDAY, 8, 0);
    let kept = add_task(&cowork, continuing("Kept"), created, cx);
    let deleted = add_task(&cowork, continuing("Deleted"), created, cx);
    let mut threads = Vec::new();
    for task_id in [kept, deleted] {
        cowork.update(cx, |cowork, cx| cowork.run_scheduled_task_now(task_id, cx));
        let thread_id = running_thread(&cowork, cx).unwrap();
        finish(&cowork, thread_id, protocol::RunOutcome::Completed, cx);
        cx.update(|window, cx| {
            cowork.update(cx, |cowork, cx| {
                cowork.archive_thread(thread_id, window, cx)
            });
        });
        threads.push(thread_id);
    }
    cx.update(|window, cx| cowork.update(cx, |cowork, cx| cowork.open_archive(window, cx)));
    draw(cx);

    // Deleting only the thread: its task starts a new thread next time.
    click(selector("delete-thread", threads[0]), cx);
    assert!(dialog_open(cx));
    click("delete-thread-only", cx);
    assert!(!dialog_open(cx));
    assert_eq!(task(&cowork, kept, cx).thread_id, None);

    // Or the task goes with it.
    click(selector("delete-thread", threads[1]), cx);
    click("delete-thread-and-schedule", cx);
    let ids: Vec<_> = tasks(&cowork, cx).iter().map(|task| task.id).collect();
    assert_eq!(ids, [kept]);
    cowork.read_with(cx, |cowork, cx| {
        assert!(cowork.thread_store.read(cx).archived.is_empty());
    });
}
