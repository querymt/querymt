# Scenario audit: add-portable-repository-knowledge

Read-only audit, 2026-11-12. Each delta-spec scenario is mapped to tests that assert its THEN outcome.
`DELIV::` = agent::execution_tests::repository_delivery, `TOOL::` = tools::builtins::repo_knowledge::tests,
`matrix::` = tests/repository_knowledge_matrix.rs, `example::` = tests inside examples/repository_knowledge.rs, other prefixes = knowledge::repository::<module>::tests.

Totals after fixes (2026-11-12): covered 95, partial 7, uncovered 3 (of 105).

## deterministic-code-maps (12: 9 covered, 2 partial, 1 uncovered)

| # | Scenario | Status | Tests | Gap |
|---|---|---|---|---|
| 1 | Rust module is indexed | covered | code::rust_declarations_carry_kind_signature_parent_and_ranges; code::imports_are_syntax_not_resolved_dependencies |  |
| 2 | Existing Rust documentation is available | covered | code::rust_documentation_is_verbatim_with_coordinates; code::documentation_is_never_invented_for_other_languages |  |
| 3 | Fresh clone generates the same map | covered | export::exports_are_byte_identical_across_checkout_paths; map::identical_sources_render_identical_maps_in_different_checkouts |  |
| 4 | Offline generation | partial | (none) | Offline by construction (no network code in crates/agent-knowledge); no test denies network or credentials. |
| 5 | One file changes | covered | reconcile::only_the_changed_file_is_re_extracted |  |
| 6 | Extractor version changes | covered | reconcile::a_new_extractor_format_re_extracts_code_but_not_documents |  |
| 7 | Unsupported selected source | covered | code::unsupported_languages_keep_a_path_level_record_with_a_diagnostic |  |
| 8 | Large directory map | covered | map::over_budget_maps_are_bounded_and_say_how_to_narrow |  |
| 9 | Export into an authored document | covered | export::existing_documents_without_a_block_are_refused_and_untouched |  |
| 10 | Curated concept links to source code | covered | TOOL::symbols_maps_and_purpose_rows_are_retrievable_and_labelled; evidence::indexing_and_fetching_never_acknowledge_anything |  |
| 11 | Existing outline fixture | partial | DELIV::successful_index_gets_attributed_context_and_an_unchanged_result | On/off byte equality for one Rust fixture only; the language outline suite never runs with the feature on. |
| 12 | Source changed after map retrieval | uncovered | (none) | No test fetches a map, edits the file, then attempts an edit. True by construction: edit tools never consult the knowledge index. |

## portable-repository-knowledge (28: 28 covered, 0 partial, 0 uncovered)

