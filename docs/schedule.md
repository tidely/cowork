# Scheduled tasks

A scheduled task runs a saved prompt at a given time or interval. This lists
the options a task has and how runs behave.

## Status

Implemented: the Scheduled page (`schedules.rs`, opened from the sidebar),
the task model and its occurrence policy (`scheduled_task.rs`), and the
scheduler running the queue and saving tasks (`scheduler.rs`), with every
option and behavior below. Tasks and their run history are saved to
`~/.cowork/schedules.json`.

Not yet implemented:

- saving threads, so after a restart a **continue the last thread** task's
  thread is gone and its next run starts a new one (see
  [Constraints](#constraints)).

## Task options

| Option | Choices |
|---|---|
| Prompt | The text submitted on each run. |
| Name | Shown in the list, and the title of the threads it creates. Defaults to the start of the prompt, like a thread title. |
| When | An [interval](#interval) or a [schedule](#schedule). |
| Model | Any model in the host's catalog. Starts as the model new threads start with. |
| Project | Folders on the host's machine each run's thread works in, and whether they are open for **Read** (the default) or **Write**. See [Project](#project). |
| Thread | **New thread each run**, or **continue the last thread**. |
| Missed runs | **Run once when available**, or **skip**. |
| Status | Active or paused, with the switch on the task's row. |

### Interval

Every *N* minutes, hours, or days, starting from a chosen time. Occurrences
are counted from that start time, not from when the last run finished, so a
slow run doesn't push the later ones back.

### Schedule

- **Once**, at a date and time.
- **Daily**, at a time.
- **Weekdays**, at a time.
- **Weekly**, on one or more days, at a time.
- **Monthly**, on a day of the month, at a time. Months without that day are
  skipped, as RFC 5545 does.
- **Custom**, as an RFC 5545 `RRULE`, for anything the presets can't express,
  evaluated with the `rrule` crate. Its `DTSTART` is when the rule was saved,
  which gives the dates and times of day the rule leaves out. `COUNT` and
  `UNTIL` end it.

Times are local wall-clock times, so a daily 09:00 task stays at 09:00 across
daylight saving changes. Once a one-time task has run, or its time has
passed, it shows no more runs; editing its time schedules it again.

### Project

The same folders and mode a thread's bottom bar sets (see
[Project folders](collaboration.md#project-folders)), chosen in the task's
dialog with the same folder picker. Before each run is submitted, its thread
is given exactly the task's folders and mode, so the run's sandbox mounts
them from its first command. An edit applies from the next run, also in a
continued thread, whose folders someone may have changed meanwhile; a changed
project restarts the thread's sandbox without asking, as nothing runs in it
then. The user's own new-thread project is left alone.

A run whose folder no longer exists fails without making a thread, and the
queue goes on.

### Thread

- **New thread each run**: every run starts a fresh thread containing only the
  prompt, titled with the task's name. Runs don't see each other.
- **Continue the last thread**: the first run creates a thread, and every later
  run submits its prompt to that same thread, so the run sees the thread's
  history. A user can also keep chatting in that thread between runs.

Runs never open their thread or touch the new-thread draft; their threads
appear at the top of the sidebar's recents. A continued thread that was
archived is restored for the next run.

Deleting a thread from the archive that a **continue the last thread** task
is using shows a warning naming the task. Confirming deletes the thread, and
the task's next run starts a new thread, which later runs continue. The
warning also offers to delete the task along with the thread. Threads created
by **new thread each run** tasks are deleted without a warning, since no
later run uses them. Deleting every archived thread at once says how many
tasks continue them.

### Missed runs

The scheduler looks at the clock every 15 seconds while the app runs. An
occurrence it finds more than 2 minutes after its time was due while the app
wasn't running or the computer was asleep: a missed run. Each task saves how
far it has looked, so occurrences due while the app was closed are found
when it starts.

- **Run once when available**: queue one catch-up run for the most recent
  missed occurrence. Older missed occurrences are recorded as skipped.
- **Skip**: record every missed occurrence as skipped and wait for the next
  one.

If an occurrence is due on time when missed ones are found, as when the
computer wakes just after a run's time, that run is queued and the missed
ones are skipped either way, since a run happens now anyway.

There is no maximum age: a catch-up run happens however long ago the missed
occurrence was. Missed occurrences are never all replayed. A run executes
now, not at the time it was due, so replaying several would mostly repeat
the same work.

A run that is waiting in the [queue](#execution-order) is not missed, even if
its time has passed. Queued runs are saved, so they run after a restart. A run
going on when Cowork quits is recorded as interrupted.

## Execution order

All scheduled runs go through one queue and run one at a time, in order of
their scheduled time. Ties go to the task that was created first. A run that
comes due while another is running waits for it to finish. A catch-up run
takes its place by its original scheduled time, so after a long sleep missed
runs execute in the order they were due.

A task has at most one run queued. An occurrence due while its previous run
still waits is recorded as skipped, as missed runs are.

The next run waits, holding up the queue behind it:

- while it is a **continue the last thread** run and that thread is
  generating, for example while someone is chatting in it;
- while no models are known, as at startup before Ollama has answered. The
  scheduler asks Ollama for its models every minute until it does.

A run whose model the catalog doesn't offer fails, and the queue goes on.

## Tool approval

Scheduled runs keep the usual approval rules (see
[Tool approval](collaboration.md#tool-approval)): tools on the always-allowed
list run, and every other call needs the host or an `Admin` peer to allow it
(`run_command` only the host). Since no one may be around, a scheduled run
doesn't wait the first time:

1. A call that needs approval fails at once. Its result tells the model that
   the call needs the user's permission, and that calling it again with the
   same arguments will ask the user.
2. If the model makes the call again in the same run, with the same tool and
   identical arguments (their keys in any order), the call waits for approval
   as usual, and the user is notified: in the app, and through the operating
   system's notifications where they work. Clicking the notification opens
   the thread.

The model can therefore carry on without the call, or decide the call is
worth waiting for. Calls made in the thread outside a scheduled run, such as
someone chatting in a **continue the last thread** thread, are approved as
usual. `run_command` in a read-only project runs without asking, as it does
outside scheduled runs.

## Managing tasks

Each row shows the task's name, its schedule, model, and thread mode, its
last run, and what it is doing: its next run, queued, running, waiting for
approval, for its thread, or for models.

- **Run now** adds a run to the end of the queue, outside the schedule. A task
  that already has a run queued can't add another.
- **Open its thread**: the thread of the run going on, or of the last run.
  Disabled while there is none in the sidebar, its tooltip saying why: no
  thread yet before the first run, or that the thread is archived or was
  deleted. Archived threads aren't restored from here; that's done from the
  archive.
- **Pause** and **resume**. Occurrences that pass while a task is paused are
  not missed runs. They are not run when the task resumes. A run queued when
  the task is paused is dropped and recorded as skipped.
- **Edit** any option. Changes apply from the next run. A changed schedule
  counts from the edit, so editing never finds missed runs.
- **Delete**. A run going on finishes. The threads the task created are kept.
- **History**, in the task's dialog, lists its latest runs with when they
  started, whether they were scheduled, a catch-up, or run now, and how they
  ended or why they were skipped. Each opens its thread while that is in the
  sidebar. The last 50 runs are kept.

## Constraints

- Only the host runs scheduled tasks, in its own threads. A run's submission
  goes through the same code path as anyone else's (see
  [collaboration.md](collaboration.md)) and doesn't touch the shared draft. A
  shared thread's collaborators see the run like any other.
- Tasks and their run history are the first state Cowork saves (the
  profile's statistics, in `statistics.rs`, are the only other). The file is
  written whole, replacing the old one only once written. One that can't be
  read, or has another format version, is moved aside rather than
  overwritten.
- **Continue the last thread** needs the thread itself to survive a restart,
  so threads must be saved to disk too. Until they are, the next run after a
  restart starts a new thread.

## Open questions

- **Waiting blocks the queue.** A run waiting for approval holds up every run
  queued behind it, possibly for hours. Should it stay blocked, or should a
  waiting run step aside after some time?
