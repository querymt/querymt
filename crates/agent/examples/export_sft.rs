//! SFT Training Data Export Tool
//!
//! Exports agent session data from the database as JSONL training data
//! suitable for fine-tuning LLMs via SFT (Supervised Fine-Tuning).
//!
//! ## Usage
//!
//! ```bash
//! # Export all sessions as OpenAI chat format (default DB path)
//! cargo run --example export_sft
//!
//! # Export to a file
//! cargo run --example export_sft -- --output training.jsonl
//!
//! # Export only Claude Opus sessions as ShareGPT format
//! cargo run --example export_sft -- --format sharegpt --models claude-opus-4-6
//!
//! # Show stats without exporting
//! cargo run --example export_sft -- --stats
//!
//! # Custom DB path, with path scrubbing
//! cargo run --example export_sft -- --db /path/to/agent.db --scrub-paths
//!
//! # Filter: at least 5 turns, exclude sessions with errors
//! cargo run --example export_sft -- --min-turns 5 --exclude-errored
//! ```

use querymt_agent::export::sft::{SessionFilter, SftExportOptions, SftFormat};
use querymt_agent::session::backend::default_agent_db_path;
use querymt_agent::session::sqlite_storage::SqliteStorage;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug)]
struct ExportArgs {
    db_path: PathBuf,
    output: Option<PathBuf>,
    format: SftFormat,
    stats_only: bool,
    min_turns: usize,
    max_tool_error_rate: f32,
    models: Option<Vec<String>>,
    exclude_errored: bool,
    scrub_paths: bool,
    include_thinking: bool,
    include_tool_results: bool,
    max_context: Option<usize>,
}

fn parse_args() -> Result<ExportArgs, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut db_path: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut format = SftFormat::OpenAiChat;
    let mut stats_only = false;
    let mut min_turns = 1;
    let mut max_tool_error_rate = 1.0f32;
    let mut models: Option<Vec<String>> = None;
    let mut exclude_errored = false;
    let mut scrub_paths = false;
    let mut include_thinking = false;
    let mut include_tool_results = true;
    let mut max_context: Option<usize> = Some(40);

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            "--db" => {
                i += 1;
                db_path = Some(PathBuf::from(args.get(i).ok_or("--db requires a path")?));
            }
            "--output" | "-o" => {
                i += 1;
                output = Some(PathBuf::from(
                    args.get(i).ok_or("--output requires a path")?,
                ));
            }
            "--format" | "-f" => {
                i += 1;
                let fmt_str = args.get(i).ok_or("--format requires a value")?;
                format = SftFormat::parse(fmt_str).ok_or_else(|| {
                    format!("Unknown format '{}'. Use 'openai' or 'sharegpt'.", fmt_str)
                })?;
            }
            "--stats" => stats_only = true,
            "--min-turns" => {
                i += 1;
                min_turns = args
                    .get(i)
                    .ok_or("--min-turns requires a number")?
                    .parse()
                    .map_err(|_| "Invalid --min-turns value".to_string())?;
            }
            "--max-tool-error-rate" => {
                i += 1;
                max_tool_error_rate = args
                    .get(i)
                    .ok_or("--max-tool-error-rate requires a number")?
                    .parse()
                    .map_err(|_| "Invalid --max-tool-error-rate value".to_string())?;
            }
            "--models" => {
                i += 1;
                let m = args.get(i).ok_or("--models requires a value")?;
                models = Some(
                    m.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            }
            "--exclude-errored" => exclude_errored = true,
            "--scrub-paths" => scrub_paths = true,
            "--include-thinking" => include_thinking = true,
            "--no-tool-results" => include_tool_results = false,
            "--max-context" => {
                i += 1;
                let v: usize = args
                    .get(i)
                    .ok_or("--max-context requires a number")?
                    .parse()
                    .map_err(|_| "Invalid --max-context value".to_string())?;
                max_context = Some(v);
            }
            "--no-context-limit" => max_context = None,
            arg => return Err(format!("Unknown argument: {}", arg)),
        }
        i += 1;
    }

    // Default DB path
    let db_path = match db_path {
        Some(p) => p,
        None => {
            default_agent_db_path().map_err(|e| format!("Could not determine DB path: {}", e))?
        }
    };

    Ok(ExportArgs {
        db_path,
        output,
        format,
        stats_only,
        min_turns,
        max_tool_error_rate,
        models,
        exclude_errored,
        scrub_paths,
        include_thinking,
        include_tool_results,
        max_context,
    })
}

