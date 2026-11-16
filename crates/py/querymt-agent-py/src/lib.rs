use anyhow::Result;
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::sync::Arc;

use ::querymt_agent::agent::core::AgentMode;
use ::querymt_agent::api::{AgentWorktrees, SessionOptions};
use ::querymt_agent::config::{Permissions, ToolSpec, WorkspaceSettings};
use ::querymt_agent::worktree::{
    BaselineMode, HandoffOptions, WorkspaceTarget, WorktreeCreateRequest, WorktreeOpError,
};

use ::querymt_agent::api::DelegateConfigBuilder;
use ::querymt_agent::delegation::roster::RosterError;

type AgentHandle = ::querymt_agent::api::Agent;
type AgentSessionHandle = ::querymt_agent::api::AgentSession;

pyo3::create_exception!(
    querymt_agent,
    WorktreeError,
    PyRuntimeError,
    "A managed-worktree operation failed. `.code` is a stable machine-readable \
     identifier (for example `workspace_service_unavailable`, `invalid_options`, \
     `removal_refused`, `owner_active`, `workspace_not_archived`, `unpreserved_content`, \
     `unpreserved_commits`, `workspace_archived`, `workspace_not_found`, \
     `isolation_unsupported_layout`)."
);

pyo3::create_exception!(
    querymt_agent,
    SubagentError,
    PyRuntimeError,
    "Changing the agent's subagents failed. `.code` is a stable machine-readable \
     identifier (`invalid_id`, `duplicate_id`, `unknown_agent`, `delegation_disabled`, \
     `mesh_required`, `peer_routing_unsupported`, `file_prompt_part`, \
     `isolation_without_workspace`, `build_failed`)."
);

/// What a subagent is configured with, shared by `Agent.single(subagents=[...])`
/// and `Agent.add_subagent(...)`.
#[derive(Default)]
struct SubagentOpts {
    provider: String,
    model: String,
    system: Option<String>,
    tools: Option<Vec<String>>,
    description: Option<String>,
    capabilities: Option<Vec<String>>,
    mode: Option<AgentMode>,
    api_key: Option<String>,
    base_url: Option<String>,
}

impl SubagentOpts {
    fn configure(self, mut s: DelegateConfigBuilder) -> DelegateConfigBuilder {
        s = s.provider(self.provider, self.model);
        if let Some(system) = self.system {
            s = s.system(system);
        }
        if let Some(tools) = self.tools {
            s = s.tools(tools);
        }
        if let Some(description) = self.description {
            s = s.description(description);
        }
        if let Some(capabilities) = self.capabilities {
            s = s.capabilities(capabilities);
        }
        if let Some(mode) = self.mode {
            s = s.mode(mode);
        }
        if let Some(api_key) = self.api_key {
            s = s.api_key(api_key);
        }
        if let Some(base_url) = self.base_url {
            s = s.base_url(base_url);
        }
        s
    }
}

/// `{"id": ..., "provider": ..., "model": ..., ...}` -> (id, options).
/// Unknown keys raise `ValueError`.
fn parse_subagent_dict(item: &Bound<'_, PyAny>) -> PyResult<(String, SubagentOpts)> {
    let dict = item
        .cast::<PyDict>()
        .map_err(|_| PyValueError::new_err("each subagent must be a dict"))?;
    let mut id = None;
    let mut opts = SubagentOpts::default();
    let mut provider = None;
    let mut model = None;
    for (key, value) in dict.iter() {
        let key: String = key.extract()?;
        match key.as_str() {
            "id" => id = Some(value.extract::<String>()?),
            "provider" => provider = Some(value.extract::<String>()?),
            "model" => model = Some(value.extract::<String>()?),
            "system" => opts.system = value.extract()?,
            "tools" => opts.tools = value.extract()?,
            "description" => opts.description = value.extract()?,
            "capabilities" => opts.capabilities = value.extract()?,
            "api_key" => opts.api_key = value.extract()?,
            "base_url" => opts.base_url = value.extract()?,
            "mode" => {
                opts.mode = value
                    .extract::<Option<String>>()?
                    .as_deref()
                    .map(parse_mode)
                    .transpose()?
            }
            other => {
                return Err(PyValueError::new_err(format!(
                    "unknown subagent option `{other}` (expected id, provider, model, system, \
                     tools, description, capabilities, mode, api_key or base_url)"
                )));
            }
        }
    }
    let missing = |name: &str| PyValueError::new_err(format!("subagent is missing `{name}`"));
    opts.provider = provider.ok_or_else(|| missing("provider"))?;
    opts.model = model.ok_or_else(|| missing("model"))?;
    Ok((id.ok_or_else(|| missing("id"))?, opts))
}

