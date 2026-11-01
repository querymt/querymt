//! Deterministic repository-knowledge maintenance. No model, no provider
//! credentials, no network.
//!
//! ```text
//! cargo run -p querymt-agent --example repository_knowledge -- --root . check --json
//! ```
//!
//! Operations:
//!
//! * `refresh`      reconcile the local cache and report coverage;
//! * `map`          render a bounded source map (`--output FILE` previews an
//!   export, `--apply` writes it);
//! * `check`        read-only diagnostics; exits 1 when any warning or error is found;
//! * `scaffold`     preview empty-purpose inventory rows (`--apply` writes);
//! * `acknowledge`  preview evidence for explicitly selected records
//!   (`--apply` writes).
//!
//! Nothing writes to the repository without `--apply` (or `map --output ... --apply`),
//! nothing is ever written under `.agents/` or `.git/`, and no Git commit is
//! made. The cache lives under the QueryMT config directory (`--config-dir`
//! overrides it), never in the repository.
//!
//! Exit status: `0` success, `1` the check found problems, `2` the operation
//! could not run or its request was refused.

use clap::{Args, Parser, Subcommand};
use querymt_agent::config::RepositoryKnowledgeSettings;
use querymt_agent::knowledge::repository::evaluation::{
    self, QuestionSet, ReplayOptions, SplitSelect, TrialFile, corpus_fingerprint,
};
use querymt_agent::knowledge::repository::{
    AcknowledgeRequest, AcknowledgeSelection, CheckRequest, ExportAction, ExportRequest,
    MapRequest, ReconcileOptions, RefreshPolicy, RepositoryService, RootRequest, ScaffoldRequest,
    default_config_dir, resolve_root,
};
use querymt_agent::knowledge::repository::{GATE_VERSION, gate_fingerprint};
use serde_json::{Value, json};
use std::fmt::Display;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(about = "Deterministic maintenance for portable repository knowledge")]
struct Cli {
    /// Repository (knowledge) root.
    #[arg(long, default_value = ".")]
    root: PathBuf,

    /// Directory holding the disposable index cache. Defaults to the QueryMT
    /// config directory. Must be outside the repository.
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Reconcile the local cache with the repository.
    Refresh,
    /// Render a bounded source map.
    Map(MapArgs),
    /// Read-only checks; exits 1 on warnings or errors.
    Check(CheckArgs),
    /// Preview (or with --apply, write) empty-purpose inventory rows.
    Scaffold(ScaffoldArgs),
    /// Preview (or with --apply, write) evidence for selected records.
    Acknowledge(AcknowledgeArgs),
    /// Offline pilot evaluation: replay, fingerprints, model-trial reports.
    Eval(EvalArgs),
}

#[derive(Debug, Args)]
struct EvalArgs {
    /// Frozen question set, relative to --root.
    #[arg(long, default_value = "crates/agent/knowledge/pilot/questions.toml")]
    questions: String,
    /// Questions to use.
    #[arg(long, value_parser = ["dev", "held-out", "all"], default_value = "dev")]
    split: String,
    #[command(subcommand)]
    action: EvalAction,
}

#[derive(Debug, Subcommand)]
enum EvalAction {
    /// Print the gate identity and each arm's corpus fingerprint (for freezing).
    Fingerprint,
    /// Deterministic retrieval/delivery replay of one arm. No model.
    Replay {
        #[arg(long)]
        arm: String,
        /// Recall cutoff.
        #[arg(long, default_value_t = evaluation::DEFAULT_RECALL_K)]
        k: usize,
        /// Replay held-out questions even if the gate differs from the frozen one.
        #[arg(long)]
        allow_unfrozen: bool,
        /// Omit wall-clock timing (for byte-for-byte comparisons).
        #[arg(long)]
        no_timing: bool,
    },
    /// Print the model-backed comparison matrix that would be run.
    TrialsPlan {
        #[arg(long, default_value_t = evaluation::DEFAULT_REPEATS)]
        repeats: u32,
    },
    /// Summarize a recorded trial file (JSON). Reads only the file.
    TrialsReport {
        /// Trial file path, relative to --root unless absolute.
        file: String,
        #[arg(long, default_value_t = evaluation::DEFAULT_REPEATS)]
        repeats: u32,
    },
}

