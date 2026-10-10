# Cowork

Cowork is a collaborative agentic assistant. Work with collaborators from anywhere, on anything. Cowork uses peer-to-peer connections and end-to-end encryption, so nobody can snoop on your threads. Using a local language model provider allows you to have private, collaborative agentic workflows. The agent's shell commands run in an isolated virtual machine with no network access.

## Installation

Cowork currently does not have a release schedule, so you need to build it yourself. First, install [Rust](https://rust-lang.org), then run the following commands:

```sh
git clone https://github.com/tidely/cowork
cd cowork
cargo run --release
```

That's it!

## How does Cowork work?

The agent has access to provided projects and shell commands inside of a sandbox powered by [microsandbox](https://github.com/superradcompany/microsandbox). Each thread gets its own microVM: a lightweight virtual machine with its own Linux kernel, kept apart from your computer by hardware virtualization.

Cowork establishes peer-to-peer connections over the [iroh](https://www.iroh.computer) network. Collaborators connect using a public key that identifies the endpoint and helps secure the connection. This means your chats are end-to-end encrypted. Each thread is backed by a conflict-free replicated data type (CRDT), which allows multiple users to edit any part of the thread at the same time.
