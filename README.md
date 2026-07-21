# lavis

Minimal Rust foundation for a personal Telegram userbot. Telegram integration is not part of this stage.

## Development

```bash
nix develop
cargo check
cargo test
nix build
nix run
nix flake check
```

`nix run` starts the process and waits for Ctrl-C; it does not need credentials at this stage.

When Telegram authentication is implemented, provide credentials through the environment:

```bash
export LAVIS_API_ID='your-api-id'
export LAVIS_API_HASH='your-api-hash'
```

The default command prefix is `.`. Session state defaults to `$XDG_STATE_HOME/lavis/session`, or `$HOME/.local/state/lavis/session`.
