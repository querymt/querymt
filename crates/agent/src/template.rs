//! MiniJinja-based system prompt templating.
//!
//! Templates use `{{ var }}` / `{% if %}` syntax from Jinja2.  They are
//! **kept unresolved** in the stored config, then **resolved per-session** at
//! session creation time so that session-specific values (cwd, datetime, …)
//! are available.
//!
//! # Two-phase design
//!
//! 1. **Config load** — `validate_template` parses every system prompt string
//!    and errors if unknown variable names are referenced.  The template string
//!    itself is left unchanged in `LLMParams.system`.
//!
//! 2. **Session creation** — `SessionTemplateContext::builder()` collects the
//!    runtime values, and `resolve_params` renders all template strings before
//!    the resolved `LLMParams` is written to the DB.

use anyhow::{Result, anyhow};
use gix;
use minijinja::{Environment, context};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use querymt::LLMParams;

/// All template variable names that are allowed in system prompts.
///
/// Used for strict validation at config load time.  Any variable not in this
/// list causes `validate_template` to return an error.
pub const KNOWN_TEMPLATE_VARS: &[&str] = &[
    // Working directory & git
    "cwd",
    "is_git",
    "git_tree",
    // Time
    "date",
    "datetime",
    "timezone",
    // Machine identity
    "platform",
    "os_version",
    "arch",
    "hostname",
    "username",
    "shell",
    "home_dir",
    "locale",
    // Agent / LLM
    "provider",
    "model",
    "agent_id",
    // Mesh
    "has_mesh",
];

/// Validate that a template string only references known variables.
///
/// Called at config load time.  Does **not** resolve values — templates stay
/// as literal strings in `LLMParams`.
///
/// No-ops for strings without `{{` or `{%` — plain prompts pass through
/// instantly without parsing overhead.
pub fn validate_template(content: &str) -> Result<()> {
    if !content.contains("{{") && !content.contains("{%") {
        return Ok(());
    }

    let env = Environment::new();
    let tmpl = env
        .template_from_str(content)
        .map_err(|e| anyhow!("Template syntax error in system prompt: {e}"))?;

    let known: HashSet<&str> = KNOWN_TEMPLATE_VARS.iter().copied().collect();
    let unknown: Vec<_> = tmpl
        .undeclared_variables(true)
        .into_iter()
        .filter(|v| !known.contains(v.as_str()))
        .collect();

    if !unknown.is_empty() {
        let mut unknown = unknown;
        unknown.sort();
        return Err(anyhow!(
            "Unknown template variable(s): {}. Known: {}",
            unknown
                .iter()
                .map(|v| format!("{{{{ {v} }}}}"))
                .collect::<Vec<_>>()
                .join(", "),
            KNOWN_TEMPLATE_VARS.join(", "),
        ));
    }

    Ok(())
}

// ============================================================================
// Session context
// ============================================================================

/// Session-scoped template context.
///
/// Built once per session creation via [`SessionTemplateContextBuilder`] and
/// used to render all system prompt template strings before writing the
/// resolved config to the database.
///
/// Construct with [`SessionTemplateContext::builder()`].
#[derive(Debug, Clone)]
pub struct SessionTemplateContext {
    // Working directory & git
    pub cwd: String,
    pub is_git: String,
    pub git_tree: String,
    // Time
    pub date: String,
    pub datetime: String,
    pub timezone: String,
    // Machine identity
    pub platform: String,
    pub os_version: String,
    pub arch: String,
    pub hostname: String,
    pub username: String,
    pub shell: String,
    pub home_dir: String,
    pub locale: String,
    // Agent / LLM
    pub provider: String,
    pub model: String,
    pub agent_id: String,
    // Mesh
    pub has_mesh: String,
}

impl SessionTemplateContext {
    /// Start building a [`SessionTemplateContext`].
    pub fn builder() -> SessionTemplateContextBuilder {
        SessionTemplateContextBuilder::default()
    }