fn subagent_code(err: &RosterError) -> &'static str {
    match err {
        RosterError::InvalidId => "invalid_id",
        RosterError::DuplicateId { .. } => "duplicate_id",
        RosterError::UnknownAgent(_) => "unknown_agent",
        RosterError::DelegationDisabled => "delegation_disabled",
        RosterError::MeshRequired => "mesh_required",
        RosterError::PeerRoutingUnsupported => "peer_routing_unsupported",
        RosterError::FilePromptPart { .. } => "file_prompt_part",
        RosterError::IsolationWithoutWorkspace { .. } => "isolation_without_workspace",
        RosterError::Build { .. } => "build_failed",
        _ => "error",
    }
}

/// Raise `SubagentError` carrying the stable `.code`; the message includes
/// the cause chain (for example which tool selector was invalid).
fn subagent_err(err: RosterError) -> PyErr {
    let mut message = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    Python::attach(|py| {
        let py_err = SubagentError::new_err(message);
        match py_err.value(py).setattr("code", subagent_code(&err)) {
            Ok(()) => py_err,
            Err(set_err) => set_err,
        }
    })
}

/// Wraps a Python callable so the runtime can call it from its event task.
/// An exception in the callback is printed, never propagated.
fn callable(callback: Py<PyAny>) -> PyResult<Py<PyAny>> {
    Python::attach(|py| {
        if callback.bind(py).is_callable() {
            Ok(callback)
        } else {
            Err(PyTypeError::new_err("callback must be callable"))
        }
    })
}

fn warn_deprecated(py: Python<'_>, message: &str) -> PyResult<()> {
    py.import("warnings")?.call_method1(
        "warn",
        (
            message,
            py.get_type::<pyo3::exceptions::PyDeprecationWarning>(),
            3,
        ),
    )?;
    Ok(())
}

fn call_python(callback: &Py<PyAny>, args: impl for<'py> pyo3::call::PyCallArgs<'py>) {
    Python::attach(|py| {
        if let Err(err) = callback.call1(py, args) {
            err.print(py);
        }
    });
}

#[pyclass(name = "Worktrees")]
struct PyWorktrees {
    inner: AgentWorktrees,
}

#[pyclass(name = "Agent")]
struct PyAgent {
    inner: Arc<AgentHandle>,
}

#[pyclass(name = "AgentSession")]
struct PyAgentSession {
    inner: Arc<AgentSessionHandle>,
}

