#!/usr/bin/env python3
"""Wrap selected agent shell commands with with-limits.

This example speaks the Claude Code and Codex PreToolUse JSON protocols. It is
intended for POSIX shells and deliberately targets only uv run workloads.
"""

from __future__ import annotations

import json
import os
import re
import shlex
import shutil
import sys
from collections.abc import Callable, Mapping


_ASSIGNMENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
_EXPLICIT_GUARD = re.compile(
    r"(?:^|[;&|]\s*)(?:(?:command|exec|env)\s+)?(?:[^\s;&|]*/)?with-limits(?:\s|$)"
)


def diagnostic(message: str) -> dict[str, str]:
    """Return a visible, non-blocking hook diagnostic."""
    return {"systemMessage": f"with-limits hook: {message}"}


def split_command(command: str) -> list[str]:
    """Split enough POSIX shell syntax to recognize a leading launcher."""
    return shlex.split(command, posix=True)


def starts_uv_run(words: list[str]) -> bool:
    """Recognize uv run after common environment and launcher prefixes."""
    while words and _ASSIGNMENT.match(words[0]):
        words = words[1:]

    if words and words[0] in {"command", "exec", "env", "time"}:
        words = words[1:]
        while words and _ASSIGNMENT.match(words[0]):
            words = words[1:]

    if len(words) >= 2 and os.path.basename(words[0]) == "uv":
        return words[1] == "run"

    if words and os.path.basename(words[0]) == "mise" and "--" in words:
        index = words.index("--") + 1
        return (
            len(words) > index + 1
            and os.path.basename(words[index]) == "uv"
            and words[index + 1] == "run"
        )

    return False


def rewrite_payload(
    payload: object,
    *,
    environ: Mapping[str, str] | None = None,
    which: Callable[[str], str | None] = shutil.which,
) -> dict[str, object] | None:
    """Validate and, when applicable, rewrite one PreToolUse payload."""
    if not isinstance(payload, dict):
        return diagnostic("expected a JSON object")

    event = payload.get("hook_event_name")
    if event != "PreToolUse":
        return diagnostic(f"expected hook_event_name PreToolUse, received {event!r}")

    if payload.get("tool_name") != "Bash":
        return None

    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        return diagnostic("Bash tool_input is not an object")

    command = tool_input.get("command")
    if not isinstance(command, str) or not command.strip():
        return diagnostic("Bash tool_input.command is not a non-empty string")

    active_environ = os.environ if environ is None else environ
    if active_environ.get("WITH_LIMITS_ACTIVE") == "1":
        return None
    if _EXPLICIT_GUARD.search(command):
        return None

    try:
        words = split_command(command)
    except ValueError as error:
        return diagnostic(f"could not parse Bash command: {error}")
    if not starts_uv_run(words):
        return None

    guard = which("with-limits")
    if guard is None:
        return diagnostic("with-limits is not on PATH; running the original command")

    updated_input = dict(tool_input)
    updated_input["command"] = f"{shlex.quote(guard)} -c {shlex.quote(command)}"
    return {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "updatedInput": updated_input,
        }
    }


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError as error:
        output: dict[str, object] | None = diagnostic(f"invalid JSON input: {error}")
    else:
        output = rewrite_payload(payload)

    if output is not None:
        print(json.dumps(output, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
