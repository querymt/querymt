#[cfg(test)]
mod tests {
    use super::super::*;
    use pyo3::Python;
    use std::sync::Once;

    fn with_python(f: impl for<'py> FnOnce(Python<'py>)) {
        static INIT: Once = Once::new();
        INIT.call_once(Python::initialize);
        Python::attach(f);
    }

    #[test]
    fn session_list_page_to_python_dict() {
        with_python(|py| {
            let page = ::querymt_agent::api::SessionListPage {
                groups: vec![::querymt_agent::api::SessionGroup {
                    cwd: Some("/tmp/project".to_string()),
                    sessions: vec![::querymt_agent::api::SessionSummary {
                        session_id: "session-1".to_string(),
                        name: Some("Session One".to_string()),
                        cwd: Some("/tmp/project".to_string()),
                        title: Some("Session One".to_string()),
                        created_at: Some("2026-01-01T00:00:00Z".to_string()),
                        updated_at: Some("2026-01-02T00:00:00Z".to_string()),
                        parent_session_id: None,
                        fork_origin: None,
                        session_kind: None,
                        has_children: false,
                        fork_count: 0,
                        node: None,
                        node_id: None,
                        attached: None,
                        runtime_state: None,
                    }],
                    latest_activity: Some("2026-01-02T00:00:00Z".to_string()),
                    total_count: Some(1),
                    next_cursor: None,
                }],
                next_cursor: Some("next-1".to_string()),
                total_count: 1,
            };

            let value = session_list_page_to_py(py, page).expect("page should convert");
            let bound = value.bind(py);
            let groups: Vec<pyo3::Py<pyo3::PyAny>> = bound
                .get_item("groups")
                .expect("groups item")
                .extract()
                .expect("groups list");
            assert_eq!(groups.len(), 1);
            let total_count: u64 = bound
                .get_item("total_count")
                .expect("total_count item")
                .extract()
                .expect("total_count value");
            assert_eq!(total_count, 1);
        });
    }
}