#[pymethods]
impl PyAgent {
    /// Deprecated alias of `Agent.create(...)`: every agent can have subagents.
    #[staticmethod]
    #[pyo3(signature = (*args, **kwargs))]
    fn single<'py>(
        py: Python<'py>,
        args: &Bound<'py, pyo3::types::PyTuple>,
        kwargs: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        warn_deprecated(py, "Agent.single() is deprecated; use Agent.create()")?;
        py.get_type::<PyAgent>()
            .getattr("create")?
            .call(args, kwargs)
    }

    #[staticmethod]
    #[pyo3(signature = (provider, model, cwd=None, tools=None, system=None, api_key=None, base_url=None, db=None, ask=None, always_ask=None, approval_timeout_sec=None, mode=None, max_steps=None, max_prompt_bytes=None, execution_timeout_secs=None, worktrees=None, subagents=None))]
    fn create<'py>(
        py: Python<'py>,
        provider: String,
        model: String,
        cwd: Option<String>,
        tools: Option<Vec<String>>,
        system: Option<String>,
        api_key: Option<String>,
        base_url: Option<String>,
        db: Option<String>,
        ask: Option<Vec<String>>,
        always_ask: Option<Vec<String>>,
        approval_timeout_sec: Option<i64>,
        mode: Option<String>,
        max_steps: Option<usize>,
        max_prompt_bytes: Option<usize>,
        execution_timeout_secs: Option<u64>,
        worktrees: Option<Bound<'py, PyAny>>,
        subagents: Option<Vec<Bound<'py, PyAny>>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        // Parsed eagerly so a bad argument raises ValueError at the call site.
        let subagents = subagents
            .unwrap_or_default()
            .iter()
            .map(parse_subagent_dict)
            .collect::<PyResult<Vec<_>>>()?;
        let workspace = parse_worktrees(worktrees.as_ref())?;
        let permissions = parse_permissions(ask, always_ask, approval_timeout_sec)?;
        let mode = mode.as_deref().map(parse_mode).transpose()?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut builder = AgentHandle::builder().provider(provider, model);
            for (id, opts) in subagents {
                builder = builder.subagent(id, |s| opts.configure(s));
            }
            if let Some(settings) = workspace {
                builder = builder.workspace(settings);
            }

            if let Some(cwd) = cwd {
                builder = builder.cwd(cwd);
            }
            if let Some(tools) = tools {
                builder = builder.tools(tools);
            }
            if let Some(system) = system {
                builder = builder.system(system);
            }
            if let Some(api_key) = api_key {
                builder = builder.api_key(api_key);
            }
            if let Some(base_url) = base_url {
                builder = builder.parameter("base_url", base_url);
            }
            if let Some(db) = db {
                builder = builder.db(db);
            }
            if let Some(permissions) = permissions {
                builder = builder.permissions(permissions);
            }
            if let Some(mode) = mode {
                builder = builder.mode(mode);
            }
            if let Some(max_steps) = max_steps {
                builder = builder.max_steps(max_steps);
            }
            if let Some(max_prompt_bytes) = max_prompt_bytes {
                builder = builder.max_prompt_bytes(max_prompt_bytes);
            }
            if let Some(execution_timeout_secs) = execution_timeout_secs {
                builder = builder.execution_timeout_secs(execution_timeout_secs);
            }

            let agent = builder.build().await.map_err(into_py_err)?;
            Python::attach(|py| {
                Py::new(
                    py,
                    PyAgent {
                        inner: Arc::new(agent),
                    },
                )
            })
        })
    }

    #[staticmethod]
    fn from_config<'py>(py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let runner = ::querymt_agent::runner::from_config(path)
                .await
                .map_err(into_py_err)?;
            Python::attach(|py| {
                Py::new(
                    py,
                    PyAgent {
                        inner: Arc::new(runner.into_agent()),
                    },
                )
            })
        })
    }

    fn chat<'py>(&self, py: Python<'py>, prompt: String) -> PyResult<Bound<'py, PyAny>> {
        let agent = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response = agent.chat(&prompt).await.map_err(into_py_err)?;
            Python::attach(|py| Ok(response.into_pyobject(py)?.into_any().unbind()))
        })
    }

    /// Create a session. With no arguments this is a plain local session.
    ///
    /// `workspace="worktree"` requests an isolated checkout; `name`,
    /// `base_ref` and `include_local_changes` refine the request. Requested
    /// isolation fails closed with `WorktreeError` (never a shared fallback).
    #[pyo3(signature = (workspace=None, name=None, base_ref=None, include_local_changes=None))]
    fn chat_session<'py>(
        &self,
        py: Python<'py>,
        workspace: Option<String>,
        name: Option<String>,
        base_ref: Option<String>,
        include_local_changes: Option<bool>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let options = session_options(workspace, name, base_ref, include_local_changes)?;
        let agent = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let session = match options {
                Some(options) => agent
                    .chat_session_with(options)
                    .await
                    .map_err(worktree_err)?,
                None => agent.chat_session().await.map_err(into_py_err)?,
            };
            Python::attach(|py| {
                Py::new(
                    py,
                    PyAgentSession {
                        inner: Arc::new(session),
                    },
                )
            })
        })
    }

    /// Managed-worktree operations, or `None` when worktrees are not enabled.
    #[getter]
    fn worktrees(&self) -> Option<PyWorktrees> {
        self.inner.worktrees().map(|inner| PyWorktrees { inner })
    }

    fn list_sessions<'py>(
        &self,
        py: Python<'py>,
        mode: Option<String>,
        cursor: Option<String>,
        limit: Option<u32>,
        cwd: Option<String>,
        query: Option<String>,
        session_scope: Option<String>,
        remote: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let agent = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let options = ::querymt_agent::api::ListSessionsOptions {
                mode: parse_session_list_mode(mode)?,
                cursor,
                limit,
                cwd,
                query,
                session_scope: parse_session_scope(session_scope)?,
                remote: parse_remote_session_mode(remote)?,
            };
            let page = agent.list_sessions(options).await.map_err(into_py_err)?;
            Python::attach(|py| session_list_page_to_py(py, page))
        })
    }

    fn load_session<'py>(
        &self,
        py: Python<'py>,
        session_id: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let agent = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let session = agent.load_session(&session_id).await.map_err(into_py_err)?;
            Python::attach(|py| {
                Py::new(
                    py,
                    PyAgentSession {
                        inner: Arc::new(session),
                    },
                )
            })
        })
    }

    fn delete_session<'py>(
        &self,
        py: Python<'py>,
        session_id: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let agent = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            agent
                .delete_session(&session_id)
                .await
                .map_err(into_py_err)?;
            Python::attach(|py| Ok(py.None()))
        })
    }

    fn set_provider<'py>(
        &self,
        py: Python<'py>,
        provider: String,
        model: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let agent = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            agent
                .set_provider(&provider, &model)
                .await
                .map_err(into_py_err)?;
            Python::attach(|py| Ok(py.None()))
        })
    }

    /// True when the agent has subagents it can delegate to, whether they
    /// came from configuration, the `.agents` directory or were added while
    /// running.
    fn has_subagents(&self) -> bool {
        self.inner.has_subagents()
    }

    /// Deprecated alias of `has_subagents()`.
    fn is_multi(&self, py: Python<'_>) -> PyResult<bool> {
        warn_deprecated(py, "Agent.is_multi() is deprecated; use has_subagents()")?;
        Ok(self.inner.has_subagents())
    }

    /// Add a subagent while the agent runs; the root's `delegate` tool lists
    /// it from the next turn. Returns the new roster generation. Raises
    /// `SubagentError` (see `.code`); the roster is unchanged on failure.
    #[pyo3(signature = (id, provider, model, system=None, tools=None, description=None, capabilities=None, mode=None, api_key=None, base_url=None))]
    #[allow(clippy::too_many_arguments)]
    fn add_subagent<'py>(
        &self,
        py: Python<'py>,
        id: String,
        provider: String,
        model: String,
        system: Option<String>,
        tools: Option<Vec<String>>,
        description: Option<String>,
        capabilities: Option<Vec<String>>,
        mode: Option<String>,
        api_key: Option<String>,
        base_url: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let opts = SubagentOpts {
            provider,
            model,
            system,
            tools,
            description,
            capabilities,
            mode: mode.as_deref().map(parse_mode).transpose()?,
            api_key,
            base_url,
        };
        let agent = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let generation = agent
                .add_subagent(id, |s| opts.configure(s))
                .await
                .map_err(subagent_err)?;
            Python::attach(|py| Ok(generation.get().into_pyobject(py)?.into_any().unbind()))
        })
    }

    /// Remove a subagent by ID, whatever its origin. A delegation already
    /// running on it finishes. Returns the new roster generation.
    fn remove_subagent<'py>(&self, py: Python<'py>, id: String) -> PyResult<Bound<'py, PyAny>> {
        let agent = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let generation = agent.remove_subagent(&id).await.map_err(subagent_err)?;
            Python::attach(|py| Ok(generation.get().into_pyobject(py)?.into_any().unbind()))
        })
    }

    /// The current subagents, ordered by id: dicts with `id`, `name`,
    /// `description`, `capabilities`, `origin` (`config`/`protocol`/`runtime`)
    /// and `location` (`local`/`remote`).
    #[getter]
    fn subagents(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        serialize_to_py(py, &self.inner.subagents())
    }

    /// How many times the set of subagents changed since the agent started.
    #[getter]
    fn roster_generation(&self) -> u64 {
        self.inner.roster_generation()
    }

    /// Call `callback(change)` whenever a subagent is added or removed, from
    /// any path. `change` is a dict: `kind` (`added`/`removed`), `id`, `name`,
    /// `description`, `capabilities`, `origin`, `location` and `generation`.
    /// Refused changes are not reported. An exception in the callback is
    /// printed and does not affect the change.
    fn on_subagents_changed(&self, callback: Py<PyAny>) -> PyResult<()> {
        let callback = callable(callback)?;
        let _runtime = pyo3_async_runtimes::tokio::get_runtime().enter();
        self.inner.on_subagents_changed(move |change| {
            Python::attach(|py| match serialize_to_py(py, change) {
                Ok(change) => call_python(&callback, (change,)),
                Err(err) => err.print(py),
            });
        });
        Ok(())
    }

    /// Call `callback(agent_id, objective)` whenever the root delegates.
    fn on_delegation(&self, callback: Py<PyAny>) -> PyResult<()> {
        let callback = callable(callback)?;
        // Registering starts the event listener task, which needs a runtime
        // even though this method is called synchronously from Python.
        let _runtime = pyo3_async_runtimes::tokio::get_runtime().enter();
        self.inner.on_delegation(move |agent_id, objective| {
            call_python(&callback, (agent_id, objective));
        });
        Ok(())
    }

    /// Call `callback(tool_name, arguments)` for every tool call; `arguments`
    /// is the decoded JSON value.
    fn on_tool_call(&self, callback: Py<PyAny>) -> PyResult<()> {
        let callback = callable(callback)?;
        let _runtime = pyo3_async_runtimes::tokio::get_runtime().enter();
        self.inner.on_tool_call(move |name, args| {
            Python::attach(|py| match json_to_py(py, &args) {
                Ok(args) => call_python(&callback, (name, args)),
                Err(err) => err.print(py),
            });
        });
        Ok(())
    }
}

