# Module API v2 (alpha)

Module API v2 extends Lavis with support for **external modules** — self-contained
programs launched as child processes and controlled via a JSON-line protocol.

## Architecture

```
┌──────────────┐   JSON (stdin/stdout)    ┌──────────────────┐
│   Lavis      │ ◄──────────────────────► │  Module Process  │
│  (Rust)      │                          │  (any language)   │
└──────────────┘                          └──────────────────┘
```

Each enabled external module is a separate OS process started during Lavis
startup. Lavis
communicates with it over stdin/stdout using newline-delimited JSON messages.

## Module directory layout

```
modules/
└── my-module/
    ├── module.json        # Required: module metadata
    └── entrypoint.sh      # Required: executable entrypoint
```

## module.json

```json
{
    "schema_version": 2,
    "id": "my-module",
    "name": "My Module",
    "version": "1.0.0",
    "author": "Your Name",
    "entrypoint": "entrypoint.sh",
    "capabilities": [],
    "commands": [
        {
            "name": "greet",
            "summary_ru": "Приветствие",
            "description_ru": "Возвращает приветствие с переданным именем.",
            "usage": "[имя]",
            "examples": ["Alice", "Мир"]
        }
    ]
}
```

### Fields

| Field           | Required | Description                              |
|-----------------|----------|------------------------------------------|
| `schema_version`| yes      | Must be `2`.                             |
| `id`            | yes      | Unique 1–32 character identifier: lowercase ASCII first, then `a-z`, `0-9`, or `-`.|
| `name`          | yes      | Human-readable display name (1–64 chars).|
| `version`       | yes      | Module version string (1–32 chars).      |
| `author`        | yes      | Author name or handle (1–128 chars).     |
| `entrypoint`    | yes      | Relative path to the executable.         |
| `capabilities`  | no       | List of capability strings (see below).  |
| `commands`      | yes      | List of command descriptors (1–32).      |

### Commands

Each command descriptor has:

| Field           | Required | Description                              |
|-----------------|----------|------------------------------------------|
| `name`          | yes      | Command name (a-z, 0-9, `-`, 1–32 chars).|
| `summary_ru`    | yes      | Short Russian summary (1–120 chars, single-line). |
| `description_ru`| yes      | Long Russian description (1–2000 Unicode characters; ordinary newlines allowed, no other controls or bidi controls). |
| `usage`         | yes      | Argument syntax only, without the command name (1–256 Unicode characters; single-line, no control/bidi). |
| `examples`      | no       | Up to 16 argument-only example strings (≤256 Unicode characters each, single-line, no control/bidi). |

### Capabilities

Optional. Values:

- `host_information` — reads host system info
- `network` — network access
- `persistent_state_read` — reads state files
- `persistent_state_write` — writes state files

## Protocol

Protocol version: **2**

Messages are newline-delimited JSON. Lavis sends `CoreMessage` to stdin;
the module replies with `ModuleMessage` on stdout.

### Core → Module

```jsonc
// Initialize
{"protocol_version": 2, "type": "initialize", "request_id": "…", "module_id": "…"}

// Execute command
{"protocol_version": 2, "type": "execute", "request_id": "…", "command": "greet", "arguments": "Alice"}

// Health check
{"protocol_version": 2, "type": "health", "request_id": "…"}

// Shutdown
{"protocol_version": 2, "type": "shutdown", "request_id": "…"}
```

### Module → Core

```jsonc
// Initialization complete
{"protocol_version": 2, "type": "initialized", "request_id": "…", "module_id": "…"}

// Command result
{"protocol_version": 2, "type": "result", "request_id": "…", "text": "Hello, Alice!"}

// Error
{"protocol_version": 2, "type": "error", "request_id": "…", "code": "…", "message": "…"}

// Health OK
{"protocol_version": 2, "type": "health", "request_id": "…"}

// Log message
{"protocol_version": 2, "type": "log", "level": "info", "message": "…"}
```

## Security

External modules run as **untrusted child processes**. No sandboxing or
capability enforcement is applied. Only enable modules you trust. Capabilities
are descriptive metadata, not permissions.

The child environment is cleared. Lavis supplies only fixed display-related
variables and a fixed minimal `PATH`; the host `PATH`, Telegram credentials,
and arbitrary host environment variables are not inherited. The entrypoint
must be directly executable. stderr is drained with a size cap and its raw
content is not logged by default.

## Limitations (alpha)

- Single-process, single-module runtime
- No hot-reload
- No module-to-module communication
- No WASM or other sandbox
- No installer, remote repository, terminal, sudo, or arbitrary shell
