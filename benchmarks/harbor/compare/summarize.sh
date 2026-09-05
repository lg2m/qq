#!/usr/bin/env bash
# Summarize one or more Harbor jobs side by side from their trial result.json
# files: pass rate, dollars per attempt and per pass, and median wall time.
# Works for any Harbor agent (qq, claude-code, codex, opencode) because it reads
# only Harbor's own result schema, not the QQ trace. `cargo xtask eval report`
# remains the authoritative QQ scorecard; this is the cross-harness view.
#
#   benchmarks/harbor/compare/summarize.sh target/qq-eval/jobs/pilot-qq target/qq-eval/jobs/pilot-cc
set -euo pipefail

[ $# -ge 1 ] || { echo "usage: $0 JOB_DIR..." >&2; exit 2; }

printf '%-28s %-12s %6s %6s %8s %10s %10s %9s %6s\n' \
  job agent trials passes rate '$/attempt' '$/pass' 'p50 wall' exc

for job in "$@"; do
  jq -rs --arg job "$(basename "$job")" '
    def secs: if . == null then null else (sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601) end;
    map(select(.trial_name != null)) as $t
    | ($t | length) as $n
    | ($t | map(select((.verifier_result.rewards.reward // 0) >= 1)) | length) as $p
    | ($t | map(.agent_result.cost_usd // empty) | add // 0) as $cost
    | ($t | map(select(.exception_info != null)) | length) as $exc
    | ($t | map(select((.verifier_result.rewards.reward // 0) >= 1)
              | ((.finished_at | secs) - (.started_at | secs)) // empty)
        | sort | if length == 0 then null else .[(length / 2 | floor)] end) as $p50
    | [$job,
       ($t[0].config.agent.name // "?" | if contains(":") then split(":")[1] else . end),
       $n, $p,
       (if $n > 0 then ($p / $n * 100 | floor | tostring) + "%" else "-" end),
       (if $n > 0 then ($cost / $n * 1000 | round / 1000) else "-" end),
       (if $p > 0 then ($cost / $p * 1000 | round / 1000) else "-" end),
       (if $p50 == null then "-" else ($p50 | tostring) + "s" end),
       $exc]
    | @tsv' "$job"/*/result.json \
  | awk -F'\t' '{printf "%-28s %-12s %6s %6s %8s %10s %10s %9s %6s\n", $1,$2,$3,$4,$5,$6,$7,$8,$9}'
done