| # | Scenario | Status | Tests | Gap |
|---|---|---|---|---|
| 13 | Feature is disabled | covered | api::disabled_or_omitted_settings_bind_nothing; registry::disabled_settings_produce_no_registry; agent::repository_delivery::no_policy_when_disabled_or_retrieval_tools_are_excluded |  |
| 14 | Enabled feature has no manifest | covered | manifest::missing_manifest_is_not_configured_and_creates_nothing; query::a_missing_manifest_answers_with_status_not_a_crawl |  |
| 15 | Unsupported manifest version or table | covered | manifest::rejects_unsupported_and_missing_versions; manifest::rejects_unknown_top_level_tables_and_keys; reconcile::missing_or_invalid_manifest_empties_the_corpus_with_a_diagnostic |  |
| 16 | Manifest placed under .agents | covered | manifest::agents_directory_manifest_is_never_read |  |
| 17 | Protocol memories are selected as documents | covered | tools::builtins::repo_knowledge::tests::the_protocol_loader_and_repository_indexing_read_one_memory_independently; matrix::nothing_under_agents_or_the_sources_is_modified_by_read_operations | Protocol loader enabled on the same checkout; both read the memory; `.agents/` byte-identical. |
| 18 | Another developer clones the repository | covered | matrix::clone_and_cache_loss_rebuild_to_equal_logical_results; identity::identities_and_keys_are_identical_across_checkouts |  |
| 19 | Identified document moves | covered | browse::explicit_ids_survive_a_move; identity::explicit_ids_survive_moves_and_path_ids_do_not |  |
| 20 | Plain Markdown and legacy memory coexist | covered | identity::mixed_markdown_unknown_keys_and_legacy_memories_share_one_projection; matrix::mixed_inputs_are_one_projection_with_isolated_failures; document::legacy_memory_fields_match_the_protocol_parser |  |
| 21 | Repository supplies an orientation entrypoint | covered | DELIV::orientation_cards_are_native_and_do_not_call_tools (disabled-stage branch); check::orientation_entrypoints_must_resolve |  |
| 22 | Directory association does not acknowledge evidence | covered | path_knowledge::declared_sources_and_applicability_globs_attach_notes_with_their_own_provenance; path_knowledge::files_outside_the_selected_corpus_or_unsafe_paths_get_nothing; evidence::indexing_and_fetching_never_acknowledge_anything |  |
| 23 | Selected hidden memory is indexed | covered | discovery::selected_hidden_agents_memories_are_discovered |  |
| 24 | Symlink escapes the root | covered | discovery::symlinks::external_file_symlink_is_rejected_with_a_relative_diagnostic |  |
| 25 | Conflicting IDs or size limits | covered | identity::duplicate_explicit_ids_disable_every_conflicting_document; discovery::per_file_limit_excludes_only_that_file_and_reports_incomplete |  |
| 26 | Malformed frontmatter | covered | document::malformed_frontmatter_excludes_only_that_document |  |
| 27 | Reference escapes or names an external URL | covered | references::traversal_above_the_root_is_rejected_in_every_spelling; references::urls_are_inert_external_references |  |
| 28 | Session starts inside a crate | covered | root::repository_root_and_crate_cwd_resolve_identically; registry::sessions_in_one_checkout_share_a_service |  |
| 29 | Worktrees differ | covered | matrix::linked_worktrees_keep_independent_corpora |  |
| 30 | Config directory is overridden | covered | registry::the_default_index_location_follows_the_host_config_dir; root::index_lives_under_the_supplied_config_directory | Production path (from_settings, QMT_CONFIG_DIR) now tested. |
| 31 | External edit without a watcher event | covered | reconcile::external_edits_additions_and_deletions_reconcile_without_events; service::writes_without_any_hint_are_still_reconciled_on_retrieval |  |
| 32 | Source is removed or becomes invalid | covered | service::deleted_or_invalid_sources_are_not_emitted; reconcile::malformed_and_disabled_sources_are_isolated_and_recover |  |
| 33 | Refresh overlaps another reader | covered | matrix::concurrent_refresh_and_readers_see_only_complete_generations; service::refreshing_processes_and_a_reading_process_never_see_partial_state |  |
| 34 | Cache is lost | covered | service::a_corrupted_cache_is_rebuilt_from_the_sources; store::damaged_database_pages_are_detected_and_rebuilt |  |
| 35 | Warm query with no changes | covered | reconcile::warm_reconcile_opens_no_content_and_commits_nothing |  |
| 36 | Same-size edit within the racy window | covered | reconcile::same_size_edit_in_the_racy_window_is_detected |  |
| 37 | Metadata changes but content does not | covered | reconcile::mtime_only_change_reuses_the_record_without_a_new_generation |  |
| 38 | Forced hashing is enabled | covered | matrix::forced_hashing_reads_every_selected_file |  |
| 39 | Workspace memory is also imported by the protocol loader | covered | tools::builtins::repo_knowledge::tests::the_protocol_loader_and_repository_indexing_read_one_memory_independently | Loader result identical before and after repository indexing. |
| 40 | Private entry mentions a repository symbol | covered | TOOL::private_session_entries_and_consolidations_never_cross_the_boundary; matrix::private_knowledge_is_not_reachable_and_the_cache_lives_outside_the_repo |  |

