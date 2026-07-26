# External Modules (alpha)

> **Status:** Alpha — API and protocol are unstable and may change without
> notice. Not recommended for production use.

External modules allow extending Lavis with commands implemented in any
language. Each module runs as a separate OS process.

## Quick start

### 1. Create a module directory

```bash
mkdir -p ~/.local/share/lavis/modules/my-echo
```

### 2. Create module.json

See `docs/module-api-v2.md` for the full schema.

### 3. Create entrypoint

Write an executable that reads JSON from stdin and writes JSON to stdout.
Any language works. See the example module:

```bash
ls examples/external-module-echo/
```

### 4. Enable the module

```bash
lavis modules enable my-echo
```

### 5. Use the module

```bash
,my-echo.echo Hello from Lavis!
```

## CLI commands

These are local terminal commands, not Telegram commands.

### `lavis modules enable <id>`

Register an external module by ID. The module must exist in
`$XDG_DATA_HOME/lavis/modules/<id>/`.

### `lavis modules disable <id>`

Remove a module from the active set. Does not delete its files.

### `lavis modules validate <path>`

Validate a module manifest at the given path without enabling it.

### `lavis modules status`

Show discovered external modules and their locally enabled state.

## Command resolution order

When Lavis receives a message with the configured prefix:

1. **Built-in commands** (canonical names)
2. **External commands** (namespaced: `module-id.command-name`)
3. **Aliases**

This means a built-in command always takes priority over an external one
with the same name.

## Help integration

External commands and modules appear in help output:

- `.help` shows built-in + external commands
- `.help echo.echo` shows details for a specific external command
- `.help echo` shows the external module card
- `modules` command shows all modules including external ones

## Writing modules

### Requirements

- Executable entrypoint with shebang (or compiled binary)
- Read JSON lines from stdin
- Write JSON lines to stdout
- Respond to `initialize`, `execute`, `health`, and `shutdown`
- Flush stdout after each message
- Start quickly: enabled modules are launched during Lavis startup, not on
  their first command

### Best practices

- Validate `protocol_version` on every incoming message
- Always echo the `request_id` back
- Keep startup fast (under 2 seconds)
- Use `stderr` for diagnostics (captured but not displayed)
- Set `NO_COLOR=1` / `TERM=dumb` in your entrypoint

### Environment

Lavis sets these environment variables:

- `NO_COLOR=1`
- `CLICOLOR=0`
- `CLICOLOR_FORCE=0`
- `TERM=dumb`

It uses a fixed minimal `PATH` and does **not** inherit the host `PATH` or any
other arbitrary environment variables. Telegram credentials are never passed
to a module. Entrypoints must be directly executable; Lavis does not invoke a
shell.

Secret env vars (`LAVIS_API_ID`, `LAVIS_API_HASH`) are removed from the
module's environment.

### Supported capabilities (optional)

Declare capabilities in `module.json` to tell Lavis what your module
needs. These are descriptive only — no sandboxing is applied.

## Troubleshooting

### Module not responding

Check that the entrypoint is executable and has a valid shebang. Enable
debug logging:

```bash
RUST_LOG=lavis=debug lavis
```

### Manifest validation fails

Run:

```bash
lavis modules validate ~/.local/share/lavis/modules/my-module/module.json
```

### Process crashes

Check the Lavis logs for safe crash metadata. stderr is drained and capped to
avoid blocking the child, but raw stderr is not logged by default.
