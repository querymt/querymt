//! Skills published by MCP servers (MCP Skills extension, SEP-2640).
//!
//! Discovery (listing eligible servers' skills into the session's MCP
//! snapshot) and verified loading of a skill's `SKILL.md` and files.

use base64::Engine as _;
use rmcp::{
    RoleClient,
    model::{
        GetSkillRequestParams, ReadResourceRequestParams, ResourceContents, Skill, SkillResource,
        SkillResources,
    },
    service::Peer,
};
use std::sync::Arc;

/// Longest description shown to the model, in characters (the Agent Skills
/// description limit).
pub const MCP_SKILL_DESCRIPTION_MAX: usize = 1024;

/// Largest `SKILL.md` or supporting file accepted from a server.
pub const MCP_SKILL_MAX_BYTES: u64 = 256 * 1024;

/// Longest a server may take to list its skills. Discovery is best-effort and
/// runs while a session is being created, so a stalled server must not block
/// it; on timeout the server simply contributes no skills.
pub const MCP_SKILL_LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Longest a server may take for one skill load request (`skills/get` or a
/// resource read). A stalled server must not hang the `skill` tool call.
pub const MCP_SKILL_LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Longest accepted skill name, in characters (the Agent Skills name limit).
pub const MCP_SKILL_NAME_MAX: usize = 64;

/// One valid skill published by a connected MCP server.
pub struct McpSkill {
    pub server_name: String,
    /// The validated listing entry; its description is already sanitized.
    pub skill: Skill,
    pub peer: Peer<RoleClient>,
}

impl McpSkill {
    /// The skill name from the frontmatter (never contains `.`).
    pub fn name(&self) -> &str {
        self.skill.name()
    }

    /// Callable ID: `<server>.<name>`.
    pub fn callable_id(&self) -> String {
        format!("{}.{}", self.server_name, self.name())
    }
}

impl std::fmt::Debug for McpSkill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpSkill")
            .field("server_name", &self.server_name)
            .field("uri", &self.skill.uri)
            .finish()
    }
}

/// Remove control characters, collapse whitespace runs (including newlines)
/// to one space and truncate to [`MCP_SKILL_DESCRIPTION_MAX`] characters with
/// an ellipsis.
pub fn sanitize_description(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MCP_SKILL_DESCRIPTION_MAX + 4));
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if ch.is_control() {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    if out.chars().count() > MCP_SKILL_DESCRIPTION_MAX {
        let mut truncated: String = out.chars().take(MCP_SKILL_DESCRIPTION_MAX - 1).collect();
        truncated.push('…');
        truncated
    } else {
        out
    }
}

/// Whether a server's name allows unambiguous `server.skill` callable IDs.
pub fn server_name_is_eligible(server_name: &str) -> bool {
    !server_name.contains('.')
}

/// Names are shown to the model and used in callable IDs, so they must be
/// non-empty, bounded and free of whitespace and control characters.
fn check_skill_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("empty");
    }
    if name.chars().count() > MCP_SKILL_NAME_MAX {
        return Err("too long");
    }
    if name.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("contains whitespace or control characters");
    }
    Ok(())
}

/// Keep only the usable entries of a listing: structurally valid, with a
/// well-formed name without `.`; descriptions are sanitized. Dropped entries are logged with
/// the server name and URI.
pub fn filter_listed_skills(server_name: &str, listed: Vec<Skill>) -> Vec<Skill> {
    let mut seen = std::collections::HashSet::new();
    let mut kept = Vec::new();
    for mut skill in listed {
        if let Err(error) = skill.validate() {
            log::warn!(
                "MCP server '{server_name}': excluding invalid skill '{}': {error}",
                skill.uri
            );
            continue;
        }
        if skill.name().contains('.') {
            log::warn!(
                "MCP server '{server_name}': excluding skill '{}': its name contains '.'",
                skill.uri
            );
            continue;
        }
        if let Err(why) = check_skill_name(skill.name()) {
            log::warn!(
                "MCP server '{server_name}': excluding skill '{}': invalid name: {why}",
                skill.uri
            );
            continue;
        }
        if !seen.insert(skill.name().to_string()) {
            log::warn!(
                "MCP server '{server_name}': excluding duplicate skill name '{}' ({})",
                skill.name(),
                skill.uri
            );
            continue;
        }
        skill.frontmatter.description = sanitize_description(&skill.frontmatter.description);
        kept.push(skill);
    }
    kept
}

