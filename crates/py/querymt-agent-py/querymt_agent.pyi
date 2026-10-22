"""Type stubs for the QueryMT Agent Python bindings."""

from typing import Any, Literal, Mapping, TypedDict

__all__: list[str]

class WorktreeError(RuntimeError):
    """A managed-worktree operation failed.

    ``code`` is a stable machine-readable identifier, for example
    ``workspace_service_unavailable``, ``invalid_options``, ``removal_refused``,
    ``workspace_not_found``, ``owner_conflict``, ``conflicting_cwd`` or
    ``isolation_unsupported_layout``.
    """

    code: str

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
    ) -> dict[str, Any]: ...
    async def list(self) -> dict[str, Any]: ...
    async def status(self, workspace_id: str) -> dict[str, Any]: ...
    async def result(self, workspace_id: str) -> dict[str, Any]: ...
    async def publish(
        self,
        workspace_id: str | None = None,
        session_id: str | None = None,
    ) -> dict[str, Any]: ...
    async def archive(self, workspace_id: str) -> dict[str, Any]: ...
    async def remove(self, workspace_id: str) -> dict[str, Any]: ...
    async def recover(self, workspace_id: str) -> dict[str, Any]: ...

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
    async def single(
        provider: str,
        model: str,
        cwd: str | None = None,
        tools: list[str] | None = None,
        system: str | None = None,
        api_key: str | None = None,
        base_url: str | None = None,
        db: str | None = None,
        assume_mutating: bool | None = None,
        mutating_tools: list[str] | None = None,
        max_steps: int | None = None,
        max_prompt_bytes: int | None = None,
        execution_timeout_secs: int | None = None,
        worktrees: bool | WorktreeOptions | None = None,
    ) -> Agent:
        """``worktrees=True`` (or a dict with ``storage_root``/``baseline``)
        enables the workspace service. It never isolates a session on its own:
        isolation is requested per session via ``chat_session(workspace=...)``.
        Unknown dict keys raise ``ValueError``."""
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
    def is_multi(self) -> bool: ...
