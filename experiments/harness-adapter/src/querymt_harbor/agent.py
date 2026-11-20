"""Harbor agent that runs the pinned `qmt-headless` with the frozen harness adapter profile.

Design D2: `install()` uploads a host-verified `qmt-headless` archive, the profile, its
prompts and the pinned provider plugin, and runs no package-index or network command.
`run()` starts `qmt-headless run` once, writing a session database into the agent log
directory. In a `finally` (so also when Harbor cancels the run at its agent timeout) it
kills the run process if it is still alive and runs `qmt-headless export`, which writes
the native ATIF trajectory and a summary from that database.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import shlex
from pathlib import Path, PurePosixPath
from typing import Any, override

from harbor.agents.capabilities import AgentCapabilities
from harbor.agents.installed.base import BaseInstalledAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from .config import Reference, load_reference
from .plugin import EMPTY_MODELS_REGISTRY, HttpGet, fetch_plugin, httpx_get, providers_toml
from .release import fetch_archive

_MAX_STEPS = re.compile(r"^(max_steps\s*=\s*)\d+(\s*(?:#.*)?)$", re.MULTILINE)
_MACHINES = {"amd64": "x86_64", "arm64": "aarch64"}
DEFAULT_EXPORT_TIMEOUT_SEC = 60


def set_max_steps(profile_text: str, step_limit: int) -> str:
    """Rewrite the profile's `max_steps` and nothing else."""
    matches = _MAX_STEPS.findall(profile_text)
    if len(matches) != 1:
        raise ValueError(
            f"expected exactly one max_steps line in the profile, found {len(matches)}"
        )
    return _MAX_STEPS.sub(lambda m: f"{m.group(1)}{step_limit}{m.group(2)}", profile_text)


def _check_step_limit(step_limit: Any) -> int | None:
    if step_limit is None:
        return None
    if isinstance(step_limit, bool) or not isinstance(step_limit, int) or step_limit < 1:
        raise ValueError(f"step_limit must be an integer >= 1, got {step_limit!r}")
    return step_limit


def _check_export_timeout(value: Any) -> float:
    if isinstance(value, bool) or not isinstance(value, int | float) or value <= 0:
        raise ValueError(f"export_timeout_sec must be a number > 0, got {value!r}")
    return float(value)


def default_cache_dir() -> Path:
    override = os.environ.get("QMT_HARNESS_CACHE")
    return Path(override) if override else Path.home() / ".cache" / "querymt-harbor"


def platform_id(uname_s: str, uname_m: str) -> str:
    machine = uname_m.strip().lower()
    return f"{uname_s.strip().lower()}-{_MACHINES.get(machine, machine)}"