fn print_usage() {
    eprintln!(
        r#"SFT Training Data Export Tool

USAGE:
    export_sft [OPTIONS]

OPTIONS:
    --db <path>              Path to agent.db (default: ~/.qmt/agent.db)
    --output, -o <path>      Output file (default: stdout)
    --format, -f <fmt>       Output format: openai (default), sharegpt
    --stats                  Show export stats without writing data
    --min-turns <n>          Minimum LLM turns per session (default: 1)
    --max-tool-error-rate <r> Max tool error rate 0.0-1.0 (default: 1.0)
    --models <m1,m2,...>     Only include sessions using these models
    --exclude-errored        Exclude sessions with error events
    --scrub-paths            Replace home directory paths with /workspace
    --include-thinking       Include thinking/reasoning content
    --no-tool-results        Omit tool result content (reduces size)
    --max-context <n>        Max context messages per example (default: 40)
    --no-context-limit       Include full conversation history
    --help, -h               Print this help message

EXAMPLES:
    # Quick stats
    export_sft --stats

    # Export Claude Opus sessions
    export_sft --models claude-opus-4-6 -o opus_training.jsonl

    # Export for unsloth (ShareGPT format)
    export_sft -f sharegpt --min-turns 3 --scrub-paths -o training.jsonl

    # Export everything, full context
    export_sft --no-context-limit --include-thinking -o full_export.jsonl
"#
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let args = parse_args()?;

    eprintln!("Opening database: {}", args.db_path.display());

    let storage = Arc::new(SqliteStorage::connect_with_options(args.db_path.clone(), false).await?);

    let options = SftExportOptions {
        format: args.format,
        filter: SessionFilter {
            min_turns: args.min_turns,
            max_tool_error_rate: args.max_tool_error_rate,
            source_models: args.models,
            exclude_errored: args.exclude_errored,
        },
        scrub_paths: args.scrub_paths,
        path_replacement: "/workspace".to_string(),
        max_context_messages: args.max_context,
        include_thinking: args.include_thinking,
        include_tool_results: args.include_tool_results,
    };

    if args.stats_only {
        eprintln!("Computing export stats...");
        let stats = querymt_agent::export::sft::preview_export(storage.as_ref(), &options).await?;

        println!("Sessions total:     {}", stats.sessions_total);
        println!("Sessions exported:  {}", stats.sessions_exported);
        println!("Sessions skipped:   {}", stats.sessions_skipped);
        println!("Training examples:  {}", stats.training_examples);
        return Ok(());
    }

    // Open output writer
    let mut writer: Box<dyn std::io::Write + Send> = match &args.output {
        Some(path) => {
            eprintln!("Writing to: {}", path.display());
            Box::new(std::io::BufWriter::new(std::fs::File::create(path)?))
        }
        None => {
            eprintln!("Writing to stdout...");
            Box::new(std::io::BufWriter::new(std::io::stdout()))
        }
    };

    let stats =
        querymt_agent::export::sft::export_all_sessions(storage.as_ref(), &options, &mut writer)
            .await?;

    // Flush
    writer.flush()?;

    eprintln!();
    eprintln!("Export complete:");
    eprintln!("  Sessions total:     {}", stats.sessions_total);
    eprintln!("  Sessions exported:  {}", stats.sessions_exported);
    eprintln!("  Sessions skipped:   {}", stats.sessions_skipped);
    eprintln!("  Training examples:  {}", stats.training_examples);

    Ok(())
}
