# QueryMT Agent - Profiles

Profiles allow you to save, manage, and switch between different agent configurations. They enable different workflows, model preferences, tool sets, and behaviors for different tasks.

## Overview

QueryMT profiles provide:

- **Multiple configurations**: Switch between different agent setups instantly
- **Task-specific settings**: Different profiles for coding, reviewing, planning, etc.
- **Team collaboration**: Share profiles across team members via version control
- **Environment-specific configs**: Separate profiles for different projects or contexts
- **Session persistence**: Sessions are bound to the profile that created them

### Use Cases

| Use Case | Profile Example | Description |
|----------|-----------------|-------------|
| **Coding** | `coder` | Full read/write access, development tools |
| **Code Review** | `reviewer` | Read-only, review-focused prompts |
| **Planning** | `planner` | Read-only, planning and analysis |
| **Documentation** | `docs-writer` | Documentation-focused tools and prompts |
| **Debugging** | `debugger` | Debugging tools, logging, diagnostics |
| **Quick queries** | `assistant` | Minimal tools, general assistance |
| **Team lead** | `multi-agent` | Multi-agent with delegation |
| **GPU tasks** | `gpu-coder` | Remote GPU node, fast models |

## Quick Start

### List Available Profiles

```bash
# List all profiles
cargo run --example qmtcode --list-profiles

# Output:
# Available profiles:
#   default      - Default single coder profile
#   coder        - Full-featured coding agent
#   reviewer     - Code review specialist
#   planner      - Planning and analysis mode
```

### Use a Specific Profile

```bash
# Use profile by ID
cargo run --example qmtcode --profile coder

# Use profile with custom profiles directory
cargo run --example qmtcode --profiles-dir ./my-profiles --profile custom

# Use profile with dashboard
cargo run --example qmtcode --features dashboard --profile coder --dashboard
```

### Switch Profiles

In dashboard mode, use `Ctrl+X p` to open the profile switcher. This allows you to switch between available profiles without restarting.

Alternatively, specify the profile when starting:

```bash
cargo run --example qmtcode --profile reviewer
```

## Profile Structure

### Basic Profile Format

Profiles are TOML files with optional metadata:

```toml
# Optional profile metadata (for local catalogs)
[profile]
id = "coder"
name = "Full-Featured Coder"
description = "Complete coding agent with all tools enabled"
tags = ["coding", "development", "full-access"]

# Agent configuration (same as regular config)
[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = [
    "read_tool", "edit", "write_file", "multiedit",
    "shell", "glob", "search_text", "index",
    "get_symbol", "get_function", "replace_symbol",
    "create_task", "todowrite", "todoread",
    "question"
]
system = [
    { file = "prompts/coder_system.txt" },
    { file = "prompts/code_meta.jinja2" ]
]

# Middleware stack
[[middleware]]
type = "agent_mode"
default = "build"

[[middleware]]
type = "limits"
max_steps = 200
max_turns = 50

[[middleware]]
type = "context"
warn_at_percent = 80
compact_at_percent = 90
```

### Profile Metadata Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | string | No* | Unique identifier (auto-generated from filename if absent) |
| `name` | string | No* | Human-readable name (defaults to id if absent) |
| `description` | string | No | Description shown in profile list |
| `tags` | string[] | No | Tags for filtering and organization |

*Auto-generated from filename if not specified.

### Profile Configuration Kinds

Profiles can define two types of agents:

1. **Single Agent** (default):
   ```toml
   [agent]
   provider = "anthropic"
   model = "claude-sonnet-4-5-20250929"
   # ... agent settings
   ```

2. **Multi-Agent (Quorum)**:
   ```toml
   [quorum]
   cwd = "."
   
   [planner]
   provider = "anthropic"
   model = "claude-sonnet-4-5-20250929"
   # ... planner settings
   
   [[delegates]]
   id = "coder"
   provider = "anthropic"
   model = "claude-sonnet-4-5-20250929"
   # ... delegate settings
   ```

## Profile Catalogs

Profile catalogs manage collections of profiles from different sources.

### Catalog Types

#### 1. Local Catalog (Filesystem)

Load profiles from a local directory:

```bash
# Use profiles from specific directory
cargo run --example qmtcode --profiles-dir ./my-profiles --profile coder

# Default location: ~/.qmt/profiles/
cargo run --example qmtcode --profile coder
```

**Directory structure:**
```
~/.qmt/profiles/
├── coder.toml
├── reviewer.toml
├── planner.toml
└── docs-writer.toml
```

#### 2. Embedded Catalog

Built-in profiles shipped with QueryMT:

```bash
# Use embedded profile
cargo run --example qmtcode --profile default
```

**Available embedded profiles:**
- `default` - Default single coder configuration
- `coder_delegate` - Coder with delegation support

### Catalog Resolution Order

When searching for a profile:

1. **Explicit `--profiles-dir`**: Search specified directory first
2. **Default local catalog**: `~/.qmt/profiles/`
3. **Embedded catalog**: Built-in profiles

### Profile Name Resolution

Profile names are resolved as:

1. **Exact match**: Profile ID matches exactly
2. **Filename match**: Profile filename (without `.toml`) matches
3. **Case-insensitive**: Case is ignored during matching

## Using Profiles

### CLI Usage

#### List All Profiles

```bash
cargo run --example qmtcode --list-profiles

# With custom directory
cargo run --example qmtcode --profiles-dir ./my-profiles --list-profiles
```

**Output format:**
```
Available profiles:

  default (Default)
    Default single coder profile
    Tags: coding, single-agent
    Source: embedded

  coder (Full-Featured Coder)
    Complete coding agent with all tools enabled
    Tags: coding, development, full-access
    Source: local (~/.qmt/profiles/coder.toml)

  reviewer (Code Review Specialist)
    Read-only code review agent
    Tags: review, read-only
    Source: local (~/.qmt/profiles/reviewer.toml)
```

#### Use Profile with Specific Mode

```bash
# Use profile with ACP mode
cargo run --example qmtcode --profile coder --acp

# Use profile with API mode
cargo run --example qmtcode --profile coder --api=0.0.0.0:8080

# Use profile with dashboard
cargo run --example qmtcode --features dashboard --profile coder --dashboard

# Use profile with mesh
cargo run --example qmtcode --features remote --profile coder --mesh
```

### Programmatic Usage

```rust
use querymt_agent::prelude::*;
use querymt_agent::profiles::{LocalProfileCatalog, ProfileCatalog};

// Load profile catalog from directory
let catalog = LocalProfileCatalog::new("~/.qmt/profiles")?;

// List available profiles
let profiles = catalog.list_profiles().await?;
for profile in profiles {
    println!("{}: {}", profile.id, profile.name);
}

// Load specific profile
let profile_doc = catalog.load_profile("coder").await?;

// Use profile's agent configuration
let agent = from_config(ConfigSource::Toml(
    toml::to_string(&profile_doc.config)?
)).await?;
```

### Dashboard Profile Switching

Profiles can be selected when starting the dashboard. The profile determines the agent configuration for all sessions created in that dashboard session.

**Session behavior:**
- New sessions use the current profile
- Existing sessions retain their original profile
- To use a different profile, restart with `--profile <id>`

## Creating Profiles

### Basic Profile Creation

1. **Create profile file**:
   ```bash
   mkdir -p ~/.qmt/profiles
   nano ~/.qmt/profiles/my-profile.toml
   ```

2. **Add profile content**:
   ```toml
   [profile]
   id = "my-profile"
   name = "My Custom Profile"
   description = "Customized for my workflow"
   tags = ["custom", "personal"]

   [agent]
   provider = "anthropic"
   model = "claude-sonnet-4-5-20250929"
   tools = ["read_tool", "edit", "write_file", "shell"]
   system = "You are a helpful coding assistant."
   ```

3. **Test the profile**:
   ```bash
   cargo run --example qmtcode --profile my-profile
   ```

### Profile Templates

#### Minimal Profile

```toml
[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["read_tool", "edit", "write_file", "shell"]
```

#### Coder Profile

```toml
[profile]
id = "coder"
name = "Coder"
description = "Full-featured coding agent"
tags = ["coding", "development"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
assume_mutating = false
mutating_tools = ["edit", "multiedit", "write_file", "shell", "replace_symbol"]
tools = [
    "edit", "read_tool", "write_file", "multiedit", "replace_symbol",
    "glob", "search_text", "ls", "index", "get_symbol", "get_function",
    "find_references", "shell", "create_task", "todowrite", "todoread",
    "question", "browse"
]
system = [
    { file = "prompts/coder_system.txt" },
    { file = "prompts/code_meta.jinja2" }
]

[agent.execution.snapshot]
backend = "git"

[[middleware]]
type = "agent_mode"
default = "build"

[[middleware]]
type = "limits"
max_steps = 200
max_turns = 50

[[middleware]]
type = "context"
warn_at_percent = 80
compact_at_percent = 90
fallback_max_tokens = 150000
```

#### Reviewer Profile (Read-Only)

