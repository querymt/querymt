//! Facade integration tests, merged into one test binary (`it`) so that the
//! build compiles and links a single target instead of one per file.

mod dotagents_manifest_preview_test;
mod dotagents_runtime_e2e_test;
mod hybrid_id_scheme_test;
#[cfg(feature = "remote")]
mod lan_mesh_capabilities;
mod model_switching_preserves_system_prompt_test;
mod repository_knowledge_matrix;
mod set_planning_context_preserves_custom_params_test;
