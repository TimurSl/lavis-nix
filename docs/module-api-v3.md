# Module API v3

Module API v3 adds narrowly scoped events and typed actions to the v2 external
child-process runtime. It is normative for v3 modules; see the [ADR](adr/0002-module-runtime-and-distribution.md)
for the separation from packaging and installation.

## Compatibility and versions

Schema/protocol v2 modules continue to work unchanged. V3 is additive: v2
modules receive no events and retain their v2 execute wire shape. Unknown schema
or protocol versions fail closed. A process handshakes and speaks the protocol
version declared by its validated manifest.

## V3 manifest additions

V3 manifests add `default_command`, `subscriptions`, `actions`, and
`capabilities`. Initial values are:

| Field | Values |
| --- | --- |
| `subscriptions` | `message.created` |
| `actions` | `message.react` |
| `capabilities` | `message.read`, `message.react` |

Values are unique and known. `default_command` names an existing command.
`message.created` requires `message.read`; `message.react` requires
`message.react`. `message.edited` is deferred.

## Command routing and entity context

Resolution order is: built-in canonical command, explicit external
`module.command`, external module ID using `default_command`, then alias. Thus
`,fastfetch` calls `fastfetch.<default_command>` while `,fastfetch.show` stays
explicit. A module without a default command shows module help or a no-default
error; it never guesses.

A v3 execute message may include command-message custom emoji metadata:

```json
{"protocol_version":3,"type":"execute","request_id":"req-…","command":"manage","arguments":"add lavis | …","context":{"argument_entities":[{"type":"custom_emoji","offset_utf16":12,"length_utf16":2,"document_id":"5456140674028019486"}]}}
```

Offsets are relative to `arguments`, use UTF-16 code units, and custom emoji
document IDs are decimal strings. Ordinary emoji need no entity. This allows a
module to distinguish Telegram Premium custom emoji in syntax such as
`,autoreact add lavis | <custom emoji>`.

## Events and opaque references

The initial event is `message.created`:

```json
{"protocol_version":3,"type":"event","request_id":"req-…","event":"message.created","payload":{"event_id":"evt-…","message_ref":"mref-…","text":"lavis is cooking","outgoing":false,"entities":[]}}
```

The payload intentionally exposes no Telegram peer IDs, access hashes, message
IDs, sender IDs, or client object. `message_ref` is an opaque core-generated
capability token bound to the receiving module instance and current event
request. It refers only to that event's message, expires immediately after its
matching result, is never durable across restart, and cannot target arbitrary
messages.

Events go only to enabled, initialized v3 modules declaring both the
subscription and `message.read`. Event timeouts and broken modules are isolated
from the Lavis update loop.

## Event results and actions

An event result currently returns zero or one typed action:

```json
{"protocol_version":3,"type":"event_result","request_id":"req-…","actions":[{"type":"message.react","message_ref":"mref-…","reaction":{"type":"emoji","emoji":"🔥"}}]}
```

A custom reaction uses `{"type":"custom_emoji","document_id":"5456140674028019486"}`.
Core validates the matching request ID, module-bound reference, declared action
and capability, action count, and payload before it performs MTProto. A custom
document ID is a non-empty decimal string parseable by Telegram's actual integer
type. An ordinary emoji is non-empty, single-line, bounded, and contains no
controls or bidi controls. Telegram remains authoritative for chat-specific
reaction availability.

Core reports action failures without leaking raw Telegram errors. Modules never
receive credentials, session contents, arbitrary environment variables, raw
MTProto authority, or shell access. See [Telegram reactions](https://core.telegram.org/api/reactions),
[custom emoji reactions](https://core.telegram.org/constructor/reactionCustomEmoji),
and [entities](https://core.telegram.org/api/entities).
