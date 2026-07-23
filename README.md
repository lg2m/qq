# qq
A composable toolkit for building, running, and orchestrating AI agents.

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
`/resume`), and `/quit` (also `/exit`). Selecting a model creates a new session
because a session's model is immutable. The picker only lists built-in
providers with an available credential, and the footer shows context usage,
the selected model, working directory, and focused session cost.