#[pymethods]
impl PyAgentSession {
    #[getter]
    fn id(&self) -> String {
        self.inner.id().to_owned()
    }

    /// The managed-workspace binding (execution cwd, workspace id, branch,
    /// baseline) as a dict, or `None` for a local session.
    #[getter]
    fn workspace(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        self.inner
            .workspace()
            .map(|value| json_to_py(py, value))
            .transpose()
    }

    fn chat<'py>(&self, py: Python<'py>, prompt: String) -> PyResult<Bound<'py, PyAny>> {
        let session = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response = session.chat(&prompt).await.map_err(into_py_err)?;
            Python::attach(|py| Ok(response.into_pyobject(py)?.into_any().unbind()))
        })
    }

    fn set_provider<'py>(
        &self,
        py: Python<'py>,
        provider: String,
        model: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let session = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            session
                .set_provider(&provider, &model)
                .await
                .map_err(into_py_err)?;
            Python::attach(|py| Ok(py.None()))
        })
    }
}

/// Await a typed worktree operation and render its response as a dict.
macro_rules! worktree_future {
    ($py:expr, $ops:expr, |$w:ident| $call:expr) => {{
        let $w = $ops.clone();
        pyo3_async_runtimes::tokio::future_into_py($py, async move {
            let response = $call.await.map_err(worktree_err)?;
            Python::attach(|py| serialize_to_py(py, &response))
        })
    }};
}

