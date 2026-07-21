# lavis

Minimal Rust foundation for a personal Telegram userbot. This stage authorizes one user account and persists its Telegram session locally; it does not process updates or commands.

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

The default command prefix is `.`. Session state defaults to `$XDG_STATE_HOME/lavis/session`, or `$HOME/.local/state/lavis/session`. The session database and any SQLite sidecar files are sensitive authentication material: keep them outside Git and do not share or copy them.