#[derive(Debug, Args)]
struct MapArgs {
    /// Directory or file to confine the map to.
    #[arg(long)]
    path: Option<String>,
    /// Byte budget.
    #[arg(long)]
    max_bytes: Option<usize>,
    /// Export the map into this file's `qmt:map` block instead of printing it.
    #[arg(long)]
    output: Option<String>,
    /// Write the export. Without it the export is only previewed.
    #[arg(long, requires = "output")]
    apply: bool,
}

#[derive(Debug, Args)]
struct CheckArgs {
    /// Require a File/Purpose row for every code file under this directory
    /// (repeatable). Without it, per-file rows are never required.
    #[arg(long = "exhaustive", value_name = "DIR")]
    exhaustive: Vec<String>,
}

#[derive(Debug, Args)]
struct ScaffoldArgs {
    /// Non-root directory to scaffold.
    directory: String,
    /// Document name inside the directory (default `AGENTS.md`).
    #[arg(long)]
    file_name: Option<String>,
    /// Write the change. Without it the change is only previewed.
    #[arg(long)]
    apply: bool,
}

#[derive(Debug, Args)]
struct AcknowledgeArgs {
    /// Indexed document to acknowledge.
    document: String,
    /// Record key to acknowledge (repeatable), as listed by `check`/evidence.
    #[arg(long = "record", value_name = "KEY")]
    records: Vec<String>,
    /// Acknowledge every record of this one document.
    #[arg(long, conflicts_with = "records")]
    whole_document: bool,
    /// Write the evidence file. Without it the change is only previewed.
    #[arg(long)]
    apply: bool,
}

/// An operation that could not run, as opposed to a check that found problems.
#[derive(Debug)]
struct Refused {
    code: String,
    message: String,
}

impl Refused {
    fn new(code: &str, error: impl Display) -> Self {
        Self {
            code: code.to_owned(),
            message: error.to_string(),
        }
    }
}

fn action_name(action: ExportAction) -> &'static str {
    match action {
        ExportAction::Create => "create",
        ExportAction::Replace => "replace",
        ExportAction::Unchanged => "unchanged",
    }
}

fn open(cli: &Cli) -> Result<RepositoryService, Refused> {
    let root = resolve_root(&RootRequest {
        host_root: Some(&cli.root),
        ..Default::default()
    })
    .map_err(|error| Refused::new(error.code(), error))?;
    let config_dir = match &cli.config_dir {
        Some(dir) => dir.clone(),
        None => default_config_dir().map_err(|error| Refused::new(error.code(), error))?,
    };
    let index = root
        .index_path(&config_dir)
        .map_err(|error| Refused::new(error.code(), error))?;
    RepositoryService::open(
        root,
        &index,
        ReconcileOptions::from(&RepositoryKnowledgeSettings::default()),
        RefreshPolicy::default(),
    )
    .map_err(|error| Refused::new(error.code(), error))
}

fn emit(out: &mut impl Write, json_mode: bool, value: &Value, text: &str) {
    let _ = if json_mode {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_default()
        )
    } else {
        writeln!(out, "{text}")
    };
}

/// Run one operation, writing its result to `out`. Returns the exit status.
fn split_of(raw: &str) -> SplitSelect {
    match raw {
        "held-out" => SplitSelect::HeldOut,
        "all" => SplitSelect::All,
        _ => SplitSelect::Dev,
    }
}

fn eval_refused(error: impl Display) -> Refused {
    Refused::new("eval_failed", error)
}

