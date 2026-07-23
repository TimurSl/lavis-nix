#!/usr/bin/env python3
"""Echo module for Lavis external modules protocol v2."""

import json
import sys

PROTOCOL_VERSION = 2


def send(msg: dict) -> None:
    msg["protocol_version"] = PROTOCOL_VERSION
    sys.stdout.write(json.dumps(msg, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            send({"type": "error", "request_id": "?", "code": "decode_error", "message": "Invalid JSON"})
            continue

        pv = msg.get("protocol_version", 0)
        if pv != PROTOCOL_VERSION:
            send({
                "type": "error",
                "request_id": msg.get("request_id", "?"),
                "code": "protocol_version_mismatch",
                "message": "Expected protocol v" + str(PROTOCOL_VERSION),
            })
            continue

        msg_type = msg.get("type", "")
        request_id = msg.get("request_id", "?")

        if msg_type == "initialize":
            send({"type": "initialized", "request_id": request_id, "module_id": msg.get("module_id", "")})

        elif msg_type == "execute":
            command = msg.get("command", "")
            arguments = msg.get("arguments", "")
            if command == "echo":
                send({"type": "result", "request_id": request_id, "text": arguments})
            else:
                send({
                    "type": "error",
                    "request_id": request_id,
                    "code": "unknown_command",
                    "message": f"Unknown command: {command}",
                })

        elif msg_type == "health":
            send({"type": "health", "request_id": request_id})

        elif msg_type == "shutdown":
            send({"type": "initialized", "request_id": request_id, "module_id": msg.get("module_id", "")})
            sys.exit(0)

        else:
            send({
                "type": "error",
                "request_id": request_id,
                "code": "unknown_message",
                "message": f"Unknown message type: {msg_type}",
            })


if __name__ == "__main__":
    main()
