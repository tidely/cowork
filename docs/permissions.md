# Permissions

A command's **scope** says what its sandbox permits. The project's
**auto-approve level** says which scopes can run without asking you.
These are separate: choosing Read does not remove write or network tools;
the agent can request more access, and you decide whether to allow it.

| Command scope | Auto-approve: Read | Auto-approve: Write | Auto-approve: Write + Network |
|---|---|---|---|
| Read | Automatic | Automatic | Automatic |
| Write | Ask | Automatic | Automatic |
| Network (write + network) | Ask | Ask | Automatic |

When the agent omits `access`, the command uses the auto-approve level (or
higher permissions already granted for this run). It can explicitly choose
a narrower scope: a Read command remains read-only even when Write is
automatically approved. Approval grants the scope for that command only.

All scopes can read the host filesystem, as Codex does, subject to OS
permissions and Cowork's credential denials. Write and Network scopes can
change project folders; scratch is always writable. Writing other folders
requires approval unless already granted. Merely reading another folder
does not. Network allows outbound connections, excluding host loopback.

On macOS, the command sandbox blocks credential contents and directory listings,
but mxc's generated profile can still expose existence metadata for known paths.
Credential locations are listed in `crates/sandbox/src/settings.rs`.

| File tool | Read | Write | Write + Network |
|---|---|---|---|
| `read_file`, `list_directory` | Automatic | Automatic | Automatic |
| `write_file` | Ask | Automatic | Automatic |

File tools are app-managed. Reads/listings accept absolute host paths;
writes stay in project or scratch folders. Approving a write in Read permits
that call without changing auto-approve. Credential and path checks still
apply.

`request_permissions` keeps approved command permissions for the rest of
one run. Lowering auto-approve affects future calls but does not revoke
those grants or a call already approved. Only the host can approve commands,
file writes, and permission grants; Admin peers can approve other tools.
`calculate` and `respond_to_comment` always run without approval, including
in scheduled runs. Other tools ask unless explicitly allowlisted.

Cards show what approval adds: Network in Write, or Write + Network when
new writable folders are also needed. Scheduled calls needing approval fail
once; an identical repeat asks and waits. Low-level sandbox settings live
in `crates/sandbox/src/settings.rs`.
