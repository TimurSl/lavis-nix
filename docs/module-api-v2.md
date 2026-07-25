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

Each external module is a separate OS process started on demand. Lavis
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
| `id`            | yes      | Unique module identifier (a-z, 0-9, `-`).|
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
| `description_ru`| yes      | Long Russian description (1–2000 chars). |
| `usage`         | yes      | Usage string (1–256 chars, single-line, no control/bidi). |
| `examples`      | no       | Up to 16 example strings (≤256 chars each, single-line). |

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

External modules run as **untrusted child processes**. No sandboxing is
applied. Only enable modules you trust. Capabilities constrain *Lavis-level
permissions*, not OS-level isolation.

## Limitations (alpha)

- Single-process, single-module runtime
- No hot-reload
- No module-to-module communication
- No WASM or other sandbox