#[pymethods]
impl PyWorktrees {
    /// Create a new isolated session with its own checkout. `cwd` defaults to
    /// the agent's working directory. Never moves an existing session.
    ///
    /// `source_session_id` records lineage (the new session nests under it, no
    /// history is copied); an empty `cwd` then defaults to that session's cwd.
    #[pyo3(signature = (cwd=None, name=None, base_ref=None, include_local_changes=None, source_session_id=None))]
    fn create<'py>(
        &self,
        py: Python<'py>,
        cwd: Option<String>,
        name: Option<String>,
        base_ref: Option<String>,
        include_local_changes: Option<bool>,
        source_session_id: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let request = WorktreeCreateRequest {
            cwd: cwd.unwrap_or_default(),
            include_local_changes,
            base_ref,
            name,
            source_session_id,
        };
        worktree_future!(py, self.inner, |w| w.create(request))
    }

    /// Hand ownership of a ready workspace to a new session in the same
    /// checkout. Provide exactly one of `workspace_id` or `session_id` (the
    /// owner). `summary=True` seeds the new session with a summary of the
    /// previous conversation. The owner must be local and idle.
    #[pyo3(signature = (workspace_id=None, session_id=None, name=None, summary=false))]
    fn handoff<'py>(
        &self,
        py: Python<'py>,
        workspace_id: Option<String>,
        session_id: Option<String>,
        name: Option<String>,
        summary: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let target = WorkspaceTarget::from_parts(workspace_id, session_id).map_err(worktree_err)?;
        let options = HandoffOptions { name, summary };
        worktree_future!(py, self.inner, |w| w.handoff(target, options))
    }

    fn list<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        worktree_future!(py, self.inner, |w| w.list())
    }

    fn status<'py>(&self, py: Python<'py>, workspace_id: String) -> PyResult<Bound<'py, PyAny>> {
        worktree_future!(py, self.inner, |w| w.status(&workspace_id))
    }

    fn result<'py>(&self, py: Python<'py>, workspace_id: String) -> PyResult<Bound<'py, PyAny>> {
        worktree_future!(py, self.inner, |w| w.result(&workspace_id))
    }

    /// Publish a workspace's current working state as an immutable result.
    /// Provide exactly one of `workspace_id` or `session_id`.
    #[pyo3(signature = (workspace_id=None, session_id=None))]
    fn publish<'py>(
        &self,
        py: Python<'py>,
        workspace_id: Option<String>,
        session_id: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let target = WorkspaceTarget::from_parts(workspace_id, session_id).map_err(worktree_err)?;
        worktree_future!(py, self.inner, |w| w.publish(target))
    }

    /// Retire a workspace's execution, from any session. Provide exactly one
    /// of `workspace_id` or `session_id` (the owner). An idle owner is
    /// unloaded; a busy one raises `WorktreeError` with code `owner_active`.
    #[pyo3(signature = (workspace_id=None, session_id=None))]
    fn archive<'py>(
        &self,
        py: Python<'py>,
        workspace_id: Option<String>,
        session_id: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let target = WorkspaceTarget::from_parts(workspace_id, session_id).map_err(worktree_err)?;
        worktree_future!(py, self.inner, |w| w.archive(target))
    }

    /// Conservative removal; there is no force option. Provide exactly one of
    /// `workspace_id` or `session_id`. `archive=True` archives an owned
    /// workspace first (it stays archived if removal is then refused).
    #[pyo3(signature = (workspace_id=None, session_id=None, archive=false))]
    fn remove<'py>(
        &self,
        py: Python<'py>,
        workspace_id: Option<String>,
        session_id: Option<String>,
        archive: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let target = WorkspaceTarget::from_parts(workspace_id, session_id).map_err(worktree_err)?;
        worktree_future!(py, self.inner, |w| w.remove(target, archive))
    }

    fn recover<'py>(&self, py: Python<'py>, workspace_id: String) -> PyResult<Bound<'py, PyAny>> {
        worktree_future!(py, self.inner, |w| w.recover(&workspace_id))
    }
}

