# Projects

Projects group the user's own chats in the sidebar. A project is a name and
the source folders its new chats start with. It is local organization only:
nothing about projects is sent to collaborators.

## In the sidebar

- The Projects section is above Recents. Its header collapses it like
  Recents does, and has a `+` button, shown only while the header is
  hovered, opening the Create project dialog without collapsing the section.
- Projects are listed newest first, each as a folder icon and its name. A
  project's row toggles whether its chats show below it, indented; while
  hovered it shows a compose button.
- An open project shows its five most recently active chats (activity is
  when a chat's latest run started), and "Show more" reveals five more at a
  time until all are shown. Closing a project, or the whole section, starts
  it over at five.
- On every launch the section is open and every project closed. A project
  just created is open.
- Chats in a project are not in Recents. Shared ones are in "Shared by me"
  instead of under their project, and archived ones are in the archive; both
  keep their project and go back under it once sharing ends or they are
  restored. A chat whose project is no longer known (its record was lost)
  shows in Recents, keeping its membership.

## Creating a project and its chats

- The dialog asks for a name (trimmed, not blank, need not be unique) and
  any number of source folders, chosen with the native folder picker (several
  at a time, a folder chosen twice is kept once), shown by name with the full
  path as a tooltip. Creating the project only adds it to the sidebar.
- The project's compose button opens an empty new chat with the project's
  folders copied in. The chat is created, and filed under the project, by
  its first submission (or by sharing it before then), exactly as any new
  chat is created. From then on its folders are its own: changing them
  doesn't change the project, nor the project's its.
- New chat and Recents' compose button always start a chat in no project;
  if the new chat being composed was in a project, its copied folders go
  with it.
- Saved folders that no longer exist are kept; a chat started with one
  handles it as any chat with a missing folder does.

## Saving

- Project records are saved in `~/.cowork/projects.json` when one is
  created (`crates/cowork/src/projects.rs`), with a format version of their
  own; a file of another version or unreadable is moved aside.
- Which project a chat belongs to is saved with the chat, as `project` in its
  `thread.json` (`thread_files.rs`). It is always explicit, never inferred
  from folders. Files saved before projects read as in no project, so the
  thread format needed no new version.
- Which projects are open, and how many chats they show, isn't saved.
- `--ephemeral` launches save no projects.

## Not yet

Editing, renaming, deleting, and reordering projects (see `TODO.md`).