/// List the skills of one connected server.
///
/// Returns `None` when the server is not eligible (no skills extension, or a
/// name containing `.`, which is logged) or when listing failed (logged);
/// returns `Some(skills)` otherwise. Never fails the connection, and never
/// waits longer than [`MCP_SKILL_LIST_TIMEOUT`].
pub async fn discover_server_skills(
    server_name: &str,
    peer: &Peer<RoleClient>,
) -> Option<Vec<Arc<McpSkill>>> {
    let declares = peer
        .peer_info()
        .is_some_and(|info| info.capabilities.supports_skills());
    if !declares {
        return None;
    }
    if !server_name_is_eligible(server_name) {
        log::warn!(
            "MCP server '{server_name}' declares skills but its name contains '.'; \
             its skills are not offered (rename the server to enable them)"
        );
        return None;
    }
    discover_with_timeout(server_name, peer, MCP_SKILL_LIST_TIMEOUT).await
}

async fn discover_with_timeout(
    server_name: &str,
    peer: &Peer<RoleClient>,
    timeout: std::time::Duration,
) -> Option<Vec<Arc<McpSkill>>> {
    let listing = match tokio::time::timeout(timeout, peer.list_all_skills()).await {
        Ok(listing) => listing,
        Err(_) => {
            log::warn!(
                "MCP server '{server_name}': listing skills timed out after {}s",
                timeout.as_secs_f32()
            );
            return None;
        }
    };
    match listing {
        Ok(listed) => Some(
            filter_listed_skills(server_name, listed)
                .into_iter()
                .map(|skill| {
                    Arc::new(McpSkill {
                        server_name: server_name.to_string(),
                        skill,
                        peer: peer.clone(),
                    })
                })
                .collect(),
        ),
        Err(error) => {
            log::warn!("MCP server '{server_name}': listing skills failed: {error}");
            None
        }
    }
}

/// Whether a connected server can contribute skills: it declares the
/// extension and its name allows unambiguous `server.skill` IDs.
pub fn is_skill_server(server_name: &str, peer: &Peer<RoleClient>) -> bool {
    server_name_is_eligible(server_name)
        && peer
            .peer_info()
            .is_some_and(|info| info.capabilities.supports_skills())
}

/// A verified, parsed MCP skill.
#[derive(Debug)]
pub struct LoadedMcpSkill {
    pub description: String,
    /// Markdown body without frontmatter.
    pub body: String,
    /// Skill-relative file keys a `file` argument accepts; `None` for a
    /// dynamic skill, whose files are not listed.
    pub files: Option<Vec<String>>,
}

fn rejected(id: &str, message: impl std::fmt::Display) -> String {
    format!("MCP skill '{id}' rejected: {message}")
}

/// Fetch the skill's current entry (`skills/get`), bounded by
/// [`MCP_SKILL_LOAD_TIMEOUT`].
async fn fetch_current_entry(skill: &McpSkill, id: &str) -> Result<Skill, String> {
    let response = tokio::time::timeout(
        MCP_SKILL_LOAD_TIMEOUT,
        skill
            .peer
            .get_skill(GetSkillRequestParams::new(skill.skill.uri.clone())),
    )
    .await
    .map_err(|_| {
        rejected(
            id,
            format!(
                "skills/get timed out after {}s",
                MCP_SKILL_LOAD_TIMEOUT.as_secs()
            ),
        )
    })?
    .map_err(|error| rejected(id, format!("skills/get failed: {error}")))?;
    Ok(response.skill)
}

