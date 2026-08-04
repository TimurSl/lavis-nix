# Module API v5: account-status gateway

Module API v5 adds one small, core-mediated Telegram gateway capability to the
external-process API. It does not change v1–v4 manifests or JSON Lines
messages. A module selects v5 by setting both its manifest `schema_version` and
every JSON Lines `protocol_version` to `5`.

> V5 is a frozen wire contract for the current phase. Existing v1–v4 modules
> retain their documented contracts: see [v1](module-api-v1.md),
> [v2/v3](module-api-v2.md), and [v4](module-api-v4.md).

## Manifest capability

V5 modules may declare `telegram.account.status`. It is required before the
module can call the sole allowlisted gateway method, `account.updateStatus`.
The capability is invalid in schemas 2–4.

```json
{
  "schema_version": 5,
  "id": "status-example",
  "name": "Status example",
  "version": "1.0.0",
  "author": "Example",
  "entrypoint": "bin/status-example",
  "capabilities": ["telegram.account.status"],
  "commands": [{
    "name": "status",
    "summary_ru": "Обновить статус аккаунта",
    "description_ru": "Запрашивает обновление статуса аккаунта через gateway.",
    "usage": "[online|offline]",
    "examples": []
  }]
}
```

Timers are **not** part of V5. V5 defines no timer capability, timer
subscription, or timer event. If a module needs periodic work, it owns that
scheduling itself and remains responsible for avoiding excessive work.

## Gateway request and result

During an active core request, a V5 module may emit at most one gateway request
before its terminal reply. It carries the active parent `request_id`:

```json
{
  "protocol_version": 5,
  "type": "telegram.invoke",
  "request_id": "41",
  "call_id": "status_1",
  "method": "account.updateStatus",
  "params": {"offline": false}
}
```

`call_id` is module-supplied and must be 1–64 ASCII characters matching
`[A-Za-z0-9_-]+`. The core echoes both IDs on stdin. A module must wait for the
matching result before emitting its terminal reply.

```json
{"protocol_version":5,"type":"telegram.result","request_id":"41","call_id":"status_1","ok":true,"result":true}
```

On failure, `ok` is `false` and `error` has required `kind` and `message`.
Optional `code` is a numeric JSON integer; `name` and
`retry_after_seconds` appear when applicable.

```json
{
  "protocol_version": 5,
  "type": "telegram.result",
  "request_id": "41",
  "call_id": "status_1",
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

## Policy boundary

The core has one handwritten `account.updateStatus` handler. It accepts only
the parameter object `{ "offline": true }` or `{ "offline": false }`; unknown
fields are rejected. There is no generic JSON-to-TL dispatcher.

Raw MTProto access is prohibited. Modules receive no Telegram client, session,
credentials, peer access hashes, or other account internals. Uploads,
downloads, opaque peer references, opaque file references, and arbitrary
Telegram methods are outside this API.

## Security and packaging

V5 does not sandbox the child. An external module remains arbitrary executable
code with the Lavis user's OS permissions. The gateway capability is enforced
at the core boundary, but does not constrain direct OS access. `.lmod`
inspection is structural; it is not malware analysis, provenance verification,
or a trust decision. Package and enable only code you trust. See
[External modules](external-modules.md) and
[Packaging `.lmod`](lmod-packaging.md).