class QueryMTAgent(BaseInstalledAgent):
    """Runs `qmt-headless` in a Harbor sandbox and reports its native ATIF trajectory."""

    capabilities = AgentCapabilities(atif=True)

    INSTALL_DIR = "/installed-agent/qmt"
    BIN_PATH = "/opt/qmt/bin/qmt-headless"
    BINARY_NAME = "qmt-headless"

    def __init__(
        self,
        *,
        step_limit: int | None = None,
        reference: Reference | str | Path | None = None,
        cache_dir: str | Path | None = None,
        export_timeout_sec: float = DEFAULT_EXPORT_TIMEOUT_SEC,
        **kwargs: Any,
    ) -> None:
        # Validate before anything else so a bad budget never reaches a sandbox.
        self._step_limit = _check_step_limit(step_limit)
        self._export_timeout_sec = _check_export_timeout(export_timeout_sec)
        self._reference = (
            reference if isinstance(reference, Reference) else load_reference(reference)
        )
        self._cache_dir = Path(cache_dir) if cache_dir else default_cache_dir()
        # Replaceable in tests; downloads happen on the host, never in the sandbox.
        self._http_get: HttpGet = httpx_get
        super().__init__(**kwargs)

    @staticmethod
    @override
    def name() -> str:
        return "querymt"

    @override
    def version(self) -> str | None:
        return self._reference.headless.version

    # --- layout -----------------------------------------------------------------

    @property
    def profile_remote_path(self) -> str:
        # The profile names its prompts as ../prompts/<file>, so keep that layout.
        return f"{self.INSTALL_DIR}/confs/{self._reference.profile.name}"

    @property
    def plugin_remote_path(self) -> str:
        return f"{self.INSTALL_DIR}/plugins/{self._reference.plugin.filename}"

    @property
    def qmt_log_dir(self) -> str:
        """Where the run records its PID, session database and summary (agent log dir)."""
        return f"{self.environment_logs_dir.as_posix()}/qmt"

    @property
    def trajectory_remote_path(self) -> str:
        return f"{self.environment_logs_dir.as_posix()}/trajectory.json"

    @property
    def instruction_remote_path(self) -> str:
        return f"{self.INSTALL_DIR}/instruction.txt"

    # --- files uploaded to the sandbox -------------------------------------------

    def endpoint_url(self) -> str:
        url = self._extra_env.get("QMT_BASE_URL")
        if not url:
            raise ValueError("QMT_BASE_URL is not set in the agent environment")
        return url

    def stage_files(self) -> dict[str, bytes]:
        """Remote path -> content for every small file the sandbox needs."""
        profile = self._reference.profile.read_text()
        if self._step_limit is not None:
            profile = set_max_steps(profile, self._step_limit)
        files = {self.profile_remote_path: profile.encode()}
        for prompt in self._reference.prompts:
            files[f"{self.INSTALL_DIR}/prompts/{prompt.name}"] = prompt.read_bytes()
        files[f"{self.INSTALL_DIR}/providers.toml"] = providers_toml(
            self.plugin_remote_path, self.endpoint_url()
        ).encode()
        files[f"{self.INSTALL_DIR}/models.dev.json"] = EMPTY_MODELS_REGISTRY.encode()
        return files

    # --- install ----------------------------------------------------------------

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        files = self.stage_files()  # fails on a missing endpoint before any sandbox command
        # Everything downloaded is verified on the host before anything is uploaded.
        plugin = fetch_plugin(self._reference.plugin, self._cache_dir, self._http_get)
        platform = await self._detect_platform(environment)
        archive = fetch_archive(self._reference.headless, platform, self._cache_dir, self._http_get)

        await self._install_binary(environment, archive)
        await self._upload_state(environment, files, plugin)

    async def _detect_platform(self, environment: BaseEnvironment) -> str:
        result = await environment.exec(command="uname -s && uname -m")
        lines = [line.strip() for line in (result.stdout or "").splitlines() if line.strip()]
        if result.return_code != 0 or len(lines) < 2:
            raise RuntimeError(f"Failed to detect the sandbox platform: {result.stdout!r}")
        return platform_id(lines[0], lines[1])

    async def _upload_file(self, environment: BaseEnvironment, local: Path, remote: str) -> None:
        parent = shlex.quote(str(PurePosixPath(remote).parent))
        await self.exec_as_root(environment, command=f"mkdir -p {parent}")
        await environment.upload_file(source_path=local, target_path=remote)

    async def _install_binary(self, environment: BaseEnvironment, archive: Path) -> None:
        remote_archive = f"/tmp/{archive.name}"
        await self._upload_file(environment, archive, remote_archive)
        bin_path = shlex.quote(self.BIN_PATH)
        # The release archive nests the binary in a versioned directory.
        await self.exec_as_root(
            environment,
            command=(
                "set -eu; d=$(mktemp -d); "
                f'tar -xzf {shlex.quote(remote_archive)} -C "$d"; '
                f'b=$(find "$d" -type f -name {self.BINARY_NAME} | head -n 1); '
                'test -n "$b"; '
                f'install -D -m 0755 "$b" {bin_path}; '
                f'rm -rf "$d" {shlex.quote(remote_archive)}'
            ),
        )

    async def _upload_state(
        self, environment: BaseEnvironment, files: dict[str, bytes], plugin: Path
    ) -> None:
        stage = self.logs_dir / "qmt-stage"
        stage.mkdir(parents=True, exist_ok=True)
        for index, (remote, content) in enumerate(sorted(files.items())):
            local = stage / f"{index}-{PurePosixPath(remote).name}"
            local.write_bytes(content)
            await self._upload_file(environment, local, remote)
        await self._upload_file(environment, plugin, self.plugin_remote_path)
        install_dir = shlex.quote(self.INSTALL_DIR)
        # The agent user writes provider caches; the uploaded files stay readable.
        await self.exec_as_root(environment, command=f"chmod -R a+rwX {install_dir}")

    # --- run --------------------------------------------------------------------

    def _state_env(self) -> dict[str, str]:
        return {
            "QMT_HOME": self.INSTALL_DIR,
            "QMT_CONFIG_DIR": self.INSTALL_DIR,
            "QMT_PROVIDER_CACHE_DIR": self.INSTALL_DIR,
        }

    def run_command(self) -> str:
        """One `qmt-headless run`; records its PID first so a timeout can kill it."""
        q = shlex.quote
        qdir = self.qmt_log_dir
        return (
            f"mkdir -p {q(qdir)} && echo $$ > {q(qdir + '/run.pid')} && "
            f"exec {q(self.BIN_PATH)} run {q(self.profile_remote_path)} "
            f'--cwd "$PWD" --instruction-file {q(self.instruction_remote_path)} '
            f"--db {q(qdir + '/sessions.db')}"
        )

    def kill_command(self) -> str:
        pid_file = shlex.quote(self.qmt_log_dir + "/run.pid")
        return (
            f"pid=$(cat {pid_file} 2>/dev/null || true); "
            'if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then '
            'kill -TERM "$pid"; '
            'for i in 1 2 3 4 5; do kill -0 "$pid" 2>/dev/null || break; sleep 1; done; '
            'if kill -0 "$pid" 2>/dev/null; then kill -KILL "$pid"; fi; '
            "fi; true"
        )

    def export_command(self) -> str:
        q = shlex.quote
        qdir = self.qmt_log_dir
        return (
            f"mkdir -p {q(qdir)} && {q(self.BIN_PATH)} export --db {q(qdir + '/sessions.db')} "
            f"--atif {q(self.trajectory_remote_path)} --summary {q(qdir + '/summary.json')}"
        )

    @override
    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        try:
            await self._upload_instruction(environment, self.render_instruction(instruction))
            # Harbor bounds this call (asyncio.wait_for); no time budget is passed on.
            await self.exec_as_agent(environment, command=self.run_command(), env=self._state_env())
        finally:
            await self._stop_and_export(environment)

    async def _upload_instruction(self, environment: BaseEnvironment, instruction: str) -> None:
        stage = self.logs_dir / "qmt-stage"
        stage.mkdir(parents=True, exist_ok=True)
        local = stage / "instruction.txt"
        local.write_text(instruction)
        await self._upload_file(environment, local, self.instruction_remote_path)
        await self.exec_as_root(
            environment, command=f"chmod a+r {shlex.quote(self.instruction_remote_path)}"
        )

    async def _stop_and_export(self, environment: BaseEnvironment) -> None:
        """Kill a surviving run, then export. Never masks the error being propagated."""
        try:
            await self.exec_as_agent(environment, command=self.kill_command())
        except Exception as error:  # noqa: BLE001 - must still try to export
            self.logger.warning(f"could not stop the qmt-headless run: {error}")
        try:
            await asyncio.wait_for(
                self.exec_as_agent(
                    environment,
                    command=self.export_command(),
                    env=self._state_env(),
                    timeout_sec=int(self._export_timeout_sec),
                ),
                timeout=self._export_timeout_sec,
            )
        except Exception as error:  # noqa: BLE001 - a failed export means atif=none
            self.logger.warning(f"qmt-headless export failed, no trajectory: {error!r}")

    # --- context ----------------------------------------------------------------

    @override
    def populate_context_post_run(self, context: AgentContext) -> None:
        path = self.logs_dir / "trajectory.json"
        if not path.is_file():
            return
        try:
            metrics = json.loads(path.read_text()).get("final_metrics") or {}
        except (OSError, ValueError) as error:
            self.logger.warning(f"unreadable trajectory {path}: {error}")
            return
        context.n_input_tokens = metrics.get("total_prompt_tokens")
        context.n_output_tokens = metrics.get("total_completion_tokens")
        context.n_cache_tokens = metrics.get("total_cached_tokens")
        context.cost_usd = metrics.get("total_cost_usd")
