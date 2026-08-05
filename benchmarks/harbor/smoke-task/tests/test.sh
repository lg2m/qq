#!/bin/bash
set -eu

if [ -f /app/qq-smoke.txt ] && [ "$(cat /app/qq-smoke.txt)" = "qq-harbor-smoke" ]; then
    echo 1 > /logs/verifier/reward.txt
    exit 0
fi

echo 0 > /logs/verifier/reward.txt
exit 1
