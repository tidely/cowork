# Scheduled tasks

Draft. A scheduled task runs a saved prompt at a given time or interval.
This lists the options a task has and how runs behave.

## Status

Not started.

## Task options

| Option | Choices |
|---|---|
| Prompt | The text submitted on each run. |
| When | An [interval](#interval) or a [schedule](#schedule). |
| Model | Any model in the host's catalog. |
| Thread | **New thread each run**, or **continue the last thread**. |
| Missed runs | **Run once when available**, or **skip**. |
| Status | Active or paused. |

### Interval

Every *N* minutes, hours, or days, starting from a chosen time. Occurrences
are counted from that start time, not from when the last run finished, so a
slow run doesn't push the later ones back.

### Schedule

- **Once**, at a date and time.
- **Daily**, at a time.
- **Weekdays**, at a time.
- **Weekly**, on one or more days, at a time.
- **Monthly**, on a day of the month, at a time.
- **Custom**, as an RFC 5545 `RRULE`, for anything the presets can't express.

Times are local wall-clock times, so a daily 09:00 task stays at 09:00 across
daylight saving changes. A one-time task pauses itself after it runs.

### Thread

- **New thread each run**: every run starts a fresh thread containing only the
  prompt. Runs don't see each other.
- **Continue the last thread**: the first run creates a thread, and every later
  run submits its prompt to that same thread, so the run sees the thread's
  history. A user can also keep chatting in that thread between runs.

Deleting a thread from the archive that a **continue the last thread** task
is using shows a warning naming the task. Confirming deletes the thread, and
the task's next run starts a new thread, which later runs continue. The
warning also offers to delete the task along with the thread. Threads created
by **new thread each run** tasks are deleted without a warning, since no
later run uses them.

### Missed runs

A run is missed when the app wasn't running, or the computer was asleep, at
its scheduled time. When the app starts or the computer wakes, Cowork checks
each active task for missed occurrences:

- **Run once when available**: queue one catch-up run for the most recent
  missed occurrence. Older missed occurrences are recorded as skipped.
- **Skip**: record every missed occurrence as skipped and wait for the next
  one.

There is no maximum age: a catch-up run happens however long ago the missed
occurrence was. Missed occurrences are never all replayed. A run executes
now, not at the time it was due, so replaying several would mostly repeat
the same work.

A run that is waiting in the [queue](#execution-order) is not missed, even if
its time has passed.

## Execution order

All scheduled runs go through one queue and run one at a time, in order of
their scheduled time. Ties go to the task that was created first. A run that
comes due while another is running waits for it to finish. A catch-up run
takes its place by its original scheduled time, so after a long sleep missed
runs execute in the order they were due.

A **continue the last thread** run also waits while that thread is generating,
for example while someone is chatting in it.

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
   identical arguments, the call waits for approval as usual, and the user is
   notified.

The model can therefore carry on without the call, or decide the call is
worth waiting for. Calls made in the thread outside a scheduled run, such as
someone chatting in a **continue the last thread** thread, are approved as
usual.

## Managing tasks

- **Run now** adds a run to the end of the queue, outside the schedule.
- **Pause** and **resume**. Occurrences that pass while a task is paused are
  not missed runs. They are not run when the task resumes.
- **Edit** any option. Changes apply from the next run.
- **Delete**. The threads the task created are kept.
- **History** lists each run with its scheduled time, its start time, and
  whether it was scheduled, a catch-up, run manually, or skipped (and why).

## Constraints

- Only the host runs scheduled tasks. A run's submission goes through the
  same code path as anyone else's (see
  [collaboration.md](collaboration.md)) and doesn't touch the shared draft.
- Tasks and their run history must be saved to disk. This is the first state
  Cowork persists.
- **Continue the last thread** needs the thread itself to survive a restart,
  so threads must be saved to disk too.


## Open questions

- **Waiting blocks the queue.** A run waiting for approval holds up every run
  queued behind it, possibly for hours. Should it stay blocked, or should a
  waiting run step aside after some time?
- **Archived thread.** When a **continue the last thread** task's thread is
  archived, should its next run restore the thread, skip, or pause the task?
- **Pile-up.** If a task's next occurrence comes due while its previous run
  is still queued, should both stay queued? Keeping only one would match how
  [missed runs](#missed-runs) are caught up.
