"""Harbor installed-agent adapter that runs ``qq run`` inside task containers.

The adapter:

1. Uploads a prebuilt ``qq`` binary into the container (``QQ_BINARY_PATH`` or
   the repository's release build), or — explicitly opted into — builds from
   source inside the container via ``install-from-source.sh``.
2. Runs ``qq run`` in the task workspace with the explicit unattended policy
   (``--approval auto``), JSONL output, and a durable trace file under
   Harbor's agent-log mount. Provider credentials pass through the
   environment and are never logged.
3. Converts the JSONL trace to an ATIF-v1.7 ``trajectory.json`` after the
   run, embedding child (sub-agent) trajectories, validates it with Harbor's
   validator, and populates Harbor's cost/token fields from the trial's
   outcome record. Unknown usage or pricing stays unknown.
"""

from __future__ import annotations

import os
import shlex
import subprocess
from pathlib import Path
from typing import Any, override

from harbor.agents.installed.base import BaseInstalledAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.utils.trajectory_utils import format_trajectory_json
from harbor.utils.trajectory_validator import TrajectoryValidator

from qq_harbor.atif import TraceError, convert_trace, load_trace

_NANOS_PER_USD = 1_000_000_000

# Environment variables forwarded from the Harbor host into the container so
# qq's built-in providers can authenticate. Values are never logged.
_CREDENTIAL_ENV_VARS = (
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "GEMINI_API_KEY",
    "XAI_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
)

# QQ_-prefixed configuration passed through verbatim, except the adapter's own
# host-side install knobs.
_ADAPTER_ONLY_ENV_VARS = {"QQ_BINARY_PATH", "QQ_BUILD_FROM_SOURCE"}

_BINARY_DEST = "/installed-agent/qq"
_SOURCE_TAR_DEST = "/installed-agent/qq-src.tar.gz"
_INSTALL_SCRIPT_DEST = "/installed-agent/install-from-source.sh"

_TRACE_FILENAME = "qq-trace.jsonl"
_LOG_FILENAME = "qq-run.log"
_TRAJECTORY_FILENAME = "trajectory.json"

# Container paths under Harbor's mounted agent-log directory.
_CONTAINER_TRACE_PATH = f"/logs/agent/{_TRACE_FILENAME}"
_CONTAINER_LOG_PATH = f"/logs/agent/{_LOG_FILENAME}"

# qq run exit statuses (see src/headless.rs). Codes that describe the task's
# fate produce a trajectory and let the verifier score the attempt; codes that
# describe a broken harness or configuration fail the trial loudly.
_TASK_LEVEL_EXIT_CODES = {
    0: "completed",
    1: "task_failed",
    3: "timed_out_or_budget_exhausted",
}
_FATAL_EXIT_CODES = {
    2: "invalid configuration",
    4: "harness or persistence failure",
    130: "interrupted",
}


