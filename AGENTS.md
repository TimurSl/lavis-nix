# AGENTS.md

## Project

`lavis` is a personal Telegram userbot written in Rust and packaged with Nix flakes.

The project must remain:

- small;
- statically typed;
- testable without Telegram where possible;
- reproducible through Nix;
- secure by default;
- understandable without framework-level magic.

This is not a line-by-line port of Heroku, Hikka, FTG, or Telethon.

## Workspace layout

Expected local layout:

```text
~/project/
├── heroku-reference/
└── lavis/
```

The current repository is `lavis`.

`../heroku-reference` is reference material only.

## Reference repository policy

The agent may inspect `../heroku-reference` to understand:

- user-visible behavior;
- command names and semantics;
- lifecycle ideas;
- error handling expectations;
- useful feature boundaries.

The agent must not:

- modify the reference repository;
- vendor it into this repository;
- copy large source fragments;
- reproduce Python-specific architecture;
- preserve compatibility with Heroku/Hikka modules unless explicitly requested;
- assume that Telethon APIs map directly to Rust APIs.

Translate behavior into idiomatic Rust. Do not translate class hierarchies mechanically.

## Current product scope

The initial product is:

- one Telegram user account;
- one local process;
- commands accepted only from the authenticated user's own outgoing messages;
- configurable command prefix;
- Linux/NixOS as the primary platform;
- static command registration;
- local configuration and mutable state outside `/nix/store`;
- no public module marketplace;
- no remote code execution;
- no Python compatibility layer.

Features outside the current task must not be implemented speculatively.

## Architecture

Prefer simple modules with explicit responsibilities:

```text
src/
├── main.rs
├── app.rs
├── auth.rs
├── client.rs
├── command.rs
├── config.rs
├── error.rs
└── commands/
    ├── mod.rs
    ├── help.rs
    ├── id.rs
    └── ping.rs
```

Not every file must exist from the first commit. Add files only when their responsibility is real.

Core rules:

- Telegram-independent logic must remain Telegram-independent.
- Command parsing must be testable without a network.
- Configuration loading must be testable without a Telegram session.
- Transport-specific types must not leak through the entire application.
- Prefer explicit data flow over service locators or global registries.
- Prefer composition over inheritance-like emulation.
- Prefer enums and structs over stringly typed state.

## Rust rules

Use stable Rust.

Required standards:

- no `unsafe`;
- no global mutable state;
- no ignored `Result`;
- no broad `allow` attributes used to silence real problems;
- no unnecessary cloning to bypass ownership design;
- no blocking I/O inside async tasks;
- no `unwrap()` or `expect()` in production paths unless a local invariant is proven and documented;
- errors must include useful context;
- public interfaces must be narrow;
- dependencies must be minimal and justified.

Prefer:

- `thiserror` for typed library/domain errors;
- `anyhow` at the application boundary;
- `tracing` for structured diagnostics;
- `tokio` for the async runtime;
- immutable values by default;
- exhaustive matching;
- small functions with clear ownership.

Do not create traits merely because multiple implementations might exist someday.

## Telegram integration rules

The intended Telegram library is `grammers`, but it must only be added when the task explicitly reaches Telegram integration.

Before using a `grammers` API:

1. inspect the exact versions in `Cargo.toml` and `Cargo.lock`;
2. inspect local crate sources or authoritative documentation;
3. verify method names and return types;
4. do not guess based on Telethon, Pyrogram, TDLib, or an older `grammers` release.

Telegram integration must eventually support:

- phone-number login;
- login code;
- optional 2FA password;
- persistent session storage;
- reconnect behavior;
- graceful shutdown;
- filtering to the authenticated user's own outgoing command messages.

Session data is secret mutable state and must never be stored in Git or `/nix/store`.

## Command system

The command system starts with static registration.

Do not introduce:

- dynamic libraries;
- Python embedding;
- Lua;
- WASM plugins;
- runtime package installation;
- remote module loading;
- arbitrary code evaluation.

A command should have a clear interface and must not parse the raw prefix independently when a central parser already exists.

Parser behavior must be covered by tests, including:

- empty input;
- non-command text;
- prefix-only input;
- command without arguments;
- command with arguments;
- repeated whitespace;
- non-default prefix;
- Unicode arguments.

## Configuration and secrets

Configuration and mutable state belong under XDG paths:

```text
~/.config/lavis/
~/.local/state/lavis/
~/.local/share/lavis/
```

Secrets include:

- Telegram `api_hash`;
- authentication/session data;
- passwords;
- future tokens.

Secrets must not appear in:

- `flake.nix`;
- Nix module option defaults;
- generated Nix store files;
- committed `.env` files;
- tests;
- logs;
- error messages.

Environment variables may be used during early development. Later secret management may use `sops-nix` or `agenix`, but only when explicitly requested.

## Nix rules

The flake is responsible for:

- development shell;
- package build;
- application entry point;
- checks;
- reproducible dependency closure.

The flake is not responsible for creating mutable session data.

Required user workflows:

```bash
nix develop
cargo check
cargo test
nix build
nix run
nix flake check
```

Avoid unnecessary Nix frameworks. Use plain flake outputs unless additional abstraction clearly reduces complexity.

Do not hardcode user-specific absolute paths into the package.

NixOS and Home Manager modules should be added only after the binary and configuration model are stable.

## Security

Security takes precedence over feature parity.

Never add the following without an explicit, narrowly scoped request:

- `.eval`;
- shell execution commands;
- remote code download and execution;
- automatic installation of third-party modules;
- unauthenticated HTTP endpoints;
- plaintext secret persistence;
- logging of session material;
- permissive file permissions for secrets.

Commands must eventually verify that the message was sent by the authenticated account.

Treat all incoming Telegram content as untrusted input.

## Testing

Every behavior change requires tests where practical.

Minimum local verification:

```bash
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

For Nix-related changes also run:

```bash
nix flake check
nix build
```

Do not claim that checks passed unless they were actually executed successfully.

Do not delete or weaken a test merely to make the suite pass.

## Change discipline

Before editing:

1. read this file;
2. inspect the relevant files;
3. inspect `../heroku-reference` only if its behavior is relevant;
4. identify the smallest coherent change;
5. state a brief plan.

During editing:

- keep the diff narrow;
- compile after each meaningful step;
- fix root causes rather than symptoms;
- avoid unrelated formatting or refactoring;
- do not modify generated lock files manually.

After editing:

1. run the required checks;
2. summarize changed files;
3. explain architectural decisions;
4. report failures honestly;
5. identify the next smallest step.

## Git policy

The agent may inspect Git state and diffs.

The agent must not perform these actions unless explicitly requested:

- commit;
- amend;
- rebase;
- merge;
- push;
- force-push;
- reset;
- clean;
- delete branches;
- change remotes.

Never modify `../heroku-reference` even if it has uncommitted changes.

## Completion format

Every implementation response must end with:

- summary of implemented behavior;
- changed files;
- commands executed;
- verification results;
- known limitations;
- next minimal step.

Do not proceed into the next development phase without a separate request.