#[pymodule]
fn querymt_agent(py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyAgent>()?;
    module.add_class::<PyAgentSession>()?;
    module.add_class::<PyWorktrees>()?;
    module.add("WorktreeError", py.get_type::<WorktreeError>())?;
    module.add("SubagentError", py.get_type::<SubagentError>())?;
    module.add(
        "__all__",
        vec![
            "Agent",
            "AgentSession",
            "Worktrees",
            "WorktreeError",
            "SubagentError",
        ],
    )?;
    Ok(())
}

/// Approval lists and timeout; a bad selector or a non-positive timeout
/// raises `ValueError` at the call site.
fn parse_permissions(
    ask: Option<Vec<String>>,
    always_ask: Option<Vec<String>>,
    timeout_sec: Option<i64>,
) -> PyResult<Option<Permissions>> {
    if ask.is_none() && always_ask.is_none() && timeout_sec.is_none() {
        return Ok(None);
    }
    let specs = |list: Option<Vec<String>>| -> PyResult<Vec<ToolSpec>> {
        list.unwrap_or_default()
            .iter()
            .map(|s| s.parse::<ToolSpec>().map_err(into_value_err))
            .collect()
    };
    let mut permissions = Permissions::new()
        .ask(specs(ask)?)
        .always_ask(specs(always_ask)?);
    if let Some(secs) = timeout_sec {
        let secs = u64::try_from(secs)
            .ok()
            .filter(|secs| *secs > 0)
            .ok_or_else(|| PyValueError::new_err("approval_timeout_sec must be positive"))?;
        permissions = permissions.timeout(std::time::Duration::from_secs(secs));
    }
    Ok(Some(permissions))
}

