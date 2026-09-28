# Cowork

Cowork is a collaborative agenic assistant. Work together with collaborators from anywhere, on anything. Cowork works peer-to-peer and utilizes has end-to-end encryption, meaning nobody can snoop in on your threads. Using a local language model provider allows you to have completely private, but collaborative agentic workflows.

## Installation

Cowork currently does not have a release schedule. This means you need to build Cowork yourself. First install [rust](https://rust-lang.org), and then run the following commands:

```sh
git clone https://github.com/tidely/cowork
cd cowork
cargo run --release
```

That's it!

## How does cowork... work?

Cowork establishes a peer-to-peer connection over the [iroh](https://www.iroh.computer) network. Collabortors connect using a public key which deals as the connection endpoint, as well as the encryption key. This means all your chats are guaranteed to be end-to-end encrypted.

Each thread is backed by a Conflict-free replicated data type (CRDT), which allows multiple users to edit any part of the thread at the same time.