class QQAgent(BaseInstalledAgent):
    """Runs the qq durable headless mode as a Harbor installed agent."""

    SUPPORTS_ATIF: bool = True

    def __init__(
        self,
        *args: Any,
        binary_path: str | None = None,
        timeout_seconds: int | None = None,
        max_turns: int | None = None,
        max_cost_usd: float | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        self._binary_path = binary_path
        self._timeout_seconds = timeout_seconds
        self._max_turns = max_turns
        self._max_cost_usd = max_cost_usd
        self._exit_code: int | None = None

    @staticmethod
    @override
    def name() -> str:
        return "qq"

    @override
    def get_version_command(self) -> str | None:
        return f"{_BINARY_DEST} --version"

    @override
    def parse_version(self, stdout: str) -> str:
        """Return the semver from Cargo's conventional ``qq <version>`` output."""
        fields = stdout.strip().split()
        return fields[-1] if fields else ""

    # ------------------------------------------------------------------
    # Install
    # ------------------------------------------------------------------

    def _repo_root(self) -> Path:
        # benchmarks/harbor/qq_harbor/agent.py -> repository root.
        return Path(__file__).resolve().parents[3]

    def _resolve_host_binary(self) -> Path | None:
        """Locate a prebuilt Linux qq binary on the Harbor host."""
        candidates: list[Path] = []
        if self._binary_path:
            candidates.append(Path(self._binary_path))
        env_path = self._get_env("QQ_BINARY_PATH")
        if env_path:
            candidates.append(Path(env_path))
        root = self._repo_root()
        candidates.append(root / "target" / "x86_64-unknown-linux-musl" / "release" / "qq")
        candidates.append(root / "target" / "release" / "qq")
        for candidate in candidates:
            if candidate.is_file():
                return candidate
        return None

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        binary = self._resolve_host_binary()
        if binary is not None:
            self.logger.info(f"Uploading qq binary from {binary}")
            await environment.upload_file(binary, _BINARY_DEST)
            await self.exec_as_root(environment, f"chmod 755 {_BINARY_DEST}")
            return

        if self._get_env("QQ_BUILD_FROM_SOURCE"):
            await self._install_from_source(environment)
            return

        raise RuntimeError(
            "No qq binary found. Set QQ_BINARY_PATH to a prebuilt Linux binary "
            "(build one with `cargo build --release` or, for maximum "
            "portability, `cargo build --release --target "
            "x86_64-unknown-linux-musl`), pass binary_path as an agent kwarg, "
            "or set QQ_BUILD_FROM_SOURCE=1 to compile inside the container "
            "(slow; documented fallback, not the default)."
        )

    async def _install_from_source(self, environment: BaseEnvironment) -> None:
        """Cargo-build fallback: archive the repo, build in the container."""
        root = self._repo_root()
        setup_dir = self.logs_dir / "setup"
        setup_dir.mkdir(parents=True, exist_ok=True)
        tarball = setup_dir / "qq-src.tar.gz"
        self.logger.info("Archiving qq sources for an in-container cargo build")
        subprocess.run(
            ["git", "-C", str(root), "archive", "--format=tar.gz",
             "-o", str(tarball), "HEAD"],
            check=True,
        )
        await environment.upload_file(tarball, _SOURCE_TAR_DEST)
        script = Path(__file__).resolve().parent / "install-from-source.sh"
        await environment.upload_file(script, _INSTALL_SCRIPT_DEST)
        await self.exec_as_root(
            environment,
            f"chmod 755 {_INSTALL_SCRIPT_DEST} && {_INSTALL_SCRIPT_DEST} "
            f"{_SOURCE_TAR_DEST} {_BINARY_DEST}",
            timeout_sec=3600,
        )

    # ------------------------------------------------------------------
    # Run
    # ------------------------------------------------------------------

    def _run_env(self) -> dict[str, str]:
        env: dict[str, str] = {}
        for key in _CREDENTIAL_ENV_VARS:
            value = self._get_env(key)
            if value is not None:
                env[key] = value
        for key, value in {**os.environ, **self._extra_env}.items():
            if key.startswith("QQ_") and key not in _ADAPTER_ONLY_ENV_VARS:
                env[key] = value
        return env

    def _build_command(self, instruction: str) -> str:
        parts = [_BINARY_DEST]
        if self.model_name:
            parts.extend(["--model", shlex.quote(self.model_name)])
        parts.extend(
            [
                "run",
                "--approval",
                "auto",
                "--format",
                "jsonl",
                "--trace",
                _CONTAINER_TRACE_PATH,
            ]
        )
        if self._timeout_seconds is not None:
            parts.extend(["--timeout-seconds", str(int(self._timeout_seconds))])
        if self._max_turns is not None:
            parts.extend(["--max-turns", str(int(self._max_turns))])
        if self._max_cost_usd is not None:
            parts.extend(["--max-cost-usd", str(self._max_cost_usd)])
        parts.extend(["--", shlex.quote(instruction)])
        command = " ".join(parts)
        return f"{command} >{_CONTAINER_LOG_PATH} 2>&1"

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        instruction = self.render_instruction(instruction)
        command = self._build_command(instruction)
        env = self._run_env()

        # Deliberately not routed through _exec: a nonzero task-level exit
        # (task failure, timeout, budget) still carries a complete trace and
        # must reach the verifier instead of erroring the trial.
        self.logger.info("Running qq headless task")
        result = await environment.exec(
            command=command,
            env=env,
            timeout_sec=None if self._timeout_seconds is None
            else int(self._timeout_seconds) + 120,
        )
        self._exit_code = result.return_code
        context.metadata = {
            **(context.metadata or {}),
            "qq_exit_code": result.return_code,
        }

        if result.return_code in _TASK_LEVEL_EXIT_CODES:
            self.logger.info(
                "qq run finished: %s (exit %d)",
                _TASK_LEVEL_EXIT_CODES[result.return_code],
                result.return_code,
            )
            return

        reason = _FATAL_EXIT_CODES.get(result.return_code, "unknown failure")
        tail = await environment.exec(command=f"tail -c 4000 {_CONTAINER_LOG_PATH}")
        raise self._classify_exec_error(
            f"qq run ({reason})",
            type(result)(
                stdout=tail.stdout,
                stderr=result.stderr,
                return_code=result.return_code,
            ),
        )

    # ------------------------------------------------------------------
    # Post-run: ATIF conversion, validation, accounting
    # ------------------------------------------------------------------

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        trace_path = self.logs_dir / _TRACE_FILENAME
        if not trace_path.exists():
            raise RuntimeError(
                f"QQ produced no durable trace at {trace_path}; the trial is invalid"
            )

        try:
            records = load_trace(trace_path)
            trajectory = convert_trace(records)
        except TraceError as error:
            raise RuntimeError(f"Could not convert the QQ trace to ATIF: {error}") from error

        trajectory_path = self.logs_dir / _TRAJECTORY_FILENAME
        trajectory_path.write_text(format_trajectory_json(trajectory) + "\n")

        validator = TrajectoryValidator()
        if not validator.validate(trajectory_path):
            raise RuntimeError(
                "Generated QQ trajectory failed ATIF validation: "
                + "; ".join(validator.get_errors())
            )

        outcome = next(
            (record for record in records if record.get("type") == "outcome"), None
        )
        if outcome is None:
            raise RuntimeError("QQ trace has no terminal outcome record")

        usage = outcome.get("usage")
        if isinstance(usage, dict):
            fresh = int(usage.get("input_tokens", 0))
            cache_read = int(usage.get("cache_read_input_tokens", 0))
            cache_write = int(usage.get("cache_write_input_tokens", 0))
            context.n_input_tokens = fresh + cache_read + cache_write
            context.n_cache_tokens = cache_read
            context.n_output_tokens = int(usage.get("output_tokens", 0))

        cost_nanos = outcome.get("estimated_cost_usd_nanos")
        if isinstance(cost_nanos, int):
            context.cost_usd = cost_nanos / _NANOS_PER_USD

        context.metadata = {
            **(context.metadata or {}),
            "qq_status": outcome.get("status"),
            "qq_exit_code": outcome.get("exit_code", self._exit_code),
        }
        message = outcome.get("message")
        if message:
            context.metadata["qq_message"] = message