fn parse_mode(mode: &str) -> PyResult<AgentMode> {
    match mode {
        "build" => Ok(AgentMode::Build),
        "plan" => Ok(AgentMode::Plan),
        "review" => Ok(AgentMode::Review),
        other => Err(PyValueError::new_err(format!(
            "unknown mode `{other}` (expected `build`, `plan` or `review`)"
        ))),
    }
}

fn into_value_err(err: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(err.to_string())
}

fn parse_worktrees(arg: Option<&Bound<'_, PyAny>>) -> PyResult<Option<WorkspaceSettings>> {
    let Some(arg) = arg.filter(|a| !a.is_none()) else {
        return Ok(None);
    };
    if let Ok(flag) = arg.extract::<bool>() {
        return Ok(flag.then(WorkspaceSettings::enabled));
    }
    let dict = arg.cast::<PyDict>().map_err(|_| {
        PyValueError::new_err("worktrees must be a bool or a dict with `storage_root`/`baseline`")
    })?;
    let mut settings = WorkspaceSettings::enabled();
    for (key, value) in dict.iter() {
        let key: String = key.extract()?;
        match key.as_str() {
            "storage_root" => settings.storage_root = Some(value.extract::<String>()?.into()),
            "baseline" => {
                settings.baseline = value
                    .extract::<String>()?
                    .parse::<BaselineMode>()
                    .map_err(PyValueError::new_err)?;
            }
            other => {
                return Err(PyValueError::new_err(format!(
                    "unknown worktrees option `{other}` (expected `storage_root` or `baseline`)"
                )));
            }
        }
    }
    Ok(Some(settings))
}

/// `None` means a plain local session (the legacy path, unchanged).
fn session_options(
    workspace: Option<String>,
    name: Option<String>,
    base_ref: Option<String>,
    include_local_changes: Option<bool>,
) -> PyResult<Option<SessionOptions>> {
    let mut options = match workspace.as_deref() {
        None | Some("local") => SessionOptions::local(),
        Some("worktree") => SessionOptions::worktree(),
        Some(other) => {
            return Err(PyValueError::new_err(format!(
                "invalid workspace `{other}` (expected \"local\" or \"worktree\")"
            )));
        }
    };
    let refined = name.is_some() || base_ref.is_some() || include_local_changes.is_some();
    if let Some(name) = name {
        options = options.name(name);
    }
    if let Some(base_ref) = base_ref {
        options = options.base_ref(base_ref);
    }
    if let Some(include) = include_local_changes {
        options = options.include_local_changes(include);
    }
    Ok((workspace.is_some() || refined).then_some(options))
}

/// Raise `WorktreeError` carrying the stable `.code`.
fn worktree_err(err: WorktreeOpError) -> PyErr {
    Python::attach(|py| {
        let py_err = WorktreeError::new_err(err.to_string());
        match py_err.value(py).setattr("code", err.code()) {
            Ok(()) => py_err,
            Err(set_err) => set_err,
        }
    })
}

fn json_to_py(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
    Ok(py
        .import("json")?
        .call_method1("loads", (value.to_string(),))?
        .unbind())
}

fn serialize_to_py<T: serde::Serialize>(py: Python<'_>, value: &T) -> PyResult<Py<PyAny>> {
    json_to_py(py, &serde_json::to_value(value).map_err(into_py_err)?)
}

