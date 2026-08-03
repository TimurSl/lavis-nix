# Always Online

Always Online is an independent Lavis Module API v5 executable.  On each core
`timer.tick`, it requests `account.updateStatus` with `{"offline":false}`.

## Commands

- `,online on` — enable updates;
- `,online off` — disable updates;
- `,online status` (or `,online`) — show state;
- `,online interval <seconds>` — store a desired interval from 30 to 86400.

The manifest requests the static 30-second core timer. `interval` is persisted
module configuration that gates calls on those ticks: after an invocation, the
module waits until the configured interval has elapsed before invoking again.
It does not register or run a second timer, and cannot alter the core
subscription interval. Resolution is therefore one core tick (up to 30 seconds
of additional delay).

The module only sends invocations while enabled. It requires the structured
`timer.tick` payload to contain a unique `event_id`. It creates a persisted
`online_<number>` call ID (letters, digits, and underscores only), sends one
`telegram.invoke` with its parent request ID, then waits for the matching
structured `telegram.result` before emitting the terminal `event_result`.
Failed temporary, timeout, and FloodWait results apply a backoff (using
`retry_after_seconds` when present) before later ticks can call Telegram again.

State is only read from and written to `$LAVIS_MODULE_STATE_DIR/state.json`.
Writes use a synced temporary file and atomic rename; malformed state falls
back to the safe default (`enabled`, configured interval 30).

## Build and test

```bash
cd modules/always-online
gofmt -w *.go
go test ./...
./build-lmod.sh
```

`build-lmod.sh` creates a reproducible Linux amd64 `dist/always-online.lmod`.
