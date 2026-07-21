# lavis

Minimal Rust foundation for a personal Telegram userbot. This stage authorizes one user account, persists its Telegram session locally, and handles its own outgoing `,ping`, `,stats`, and `,help` messages.

## Development

```bash
nix develop
cargo check
cargo test
nix build
nix run
nix flake check
```

Before running, provide the Telegram application credentials through the environment:

```bash
export LAVIS_API_ID='your-api-id'
export LAVIS_API_HASH='your-api-hash'
```

`nix run` prompts for a phone number and, when required, a login code and two-factor password. Login codes and passwords are entered without terminal echo. There are no automatic retries; rerun the program after an invalid credential or other authorization failure.

The default command prefix is `,`; dot-prefixed commands are ignored. Use `,help` for the command overview or `,help stats` for contextual help; command help is displayed with a collapsed quote. Sending `,ping` from the authorized account measures a Telegram MTProto RPC round-trip time. `,stats` shows fresh Telegram latency, Lavis and system uptime, resident memory, recognized command count, and version. Commands edit their original message and are handled only while Lavis is running; offline commands are not replayed on the next startup. Incoming messages, unknown commands, and other updates are ignored. Update handling is sequential. Set `RUST_LOG=lavis=debug` for normal diagnostics; dependency debug logging may expose Telegram content. Session state defaults to `$XDG_STATE_HOME/lavis/session`, or `$HOME/.local/state/lavis/session`. The session database and any SQLite sidecar files are sensitive authentication material: keep them outside Git and do not share or copy them.

## System information

`,fastfetch [allowed arguments]` runs the packaged `fastfetch` executable and returns its output. The installed Nix package wraps `lavis` with `fastfetch` on its `PATH`; `fastfetch` is also available in `nix develop`.

For safety, only these option/value pairs are accepted (arguments use shell-style quoting):

- `--logo none`
- `--structure <components>`, where `<components>` is a colon-separated list of `OS`, `Kernel`, `CPU`, `GPU`, and `Memory` (case-insensitive)
- `--separator <text>`, where `<text>` is 1–64 printable ASCII characters and does not start with `--`

All other fastfetch options are rejected. Lavis always supplies `--config none --pipe`, disables color, runs fastfetch without stdin from `/`, and limits execution to five seconds.

## Aliases

Use `,alias list` (or `,alias`) to list aliases, `,alias add <name> <command> [arguments...]` to add one, and `,alias delete <name>` to remove one. For example:

```text
,alias add sys fastfetch --logo none --structure os:kernel:cpu:memory
,sys --separator " -> "
```

Aliases target a canonical aliasable command: `ping`, `stats`, `help`, or `fastfetch`; `alias` itself cannot be aliased. Alias names are normalized to lowercase, must start with an ASCII letter, and may otherwise contain ASCII letters, digits, `_`, or `-`. Preset arguments and arguments supplied when invoking the alias are combined, with preset arguments first. Alias arguments use shell-style quoting.

Aliases persist across restarts in `$XDG_STATE_HOME/lavis/aliases.json`, or `$HOME/.local/state/lavis/aliases.json`, alongside the session state. The state directory must already exist. The alias file is local state and is written with restrictive permissions on Unix; do not commit or share it.