    /// Render a template string with the session context.
    ///
    /// Returns the original string unchanged if it contains no `{{` or `{%`
    /// (fast path for plain prompts — avoids a parse + render round-trip).
    pub fn render(&self, content: &str) -> Result<String> {
        if !content.contains("{{") && !content.contains("{%") {
            return Ok(content.to_string());
        }

        let env = Environment::new();
        let tmpl = env.template_from_str(content)?;
        Ok(tmpl.render(context! {
            cwd        => &self.cwd,
            is_git     => &self.is_git,
            git_tree   => &self.git_tree,
            date       => &self.date,
            datetime   => &self.datetime,
            timezone   => &self.timezone,
            platform   => &self.platform,
            os_version => &self.os_version,
            arch       => &self.arch,
            hostname   => &self.hostname,
            username   => &self.username,
            shell      => &self.shell,
            home_dir   => &self.home_dir,
            locale     => &self.locale,
            provider   => &self.provider,
            model      => &self.model,
            agent_id   => &self.agent_id,
            has_mesh   => &self.has_mesh,
        })?)
    }

    /// Clone an `LLMParams` with all system prompt strings rendered.
    ///
    /// Non-system fields are copied unchanged.
    pub fn resolve_params(&self, config: &LLMParams) -> Result<LLMParams> {
        let mut resolved = config.clone();
        let rendered: Result<Vec<String>> = config.system.iter().map(|s| self.render(s)).collect();
        resolved.system = rendered?;
        Ok(resolved)
    }
}

// ============================================================================
// Builder
// ============================================================================

/// Builder for [`SessionTemplateContext`].
///
/// Collects session-specific and machine-specific values, then calls
/// [`build`](SessionTemplateContextBuilder::build) to produce the context.
///
/// Machine-level values (hostname, arch, os version, timezone, …) are
/// collected automatically from the environment when `build()` is called.
/// Only session-specific values need to be supplied explicitly.
///
/// # Example
///
/// ```no_run
/// use querymt_agent::template::SessionTemplateContext;
///
/// let ctx = SessionTemplateContext::builder()
///     .cwd("/home/user/project")
///     .provider("anthropic")
///     .model("claude-opus-4-5")
///     .agent_id("coder")
///     .has_mesh(true)
///     .build();
/// ```
#[derive(Default)]
pub struct SessionTemplateContextBuilder {
    cwd: Option<PathBuf>,
    provider: Option<String>,
    model: Option<String>,
    agent_id: Option<String>,
    has_mesh: bool,
}

impl SessionTemplateContextBuilder {
    /// Set the working directory.
    ///
    /// Falls back to `std::env::current_dir()` then `"."` if not set.
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set the LLM provider name (e.g. `"anthropic"`).
    pub fn provider(mut self, p: impl Into<String>) -> Self {
        self.provider = Some(p.into());
        self
    }

    /// Set the model name (e.g. `"claude-opus-4-5"`).
    pub fn model(mut self, m: impl Into<String>) -> Self {
        self.model = Some(m.into());
        self
    }

    /// Set the agent identifier used for `{{ agent_id }}`.
    pub fn agent_id(mut self, id: impl Into<String>) -> Self {
        self.agent_id = Some(id.into());
        self
    }

    /// Indicate whether this node is part of a libp2p mesh.
    pub fn has_mesh(mut self, v: bool) -> Self {
        self.has_mesh = v;
        self
    }

    /// Collect all values and produce a [`SessionTemplateContext`].
    ///
    /// Machine-level values (hostname, arch, os version, timezone, etc.) are
    /// read from the environment here. This involves a small number of
    /// subprocess calls (e.g. `sw_vers` on macOS) — expected overhead is
    /// under 50 ms total and happens once per session creation.
    pub fn build(self) -> SessionTemplateContext {
        // ── CWD ──────────────────────────────────────────────────────────────
        let effective_cwd = self
            .cwd
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| ".".to_string())
            });

        // ── Git ───────────────────────────────────────────────────────────────
        let cwd_path = self.cwd.as_deref();
        let git_root = cwd_path.map(is_git_repo).unwrap_or(false);
        let git_tree = if git_root {
            cwd_path.map(|p| get_git_tree(p, 50)).unwrap_or_default()
        } else {
            String::new()
        };

        // ── Time ─────────────────────────────────────────────────────────────
        let now =
            time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());

        let date = format!(
            "{:04}-{:02}-{:02}",
            now.year(),
            now.month() as u8,
            now.day(),
        );
        let datetime = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            now.year(),
            now.month() as u8,
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
        );
        let timezone = iana_time_zone::get_timezone().unwrap_or_else(|_| {
            // Fall back to a numeric UTC offset string, e.g. "+02:00"
            let offset = now.offset();
            format!(
                "{:+03}:{:02}",
                offset.whole_hours(),
                offset.minutes_past_hour().unsigned_abs(),
            )
        });

        // ── Machine identity ─────────────────────────────────────────────────
        let platform = std::env::consts::OS.to_string();
        let os_version = get_os_version();
        let arch = std::env::consts::ARCH.to_string();
        let hostname = get_hostname();
        let username = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_default();
        let shell = std::env::var("SHELL").unwrap_or_default();
        let home_dir = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        let locale = std::env::var("LANG")
            .or_else(|_| std::env::var("LC_ALL"))
            .unwrap_or_default();

        SessionTemplateContext {
            cwd: effective_cwd,
            is_git: if git_root { "yes" } else { "no" }.to_string(),
            git_tree,
            date,
            datetime,
            timezone,
            platform,
            os_version,
            arch,
            hostname,
            username,
            shell,
            home_dir,
            locale,
            provider: self.provider.unwrap_or_else(|| "unknown".to_string()),
            model: self.model.unwrap_or_else(|| "unknown".to_string()),
            agent_id: self.agent_id.unwrap_or_else(|| "agent".to_string()),
            has_mesh: if self.has_mesh { "yes" } else { "no" }.to_string(),
        }
    }
}

