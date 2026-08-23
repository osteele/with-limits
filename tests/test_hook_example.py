"""Contract tests for the documented PreToolUse hook."""

from __future__ import annotations

import importlib.util
import shlex
import unittest
from pathlib import Path
from types import ModuleType


SCRIPT = (
    Path(__file__).resolve().parent.parent
    / "examples"
    / "with-limits-pre-tool-use.py"
)


def load_hook() -> ModuleType:
    spec = importlib.util.spec_from_file_location("with_limits_pre_tool_use", SCRIPT)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load {SCRIPT}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


HOOK = load_hook()


def payload(command: str) -> dict[str, object]:
    return {
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command, "description": "Run a workload"},
    }


class HookExampleTest(unittest.TestCase):
    def rewrite(self, command: str) -> dict[str, object] | None:
        return HOOK.rewrite_payload(
            payload(command),
            environ={},
            which=lambda name: "/opt/tools/with-limits" if name == "with-limits" else None,
        )

    def test_wraps_uv_run_and_preserves_the_complete_input(self) -> None:
        command = "uv run python -c 'print(1)' && echo done"
        output = self.rewrite(command)
        self.assertIsNotNone(output)
        specific = output["hookSpecificOutput"]
        self.assertEqual(specific["permissionDecision"], "allow")
        updated = specific["updatedInput"]
        self.assertEqual(updated["description"], "Run a workload")
        self.assertEqual(
            shlex.split(updated["command"]),
            ["/opt/tools/with-limits", "-c", command],
        )

    def test_recognizes_supported_launch_prefixes(self) -> None:
        commands = (
            "/usr/local/bin/uv run script.py",
            "ENV=value command uv run script.py",
            "mise exec python -- uv run script.py",
        )
        for command in commands:
            with self.subTest(command=command):
                self.assertIsNotNone(self.rewrite(command))

    def test_ignores_unselected_commands(self) -> None:
        for command in ("cargo test", "echo uv run script.py", "uv sync"):
            with self.subTest(command=command):
                self.assertIsNone(self.rewrite(command))

    def test_avoids_nested_guards(self) -> None:
        self.assertIsNone(self.rewrite("with-limits -- uv run script.py"))
        self.assertIsNone(self.rewrite("command with-limits -c 'uv run script.py'"))
        self.assertIsNone(
            HOOK.rewrite_payload(
                payload("uv run script.py"),
                environ={"WITH_LIMITS_ACTIVE": "1"},
                which=lambda _name: "/opt/tools/with-limits",
            )
        )

    def test_reports_missing_guard_without_blocking(self) -> None:
        output = HOOK.rewrite_payload(
            payload("uv run script.py"), environ={}, which=lambda _name: None
        )
        self.assertIn("not on PATH", output["systemMessage"])

    def test_reports_schema_drift(self) -> None:
        cases = (
            [],
            {"hook_event_name": "PostToolUse", "tool_name": "Bash"},
            {"hook_event_name": "PreToolUse", "tool_name": "Bash"},
            {
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": 42},
            },
        )
        for case in cases:
            with self.subTest(case=case):
                output = HOOK.rewrite_payload(case, environ={})
                self.assertIn("systemMessage", output)


if __name__ == "__main__":
    unittest.main()