/// Re-check the entry returned at load time against the listed one.
pub fn check_entry(listed: &Skill, current: &Skill) -> Result<(), String> {
    current.validate().map_err(|error| error.to_string())?;
    if current.uri != listed.uri {
        return Err(format!(
            "its URI changed from {} to {}",
            listed.uri, current.uri
        ));
    }
    if current.name() != listed.name() {
        return Err(format!(
            "its name changed from '{}' to '{}'",
            listed.name(),
            current.name()
        ));
    }
    Ok(())
}

/// Pick exactly the content whose URI equals `uri` and return its bytes.
/// Other contents are ignored. Text is taken as UTF-8 bytes, a blob must be
/// valid base64.
pub fn select_content(uri: &str, contents: Vec<ResourceContents>) -> Result<Vec<u8>, String> {
    for content in contents {
        match content {
            ResourceContents::TextResourceContents {
                uri: content_uri,
                text,
                ..
            } if content_uri == uri => return Ok(text.into_bytes()),
            ResourceContents::BlobResourceContents {
                uri: content_uri,
                blob,
                ..
            } if content_uri == uri => {
                // Bound the work before decoding: base64 inflates by 4/3.
                if blob.len() as u64 > MCP_SKILL_MAX_BYTES / 3 * 4 + 8 {
                    return Err(format!(
                        "{uri} exceeds the {MCP_SKILL_MAX_BYTES} byte limit"
                    ));
                }
                return base64::engine::general_purpose::STANDARD
                    .decode(blob.as_bytes())
                    .map_err(|error| format!("{uri} is not valid base64: {error}"));
            }
            _ => {}
        }
    }
    Err(format!("the server returned no content for {uri}"))
}

/// Read one resource and return the bytes of exactly that URI, bounded by
/// [`MCP_SKILL_MAX_BYTES`].
pub async fn read_uri_bytes(peer: &Peer<RoleClient>, uri: &str) -> Result<Vec<u8>, String> {
    let result = tokio::time::timeout(
        MCP_SKILL_LOAD_TIMEOUT,
        peer.read_resource(ReadResourceRequestParams::new(uri)),
    )
    .await
    .map_err(|_| {
        format!(
            "reading {uri} timed out after {}s",
            MCP_SKILL_LOAD_TIMEOUT.as_secs()
        )
    })?
    .map_err(|error| format!("reading {uri} failed: {error}"))?;
    let bytes = select_content(uri, result.contents)?;
    if bytes.len() as u64 > MCP_SKILL_MAX_BYTES {
        return Err(format!(
            "{uri} is {} bytes, over the {MCP_SKILL_MAX_BYTES} byte limit",
            bytes.len()
        ));
    }
    Ok(bytes)
}

/// The skill-relative keys of a manifest's supporting files (everything but
/// `SKILL.md`). Entries whose relative path is not a plain path are not
/// listed, since the `file` argument would reject them.
fn listable_files(skill: &Skill, files: &[SkillResource]) -> Vec<String> {
    let Some(root) = skill.uri.strip_suffix("SKILL.md") else {
        return Vec::new();
    };
    let mut keys: Vec<String> = files
        .iter()
        .filter(|file| file.uri != skill.uri)
        .filter_map(|file| file.uri.strip_prefix(root))
        .filter(|rel| normalize_file_path(rel).is_ok())
        .map(str::to_string)
        .collect();
    keys.sort();
    keys
}