```toml
[profile]
id = "reviewer"
name = "Code Reviewer"
description = "Read-only code review specialist"
tags = ["review", "read-only"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
assume_mutating = false
tools = [
    "read_tool", "glob", "search_text", "ls", "index",
    "get_symbol", "get_function", "find_references",
    "question"
]
system = """You are a code review specialist. Analyze code for:
- Correctness and logic errors
- Security vulnerabilities
- Performance issues
- Code style and maintainability
- Documentation quality

Provide constructive feedback with specific suggestions for improvement."""

[[middleware]]
type = "agent_mode"
default = "review"
```

#### Multi-Agent Profile

```toml
[profile]
id = "multi-agent"
name = "Multi-Agent Team"
description = "Planner with specialized delegates"
tags = ["multi-agent", "delegation"]

[quorum]
cwd = "."
delegation = { auto = true, require_approval = false, timeout_secs = 300 }

[planner]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["create_task", "todowrite", "todoread", "question"]
system = """You are a planning agent. Analyze tasks and delegate to specialists."""

[[delegates]]
id = "coder"
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
description = "Code implementation specialist"
tools = ["read_tool", "edit", "write_file", "shell", "glob", "search_text"]

[[delegates]]
id = "reviewer"
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
description = "Code review specialist"
tools = ["read_tool", "glob", "search_text", "question"]
```

#### Remote GPU Profile

```toml
[profile]
id = "gpu-coder"
name = "GPU Coder"
description = "Fast coding on GPU server"
tags = ["gpu", "fast", "remote"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["read_tool", "edit", "write_file", "shell", "glob", "search_text"]

[mesh]
enabled = true

[[mesh.peers]]
name = "gpu-server"
addr = "/ip4/192.168.1.100/tcp/9000"

[[delegates]]
id = "remote-coder"
provider = "anthropic"
model = "claude-sonnet-4"
peer = "gpu-server"
tools = ["edit", "write_file", "shell"]
```

### Advanced Profile Features

#### Environment Variable Interpolation

Use environment variables in profiles:

```toml
[agent]
provider = "${QMT_PROVIDER:anthropic}"  # Default: anthropic
model = "${QMT_MODEL:claude-sonnet-4-5-20250929}"
api_key = "${ANTHROPIC_API_KEY}"
```

#### File References

Reference external files for system prompts:

```toml
[agent]
system = [
    { file = "prompts/default_system.txt" },
    { file = "prompts/code_meta.jinja2" },
    "Additional inline instructions"
]
```

**File path resolution:**
- Relative to profile directory
- Supports `~` for home directory
- Supports `../` for parent directory

#### Template Variables

Use Jinja2 templates in system prompts:

```markdown
# prompts/coder_system.txt

You are a coding assistant specialized in {{ language }}.

Current model: {{ model }}
Working directory: {{ cwd }}

Focus on writing clean, maintainable code.
```

## Profile Features

### Session Binding

Sessions are automatically bound to the profile that created them:

```rust
// Session inherits profile from creation context
let session = agent.create_session().await?;

// Session metadata includes profile binding
let metadata = session.metadata();
println!("Profile: {:?}", metadata.profile_id);
```

**Binding behavior:**
- New sessions use current profile
- Session remembers its profile
- Resume uses original profile (if available)
- Fallback to active profile if original unavailable

### Profile Fingerprinting

Each profile configuration is fingerprinted for change detection:

```rust
let profile = catalog.load_profile("coder").await?;
println!("Fingerprint: {:?}", profile.metadata.fingerprint);
```

**Use cases:**
- Detect profile changes between sessions
- Invalidate caches when profile updates
- Track profile versions

### Profile Tags

Tags enable filtering and organization:

```toml
[profile]
tags = ["coding", "review", "production", "team-a"]
```

**Tag usage:**
- Filter profiles by tags in listings
- Group profiles by category
- Search profiles programmatically

### Profile Inheritance (Future)

Profile inheritance is planned but not yet implemented:

```toml
# Future feature
[profile]
inherits = "base-coder"  # Inherit from base profile

# Override specific settings
[agent]
model = "claude-opus-4-20250914"  # Use different model
```

## Examples

### Example 1: Development Workflow

Create profiles for different development stages:

**`~/.qmt/profiles/planner.toml`:**
```toml
[profile]
id = "planner"
name = "Planner"
tags = ["planning", "read-only"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["read_tool", "glob", "search_text", "create_task", "todowrite"]
system = "You are a planning agent. Analyze tasks and create implementation plans."
```

