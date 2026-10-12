# Roadmap

Planned work that is deliberately out of scope for now. `TODO.md` holds the
owner's own feature notes.

## Tool permissions

- **Tools that always run in `Read` mode.** Let chosen tools (such as
  `run_command`) run without asking while the project is read-only, as they
  did before commands ran on the host. mxc-sdk enforces read-only folders, so
  this is a policy choice rather than a missing capability.
- **Allow-all override.** A per-thread switch letting every tool call run
  without asking, for people who accept the risk.
- **Permission page.** One place to review and change what the agent may do:
  the per-mode tool rules above, the allow-all override, and network access.
  Network is denied for now. On macOS Seatbelt can only allow or deny all
  outbound traffic, not filter by host.

## Commands

- **Cancel Command button.** A button on a running `run_command` card that
  ends just that command, without stopping the whole run. Today a command runs
  until it exits or the run is stopped, with no time limit.
