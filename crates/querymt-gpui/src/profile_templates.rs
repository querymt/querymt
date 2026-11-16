//! Curated profile templates (desktop `querymt_profile_templates`): ready-made
//! TOML profiles the user can enable. Enabling copies one into the user
//! profile directory (`~/.qmt/profiles`), which qmtcode reads and watches by
//! default. An existing copy is never overwritten.

use std::path::Path;

struct Template {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    tags: &'static [&'static str],
    toml: &'static str,
}

const TEMPLATES: &[Template] = &[
    Template {
        id: "research",
        name: "Research",
        description: "Read-heavy web and document research with curated MCP hooks.",
        tags: &["research", "mcp", "curated"],
        toml: r#"[profile]
id = "research"
name = "Research"
description = "Read-heavy web and document research with curated MCP hooks."
tags = ["research", "mcp", "curated"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["question", "todoread", "todowrite", "fetch.*"]
assume_mutating = false

[[mcp]]
name = "fetch"
transport = "stdio"
command = "${QUERYMT_MCP_FETCH}"
"#,
    },
    Template {
        id: "desktop-lite",
        name: "Desktop Lite",
        description: "Cross-platform clipboard, notification, open-file, and fetch automation.",
        tags: &["desktop", "mcp", "curated"],
        toml: r#"[profile]
id = "desktop-lite"
name = "Desktop Lite"
description = "Cross-platform desktop integration without arbitrary app control."
tags = ["desktop", "mcp", "curated"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["question", "todoread", "todowrite", "desktop_lite.*", "fetch.*"]
assume_mutating = false
mutating_tools = [
  "desktop_lite_clipboard_read",
  "desktop_lite_clipboard_write",
  "desktop_lite_open_url",
  "desktop_lite_open_file",
]

[[mcp]]
name = "desktop_lite"
transport = "stdio"
command = "${QUERYMT_MCP_DESKTOP_LITE}"

[[mcp]]
name = "fetch"
transport = "stdio"
command = "${QUERYMT_MCP_FETCH}"
"#,
    },
    Template {
        id: "browser",
        name: "Browser Connector",
        description: "Browser automation through an installed browser connector; no bundled Chromium.",
        tags: &["browser", "mcp", "curated"],
        toml: r#"[profile]
id = "browser"
name = "Browser Connector"
description = "Uses an installed browser connector; does not bundle Chromium."
tags = ["browser", "mcp", "curated"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["question", "browser.*", "fetch.*"]
assume_mutating = false
mutating_tools = ["browser_click", "browser_type", "browser_submit"]

[[mcp]]
name = "browser"
transport = "stdio"
command = "${QUERYMT_MCP_BROWSER_CONNECTOR}"

[[mcp]]
name = "fetch"
transport = "stdio"
command = "${QUERYMT_MCP_FETCH}"
"#,
    },
];

/// One template and whether this computer already has a copy.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    /// Where the enabled copy lives; `None` while not enabled.
    pub user_path: Option<String>,
}

impl TemplateInfo {
    pub fn enabled(&self) -> bool {
        self.user_path.is_some()
    }
}

#[cfg(target_family = "wasm")]
const UNAVAILABLE: &str = "Profile templates are only available in the desktop app.";

#[cfg(not(target_family = "wasm"))]
fn user_dir() -> Result<std::path::PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join(".qmt").join("profiles"))
        .ok_or_else(|| "Could not find the home directory.".into())
}

/// All templates, with their enabled state.
pub fn list() -> Result<Vec<TemplateInfo>, String> {
    #[cfg(target_family = "wasm")]
    return Err(UNAVAILABLE.into());
    #[cfg(not(target_family = "wasm"))]
    Ok(list_in(&user_dir()?))
}

/// Copy template `id` into the user profile directory unless a copy exists.
pub fn enable(id: &str) -> Result<TemplateInfo, String> {
    #[cfg(target_family = "wasm")]
    return Err(format!("{UNAVAILABLE} ({id})"));
    #[cfg(not(target_family = "wasm"))]
    enable_in(&user_dir()?, id)
}

#[cfg_attr(target_family = "wasm", allow(dead_code))]
fn info(dir: &Path, template: &Template) -> TemplateInfo {
    let path = dir.join(format!("{}.toml", template.id));
    TemplateInfo {
        id: template.id.into(),
        name: template.name.into(),
        description: template.description.into(),
        tags: template.tags.iter().map(|t| (*t).to_string()).collect(),
        user_path: path.exists().then(|| path.to_string_lossy().into_owned()),
    }
}

#[cfg_attr(target_family = "wasm", allow(dead_code))]
fn list_in(dir: &Path) -> Vec<TemplateInfo> {
    TEMPLATES.iter().map(|t| info(dir, t)).collect()
}

#[cfg_attr(target_family = "wasm", allow(dead_code))]
fn enable_in(dir: &Path, id: &str) -> Result<TemplateInfo, String> {
    let template = TEMPLATES
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| format!("Unknown profile template: {id}"))?;
    std::fs::create_dir_all(dir).map_err(|error| {
        format!(
            "Failed to create profiles directory {}: {error}",
            dir.display()
        )
    })?;
    let path = dir.join(format!("{}.toml", template.id));
    if !path.exists() {
        std::fs::write(&path, template.toml).map_err(|error| {
            format!(
                "Failed to enable profile template {}: {error}",
                path.display()
            )
        })?;
    }
    Ok(info(dir, template))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_writes_once_and_lists_the_copy() {
        let dir = std::env::temp_dir().join(format!("qmt-gpui-templates-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(list_in(&dir).iter().all(|t| !t.enabled()));
        assert!(
            enable_in(&dir, "nope")
                .unwrap_err()
                .contains("Unknown profile template")
        );

        let path = dir.join("research.toml");
        let info = enable_in(&dir, "research").unwrap();
        assert!(info.enabled() && path.exists());
        // The user's own edits are never overwritten.
        std::fs::write(&path, "edited").unwrap();
        enable_in(&dir, "research").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");

        let listed = list_in(&dir);
        assert_eq!(listed.iter().filter(|t| t.enabled()).count(), 1);
        assert!(
            listed
                .iter()
                .all(|t| !t.name.is_empty() && !t.tags.is_empty())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn templates_carry_their_profile_id() {
        for t in TEMPLATES {
            assert!(t.toml.contains(&format!("id = \"{}\"", t.id)), "{}", t.id);
        }
    }
}