## repository-knowledge-maintenance (27: 21 covered, 4 partial, 2 uncovered)

| # | Scenario | Status | Tests | Gap |
|---|---|---|---|---|
| 41 | Source changes but note does not | covered | evidence::evidence_walks_through_every_state_independently_of_lifecycle; check::evidence_drift_is_reported_only_for_acknowledged_records |  |
| 42 | Note changes without acknowledgement | covered | evidence::evidence_walks_through_every_state_independently_of_lifecycle; evidence::editing_the_declared_dependencies_makes_the_note_unverified |  |
| 43 | Evidence is missing or ambiguous | covered | evidence::a_vanished_or_ambiguous_symbol_is_never_replaced_by_a_lookalike |  |
| 44 | Independent branches acknowledge different notes | covered | acknowledge::different_documents_acknowledge_into_different_files |  |
| 45 | Document is moved | covered | check::moved_documents_leave_a_reported_orphan_evidence_file; evidence::a_moved_document_is_unverified_and_its_old_file_is_listed_as_an_orphan |  |
| 46 | Evidence file content is portable | covered | acknowledge::previewing_writes_nothing_and_applying_makes_the_evidence_unchanged (two checkouts write identical bytes, only document/entries/version keys); evidence::evidence_files_serialize_deterministically_with_sorted_keys (no absolute path in any key or value) |  |
| 47 | Clone with different line endings | covered | acknowledge::crlf_checkouts_keep_acknowledgements_valid |  |
| 48 | Substantive edit | covered | evidence::a_substantive_edit_changes_the_hash |  |
| 49 | Fresh clone checks acknowledged evidence | covered | evidence::evidence_survives_a_fresh_checkout_without_the_original_database |  |
| 50 | Source changes after acknowledgement preview | covered | acknowledge::a_dependency_edit_after_preview_invalidates_the_plan; acknowledge::a_note_edit_after_preview_invalidates_the_plan |  |
| 51 | File is moved | covered | check::orphan_file_table_rows_are_reported_not_pruned; check::broken_references_are_reported_with_relative_paths_and_left_in_place |  |
| 52 | Document includes unknown metadata and an external URL | covered | check::a_curated_corpus_without_listings_rows_or_evidence_passes |  |
| 53 | Curated corpus has no exhaustive inventory | covered | check::a_curated_corpus_without_listings_rows_or_evidence_passes; check::missing_rows_are_findings_only_in_a_selected_exhaustive_scope |  |
| 54 | Check fails in CI | covered | example::a_failing_check_exits_one_with_stable_codes_and_relative_paths; check::json_is_stable_relative_and_machine_independent | Example tests now run in CI (test-agent.yml, `--example repository_knowledge`). |
| 55 | Scaffold selected source directories | covered | scaffold::a_new_directory_document_lists_sorted_files_with_empty_purposes; scaffold::rerunning_proposes_nothing_and_never_duplicates_rows |  |
| 56 | Concurrent edit before apply | covered | scaffold::a_target_edit_between_preview_and_apply_is_rejected |  |
| 57 | Existing directory instructions lack a managed block | covered | scaffold::unmarked_and_multi_block_files_are_refused_and_left_untouched |  |
| 58 | Headless maintenance | partial | example::refresh_reports_coverage; example::a_clean_check_exits_zero_with_json_and_writes_nothing | Headless runs are tested and now run in CI. No separate interactive maintenance path exists to compare against. |
| 59 | Apply targets .agents | covered | maintenance::writes_never_target_agents_or_git; matrix::acknowledgement_writes_are_explicit_confined_and_never_under_agents |  |
| 60 | Run deterministic retrieval evaluation | covered | evaluation::replay_is_deterministic_across_fresh_caches_and_scores_cues_and_negatives |  |
| 61 | Run optional agent comparison | partial | evaluation::trial_reports_label_unrun_cells_and_refuse_unjudged_conclusions | Report format tested on synthetic records. No driver exists and no trial was run (task 6.4). |
| 62 | Delivery comparison report | partial | evaluation::trial_reports_label_unrun_cells_and_refuse_unjudged_conclusions | Same as above: report only, nothing measured. |
| 63 | Historical sessions inform fixtures | covered | evaluation::fixtures_with_paths_secrets_or_transcripts_are_rejected |  |
| 64 | Replay delivery triggers | covered | evaluation::replay_is_deterministic_across_fresh_caches_and_scores_cues_and_negatives |  |
| 65 | Candidate is faster but leaks private knowledge | uncovered | (process) | Rollout rule. Not automatable; verify by review. |
| 66 | Results are inconclusive | partial | evaluation::trial_reports_label_unrun_cells_and_refuse_unjudged_conclusions | Process rule; the report refuses unjudged conclusions, the rollout part is by review. |
| 67 | Delivery stage fails dedup gate | uncovered | (process) | Rollout rule. The example enables both stages; dedup tests pass, so the condition is currently met. |

