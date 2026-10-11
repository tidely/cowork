# Cowork

Secure & collaborative agentic workspace.

Share an agent thread with your collaborators. You can write the next prompt together, comment on the agent's replies, and decide what the agent may do. Connections are peer-to-peer and end-to-end encrypted, so nobody else can read your threads. Models run locally through [Ollama](https://ollama.com), so your work never has to leave the computers of the people in the thread. The agent's shell commands run with your own tools, inside your operating system's sandbox, with no network access, and only after the host allows each one.

## Installation

Cowork doesn't have releases yet, so you need to build it yourself. Install [Rust](https://rust-lang.org) and [Ollama](https://ollama.com), then run:

```sh
git clone https://github.com/tidely/cowork
cd cowork
cargo run --release
```

## How does Cowork work?

### Collaboration

Cowork connects peers directly over the [iroh](https://www.iroh.computer) network. Collaborators join with the host's endpoint ID, a public key that identifies the host and secures the connection end to end. The host is in charge: everything goes through them, and they choose what each collaborator may do. Collaborators can read only, edit the draft, or, as admins, also send prompts, change the model, and approve the agent's tool calls.

The draft of the next prompt is a conflict-free replicated data type (CRDT), so everyone can edit it, comment on it, and attach files at the same time without conflicts.

### Sandbox

The agent runs shell commands on the host's computer, so your own shell, toolchains, and build systems work out of the box. Each command is contained by the operating system's own sandbox through [mxc](https://github.com/microsoft/mxc): Seatbelt on macOS, Bubblewrap on Linux (install the `bwrap` package), and an AppContainer on Windows. Commands can read your installed tools and the project folders the host adds, change those folders only in `Write` mode, and write a scratch folder for their thread. They can't read your other files or credentials such as `~/.ssh`, and they have no network. Only the host can allow a command, and every command waits for them.
