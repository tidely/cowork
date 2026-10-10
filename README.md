# Cowork

Secure & collaborative agentic workspace.

Share an agent thread with your collaborators. You can write the next prompt together, comment on the agent's replies, and decide what the agent may do. Connections are peer-to-peer and end-to-end encrypted, so nobody else can read your threads. Models run locally through [Ollama](https://ollama.com), so your work never has to leave the computers of the people in the thread. The agent's shell commands run in an isolated virtual machine with no network access.

## Installation

Cowork doesn't have releases yet, so you need to build it yourself. Install [Rust](https://rust-lang.org) and [Ollama](https://ollama.com), then run:

```sh
git clone https://github.com/tidely/cowork
cd cowork
cargo run --release -p cowork
```

## How does Cowork work?

### Collaboration

Cowork connects peers directly over the [iroh](https://www.iroh.computer) network. Collaborators join with the host's endpoint ID, a public key that identifies the host and secures the connection end to end. The host is in charge: everything goes through them, and they choose what each collaborator may do. Collaborators can read only, edit the draft, or, as admins, also send prompts, change the model, and approve the agent's tool calls.

The draft of the next prompt is a conflict-free replicated data type (CRDT), so everyone can edit it, comment on it, and attach files at the same time without conflicts.

### Sandbox

The agent runs shell commands in a sandbox powered by [microsandbox](https://github.com/superradcompany/microsandbox). Each thread gets its own microVM: a lightweight virtual machine with its own Linux kernel, kept apart from your computer by hardware virtualization. It has no network, runs commands as an unprivileged user, and sees none of your files except the project folders the host adds.
