//! Conflict-aware scheduling of the tool calls in one model response.
//!
//! [`plan_waves`] is a pure function: it takes the execution class and the
//! declared file access of every call (in the model's original order) and
//! returns ordered *waves* of call indices. Calls inside one wave never
//! conflict and may run concurrently; waves run one after another.
//!
//! Guarantees:
//! - every call appears in exactly one wave;
//! - for any two conflicting calls the earlier one (model order) is in an
//!   earlier wave;
//! - non-`ParallelSafe` calls (barriers) run alone, and no call is moved across
//!   a barrier;
//! - unrelated calls are placed as early as possible to keep parallelism.

use std::path::PathBuf;

use crate::tools::{FileAccess, ReadScope, ToolExecutionClass, paths_overlap};

/// What the planner needs to know about a single tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CallInfo {
    pub class: ToolExecutionClass,
    pub access: FileAccess,
}

/// Indices (into the planned call slice) that may run concurrently.
pub(super) type Wave = Vec<usize>;

/// Whether two calls must not run concurrently because of their file access.
///
/// - `Paths` vs `Paths`: two writes overlap when the paths are equal or one
///   contains the other; a read and a write overlap when the read scope would
///   observe the write ([`ReadScope::observes_write`]). A directory read
///   (`ls`, `glob`, `search_text`) therefore waits only for writes beneath it
///   that pass the same include/exclude/file-type filter the tool applies.
/// - `Unknown` vs a call that declares at least one write.
/// - `Any` vs any call that declares file access (reads, writes, `Unknown`,
///   `Any`).
/// - Everything else (`None`, read/read, `Unknown` vs `Unknown`, `Unknown` vs
///   readers) is compatible.
fn conflicts(a: &FileAccess, b: &FileAccess) -> bool {
    match (a, b) {
        (
            FileAccess::Paths {
                reads: a_reads,
                writes: a_writes,
                tree_writes: a_trees,
            },
            FileAccess::Paths {
                reads: b_reads,
                writes: b_writes,
                tree_writes: b_trees,
            },
        ) => {
            let a_all = a_writes.iter().chain(a_trees);
            let b_all: Vec<&PathBuf> = b_writes.iter().chain(b_trees).collect();
            let write_write = a_all
                .into_iter()
                .any(|a| b_all.iter().any(|b| paths_overlap(a, b)));
            write_write
                || reads_observe(a_reads, b_writes, b_trees)
                || reads_observe(b_reads, a_writes, a_trees)
        }
        (FileAccess::Any, other) | (other, FileAccess::Any) => !matches!(other, FileAccess::None),
        (FileAccess::Unknown, other) | (other, FileAccess::Unknown) => other.has_writes(),
        (FileAccess::None, _) | (_, FileAccess::None) => false,
    }
}

fn reads_observe(reads: &[ReadScope], writes: &[PathBuf], trees: &[PathBuf]) -> bool {
    reads.iter().any(|scope| {
        writes.iter().any(|written| scope.observes_write(written))
            || trees.iter().any(|tree| scope.observes_tree_write(tree))
    })
}

/// Group `calls` into ordered waves. See the module docs for the guarantees.
pub(super) fn plan_waves(calls: &[CallInfo]) -> Vec<Wave> {
    let mut waves: Vec<Wave> = Vec::with_capacity(calls.len());
    // Index of the first wave a parallel call may join: the wave right after
    // the most recent barrier.
    let mut floor = 0;

    for (index, call) in calls.iter().enumerate() {
        if call.class != ToolExecutionClass::ParallelSafe {
            waves.push(vec![index]);
            floor = waves.len();
            continue;
        }

        let mut earliest = floor;
        for (wave_index, wave) in waves.iter().enumerate().skip(floor) {
            if wave
                .iter()
                .any(|&other| conflicts(&call.access, &calls[other].access))
            {
                earliest = earliest.max(wave_index + 1);
            }
        }

        if earliest == waves.len() {
            waves.push(vec![index]);
        } else {
            waves[earliest].push(index);
        }
    }

    waves
}

