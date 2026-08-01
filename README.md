# qq
A composable toolkit for building, running, and orchestrating AI agents.

Running `qq` with no subcommand opens the interactive TUI against a
user-scoped background server (`qq serve` runs one in the foreground).
Agents read, search, and edit workspace files, run shell commands, and
call MCP tools — every mutating action gated by an approval policy.
Design documentation lives in `docs/` (`design/` for decisions,
`plans/` for proposals).

## Quick Start

Set `OPENAI_API_KEY` and `QQ_MODEL`, then stream one response:

```sh
cargo run -- ask "Reply with pong"
```

To use a ChatGPT Codex subscription instead of an API key, sign in through the
browser and select an `openai-codex` model:

```sh
cargo run -- auth login openai-codex
QQ_MODEL=openai-codex/MODEL cargo run -- ask "Reply with pong"
```

On Windows, credentials that exceed Credential Manager's per-entry limit are
stored in a DPAPI-encrypted file bound to the current Windows user and machine.
Moving that file to another account or computer requires signing in again.

To use xAI, set `XAI_API_KEY` or sign in with OAuth. OAuth credentials are
refreshed and stored under the selected profile:

```sh
QQ_MODEL=xai/grok-4.3 cargo run -- ask "Reply with pong"
cargo run -- auth login xai --oauth
```

## Amazon Bedrock Mantle

Mantle reuses the OpenAI Responses, OpenAI Chat Completions, and Anthropic
Messages wire protocols. Configure a regional deployment with the standard AWS
credential chain:

```ron
(
    version: 1,
    model: "bedrock-mantle/MODEL",
    providers: {
        "bedrock-mantle": AmazonBedrockMantle(
            region: "us-east-1",
            api: OpenAiResponses,
            auth: Aws(DefaultChain),
        ),
    },
)
```

`api` also accepts `OpenAiChatCompletions` and `AnthropicMessages`. Authentication
may use `Aws(Profile("PROFILE"))` or a region-bound API key such as
`ApiKey(Env("BEDROCK_MANTLE_API_KEY"))`.

Profiles that use `credential_process` are currently unsupported and rejected.
QQ disables that aws-config provider because it cannot guarantee termination of
the subprocess when credential loading times out.

## Google Gemini

Set `GEMINI_API_KEY` and select a model under the built-in `google` provider:

```sh
QQ_MODEL=google/gemini-2.5-flash cargo run -- ask "Reply with pong"
```

Google API keys are sent only in the sensitive `x-goog-api-key` header, never in
the request URL.

## Tools And Approvals

Sessions run under an approval mode: `read-only` (only read-only tools
execute), `ask` (edits, writes, shell, and MCP calls each request
approval), or `auto` (workspace-contained edits and allowlisted shell
prefixes run unprompted). Approval prompts show the exact command or an
edit diff, and "approve for session" records a grant — shell grants are
command prefixes matched at word granularity. Nonzero exits and denials
return to the model as tool errors, not run failures. See
`docs/design/tools.md` for the full policy design.

## MCP Servers

MCP servers are declared in configuration and their tools join the same
approval and event flow as built-ins, namespaced `mcp__<server>__<tool>`:

```ron
mcp: {
    "executor": Stdio(command: "./executor.sh", args: ["--serve"],
                      env: ["EXECUTOR_API_KEY"], eager: true,
                      allow: ["execute", "skills"]),
}
```

Stdio and streamable-HTTP transports are supported; declarations are
trust-gated in workspace configuration.

## TUI Configuration

TUI preferences use a separate `tui.ron` document. QQ loads compiled defaults,
then the global configuration directory's `tui.ron`, then `.qq/tui.ron` files
from the repository root to the current directory.

```ron
(
    version: 1,
    layout: FoldFocus,
    bindings: (
        select_threadline: ["F1"],
        select_fold_focus: ["F2"],
        next_layout: ["Ctrl-N"],
        previous_layout: ["Ctrl-P"],
        toggle_navigator: ["Ctrl-T"],
        create_root_session: ["Alt-N"],
        create_child_session: ["Alt-C"],
        cancel_run: ["Ctrl-X"],
    ),
)
```

An omitted action inherits the previous layer. An empty list disables that
action. Invalid chords and collisions are rejected before the TUI starts.

The interactive composer recognizes `/models`, `/new`, `/sessions` (also
`/resume`), `/compact`, and `/quit` (also `/exit`). `/compact`
summarizes an idle session's history into a compact context so long
sessions keep going; stale read-only tool results are also pruned from
model context automatically. `/models` applies the choice to the
focused session (or creates one when none is focused); Ctrl-N always creates a
new session with the selected model. The pick also becomes the client default
for later `/new` creates until you choose another model. The picker only lists
built-in providers with an available credential, and the footer shows context
usage, the selected model, working directory, and focused session cost. Slash
command suggestions run immediately when selected with Enter or Tab. QQ names a
session from its first prompt; `/sessions` supports typing to search those names
before selecting one with Enter. In the session picker, Ctrl-D deletes the
highlighted session (with confirmation; a session with an active run must be
cancelled first) and Ctrl-P prunes every empty session in the workspace.
