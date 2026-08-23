# Agent shell hooks

Agent lifecycle hooks can place selected shell workloads under `with-limits`
without changing the command the model generates. A `PreToolUse` handler reads
the proposed command and replaces it with:

```sh
with-limits -c '<original command>'
```

The example in [`examples/with-limits-pre-tool-use.py`](../examples/with-limits-pre-tool-use.py)
targets POSIX shells and commands that begin with `uv run`, including common
`command`, `env`, `mise`, and absolute-path forms. It wraps the complete request,
so a trailing pipeline or `&&` sequence shares one resource budget. Edit
`starts_uv_run` to select a different workload launcher.

Rewriting a Claude Code or Codex tool input requires returning an `allow`
decision. That can skip an ordinary approval prompt. Keep the selector narrow,
and continue to use the agent's deny and ask rules for commands that must never
be approved automatically. The example does not treat hooks as a security
boundary.

## Install the shared hook

Save the example at a stable path, such as
`~/.config/with-limits/pre-tool-use.py`. The script uses only the Python standard
library and resolves `with-limits` from the hook process's `PATH`.

Test the installed copy before adding it to an agent configuration:

```sh
printf '%s\n' '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"uv run python -c '\''print(1)'\''"}}' \
  | python3 ~/.config/with-limits/pre-tool-use.py
```

The output should contain an `updatedInput.command` that invokes
`with-limits -c`. A missing executable or an unexpected payload produces a
visible `systemMessage` and leaves the original command unchanged. Other shell
commands produce no output and proceed normally.

The hook also declines to rewrite commands when `WITH_LIMITS_ACTIVE=1` is
already in its environment or when the request already invokes `with-limits`.
This prevents nested supervisors and repeated priority lowering.

## Claude Code

Add the handler to the `PreToolUse` section of `~/.claude/settings.json`. Merge
it with any existing hooks instead of replacing the file:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "python3 /Users/you/.config/with-limits/pre-tool-use.py",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
```

Use an absolute script path. Claude Code's
[hook reference](https://code.claude.com/docs/en/hooks) documents project-local
configuration and other handler types.

## Codex

Place the equivalent configuration in `~/.codex/hooks.json`, again merging it
with any hooks already present:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "^Bash$",
        "hooks": [
          {
            "type": "command",
            "command": "python3 /Users/you/.config/with-limits/pre-tool-use.py",
            "timeout": 5,
            "statusMessage": "Applying command resource limits"
          }
        ]
      }
    ]
  }
}
```

Codex hooks are enabled by default. Start Codex and use `/hooks` to review and
trust the new command hook. Repository hooks can instead live in
`.codex/hooks.json`; Codex loads them only after the project configuration is
trusted. See the [Codex hooks documentation](https://learn.chatgpt.com/docs/hooks)
for configuration locations, trust behavior, and the `PreToolUse` schema.

## OpenCode

OpenCode does not run the command hook directly. A local plugin can send the
same payload to the shared script and apply its rewritten input. Save this as
`~/.config/opencode/plugins/with-limits.ts`:

```ts
import type { Plugin } from "@opencode-ai/plugin"

const hook = `${process.env.HOME}/.config/with-limits/pre-tool-use.py`

type HookResult = {
  systemMessage?: string
  hookSpecificOutput?: { updatedInput?: { command?: string } }
}

export const WithLimits: Plugin = async () => ({
  "tool.execute.before": async (input, output) => {
    if (input.tool !== "bash") return
    const command = output.args.command
    if (typeof command !== "string" || !command) return

    const child = Bun.spawn(["python3", hook], {
      stdin: new TextEncoder().encode(
        JSON.stringify({
          hook_event_name: "PreToolUse",
          tool_name: "Bash",
          tool_input: output.args,
        }),
      ),
      stdout: "pipe",
    })
    const stdout = await new Response(child.stdout).text()
    if ((await child.exited) !== 0 || !stdout.trim()) return

    let result: HookResult
    try {
      result = JSON.parse(stdout) as HookResult
    } catch (error) {
      console.error(`with-limits hook returned invalid JSON: ${error}`)
      return
    }
    if (result.systemMessage) console.error(result.systemMessage)
    const rewritten = result.hookSpecificOutput?.updatedInput?.command
    if (rewritten) output.args.command = rewritten
  },
})
```

The plugin intentionally leaves the command unchanged when the hook produces no
rewrite. OpenCode loads global TypeScript plugins from
`~/.config/opencode/plugins/`; its
[plugin documentation](https://dev.opencode.ai/docs/plugins/) also describes
project-local plugins.

## Kimi Code CLI

Kimi Code CLI's `PreToolUse` hook can allow or block a tool call, but its
documented return protocol cannot replace `tool_input`. The shared rewrite hook
therefore cannot provide command-by-command wrapping.

Wrapping the Kimi process itself is a broader fallback:

```sh
with-limits -- kimi
```

That gives the agent process and all of its descendants one shared budget for
the entire session. A PATH-shadow wrapper for a specific workload command, such
as `uv`, provides narrower coverage. See the
[Kimi hooks documentation](https://www.kimi.com/code/docs/en/kimi-code-cli/customization/hooks.html)
for its supported decisions and event payloads.

## Windows

The shared example uses POSIX shell quoting. Windows agents need a hook or
plugin that quotes the replacement for their actual command shell. Codex
supports a separate `commandWindows` handler command, but that only selects how
the hook itself starts; the hook must still construct a replacement command for
PowerShell or `cmd.exe` correctly.