/// Number of parallel-safe calls that had to wait for an earlier wave because
/// of a conflict (i.e. calls placed after the first wave of their barrier-free
/// segment). Barrier calls are not counted: they are serial by definition.
pub(super) fn serialized_call_count(calls: &[CallInfo], waves: &[Wave]) -> usize {
    let mut floor = 0;
    let mut serialized = 0;
    for (wave_index, wave) in waves.iter().enumerate() {
        let is_barrier = wave
            .first()
            .is_some_and(|&index| calls[index].class != ToolExecutionClass::ParallelSafe);
        if is_barrier {
            floor = wave_index + 1;
        } else if wave_index > floor {
            serialized += wave.len();
        }
    }
    serialized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialized_call_count_ignores_barriers_and_first_waves() {
        let calls = [
            write("a.md"),
            write("a.md"),
            write("b.md"),
            barrier(ToolExecutionClass::SerialStateful),
            write("c.md"),
            write("c.md"),
        ];
        let waves = plan_waves(&calls);
        assert_eq!(waves, vec![vec![0, 2], vec![1], vec![3], vec![4], vec![5]]);
        // Call 1 waits on call 0 and call 5 waits on call 4.
        assert_eq!(serialized_call_count(&calls, &waves), 2);
    }

    #[test]
    fn serialized_call_count_counts_only_conflicting_calls() {
        let calls = [write("a.md"), write("b.md"), read("a.md")];
        let waves = plan_waves(&calls);
        assert_eq!(serialized_call_count(&calls, &waves), 1);
        let calls = [write("a.md"), write("b.md"), read("c.md")];
        let waves = plan_waves(&calls);
        assert_eq!(serialized_call_count(&calls, &waves), 0);
    }

    fn p(name: &str) -> PathBuf {
        PathBuf::from(format!("/ws/{name}"))
    }

    fn call(access: FileAccess) -> CallInfo {
        CallInfo {
            class: ToolExecutionClass::ParallelSafe,
            access,
        }
    }

    fn write(name: &str) -> CallInfo {
        call(FileAccess::writes([p(name)]))
    }

    fn read(name: &str) -> CallInfo {
        call(FileAccess::reads([p(name)]))
    }

    fn search(dir: &str, filter: crate::tools::ReadFilter) -> CallInfo {
        call(FileAccess::read_scopes([ReadScope::filtered(
            p(dir),
            filter,
        )]))
    }

    fn workspace_search() -> CallInfo {
        call(FileAccess::reads([PathBuf::from("/ws")]))
    }

    fn barrier(class: ToolExecutionClass) -> CallInfo {
        CallInfo {
            class,
            access: FileAccess::None,
        }
    }

    #[test]
    fn empty_batch_has_no_waves() {
        assert!(plan_waves(&[]).is_empty());
    }

    #[test]
    fn same_file_edits_are_split_into_ordered_waves() {
        let waves = plan_waves(&[write("a.md"), write("a.md"), write("a.md")]);
        assert_eq!(waves, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn different_file_edits_share_one_wave() {
        let waves = plan_waves(&[write("a.md"), write("b.md"), write("c.md")]);
        assert_eq!(waves, vec![vec![0, 1, 2]]);
    }

    #[test]
    fn unrelated_call_after_conflict_joins_earliest_wave() {
        // 0 and 1 conflict; 2 is unrelated and must not wait for wave 1.
        let waves = plan_waves(&[write("a.md"), write("a.md"), write("b.md")]);
        assert_eq!(waves, vec![vec![0, 2], vec![1]]);
    }

    #[test]
    fn parallel_reads_of_one_file_share_a_wave() {
        let waves = plan_waves(&[read("a.md"), read("a.md"), read("a.md")]);
        assert_eq!(waves, vec![vec![0, 1, 2]]);
    }

    #[test]
    fn read_after_write_runs_after_the_write() {
        let waves = plan_waves(&[write("x.rs"), read("x.rs")]);
        assert_eq!(waves, vec![vec![0], vec![1]]);
    }

    #[test]
    fn write_after_read_runs_after_the_read() {
        let waves = plan_waves(&[read("x.rs"), write("x.rs")]);
        assert_eq!(waves, vec![vec![0], vec![1]]);
    }

    #[test]
    fn read_of_other_file_does_not_wait_for_write() {
        let waves = plan_waves(&[write("x.rs"), read("y.rs")]);
        assert_eq!(waves, vec![vec![0, 1]]);
    }

    #[test]
    fn directory_read_waits_for_write_beneath_it() {
        let waves = plan_waves(&[write("src/a.rs"), read("src")]);
        assert_eq!(waves, vec![vec![0], vec![1]]);
    }

    #[test]
    fn write_beneath_directory_read_waits_for_the_read() {
        let waves = plan_waves(&[read("src"), write("src/a.rs")]);
        assert_eq!(waves, vec![vec![0], vec![1]]);
    }

    #[test]
    fn directory_read_ignores_sibling_with_common_name_prefix() {
        let waves = plan_waves(&[write("src2/a.rs"), read("src")]);
        assert_eq!(waves, vec![vec![0, 1]]);
    }

    #[test]
    fn grouped_reads_then_edits_take_two_waves() {
        // search + 2 reads, then 3 edits of the files read.
        let calls = [
            workspace_search(),
            read("a.rs"),
            read("b.rs"),
            write("a.rs"),
            write("b.rs"),
            write("c.rs"),
        ];
        assert_eq!(plan_waves(&calls), vec![vec![0, 1, 2], vec![3, 4, 5]]);
    }

    #[test]
    fn grouped_edits_then_reads_take_two_waves() {
        let calls = [
            write("a.rs"),
            write("b.rs"),
            write("c.rs"),
            workspace_search(),
            read("a.rs"),
            read("b.rs"),
        ];
        assert_eq!(plan_waves(&calls), vec![vec![0, 1, 2], vec![3, 4, 5]]);
    }

    #[test]
    fn interleaved_reads_and_edits_follow_the_dependency_chain() {
        // edit a, search all, edit b, read b: each depends on the previous one.
        let calls = [
            write("a.rs"),
            workspace_search(),
            write("b.rs"),
            read("b.rs"),
        ];
        assert_eq!(plan_waves(&calls), vec![vec![0], vec![1], vec![2], vec![3]]);
    }

    #[test]
    fn filtered_search_ignores_writes_its_filter_excludes() {
        use crate::tools::ReadFilter;
        let calls = [
            write("src/a.rs"),
            search("src", ReadFilter::new().include("*.md")),
            write("src/b.rs"),
        ];
        assert_eq!(plan_waves(&calls), vec![vec![0, 1, 2]]);
    }

    #[test]
    fn filtered_search_waits_for_writes_its_filter_matches() {
        use crate::tools::ReadFilter;
        let calls = [
            write("src/a.md"),
            search("src", ReadFilter::new().include("*.md")),
            write("src/b.md"),
        ];
        assert_eq!(plan_waves(&calls), vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn excluded_and_typed_searches_skip_unrelated_writes() {
        use crate::tools::ReadFilter;
        let calls = [
            write("src/target/out.rs"),
            write("src/notes.md"),
            search(
                "src",
                ReadFilter::new().exclude("target/**").file_type("rust"),
            ),
        ];
        assert_eq!(plan_waves(&calls), vec![vec![0, 1, 2]]);
    }

    #[test]
    fn deleting_a_searched_directory_ignores_the_filter() {
        use crate::tools::ReadFilter;
        let calls = [
            write("src"),
            search("src", ReadFilter::new().include("*.md")),
        ];
        assert_eq!(plan_waves(&calls), vec![vec![0], vec![1]]);
    }

    #[test]
    fn removing_a_directory_beneath_a_filtered_search_is_ordered() {
        use crate::tools::ReadFilter;
        let calls = [
            call(FileAccess::writes_tree(p("src/docs"))),
            search("src", ReadFilter::new().include("*.md")),
            write("src/docs/a.md"),
        ];
        assert_eq!(plan_waves(&calls), vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn any_access_conflicts_with_reads_writes_unknown_and_any() {
        // Any waits for the read; the write and Unknown wait for Any (and
        // Unknown also for the write); the second Any waits for everything.
        let calls = [
            read("a.md"),
            call(FileAccess::Any),
            write("b.md"),
            call(FileAccess::Unknown),
            call(FileAccess::Any),
        ];
        assert_eq!(
            plan_waves(&calls),
            vec![vec![0], vec![1], vec![2], vec![3], vec![4]]
        );
        // Unknown alone after Any still waits (Unknown vs Unknown would not).
        let calls = [call(FileAccess::Any), call(FileAccess::Unknown)];
        assert_eq!(plan_waves(&calls), vec![vec![0], vec![1]]);
    }

    #[test]
    fn any_access_runs_alongside_calls_without_file_access() {
        let calls = [call(FileAccess::Any), call(FileAccess::None)];
        assert_eq!(plan_waves(&calls), vec![vec![0, 1]]);
    }

    #[test]
    fn shell_waits_for_preceding_edit() {
        let waves = plan_waves(&[write("a.md"), call(FileAccess::Unknown)]);
        assert_eq!(waves, vec![vec![0], vec![1]]);
    }

    #[test]
    fn edit_waits_for_preceding_shell() {
        let waves = plan_waves(&[call(FileAccess::Unknown), write("a.md")]);
        assert_eq!(waves, vec![vec![0], vec![1]]);
    }

    #[test]
    fn shell_runs_alongside_reads_and_other_shells() {
        let waves = plan_waves(&[
            call(FileAccess::Unknown),
            read("a.md"),
            read("b.md"),
            call(FileAccess::Unknown),
        ]);
        assert_eq!(waves, vec![vec![0, 1, 2, 3]]);
    }

    #[test]
    fn multi_file_replace_symbol_conflicts_with_single_edit() {
        let multi = call(FileAccess::writes([p("a.rs"), p("b.rs")]));
        let waves = plan_waves(&[multi, write("b.rs")]);
        assert_eq!(waves, vec![vec![0], vec![1]]);
    }

    #[test]
    fn multi_file_replace_symbol_does_not_block_disjoint_edit() {
        let multi = call(FileAccess::writes([p("a.rs"), p("b.rs")]));
        let waves = plan_waves(&[multi, write("c.rs")]);
        assert_eq!(waves, vec![vec![0, 1]]);
    }

    #[test]
    fn tool_without_declared_access_runs_alongside_edit() {
        let waves = plan_waves(&[write("a.md"), call(FileAccess::None)]);
        assert_eq!(waves, vec![vec![0, 1]]);
    }

    #[test]
    fn barrier_runs_alone_and_orders_surrounding_calls() {
        let waves = plan_waves(&[
            write("a.md"),
            write("b.md"),
            barrier(ToolExecutionClass::SerialStateful),
            write("c.md"),
            write("d.md"),
        ]);
        assert_eq!(waves, vec![vec![0, 1], vec![2], vec![3, 4]]);
    }

    #[test]
    fn calls_are_never_hoisted_across_a_barrier() {
        // `c.md` has no conflict with anything before the barrier, but must
        // still run after it.
        let waves = plan_waves(&[
            barrier(ToolExecutionClass::ClarificationBoundary),
            write("c.md"),
        ]);
        assert_eq!(waves, vec![vec![0], vec![1]]);
    }

    #[test]
    fn conflicts_are_only_tracked_after_the_last_barrier() {
        let waves = plan_waves(&[
            write("a.md"),
            barrier(ToolExecutionClass::SerialStateful),
            write("a.md"),
            write("a.md"),
        ]);
        assert_eq!(waves, vec![vec![0], vec![1], vec![2], vec![3]]);
    }

    #[test]
    fn every_call_is_planned_exactly_once() {
        let calls = [
            write("a"),
            read("a"),
            call(FileAccess::Unknown),
            barrier(ToolExecutionClass::SerialStateful),
            write("a"),
            write("b"),
            call(FileAccess::None),
        ];
        let mut seen: Vec<usize> = plan_waves(&calls).into_iter().flatten().collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..calls.len()).collect::<Vec<_>>());
    }

    #[test]
    fn conflicting_pairs_keep_model_order_across_waves() {
        let calls = [
            write("a"),
            read("a"),
            write("b"),
            write("a"),
            call(FileAccess::Unknown),
            read("b"),
        ];
        let waves = plan_waves(&calls);
        let wave_of = |index: usize| {
            waves
                .iter()
                .position(|wave| wave.contains(&index))
                .expect("call is planned")
        };
        for i in 0..calls.len() {
            for j in (i + 1)..calls.len() {
                if conflicts(&calls[i].access, &calls[j].access) {
                    assert!(
                        wave_of(i) < wave_of(j),
                        "call {i} must run in an earlier wave than conflicting call {j}"
                    );
                }
            }
        }
    }
}
