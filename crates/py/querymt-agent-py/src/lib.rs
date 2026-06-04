use anyhow::Result;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::sync::Arc;

type AgentHandle = ::querymt_agent::api::Agent;
type AgentSessionHandle = ::querymt_agent::api::AgentSession;

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
    #[staticmethod]
    #[pyo3(signature = (provider, model, cwd=None, tools=None, system=None, api_key=None, base_url=None, db=None, assume_mutating=None, mutating_tools=None, max_steps=None, max_prompt_bytes=None, execution_timeout_secs=None))]
    fn single<'py>(
        py: Python<'py>,
        provider: String,
        model: String,
        cwd: Option<String>,
        tools: Option<Vec<String>>,
        system: Option<String>,
        api_key: Option<String>,
        base_url: Option<String>,
        db: Option<String>,
        assume_mutating: Option<bool>,
        mutating_tools: Option<Vec<String>>,
        max_steps: Option<usize>,
        max_prompt_bytes: Option<usize>,
        execution_timeout_secs: Option<u64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut builder = AgentHandle::single().provider(provider, model);

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
            if let Some(assume_mutating) = assume_mutating {
                builder = builder.assume_mutating(assume_mutating);
            }
            if let Some(mutating_tools) = mutating_tools {
                builder = builder.mutating_tools(mutating_tools);
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

    fn chat_session<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let agent = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let session = agent.chat_session().await.map_err(into_py_err)?;
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

    fn is_multi(&self) -> bool {
        self.inner.is_multi()
    }
}

#[pymethods]
impl PyAgentSession {
    #[getter]
    fn id(&self) -> String {
        self.inner.id().to_owned()
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

#[pymodule]
fn querymt_agent(_py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyAgent>()?;
    module.add_class::<PyAgentSession>()?;
    module.add("__all__", vec!["Agent", "AgentSession"])?;
    Ok(())
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