// ============================================================================
// System info helpers
// ============================================================================

/// Read the local hostname.
///
/// Checks `$HOSTNAME` first (cheap), then falls back to running the
/// `hostname` command (one subprocess).
fn get_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.is_empty()
    {
        return h;
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Return a human-readable OS version string.
///
/// - macOS: `"macOS 15.5"` via `sw_vers -productVersion`
/// - Linux: `PRETTY_NAME` from `/etc/os-release`
/// - Other: falls back to `std::env::consts::OS`
fn get_os_version() -> String {
    #[cfg(target_os = "macos")]
    {
        let version = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        format!("macOS {version}")
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(contents) = std::fs::read_to_string("/etc/os-release") {
            for line in contents.lines() {
                if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
                    return rest.trim_matches('"').to_string();
                }
            }
        }
        "Linux".to_string()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        std::env::consts::OS.to_string()
    }
}

// ============================================================================
// Git helpers
// ============================================================================

/// Walk up ancestor directories looking for a `.git` entry.
///
/// Returns `true` if `path` itself or any ancestor contains `.git`.
pub(crate) fn is_git_repo(path: &Path) -> bool {
    let mut current = path;
    loop {
        if current.join(".git").exists() {
            return true;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return false,
        }
    }
}

/// Walk the HEAD commit tree using the embedded `gix` client and return up to
/// `max_entries` file paths joined by `\n`.
///
/// Returns an empty string on any error (not a git repo, no commits, …).
pub(crate) fn get_git_tree(cwd: &Path, max_entries: usize) -> String {
    let mut files: Vec<String> = Vec::new();
    if collect_git_tree_files(cwd, max_entries, &mut files).is_err() {
        return String::new();
    }
    files.join("\n")
}

/// Inner fallible helper so the caller can stay infallible.
fn collect_git_tree_files(
    cwd: &Path,
    max_entries: usize,
    out: &mut Vec<String>,
) -> anyhow::Result<()> {
    let repo = gix::discover(cwd)?;
    let head_commit = repo.head_commit()?;
    let tree = head_commit.tree()?;
    collect_tree_recursive(&tree, "", max_entries, out);
    Ok(())
}

