#!/usr/bin/env bash
# Run a reference harness (Claude Code, Codex CLI, or OpenCode) through Harbor's
# built-in adapters on the same model, task list, and machine as a QQ job so the
# comparison is paired. All runs go through the operator's LiteLLM gateway,
# which speaks both the Anthropic Messages API and the OpenAI APIs.
#
#   LITELLM_BASE_URL=https://gateway/v1 LITELLM_API_KEY=... \
#     benchmarks/harbor/compare/run-reference.sh claude-code us.anthropic.claude-sonnet-5 pilot-cc \
#       -i cobol-modernization -i fix-git
#
# Arguments after the job name are passed to `harbor run` unchanged; use the
# same --include-task-name / --n-attempts / --n-concurrent as the QQ job.
# Nothing here is a QQ code path; see README.md for how the numbers line up.
set -euo pipefail

usage() {
  echo "usage: $0 {claude-code|codex|opencode} GATEWAY_MODEL_ID JOB_NAME [harbor run args...]" >&2
  exit 2
}

[ $# -ge 3 ] || usage
harness=$1
model=$2
job=$3
shift 3

: "${LITELLM_BASE_URL:?set LITELLM_BASE_URL to the gateway's /v1 base URL}"
: "${LITELLM_API_KEY:?set LITELLM_API_KEY}"

repo=$(cd "$(dirname "$0")/../../.." && pwd)
jobs_dir=${QQ_EVAL_JOBS_DIR:-$repo/target/qq-eval/jobs}
export HARBOR_TELEMETRY=off

case "$harness" in
  claude-code)
    # claude_code.py: with ANTHROPIC_BASE_URL set the model id is passed whole
    # and every model alias is pinned to it, so no silent Haiku fallbacks.
    # The gateway base URL is the Anthropic-compatible root, without /v1.
    export ANTHROPIC_API_KEY=$LITELLM_API_KEY
    export ANTHROPIC_BASE_URL=${LITELLM_BASE_URL%/v1}
    harbor_model=$model
    ;;
  codex)
    # codex.py: OPENAI_API_KEY plus OPENAI_BASE_URL, also written to
    # config.toml because codex reads openai_base_url only from there.
    export OPENAI_API_KEY=$LITELLM_API_KEY
    export OPENAI_BASE_URL=$LITELLM_BASE_URL
    harbor_model=openai/$model
    ;;
  opencode)
    # opencode.py: writes provider.openai.options.baseURL from OPENAI_BASE_URL
    # and registers the model id under that provider.
    export OPENAI_API_KEY=$LITELLM_API_KEY
    export OPENAI_BASE_URL=$LITELLM_BASE_URL
    harbor_model=openai/$model
    ;;
  *) usage ;;
esac

exec harbor run \
  --agent "$harness" \
  --model "$harbor_model" \
  --dataset terminal-bench/terminal-bench-2 \
  --job-name "$job" \
  --jobs-dir "$jobs_dir" \
  "$@"