/// Validate and normalize a skill-relative `file` argument. Applies to every
/// skill kind and runs before anything is read or requested.
///
/// Rejected: empty, absolute or scheme-qualified paths; queries, fragments
/// and backslashes; empty, `.` and `..` segments, also percent-encoded;
/// encoded separators (`%2f`, `%5c`) and control characters.
pub fn normalize_file_path(file: &str) -> Result<String, String> {
    let bad = |why: &str| Err(format!("invalid file path '{file}': {why}"));
    if file.is_empty() {
        return bad("empty");
    }
    if file.starts_with('/') {
        return bad("absolute paths are not allowed");
    }
    if file.contains(['?', '#']) {
        return bad("queries and fragments are not allowed");
    }
    if file.contains('\\') {
        return bad("backslashes are not allowed");
    }
    if file.chars().any(char::is_control) {
        return bad("control characters are not allowed");
    }
    for (index, segment) in file.split('/').enumerate() {
        if segment.is_empty() {
            return bad("empty segments are not allowed");
        }
        if index == 0 && segment.contains(':') {
            return bad("scheme-qualified or drive paths are not allowed");
        }
        let decoded = percent_decode(segment);
        if decoded == "." || decoded == ".." {
            return bad("'.' and '..' segments are not allowed");
        }
        if decoded.contains(['/', '\\', '?', '#']) || decoded.chars().any(char::is_control) {
            return bad("encoded separators are not allowed");
        }
        if index == 0 && decoded.contains(':') {
            return bad("scheme-qualified or drive paths are not allowed");
        }
    }
    Ok(file.to_string())
}

/// Decode `%XX` escapes (invalid escapes stay as written); invalid UTF-8 is
/// replaced, which can only make a check stricter.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read a supporting file of an MCP skill.
///
/// `path` is already normalized. For a file manifest only listed files are
/// readable: that is decided from the listed entry before any request, then
/// the current entry is re-checked and the bytes are verified against it. A
/// dynamic skill resolves the path under its root and reads it unverified,
/// bounded by [`MCP_SKILL_MAX_BYTES`].
pub async fn load_mcp_file(skill: &McpSkill, path: &str) -> Result<String, String> {
    let id = skill.callable_id();
    let root = skill
        .skill
        .uri
        .strip_suffix("SKILL.md")
        .ok_or_else(|| rejected(&id, "its URI does not end in SKILL.md"))?
        .to_string();
    let uri = format!("{root}{path}");

    let bytes = match &skill.skill.resources {
        SkillResources::Files(files) => {
            if !listable_files(&skill.skill, files)
                .iter()
                .any(|key| key == path)
            {
                return Err(rejected(
                    &id,
                    format!("file '{path}' is not listed in its manifest"),
                ));
            }
            let current = fetch_current_entry(skill, &id).await?;
            check_entry(&skill.skill, &current)
                .map_err(|error| rejected(&id, format!("integrity error: {error}")))?;
            let SkillResources::Files(current_files) = &current.resources else {
                return Err(rejected(
                    &id,
                    "integrity error: the manifest changed to dynamic",
                ));
            };
            let entry = current_files
                .iter()
                .find(|file| file.uri == uri)
                .ok_or_else(|| {
                    rejected(
                        &id,
                        format!("file '{path}' is no longer listed in its manifest"),
                    )
                })?;
            if entry.size > MCP_SKILL_MAX_BYTES {
                return Err(rejected(
                    &id,
                    format!(
                        "file '{path}' is declared as {} bytes, over the {MCP_SKILL_MAX_BYTES} byte limit",
                        entry.size
                    ),
                ));
            }
            let bytes = read_uri_bytes(&skill.peer, &uri)
                .await
                .map_err(|error| rejected(&id, error))?;
            entry
                .verify(&bytes)
                .map_err(|error| rejected(&id, format!("integrity error: {error}")))?;
            bytes
        }
        SkillResources::Dynamic => read_uri_bytes(&skill.peer, &uri)
            .await
            .map_err(|error| rejected(&id, error))?,
        _ => {
            return Err(rejected(
                &id,
                "integrity error: unsupported resources declaration",
            ));
        }
    };
    String::from_utf8(bytes).map_err(|_| rejected(&id, format!("file '{path}' is not valid UTF-8")))
}