/// Recursively walk a `gix::Tree`, appending file paths (relative to the repo
/// root) to `out`.  Stops early once `max_entries` paths have been collected.
fn collect_tree_recursive(
    tree: &gix::Tree<'_>,
    prefix: &str,
    max_entries: usize,
    out: &mut Vec<String>,
) {
    for entry_result in tree.iter() {
        if out.len() >= max_entries {
            break;
        }
        let entry = match entry_result {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = match std::str::from_utf8(entry.filename().as_ref()) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let full_path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", prefix, name)
        };

        if entry.mode().is_tree() {
            if let Ok(obj) = entry.object()
                && let Ok(sub_tree) = obj.try_into_tree()
            {
                collect_tree_recursive(&sub_tree, &full_path, max_entries, out);
            }
        } else if entry.mode().is_blob() || entry.mode().is_blob_or_symlink() {
            out.push(full_path);
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // -- validate_template ----------------------------------------------------

    #[test]
    fn test_validate_known_vars() {
        let tmpl = KNOWN_TEMPLATE_VARS
            .iter()
            .map(|v| format!("{{{{ {v} }}}}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            validate_template(&tmpl).is_ok(),
            "all known vars should pass"
        );
    }

    #[test]
    fn test_validate_unknown_var() {
        let err = validate_template("Hello {{ unknown_thing }}").unwrap_err();
        assert!(
            err.to_string().contains("unknown_thing"),
            "error should name the unknown variable: {err}"
        );
    }

    #[test]
    fn test_validate_syntax_error() {
        let err = validate_template("{{ unclosed").unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("syntax")
                || err.to_string().to_lowercase().contains("error")
                || err.to_string().to_lowercase().contains("template"),
            "should report a template error: {err}"
        );
    }

    #[test]
    fn test_validate_no_templates() {
        assert!(validate_template("Just a plain system prompt.").is_ok());
        assert!(validate_template("").is_ok());
    }

    #[test]
    fn test_validate_conditional() {
        let tmpl = r#"{% if is_git == "yes" %}git{% endif %}"#;
        assert!(validate_template(tmpl).is_ok());
    }

    #[test]
    fn test_validate_mixed_known_unknown() {
        let err = validate_template("{{ model }} {{ bad_var }}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bad_var"), "should mention bad_var: {msg}");
        assert!(
            !msg.contains("model") || msg.contains("Known:"),
            "should not flag known var"
        );
    }

    // -- builder --------------------------------------------------------------

    fn make_ctx() -> SessionTemplateContext {
        SessionTemplateContext::builder()
            .provider("anthropic")
            .model("claude-3-5-sonnet")
            .agent_id("planner")
            .build()
    }

    #[test]
    fn test_builder_defaults() {
        let ctx = SessionTemplateContext::builder().build();
        assert_eq!(ctx.provider, "unknown");
        assert_eq!(ctx.model, "unknown");
        assert_eq!(ctx.agent_id, "agent");
        assert_eq!(ctx.has_mesh, "no");
    }

    #[test]
    fn test_builder_with_values() {
        let dir = TempDir::new().unwrap();
        let ctx = SessionTemplateContext::builder()
            .cwd(dir.path())
            .provider("openai")
            .model("gpt-4o")
            .agent_id("coder")
            .has_mesh(true)
            .build();
        assert_eq!(ctx.provider, "openai");
        assert_eq!(ctx.model, "gpt-4o");
        assert_eq!(ctx.agent_id, "coder");
        assert_eq!(ctx.cwd, dir.path().display().to_string());
        assert_eq!(ctx.has_mesh, "yes");
    }

    #[test]
    fn test_builder_machine_fields_non_empty() {
        let ctx = SessionTemplateContext::builder().build();
        assert!(!ctx.hostname.is_empty(), "hostname should be non-empty");
        assert!(!ctx.arch.is_empty(), "arch should be non-empty");
        assert!(!ctx.os_version.is_empty(), "os_version should be non-empty");
        assert!(!ctx.platform.is_empty(), "platform should be non-empty");
        assert!(!ctx.timezone.is_empty(), "timezone should be non-empty");
    }

    #[test]
    fn test_builder_platform_is_known() {
        let ctx = SessionTemplateContext::builder().build();
        assert!(
            ctx.platform == "linux" || ctx.platform == "macos" || ctx.platform == "windows",
            "unexpected platform: {}",
            ctx.platform
        );
    }

    #[test]
    fn test_builder_date_format() {
        let ctx = SessionTemplateContext::builder().build();
        let parts: Vec<&str> = ctx.date.split('-').collect();
        assert_eq!(parts.len(), 3, "date should be YYYY-MM-DD: {}", ctx.date);
        assert_eq!(parts[0].len(), 4);
        assert_eq!(parts[1].len(), 2);
        assert_eq!(parts[2].len(), 2);
    }

    #[test]
    fn test_builder_has_mesh_false_by_default() {
        let ctx = SessionTemplateContext::builder().build();
        assert_eq!(ctx.has_mesh, "no");
    }

    #[test]
    fn test_builder_has_mesh_true() {
        let ctx = SessionTemplateContext::builder().has_mesh(true).build();
        assert_eq!(ctx.has_mesh, "yes");
    }

    // -- render ---------------------------------------------------------------

    #[test]
    fn test_render_basic() {
        let ctx = make_ctx();
        let result = ctx.render("Model: {{ model }}").unwrap();
        assert_eq!(result, "Model: claude-3-5-sonnet");
    }

    #[test]
    fn test_render_all_new_vars_accessible() {
        let ctx = SessionTemplateContext::builder()
            .provider("anthropic")
            .model("claude-opus-4-5")
            .has_mesh(true)
            .build();
        // Render a template that references every new variable — must not error.
        let tmpl = "{{ hostname }} {{ arch }} {{ os_version }} {{ shell }} \
                    {{ username }} {{ home_dir }} {{ timezone }} {{ locale }} \
                    {{ has_mesh }}";
        let result = ctx.render(tmpl);
        assert!(
            result.is_ok(),
            "rendering new vars should not fail: {:?}",
            result
        );
        let rendered = result.unwrap();
        assert!(rendered.contains("yes"), "has_mesh should be 'yes'");
    }

    #[test]
    fn test_render_conditional_git_true() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        // Without commits git_tree is empty, but is_git should be "yes"
        let ctx = SessionTemplateContext::builder().cwd(dir.path()).build();
        let tmpl = r#"{% if is_git == "yes" %}git{% endif %}"#;
        let result = ctx.render(tmpl).unwrap();
        assert_eq!(result.trim(), "git");
    }

    #[test]
    fn test_render_conditional_git_false() {
        let ctx = make_ctx();
        let tmpl = r#"{% if is_git == "yes" %}git{% endif %}"#;
        let result = ctx.render(tmpl).unwrap();
        assert_eq!(result.trim(), "");
    }

    #[test]
    fn test_render_no_templates() {
        let ctx = make_ctx();
        let plain = "Just a plain string with no templates.";
        let result = ctx.render(plain).unwrap();
        assert_eq!(result, plain);
    }

    #[test]
    fn test_render_has_mesh_conditional() {
        let ctx_mesh = SessionTemplateContext::builder().has_mesh(true).build();
        let ctx_no_mesh = SessionTemplateContext::builder().has_mesh(false).build();
        let tmpl = r#"{% if has_mesh == "yes" %}connected{% else %}local{% endif %}"#;
        assert_eq!(ctx_mesh.render(tmpl).unwrap().trim(), "connected");
        assert_eq!(ctx_no_mesh.render(tmpl).unwrap().trim(), "local");
    }

    // -- resolve_params -------------------------------------------------------

    #[test]
    fn test_resolve_params_replaces_system() {
        let ctx = make_ctx();
        let params = LLMParams::default()
            .provider("anthropic")
            .model("claude-3-5-sonnet")
            .system("Hello from {{ provider }}/{{ model }}");

        let resolved = ctx.resolve_params(&params).unwrap();
        assert_eq!(
            resolved.system,
            vec!["Hello from anthropic/claude-3-5-sonnet"]
        );
    }

    #[test]
    fn test_resolve_params_non_system_unchanged() {
        let ctx = make_ctx();
        let params = LLMParams::default()
            .provider("anthropic")
            .model("claude-3-5-sonnet")
            .system("{{ model }}");

        let resolved = ctx.resolve_params(&params).unwrap();
        assert_eq!(resolved.provider.as_deref(), Some("anthropic"));
        assert_eq!(resolved.model.as_deref(), Some("claude-3-5-sonnet"));
    }

    // -- get_os_version -------------------------------------------------------

    #[test]
    fn test_get_os_version_non_empty() {
        let v = get_os_version();
        assert!(!v.is_empty(), "os_version should always be non-empty");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_get_os_version_macos_prefix() {
        let v = get_os_version();
        assert!(
            v.starts_with("macOS"),
            "macOS version should start with 'macOS': {v}"
        );
    }

    // -- get_hostname ---------------------------------------------------------

    #[test]
    fn test_get_hostname_non_empty() {
        let h = get_hostname();
        assert!(!h.is_empty(), "hostname should always be non-empty");
    }

    // -- is_git_repo ----------------------------------------------------------

    #[test]
    fn test_is_git_repo_finds_git_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(is_git_repo(dir.path()));
    }

    #[test]
    fn test_is_git_repo_walks_ancestors() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let sub = dir.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(is_git_repo(&sub), ".git in ancestor should return true");
    }

    // -- get_git_tree ---------------------------------------------------------

    #[test]
    fn test_get_git_tree_no_git() {
        let dir = TempDir::new().unwrap();
        let result = get_git_tree(dir.path(), 50);
        assert_eq!(result, "", "non-repo dir should return empty string");
    }
}