**`~/.qmt/profiles/coder.toml`:**
```toml
[profile]
id = "coder"
name = "Coder"
tags = ["coding", "implementation"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["read_tool", "edit", "write_file", "shell", "glob", "search_text"]
system = "You are a coding agent. Implement features based on plans."
```

**`~/.qmt/profiles/reviewer.toml`:**
```toml
[profile]
id = "reviewer"
name = "Reviewer"
tags = ["review", "quality"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["read_tool", "glob", "search_text", "question"]
system = "You are a code reviewer. Analyze code quality and suggest improvements."
```

**Workflow:**
```bash
# Plan the work
cargo run --example qmtcode --profile planner

# Implement the feature
cargo run --example qmtcode --profile coder

# Review the code
cargo run --example qmtcode --profile reviewer
```

### Example 2: Team Profiles

Share profiles via version control:

**`.qmt/profiles/team-coder.toml`:**
```toml
[profile]
id = "team-coder"
name = "Team Coder"
description = "Standard team coding profile"
tags = ["team", "standard"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = [
    "read_tool", "edit", "write_file", "shell",
    "glob", "search_text", "index", "get_symbol"
]
system = [
    { file = ".qmt/prompts/team_system.txt" },
    "Follow team coding standards and conventions."
]
```

**Commit to repository:**
```bash
git add .qmt/profiles/
git commit -m "Add team coding profiles"
```

### Example 3: Language-Specific Profiles

**`~/.qmt/profiles/rust-coder.toml`:**
```toml
[profile]
id = "rust-coder"
name = "Rust Coder"
tags = ["rust", "systems"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = [
    "read_tool", "edit", "write_file", "shell",
    "glob", "search_text", "index", "get_symbol", "get_function"
]
system = """You are a Rust systems programmer.

Focus on:
- Memory safety and ownership
- Performance optimization
- Idiomatic Rust patterns
- Proper error handling with Result/Option
- Documentation with doc comments
"""
```

**`~/.qmt/profiles/python-coder.toml`:**
```toml
[profile]
id = "python-coder"
name = "Python Coder"
tags = ["python", "data-science"]

[agent]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
tools = ["read_tool", "edit", "write_file", "shell", "glob", "search_text"]
system = """You are a Python developer.

Focus on:
- Clean, readable code
- Type hints (PEP 484)
- Docstrings (Google or NumPy style)
- Virtual environments and packaging
- Testing with pytest
"""
```

## Best Practices

### Profile Organization

1. **Use descriptive IDs**: `coder`, `reviewer`, not `profile1`, `profile2`
2. **Add descriptions**: Help team members understand profile purpose
3. **Use tags consistently**: Enable easy filtering
4. **Version control team profiles**: Store in `.qmt/profiles/`
5. **Keep personal profiles local**: Store in `~/.qmt/profiles/`

### Profile Configuration

1. **Start minimal**: Begin with few tools, add as needed
2. **Test thoroughly**: Verify profile works before sharing
3. **Document custom prompts**: Explain why certain prompts are used
4. **Use file references**: Keep system prompts in separate files
5. **Set appropriate limits**: Configure middleware for profile's use case

### Security Considerations

1. **Don't commit API keys**: Use environment variables
2. **Review shared profiles**: Check for sensitive information
3. **Use read-only for untrusted contexts**: Restrict tool access
4. **Audit profile sources**: Only use trusted profile directories

## Troubleshooting

### Profile Not Found

**Symptoms:** `Profile 'xyz' not found`

**Solutions:**
1. Check profile ID is correct
2. Verify file exists in profile directory
3. Check file has `.toml` extension
4. Ensure directory is in search path

### Profile Load Error

**Symptoms:** `Failed to load profile: ...`

**Solutions:**
1. Check TOML syntax is valid
2. Verify all referenced files exist
3. Check environment variables are set
4. Review error message for specific issue

### Profile Not Appearing in List

**Symptoms:** Profile doesn't show in `--list-profiles`

**Solutions:**
1. Ensure file has `.toml` extension
2. Check file is readable
3. Verify profile directory is correct
4. Look for syntax errors in profile

### Session Using Wrong Profile

**Symptoms:** Session uses unexpected profile

**Solutions:**
1. Check session metadata for profile binding
2. Verify profile wasn't changed mid-session
3. Restart session if profile binding is stale
4. Check profile catalog resolution order

## Related Documentation

- [Configuration Guide](configuration.md) - Base configuration options
- [Mesh Networking](mesh.md) - Remote profiles and mesh setup
- [Delegation](delegation.md) - Multi-agent profiles
- [Examples](examples.md) - Profile usage examples