/// Load one MCP skill: fetch and re-check its current entry, read and verify
/// its `SKILL.md`, parse it and require the listed name. Every failure names
/// the skill and returns no content.
pub async fn load_mcp_skill(skill: &McpSkill) -> Result<LoadedMcpSkill, String> {
    let id = skill.callable_id();
    let current = fetch_current_entry(skill, &id).await?;
    check_entry(&skill.skill, &current)
        .map_err(|error| rejected(&id, format!("integrity error: {error}")))?;

    let entry = match &current.resources {
        SkillResources::Files(files) => {
            let entry = files
                .iter()
                .find(|file| file.uri == current.uri)
                .ok_or_else(|| {
                    rejected(&id, "integrity error: the manifest has no SKILL.md entry")
                })?;
            if entry.size > MCP_SKILL_MAX_BYTES {
                return Err(rejected(
                    &id,
                    format!(
                        "SKILL.md is declared as {} bytes, over the {MCP_SKILL_MAX_BYTES} byte limit",
                        entry.size
                    ),
                ));
            }
            Some(entry)
        }
        SkillResources::Dynamic => None,
        _ => {
            return Err(rejected(
                &id,
                "integrity error: unsupported resources declaration",
            ));
        }
    };

    let bytes = read_uri_bytes(&skill.peer, &current.uri)
        .await
        .map_err(|error| rejected(&id, error))?;
    if let Some(entry) = entry {
        entry
            .verify(&bytes)
            .map_err(|error| rejected(&id, format!("integrity error: {error}")))?;
    }
    let text =
        String::from_utf8(bytes).map_err(|_| rejected(&id, "SKILL.md is not valid UTF-8"))?;
    let (metadata, body) = crate::skills::parser::parse_skill_str(&text, &current.uri)
        .map_err(|error| rejected(&id, format!("{error:#}")))?;
    if metadata.name != skill.name() {
        return Err(rejected(
            &id,
            format!(
                "SKILL.md names itself '{}', the server listed '{}'",
                metadata.name,
                skill.name()
            ),
        ));
    }
    let files = match &current.resources {
        SkillResources::Files(files) => Some(listable_files(&current, files)),
        _ => None,
    };
    Ok(LoadedMcpSkill {
        description: sanitize_description(&metadata.description),
        body,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_plain_relative_paths() {
        for ok in ["a.md", "refs/guide.md", "a-b_c/d.txt", "100%.txt"] {
            assert_eq!(normalize_file_path(ok).as_deref(), Ok(ok), "{ok}");
        }
    }

    #[test]
    fn normalize_rejects_unsafe_paths() {
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../x",
            "./x",
            "a/./x",
            "a/..",
            "a//b",
            "a/",
            "C:/x",
            "file:x",
            "x:y/z",
            "a?b",
            "a#b",
            "a\\b",
            "%2e%2e/x",
            "%2E./x",
            ".%2e/x",
            "a/%2e%2E/x",
            "a%2fb",
            "a%2Fb",
            "a%5cb",
            "a%3fb",
            "%2fetc",
            "a\nb",
        ] {
            assert!(
                normalize_file_path(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn stalled_skill_listing_times_out_instead_of_blocking() {
        use crate::test_utils::mcp_harness::{HarnessOptions, McpHarness, ScriptedSkill};
        let h = McpHarness::start(HarnessOptions {
            skills: Some(vec![ScriptedSkill::new("a", "A")]),
            ..HarnessOptions::default()
        })
        .await;
        let peer = h.running.peer().clone();
        let timeout = std::time::Duration::from_millis(100);
        assert!(
            discover_with_timeout("harness", &peer, timeout)
                .await
                .is_some()
        );

        h.set_stall_list(true);
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            discover_with_timeout("harness", &peer, timeout),
        )
        .await
        .expect("discovery must be bounded by its own timeout");
        assert!(outcome.is_none(), "a stalled server contributes no skills");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn sanitize_collapses_whitespace_and_strips_controls() {
        assert_eq!(sanitize_description("  a\n\n b\t\tc\u{7}d  "), "a b cd");
        assert_eq!(sanitize_description(""), "");
    }

    #[test]
    fn sanitize_truncates_to_the_limit() {
        let long = "word ".repeat(500);
        let out = sanitize_description(&long);
        assert_eq!(out.chars().count(), MCP_SKILL_DESCRIPTION_MAX);
        assert!(out.ends_with('…'));
        assert!(!out.contains('\n'));
    }
}