## repository-knowledge-retrieval (38: 37 covered, 1 partial, 0 uncovered)

| # | Scenario | Status | Tests | Gap |
|---|---|---|---|---|
| 68 | Existing caller is unchanged | covered | TOOL::legacy_knowledge_tool_schemas_are_unchanged; TOOL::legacy_knowledge_tools_ignore_the_repository_collection |  |
| 69 | Profile allowlists only repository tools | covered | agent::agent_config_builder::tests::an_allowlist_of_repository_tools_keeps_session_memory_tools_unavailable; config::repository_knowledge_coder_example_loads_and_rolls_back_by_disabling | Runtime allowlist resolved: repo tools allowed, knowledge_* unavailable, policy applies. |
| 70 | Repository service is unavailable | covered | TOOL::unavailable_states_give_actionable_diagnostics; TOOL::repository_tools_never_read_session_memory_or_reply_for_a_missing_manifest |  |
| 71 | Identifier lookup | covered | query::an_exact_path_outranks_documents_that_merely_mention_it; query::an_exact_identifier_finds_its_declaration_with_a_range |  |
| 72 | Conceptual lookup | covered | query::conceptual_question_returns_authored_sections_without_a_symbol_match |  |
| 73 | Query contains punctuation | covered | query::punctuation_and_operator_lookalikes_are_only_text; query::empty_or_unsearchable_questions_get_clear_diagnostics |  |
| 74 | Generated and authored hits overlap | covered | TOOL::symbols_maps_and_purpose_rows_are_retrievable_and_labelled; query::purpose_rows_are_authored_navigation_hits |  |
| 75 | Large query result | covered | TOOL::output_obeys_the_configured_budget_and_discloses_truncation |  |
| 76 | Deprecated document has unchanged evidence | covered | signals::a_deprecated_document_with_unchanged_evidence_reports_both |  |
| 77 | Fetch matching architecture section | covered | browse::a_section_id_returns_the_current_body_not_a_summary; browse::long_sections_page_with_cursors_that_reassemble_exactly |  |
| 78 | Fetch a document without a section | covered | browse::a_path_only_request_returns_a_catalog_and_preamble; TOOL::get_by_path_lists_sections_instead_of_one_arbitrary_section |  |
| 79 | Source changes after query | covered | browse::a_renamed_heading_is_stale_and_never_a_different_section; TOOL::stale_selectors_list_the_sections_that_exist_now |  |
| 80 | Cross-checkout or unsafe fetch | covered | browse::unsafe_foreign_and_malformed_requests_are_rejected_without_disclosure; TOOL::ids_and_paths_from_another_checkout_are_not_found_and_disclose_nothing |  |
| 81 | Inspect corpus availability | covered | browse::stats_count_documents_by_evidence_state; browse::stats_distinguish_indexed_from_excluded_and_incomplete_sources; TOOL::list_and_stats_describe_the_corpus | Stats now report documents by evidence state. |
| 82 | Browse a curated subsystem | covered | catalog::an_authored_listing_is_followed_in_order_cited_and_left_unchanged; catalog::long_listings_are_bounded |  |
| 83 | Directory has no authored listing | covered | catalog::a_directory_without_a_listing_gets_a_derived_catalog_that_writes_nothing |  |
| 84 | Opt-in coding session | covered | agent::repository_delivery::policy_is_attached_once_after_an_unchanged_prefix; policy::text_is_compact_and_does_not_mandate_lookups | Spec amended: availability is reported by repo_knowledge_stats and tool diagnostics, keeping the policy byte-stable for prompt caching. |
| 85 | Tools are excluded | covered | agent::repository_delivery::tests::excluded_retrieval_tools_are_reported_as_a_configuration_mismatch; agent::repository_delivery::no_policy_when_disabled_or_retrieval_tools_are_excluded; DELIV::retrieval_policy_is_gated_on_usable_query_and_get | Mismatch warning now asserted (and absent when tools are usable or the feature is off). |
| 86 | Retrieved document contains instructions | covered | DELIV::instructions_inside_repository_evidence_never_gain_authority |  |
| 87 | Unfamiliar feature request has no filenames | covered | api::agent_tests::repository_knowledge_binding::a_path_free_prompt_reaches_the_first_provider_request_with_an_orientation_card (public API, mock provider, asserts the card in the first provider request; fails with orientation off); gate::a_path_free_conceptual_prompt_reaches_the_matching_section |  |
| 88 | No relevant match exists | covered | adapters::unrelated_empty_and_generic_prompts_produce_nothing; orientation::no_match_and_no_entrypoints_means_no_card_even_when_fallback_is_allowed |  |
| 89 | Gate is reproducible | covered | gate::results_are_reproducible_and_stably_ordered_across_fresh_caches |  |
| 90 | Identifier is split | covered | gate::tokens_follow_the_specification |  |
| 91 | Generic prompt | covered | gate::generic_and_non_english_prompts_yield_nothing |  |
| 92 | Exact path in prompt | covered | gate::an_exact_path_bypasses_the_token_thresholds |  |
| 93 | First task uses a configured repository map | partial | orientation::entrypoints_back_a_first_task_only_when_nothing_passes_the_gate; adapters::entrypoint_fallback_applies_to_the_first_task_only; registry::prewarm_builds_only_configured_checkouts; api::agent_tests::repository_knowledge_binding::a_path_free_prompt_reaches_the_first_provider_request_with_an_orientation_card (asserts session creation pre-warms the index before any prompt) | Cold-cache risk addressed: session creation pre-warms the index when a delivery stage is enabled. Host first-task detection from stored history is still only tested at selector level. |
| 94 | Indexed file has associated knowledge | covered | DELIV::successful_index_gets_attributed_context_and_an_unchanged_result |  |
| 95 | Same basename elsewhere | covered | path_knowledge::a_note_about_one_file_never_attaches_to_a_same_named_file_elsewhere |  |
| 96 | Explicit root differs from session cwd | covered | adapters::the_effective_root_argument_selects_the_same_file_the_tool_read; path_knowledge::index_target_resolves_effective_arguments_like_the_tool |  |
| 97 | File or execution is ineligible | covered | DELIV::failed_and_out_of_corpus_index_calls_attach_nothing; DELIV::a_denied_index_call_attaches_nothing; DELIV::a_foreign_tool_named_index_is_rejected |  |
| 98 | Knowledge service fails or output is suppressed | covered | DELIV::an_unavailable_repository_leaves_the_successful_result_intact; DELIV::a_suppressing_post_tool_hook_prevents_enrichment; DELIV::a_redacting_post_tool_hook_prevents_enrichment; agent::repository_delivery::slow_selection_is_abandoned_at_the_deadline |  |
| 99 | Associated evidence has changed | covered | orientation::evidence_labels_apply_and_missing_evidence_withholds_the_text; path_knowledge::evidence_states_label_cards_and_missing_evidence_withholds_the_text |  |
| 100 | Repository attempts to enable execution | covered | manifest::repository_content_cannot_enable_or_widen_host_controls; DELIV::instructions_inside_repository_evidence_never_gain_authority |  |
| 101 | Parallel index calls compete for context budget | covered | DELIV::parallel_cards_share_one_turn_budget_in_call_order; DELIV::orientation_and_index_draw_from_the_same_turn_budget |  |
| 102 | Orientation and index select the same note | covered | DELIV::orientation_and_index_selecting_the_same_note_deliver_it_once; budget::the_same_card_is_admitted_once_per_turn_across_channels | Was violated: card versions hashed the rendered card, so stage-specific framing made the same section two versions. Fixed: version hashes citation, summary and evidence state only (adapters.rs content_version). |
| 103 | Retry or resume | covered | DELIV::a_provider_retry_reuses_the_stored_context_without_new_selection; DELIV::resume_suppresses_cards_represented_in_persisted_history; DELIV::a_resumed_session_does_not_reinject_index_cards_already_in_history |  |
| 104 | Compaction removes useful context | covered | DELIV::compaction_ends_the_epoch_so_dropped_cards_can_be_redelivered; execution::maintenance post-compaction tests (stale payloads not resurrected) |  |
| 105 | Card is delivered and fetched | covered | diagnostics::delivery_and_fetch_are_separate_events_with_no_followed_outcome; TOOL::successful_get_is_counted_as_a_fetch_and_nothing_else |  |

