# Module API v5: timers and allowlisted Telegram calls

Module API v5 adds structured timer subscriptions and a deliberately small,
core-mediated Telegram gateway. It extends the external-process API; it does
not change the v1–v4 manifests or JSON Lines messages. A module selects v5 by
setting both its manifest `schema_version` and every JSON Lines
`protocol_version` to `5`.

> V5 is a frozen wire contract for the current phase. Existing v1–v4 modules
> continue to use their documented contracts unchanged: see [v1](module-api-v1.md),
> [v2/v3](module-api-v2.md), and [v4](module-api-v4.md).

## V5 manifest

V5 accepts the prior command, message-subscription, and action metadata plus
two V5-only capabilities:

| Capability | V5 use |
| --- | --- |
| `timer` | Required for a structured `timer.tick` subscription. |
| `telegram.invoke` | Required before the module may request an allowlisted Telegram method. |

Both capability names are invalid in schemas 2–4. A timer subscription is an
object, not the legacy string form:

```json
{
  "type": "timer.tick",
  "interval_seconds": 30
}
```

There may be **one** such subscription per V5 module. `interval_seconds` is an
integer from **30** through **86400** seconds inclusive. Unknown fields,
another `type`, duplicate/second timers, and values outside that range are
rejected. Legacy string subscriptions such as `"message.created"` keep their
existing meaning.

Example manifest fragment for a timer-only module:

```json
{
  "schema_version": 5,
  "id": "online",
  "name": "Always Online",
  "version": "1.0.0",
  "author": "Example",
  "entrypoint": "bin/online",
  "capabilities": ["timer", "telegram.invoke"],
  "subscriptions": [{"type": "timer.tick", "interval_seconds": 30}],
  "commands": [{
    "name": "online",
    "summary_ru": "Статус онлайн",
    "description_ru": "Управляет периодическим обновлением статуса.",
    "usage": "[on|off|status]",
    "examples": []
  }]
}
```

## Timer lifecycle

Lavis starts timer scheduling only after the child has successfully completed
`initialize`. The first tick waits for a full configured interval; it is not
emitted immediately. A module is single-flight: if its process is busy when a
tick is due, that tick is skipped rather than queued. A process failure stops
that module's scheduler without affecting other modules. Scheduler tasks are
cancelled and joined during replacement and shutdown, before the core Telegram
client disconnects.

Each delivery is a normal core request with a unique `event_id`:

```json
{
  "protocol_version": 5,
  "type": "event",
  "request_id": "41",
  "event": "timer.tick",
  "payload": {"event_id": "timer-42"}
}
```

Reply with the matching terminal event response and no actions:

```json
{"protocol_version":5,"type":"event_result","request_id":"41","actions":[]}
```

`actions` may be omitted where the V5 event-result compatibility rules permit
it; timer events never permit message actions.

## Nested `telegram.invoke`

While one core parent request is active, a V5 module may emit at most **one**
nested gateway request. It must carry the active parent `request_id`:

```json
{
  "protocol_version": 5,
  "type": "telegram.invoke",
  "request_id": "41",
  "call_id": "online_1",
  "method": "account.updateStatus",
  "params": {"offline": false}
}
```

`call_id` is module-supplied and is echoed with the same parent request ID.
It must be 1–64 ASCII characters matching `[A-Za-z0-9_-]+` exactly.
The core rejects a wrong/inactive parent ID, a second invocation for that
parent, a duplicate active call ID, missing capability, malformed request, or
an invocation after the terminal reply. Calls have finite deadlines: V5
reserves bounded time for the Telegram operation, for writing its result, and
for the terminal module reply. A module must wait for the result before it
emits that terminal reply.

The core replies on stdin with one of these envelopes:

```json
{"protocol_version":5,"type":"telegram.result","request_id":"41","call_id":"online_1","ok":true,"result":true}
```

```json
{
  "protocol_version": 5,
  "type": "telegram.result",
  "request_id": "41",
  "call_id": "online_1",
  "ok": false,
  "error": {
    "kind": "rpc",
    "code": 420,
    "name": "FLOOD_WAIT",
    "message": "FLOOD_WAIT",
    "retry_after_seconds": 17
  }
}
```

On success `ok` is `true` and `result` is present. On failure `ok` is `false`
and `error` contains required `kind` and `message`; `code`, when present, is a
numeric JSON integer; `name` and
`retry_after_seconds` are present only when applicable. Typical kinds are
`validation`, `capability`, `timeout`, `transport`, and `rpc`.

### Current policy

The current implementation has one handwritten allowlisted
`account.updateStatus` handler; there is no generated registry. It is **not** a
generic JSON-to-TL dispatcher. Its exact parameter object is `{ "offline":
false }` (or `true`); unknown fields are rejected. Unknown, destructive,
raw/dynamic Telegram methods and direct client, session, or credential access
are denied.

Future registry entries must receive core-owned module/request context. For
future peer or media operations, opaque references must be validated as
module-scoped and request-scoped by the core; modules do not receive Telegram
access hashes. A future file model will use opaque `file_ref` values plus
explicit upload/download operations. It will not embed file contents as base64
inside JSON Lines.

## Reference module and Go helper

[`modules/always-online/`](../modules/always-online/) is the independent V5
reference module. It subscribes to the fixed 30-second core timer, persists its
own enabled/interval state in `$LAVIS_MODULE_STATE_DIR/state.json`, and exposes
`,always-online.online on`, `,always-online.online off`,
`,always-online.online status` (or `,always-online.online`), and
`,always-online.online interval <seconds>`. Its configured interval is resolved by gating
the fixed core ticks, so it can add up to one 30-second tick of delay; it does
not create another timer. Its Go helper is optional implementation support.
The manifest and JSON Lines contract in this document are the wire source of
truth.

## Security and packaging

V5 does not sandbox the child. An external module remains arbitrary executable
code with the Lavis user's OS permissions. The `timer` and `telegram.invoke`
capabilities are enforced at the v5 core scheduler/gateway boundary, but do not
constrain direct OS access. `.lmod` inspection is structural; it is not malware
analysis, provenance verification, or a trust decision. Package and enable only
code you trust. See [External modules](external-modules.md) and
[Packaging `.lmod`](lmod-packaging.md).
