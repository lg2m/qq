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