## Added after the audit (2026-11-13: 8 scenarios, 6 covered, 2 partial)

Scenarios added to the delta specs after live runs. Numbering continues; the totals at the top of this file count only rows 1-105.

| # | Scenario | Status | Tests | Gap |
|---|---|---|---|---|
| 106 | Cold index on the first prompt (retrieval) | partial | registry::a_requested_warm_up_is_visible_until_it_finishes; agent::repository_delivery::a_running_warm_up_extends_only_the_orientation_deadline; api::agent_tests::repository_knowledge_binding::a_path_free_prompt_reaches_the_first_provider_request_with_an_orientation_card (asserts the warm-up runs before any prompt) | No automated test builds a real cold index against a slow build and asserts the card arrives. Checked once live on an empty cache: the first prompt got its card. |
| 107 | Another checkout is warming (retrieval) | covered | agent::repository_delivery::a_running_warm_up_extends_only_the_orientation_deadline; registry::a_requested_warm_up_is_visible_until_it_finishes | |
| 108 | Associated note has several sections (retrieval) | covered | path_knowledge::a_note_cites_the_section_that_names_the_indexed_file | |
| 109 | Card text matches its citation (retrieval) | covered | orientation::a_description_stands_in_only_for_the_top_level_of_its_document; adapters::a_path_free_prompt_gets_user_prompt_context_without_a_tool_call_id | |
| 110 | Retrieval-only documents are never attached unasked (retrieval) | covered | orientation::retrieval_only_documents_are_never_orientation_cards_but_stay_retrievable; path_knowledge::retrieval_only_documents_never_become_path_cards | Retrievability of the held-back document is asserted for orientation only (`fresh_document`), not through each tool. |
| 111 | The fallback is not free (retrieval) | covered | adapters::entrypoint_fallback_applies_to_the_first_task_only (asserts the "not a match for this task" label); replay `entrypoint_fallbacks` count in `eval replay` | The cost statement is documentation (`pilot/README.md`), not a test. |
| 112 | Specs stay searchable but are not attached unasked (manifest) | covered | manifest::retrieval_only_documents_are_a_validated_glob_list (including an invalid pattern as `manifest_invalid_pattern`) | |
| 113 | Refresh outlasts the deadline (retrieval) | partial | agent::repository_delivery::slow_selection_is_abandoned_at_the_deadline; DELIV::an_unavailable_repository_leaves_the_successful_result_intact | The deadline test covers a slow selection, not a refresh still running past the deadline specifically. |