fn run_eval(cli: &Cli, args: &EvalArgs, out: &mut impl Write) -> Result<i32, Refused> {
    let root = cli.root.canonicalize().map_err(eval_refused)?;
    let path = root.join(&args.questions);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| eval_refused(format!("{}: {e}", args.questions)))?;
    let set = QuestionSet::parse(&text).map_err(eval_refused)?;
    let select = split_of(&args.split);
    // Scratch lives outside the repository and is removed afterwards.
    let scratch = tempfile::tempdir().map_err(eval_refused)?;
    match &args.action {
        EvalAction::Fingerprint => {
            let mut corpora = serde_json::Map::new();
            for arm in &set.arm {
                let dir = scratch.path().join(&arm.name);
                let print = corpus_fingerprint(&root, arm, &dir).map_err(eval_refused)?;
                corpora.insert(arm.name.clone(), json!(print));
            }
            let value = json!({
                "revision": set.revision,
                "gate_version": GATE_VERSION,
                "gate_fingerprint": gate_fingerprint(),
                "corpus": corpora,
            });
            let text = serde_json::to_string_pretty(&value).unwrap_or_default();
            emit(out, true, &value, &text);
            Ok(0)
        }
        EvalAction::Replay {
            arm,
            k,
            allow_unfrozen,
            no_timing,
        } => {
            let options = ReplayOptions {
                select,
                k: *k,
                allow_unfrozen: *allow_unfrozen,
            };
            let mut report = evaluation::run(&root, &set, arm, &options, scratch.path())
                .map_err(eval_refused)?;
            if *no_timing {
                report = report.deterministic();
            }
            let value = serde_json::to_value(&report).map_err(eval_refused)?;
            let all_r = &report.retrieval.get("all/all");
            let all_d = &report.delivery.get("all");
            let text = format!(
                "arm {} ({} files, corpus {}), split {}, gate v{} {}{}\nretrieval: recall@{} {:.2}, mrr {:.2}, wrong top hit {:.2}\ndelivery: cue hits {}/{}, false positives {}/{}, wrong cards {}, tokens {}\nscenarios ok: {}/{}",
                report.arm,
                report.corpus_files,
                report.corpus_fingerprint,
                report.split,
                report.gate_version,
                report.gate_fingerprint,
                if report.gate_frozen {
                    ""
                } else {
                    " (NOT the frozen gate)"
                },
                report.k,
                all_r.map_or(0.0, |r| r.recall_at_k),
                all_r.map_or(0.0, |r| r.mrr),
                all_r.map_or(0.0, |r| r.wrong_file_rate),
                all_d.map_or(0, |d| d.cue_hits),
                all_d.map_or(0, |d| d.positives),
                all_d.map_or(0, |d| d.false_positive_prompts),
                all_d.map_or(0, |d| d.negatives),
                all_d.map_or(0, |d| d.wrong_cards),
                all_d.map_or(0, |d| d.added_tokens),
                report.scenarios.iter().filter(|s| s.ok).count(),
                report.scenarios.len(),
            );
            emit(out, cli.json, &value, &text);
            Ok(i32::from(report.scenarios.iter().any(|s| !s.ok)))
        }
        EvalAction::TrialsPlan { repeats } => {
            let plan = evaluation::plan_trials(&set, *repeats, select);
            let value = serde_json::to_value(&plan).map_err(eval_refused)?;
            let text = plan
                .iter()
                .map(|c| format!("{}/{}: {} trials", c.arm, c.mode.as_str(), c.planned_trials))
                .collect::<Vec<_>>()
                .join("\n");
            emit(out, cli.json, &value, &text);
            Ok(0)
        }
        EvalAction::TrialsReport { file, repeats } => {
            let raw = std::fs::read_to_string(root.join(file))
                .map_err(|e| eval_refused(format!("{file}: {e}")))?;
            let trials = TrialFile::parse(&raw).map_err(eval_refused)?;
            let report = evaluation::report_trials(&set, &trials, *repeats, select);
            let value = serde_json::to_value(&report).map_err(eval_refused)?;
            let text = report
                .cells
                .iter()
                .map(|c| {
                    format!(
                        "{}/{}: {:?} {}/{} trials",
                        c.arm,
                        c.mode.as_str(),
                        c.status,
                        c.recorded,
                        c.planned
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            emit(out, cli.json, &value, &text);
            Ok(i32::from(!report.problems.is_empty()))
        }
    }
}

fn run(cli: &Cli, out: &mut impl Write) -> Result<i32, Refused> {
    if let Command::Eval(args) = &cli.command {
        return run_eval(cli, args, out);
    }
    let service = open(cli)?;
    let json_mode = cli.json;
    match &cli.command {
        Command::Eval(_) => unreachable!("handled before the service is opened"),
        Command::Refresh => {
            let report = service
                .refresh()
                .map_err(|error| Refused::new(error.code(), error))?;
            let diagnostics: Vec<Value> = report
                .diagnostics
                .iter()
                .map(|d| json!({ "code": d.code, "path": d.path, "message": d.message }))
                .collect();
            let value = json!({
                "selected": report.selected,
                "added": report.added,
                "changed": report.changed,
                "removed": report.removed,
                "complete": report.is_complete(),
                "diagnostics": diagnostics,
            });
            let text = format!(
                "selected {} (added {}, changed {}, removed {}); coverage {}",
                report.selected,
                report.added,
                report.changed,
                report.removed,
                if report.is_complete() {
                    "complete"
                } else {
                    "incomplete"
                },
            );
            emit(out, json_mode, &value, &text);
            Ok(0)
        }
        Command::Map(args) => {
            if let Some(target) = &args.output {
                let plan = service
                    .plan_export(&ExportRequest {
                        target: target.clone(),
                        scope: args.path.clone(),
                        max_bytes: args.max_bytes,
                    })
                    .map_err(|error| Refused::new(error.code(), error))?;
                let written = if args.apply {
                    Some(
                        service
                            .apply_export(&plan)
                            .map_err(|error| Refused::new(error.code(), error))?
                            .written,
                    )
                } else {
                    None
                };
                let value = json!({
                    "target": plan.target,
                    "action": action_name(plan.action),
                    "applied": args.apply,
                    "written": written,
                    "truncated": plan.map.truncated,
                });
                let text = format!(
                    "{} {} ({})",
                    if args.apply { "applied" } else { "preview:" },
                    action_name(plan.action),
                    plan.target
                );
                emit(out, json_mode, &value, &text);
            } else {
                let output = service
                    .map(&MapRequest {
                        path: args.path.clone(),
                        max_bytes: args.max_bytes,
                    })
                    .map_err(|error| Refused::new(error.code(), error))?;
                let value = json!({
                    "scope": output.scope,
                    "files_total": output.map.files_total,
                    "files_shown": output.map.files_shown,
                    "truncated": output.map.truncated,
                    "narrower": output.map.narrower,
                    "map": output.map.text,
                });
                emit(out, json_mode, &value, output.map.text.trim_end());
            }
            Ok(0)
        }
        Command::Check(args) => {
            let report = service
                .check(&CheckRequest {
                    exhaustive: args.exhaustive.clone(),
                })
                .map_err(|error| Refused::new(error.code(), error))?;
            let text = if report.findings.is_empty() {
                "ok: no findings".to_owned()
            } else {
                report
                    .findings
                    .iter()
                    .map(|f| {
                        format!(
                            "{} [{}] {}: {}",
                            f.severity_name(),
                            f.code,
                            f.path.as_deref().unwrap_or("-"),
                            f.message
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            emit(out, json_mode, &report.to_json(), &text);
            Ok(i32::from(report.is_failing()))
        }
        Command::Scaffold(args) => {
            let request = ScaffoldRequest {
                directory: args.directory.clone(),
                file_name: args.file_name.clone(),
            };
            let plan = service
                .plan_scaffold(&request)
                .map_err(|error| Refused::new(error.code(), error))?;
            let receipt = if args.apply {
                Some(
                    service
                        .apply_scaffold(&plan)
                        .map_err(|error| Refused::new(error.code(), error))?,
                )
            } else {
                None
            };
            let mut value = plan.to_json();
            value["applied"] = json!(args.apply);
            value["written"] = json!(receipt.as_ref().map(|r| r.written));
            let text = format!(
                "{} {} {} row(s) in {}",
                if args.apply { "applied" } else { "preview:" },
                plan.action.as_str(),
                plan.additions.len(),
                plan.target
            );
            emit(out, json_mode, &value, &text);
            // A refused target is a finding to read, not a failed operation.
            Ok(0)
        }
        Command::Acknowledge(args) => {
            let selection = if args.whole_document {
                AcknowledgeSelection::WholeDocument
            } else {
                AcknowledgeSelection::Records(args.records.clone())
            };
            let plan = service
                .plan_acknowledge(&AcknowledgeRequest {
                    document: args.document.clone(),
                    selection,
                })
                .map_err(|error| Refused::new(error.code(), error))?;
            let receipt = if args.apply {
                Some(
                    service
                        .apply_acknowledge(&plan)
                        .map_err(|error| Refused::new(error.code(), error))?,
                )
            } else {
                None
            };
            let value = json!({
                "document": plan.document,
                "evidence_file": plan.write.path,
                "action": action_name(plan.action),
                "records": plan.entries.keys().collect::<Vec<_>>(),
                "applied": args.apply,
                "written": receipt.as_ref().map(|r| r.written),
            });
            let text = format!(
                "{} {} {} record(s) -> {}",
                if args.apply { "applied" } else { "preview:" },
                action_name(plan.action),
                plan.entries.len(),
                plan.write.path
            );
            emit(out, json_mode, &value, &text);
            Ok(0)
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let code = match run(&cli, &mut std::io::stdout()) {
        Ok(code) => code,
        Err(refused) => {
            if cli.json {
                let value =
                    json!({ "error": { "code": refused.code, "message": refused.message } });
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&value).unwrap_or_default()
                );
            } else {
                eprintln!("error [{}]: {}", refused.code, refused.message);
            }
            2
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    struct Repo {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        cache: PathBuf,
    }

    impl Repo {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("repo");
            fs::create_dir_all(root.join(".git")).unwrap();
            let cache = tmp.path().join("cache");
            let repo = Self {
                _tmp: tmp,
                root,
                cache,
            };
            repo.put(
                ".qmt/knowledge.toml",
                "version = 1\n[documents]\ninclude = [\"docs/**/*.md\"]\n[code]\ninclude = [\"src/**/*.rs\"]\n",
            );
            repo.put("src/sched/a.rs", "pub fn a() {}\n");
            repo.put("src/sched/b.rs", "pub fn b() {}\n");
            repo.put("docs/arch.md", "# Arch\nText.\n");
            repo
        }

        fn put(&self, rel: &str, content: &str) {
            let path = self.root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        /// Run the example with `args`, returning `(status, stdout)`.
        fn run(&self, args: &[&str]) -> (Result<i32, Refused>, String) {
            let mut full = vec![
                "repository_knowledge",
                "--root",
                self.root.to_str().unwrap(),
                "--config-dir",
                self.cache.to_str().unwrap(),
            ];
            full.extend_from_slice(args);
            let cli = Cli::try_parse_from(full).unwrap();
            let mut out = Vec::new();
            let status = run(&cli, &mut out);
            (status, String::from_utf8(out).unwrap())
        }

        fn read(&self, rel: &str) -> String {
            fs::read_to_string(self.root.join(rel)).unwrap()
        }

        fn files(&self) -> Vec<String> {
            fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
                for entry in fs::read_dir(dir).unwrap().flatten() {
                    let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
                    if name == ".git" {
                        continue;
                    }
                    if entry.path().is_dir() {
                        walk(&entry.path(), &format!("{name}/"), out);
                    } else {
                        out.push(name);
                    }
                }
            }
            let mut out = Vec::new();
            walk(&self.root, "", &mut out);
            out.sort();
            out
        }
    }

    #[test]
    fn a_clean_check_exits_zero_with_json_and_writes_nothing() {
        let repo = Repo::new();
        let before = repo.files();
        let (status, out) = repo.run(&["check", "--json"]);
        assert_eq!(status.unwrap(), 0);
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(
            repo.files(),
            before,
            "check writes nothing to the repository"
        );
        assert!(
            repo.cache.exists(),
            "the cache lives outside the repository"
        );
    }

    #[test]
    fn a_failing_check_exits_one_with_stable_codes_and_relative_paths() {
        let repo = Repo::new();
        repo.put("docs/bad.md", "# Bad\n[x](missing.md)\n");
        let (status, out) = repo.run(&["check", "--json"]);
        assert_eq!(status.unwrap(), 1);
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["findings"][0]["code"], "reference_broken");
        assert_eq!(value["findings"][0]["path"], "docs/bad.md");
        assert!(!out.contains(repo.root.to_str().unwrap()));

        let (status, text) = repo.run(&["check"]);
        assert_eq!(status.unwrap(), 1);
        assert!(text.contains("reference_broken") && text.contains("docs/bad.md"));
    }

    #[test]
    fn exhaustive_scopes_are_explicit_and_validated() {
        let repo = Repo::new();
        assert_eq!(repo.run(&["check"]).0.unwrap(), 0);
        assert_eq!(
            repo.run(&["check", "--exhaustive", "src/sched"]).0.unwrap(),
            1
        );
        let refused = repo.run(&["check", "--exhaustive", "../x"]).0.unwrap_err();
        assert_eq!(refused.code, "request_invalid");
    }

    #[test]
    fn refresh_reports_coverage() {
        let repo = Repo::new();
        let (status, out) = repo.run(&["refresh", "--json"]);
        assert_eq!(status.unwrap(), 0);
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["selected"], 3);
        assert_eq!(value["complete"], true);
    }

    #[test]
    fn map_prints_and_exports_only_with_apply() {
        let repo = Repo::new();
        let (_, printed) = repo.run(&["map", "--path", "src"]);
        assert!(
            printed.contains("## src/sched") && printed.contains("a.rs"),
            "{printed}"
        );
        assert!(!repo.root.join("docs/MAP.md").exists());

        let before = repo.files();
        let (_, preview) = repo.run(&["map", "--output", "docs/MAP.md", "--json"]);
        assert!(preview.contains("\"action\": \"create\""));
        assert_eq!(
            repo.files(),
            before,
            "an export without --apply writes nothing"
        );

        let (status, _) = repo.run(&["map", "--output", "docs/MAP.md", "--apply"]);
        assert_eq!(status.unwrap(), 0);
        assert!(repo.read("docs/MAP.md").contains("qmt:map:begin"));
    }

    #[test]
    fn scaffold_previews_then_applies_and_never_touches_other_files() {
        let repo = Repo::new();
        let before = repo.files();
        let (_, preview) = repo.run(&["scaffold", "src/sched", "--json"]);
        let value: Value = serde_json::from_str(&preview).unwrap();
        assert_eq!(value["action"], "create");
        assert_eq!(value["additions"], json!(["a.rs", "b.rs"]));
        assert_eq!(repo.files(), before);

        repo.run(&["scaffold", "src/sched", "--apply"]).0.unwrap();
        assert!(repo.read("src/sched/AGENTS.md").contains("| `a.rs` | |"));

        let (_, again) = repo.run(&["scaffold", "src/sched", "--json"]);
        assert!(again.contains("\"action\": \"unchanged\""));
    }

    #[test]
    fn a_refused_scaffold_target_is_a_finding_and_stays_untouched() {
        let repo = Repo::new();
        repo.put("src/sched/AGENTS.md", "Hand-written.\n");
        let (status, out) = repo.run(&["scaffold", "src/sched", "--apply", "--json"]);
        assert_eq!(status.unwrap(), 0);
        assert!(out.contains("scaffold_target_unmanaged"));
        assert_eq!(repo.read("src/sched/AGENTS.md"), "Hand-written.\n");
    }

    #[test]
    fn acknowledge_requires_an_explicit_selection_and_apply() {
        let repo = Repo::new();
        let refused = repo.run(&["acknowledge", "docs/arch.md"]).0.unwrap_err();
        assert_eq!(
            refused.code, "request_invalid",
            "no implicit bulk acknowledgement"
        );

        let (_, preview) = repo.run(&["acknowledge", "docs/arch.md", "--whole-document", "--json"]);
        assert!(preview.contains("\"applied\": false"));
        assert!(!repo.root.join(".qmt/knowledge/evidence").exists());

        repo.run(&["acknowledge", "docs/arch.md", "--whole-document", "--apply"])
            .0
            .unwrap();
        assert!(
            repo.root
                .join(".qmt/knowledge/evidence/docs/arch.md.json")
                .exists()
        );
        assert_eq!(repo.run(&["check"]).0.unwrap(), 0);

        repo.put("docs/arch.md", "# Arch\nRewritten.\n");
        assert_eq!(
            repo.run(&["check"]).0.unwrap(),
            1,
            "edited note is reported"
        );
    }

    #[test]
    fn writes_under_agents_and_git_are_refused() {
        let repo = Repo::new();
        let refused = repo
            .run(&["map", "--output", ".agents/map.md", "--apply"])
            .0
            .unwrap_err();
        assert_eq!(refused.code, "export_target_forbidden");
        let refused = repo.run(&["scaffold", ".git/hooks"]).0.unwrap_err();
        assert_eq!(refused.code, "export_target_forbidden");
        assert!(!repo.root.join(".agents").exists());
    }

    #[test]
    fn an_unusable_root_is_refused_not_guessed() {
        let repo = Repo::new();
        let missing = repo.root.join("nope");
        let cli = Cli::try_parse_from([
            "repository_knowledge",
            "--root",
            missing.to_str().unwrap(),
            "--config-dir",
            repo.cache.to_str().unwrap(),
            "check",
        ])
        .unwrap();
        assert!(run(&cli, &mut Vec::new()).is_err());
    }

    #[test]
    fn the_cache_cannot_be_placed_inside_the_repository() {
        let repo = Repo::new();
        let cli = Cli::try_parse_from([
            "repository_knowledge",
            "--root",
            repo.root.to_str().unwrap(),
            "--config-dir",
            repo.root.join("cache").to_str().unwrap(),
            "refresh",
        ])
        .unwrap();
        assert!(run(&cli, &mut Vec::new()).is_err());
    }
}
