"""Type stubs for the QueryMT Agent Python bindings."""

from typing import Any, Callable, Literal, Mapping, TypedDict
from typing_extensions import deprecated

__all__: list[str]

class WorktreeError(RuntimeError):
    """A managed-worktree operation failed.

    ``code`` is a stable machine-readable identifier, for example
    ``workspace_service_unavailable``, ``invalid_options``, ``removal_refused``,
    ``owner_active``, ``workspace_not_archived``, ``unpreserved_content``,
    ``unpreserved_commits``, ``workspace_archived``, ``workspace_not_found``, ``owner_conflict``, ``conflicting_cwd`` or
    ``isolation_unsupported_layout``. Merge refusals add ``merge_conflict``,
    ``touches_local_changes``, ``source_missing``, ``source_active`` and
    ``commit_not_on_workspace_branch``.

    ``paths`` lists the files a refusal names (empty when it names none).
    """

    code: str
    paths: list[str]

class SubagentError(RuntimeError):
    """Changing the agent's subagents failed; the set is unchanged.

    ``code`` is one of ``invalid_id``, ``duplicate_id``, ``unknown_agent``,
    ``delegation_disabled`` (the root's ``tools`` must include ``delegate``),
    ``mesh_required``, ``peer_routing_unsupported``, ``file_prompt_part``,
    ``isolation_without_workspace`` or ``build_failed``.
    """

    code: str

class SubagentSpec(TypedDict, total=False):
    id: str  # required
    provider: str  # required
    model: str  # required
    system: str
    tools: list[str]
    description: str
    capabilities: list[str]
    mode: Literal["build", "plan", "review"]
    api_key: str
    base_url: str

class SubagentInfo(TypedDict):
    id: str
    name: str
    description: str
    capabilities: list[str]
    origin: Literal["config", "protocol", "runtime"]
    location: Literal["local", "remote"]

class SubagentChange(TypedDict):
    kind: Literal["added", "removed"]
    id: str
    name: str
    description: str
    capabilities: list[str]
    origin: Literal["config", "protocol", "runtime"]
    location: Literal["local", "remote"]
    generation: int

class WorktreeOptions(TypedDict, total=False):
    storage_root: str
    baseline: Literal["commit", "current_working_tree"]

class Worktrees:
    """Management operations for managed Git worktree workspaces.

    Every method returns a dict with the same shape as the ACP
    ``querymt/worktree/*`` extensions and raises :class:`WorktreeError`.
    """

    async def create(
        self,
        cwd: str | None = None,
        name: str | None = None,
        base_ref: str | None = None,
        include_local_changes: bool | None = None,
        source_session_id: str | None = None,
    ) -> dict[str, Any]: ...
    async def list(self) -> dict[str, Any]: ...
    async def status(self, workspace_id: str) -> dict[str, Any]: ...
    async def result(self, workspace_id: str) -> dict[str, Any]: ...
    async def publish(
        self,
        workspace_id: str | None = None,
        session_id: str | None = None,
    ) -> dict[str, Any]: ...
    async def handoff(
        self,
        workspace_id: str | None = None,
        session_id: str | None = None,
        name: str | None = None,
        summary: bool = False,
    ) -> dict[str, Any]: ...
    async def archive(
        self,
        workspace_id: str | None = None,
        session_id: str | None = None,
    ) -> dict[str, Any]: ...
    async def remove(
        self,
        workspace_id: str | None = None,
        session_id: str | None = None,
        archive: bool = False,
    ) -> dict[str, Any]: ...
    async def recover(self, workspace_id: str) -> dict[str, Any]: ...
    async def merge(
        self,
        workspace_id: str | None = None,
        session_id: str | None = None,
        commit: str | None = None,
        archive: bool = False,
        remove: bool = False,
    ) -> dict[str, Any]:
        """Merge a workspace into its source checkout as one squashed commit.

        Takes exactly one of ``workspace_id`` or ``session_id`` (the owner).
        By default the working state is snapshotted (the owner must be idle)
        and merged; ``commit`` merges a retained result or a commit on the
        worktree branch instead. ``archive`` / ``remove`` (mutually
        exclusive) run once the source contains the result; a failure there
        is reported under ``follow_up["error"]`` and never undoes the merge.
        Refusals raise :class:`WorktreeError` with ``.code`` and ``.paths``."""