#[cfg(test)]
mod tests;

fn parse_session_list_mode(
    value: Option<String>,
) -> PyResult<::querymt_agent::api::SessionListMode> {
    match value.as_deref().unwrap_or("browse") {
        "browse" => Ok(::querymt_agent::api::SessionListMode::Browse),
        "group" => Ok(::querymt_agent::api::SessionListMode::Group),
        "search" => Ok(::querymt_agent::api::SessionListMode::Search),
        other => Err(PyRuntimeError::new_err(format!(
            "invalid session list mode: {other}"
        ))),
    }
}

fn parse_remote_session_mode(
    value: Option<String>,
) -> PyResult<::querymt_agent::api::RemoteSessionMode> {
    match value.as_deref().unwrap_or("none") {
        "none" => Ok(::querymt_agent::api::RemoteSessionMode::None),
        "bookmarks" => Ok(::querymt_agent::api::RemoteSessionMode::Bookmarks),
        "live" => Ok(::querymt_agent::api::RemoteSessionMode::Live),
        other => Err(PyRuntimeError::new_err(format!(
            "invalid remote session mode: {other}"
        ))),
    }
}

fn parse_session_scope(
    value: Option<String>,
) -> PyResult<Option<::querymt_agent::session::projection::SessionScope>> {
    match value.as_deref() {
        None => Ok(None),
        Some("all") => Ok(Some(
            ::querymt_agent::session::projection::SessionScope::All,
        )),
        Some("root") => Ok(Some(
            ::querymt_agent::session::projection::SessionScope::Root,
        )),
        Some("forks") => Ok(Some(
            ::querymt_agent::session::projection::SessionScope::Forks,
        )),
        Some("delegates") => Ok(Some(
            ::querymt_agent::session::projection::SessionScope::Delegates,
        )),
        Some("children") => Ok(Some(
            ::querymt_agent::session::projection::SessionScope::Children,
        )),
        Some(other) => Err(PyRuntimeError::new_err(format!(
            "invalid session scope: {other}"
        ))),
    }
}

fn session_list_page_to_py(
    py: Python<'_>,
    page: ::querymt_agent::api::SessionListPage,
) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    let groups = PyList::empty(py);
    for group in page.groups {
        groups.append(session_group_to_py(py, group)?)?;
    }
    dict.set_item("groups", groups)?;
    dict.set_item("next_cursor", page.next_cursor)?;
    dict.set_item("total_count", page.total_count)?;
    Ok(dict.into_any().unbind())
}

fn session_group_to_py(
    py: Python<'_>,
    group: ::querymt_agent::api::SessionGroup,
) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    let sessions = PyList::empty(py);
    for session in group.sessions {
        sessions.append(session_summary_to_py(py, session)?)?;
    }
    dict.set_item("cwd", group.cwd)?;
    dict.set_item("sessions", sessions)?;
    dict.set_item("latest_activity", group.latest_activity)?;
    dict.set_item("total_count", group.total_count)?;
    dict.set_item("next_cursor", group.next_cursor)?;
    Ok(dict.into_any().unbind())
}

fn session_summary_to_py(
    py: Python<'_>,
    session: ::querymt_agent::api::SessionSummary,
) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    dict.set_item("session_id", session.session_id)?;
    dict.set_item("name", session.name)?;
    dict.set_item("cwd", session.cwd)?;
    dict.set_item("title", session.title)?;
    dict.set_item("created_at", session.created_at)?;
    dict.set_item("updated_at", session.updated_at)?;
    dict.set_item("parent_session_id", session.parent_session_id)?;
    dict.set_item("fork_origin", session.fork_origin)?;
    dict.set_item("session_kind", session.session_kind)?;
    dict.set_item("has_children", session.has_children)?;
    dict.set_item("fork_count", session.fork_count)?;
    dict.set_item("node", session.node)?;
    dict.set_item("node_id", session.node_id)?;
    dict.set_item("attached", session.attached)?;
    dict.set_item("runtime_state", session.runtime_state)?;
    Ok(dict.into_any().unbind())
}

fn into_py_err(err: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

#[allow(dead_code)]
fn _assert_result_send_sync(_: Result<()>) {}