class AgentSession:
    @property
    def id(self) -> str: ...
    @property
    def workspace(self) -> dict[str, Any] | None:
        """Binding of an isolated session (``sessionCwd``, ``workspaceId``,
        ``branch``, ``baseline``, ...), or ``None`` for a local session."""
    async def chat(self, prompt: str) -> str: ...
    async def set_provider(self, provider: str, model: str) -> None: ...

class Agent:
    @staticmethod
    async def create(
        provider: str,
        model: str,
        cwd: str | None = None,
        tools: list[str] | None = None,
        system: str | None = None,
        api_key: str | None = None,
        base_url: str | None = None,
        db: str | None = None,
        ask: list[str] | None = None,
        always_ask: list[str] | None = None,
        approval_timeout_sec: int | None = None,
        mode: str | None = None,
        max_steps: int | None = None,
        max_prompt_bytes: int | None = None,
        execution_timeout_secs: int | None = None,
        worktrees: bool | WorktreeOptions | None = None,
        subagents: list[SubagentSpec] | None = None,
    ) -> Agent:
        """``worktrees=True`` (or a dict with ``storage_root``/``baseline``)
        enables the workspace service. It never isolates a session on its own:
        isolation is requested per session via ``chat_session(workspace=...)``.
        Unknown dict keys raise ``ValueError``.

        ``ask`` lists tools that prompt (with an "Allow for this session"
        option); ``always_ask`` prompts on every call. Selectors are ``shell``,
        ``server.tool`` or ``server.*``. ``approval_timeout_sec`` rejects
        unanswered prompts. ``mode`` is ``"build"``, ``"plan"`` or ``"review"``.
        Invalid values raise ``ValueError``."""
    @staticmethod
    @deprecated("Use Agent.create(); every agent can have subagents")
    async def single(*args: Any, **kwargs: Any) -> Agent: ...
    @staticmethod
    async def from_config(path: str) -> Agent: ...
    async def chat(self, prompt: str) -> str: ...
    async def chat_session(
        self,
        workspace: Literal["local", "worktree"] | None = None,
        name: str | None = None,
        base_ref: str | None = None,
        include_local_changes: bool | None = None,
    ) -> AgentSession:
        """With no arguments this is a plain local session."""
    @property
    def worktrees(self) -> Worktrees | None:
        """``None`` when worktrees are not enabled."""
    async def list_sessions(
        self,
        mode: str | None = None,
        cursor: str | None = None,
        limit: int | None = None,
        cwd: str | None = None,
        query: str | None = None,
        session_scope: str | None = None,
        remote: str | None = None,
    ) -> dict[str, Any]: ...
    async def load_session(self, session_id: str) -> AgentSession: ...
    async def delete_session(self, session_id: str) -> None: ...
    async def set_provider(self, provider: str, model: str) -> None: ...
    async def add_subagent(
        self,
        id: str,
        provider: str,
        model: str,
        system: str | None = None,
        tools: list[str] | None = None,
        description: str | None = None,
        capabilities: list[str] | None = None,
        mode: Literal["build", "plan", "review"] | None = None,
        api_key: str | None = None,
        base_url: str | None = None,
    ) -> int:
        """Add a subagent while the agent runs. The root's ``delegate`` tool
        lists it from the next turn. Returns the new roster generation.
        Raises :class:`SubagentError`. Lasts until the agent stops."""
    async def remove_subagent(self, id: str) -> int:
        """Remove a subagent. A delegation already running on it finishes.
        Returns the new roster generation."""
    @property
    def subagents(self) -> list[SubagentInfo]:
        """The current subagents, ordered by id."""
    @property
    def roster_generation(self) -> int:
        """Number of subagent changes since the agent started."""
    def on_delegation(self, callback: Callable[[str, str], None]) -> None:
        """``callback(agent_id, objective)`` when the root delegates."""
    def on_tool_call(self, callback: Callable[[str, Any], None]) -> None:
        """``callback(tool_name, arguments)`` for every tool call."""
    def has_subagents(self) -> bool:
        """True when the agent has subagents it can delegate to (from
        configuration, the ``.agents`` directory, or added while running)."""
    def on_subagents_changed(self, callback: Callable[[SubagentChange], None]) -> None:
        """``callback(change)`` whenever a subagent is added or removed, from
        any path. Refused changes are not reported. An exception in the
        callback is printed and does not affect the change."""
    @deprecated("Use has_subagents()")
    def is_multi(self) -> bool:
        """Deprecated alias of :meth:`has_subagents`."""
