# QueryMT Agent - Slash Commands

Slash commands provide a powerful way to extend QueryMT Agent with custom functionality. Similar to Claude Code's slash commands, they allow you to define reusable prompts and workflows accessible via `/command-name` syntax.

## Overview

Slash commands provide:

- **Built-in commands** for common operations (help, mcp, clear, exit)
- **Custom commands** defined in markdown files
- **Fuzzy tab completion** using nucleo-matcher for fast command lookup
- **Argument substitution** for dynamic prompts with `$ARGUMENTS`
- **Per-command model override**: Use different models for different commands
- **Tool restrictions**: Limit which tools a command can use
- **Team sharing**: Share commands via version control
- **User commands**: Personal commands in `~/.qmt/commands/`
- **Project commands**: Team commands in `.qmt/commands/`

### Use Cases

| Use Case | Command | Description |
|----------|---------|-------------|
| **Code optimization** | `/optimize` | Analyze and optimize code |
| **Code review** | `/review` | Review code changes |
| **Testing** | `/test` | Generate or run tests |
| **Documentation** | `/docs` | Generate documentation |
| **Refactoring** | `/refactor` | Refactor code with specific patterns |
| **Debugging** | `/debug` | Debug issues with specific approach |
| **Git operations** | `/commit` | Create semantic commit messages |
| **Deployment** | `/deploy` | Deployment workflow |

## Quick Start

### Using Slash Commands

1. **Type `/`** in the prompt to see available commands
2. **Start typing** to filter commands (fuzzy matching)
3. **Press Tab** to autocomplete
4. **Press Enter** to execute

```bash
# Example usage
> /optimize src/main.rs
> /review HEAD
> /test --coverage
> /docs README.md
```

### Tab Completion

Slash commands support fuzzy tab completion:

```bash
# Type partial command
> /opt

# Press Tab to complete
> /optimize

# With arguments
> /optimize src/[Tab]  # Shows file completions
> /optimize src/main.rs
```

### Creating Your First Command

1. **Create commands directory**:
   ```bash
   mkdir -p ~/.qmt/commands
   ```

2. **Create a command file**:
   ```bash
   cat > ~/.qmt/commands/optimize.md << 'EOF'
   ---
   description: "Analyze and optimize code for performance"
   argument-hint: "<file-path>"
   ---
   
   Please analyze and optimize the code in $ARGUMENTS for:
   - Performance improvements
   - Memory efficiency
   - Algorithmic complexity
   - Code readability
   
   Provide specific, actionable suggestions.
   EOF
   ```

3. **Use the command**:
   ```bash
   cargo run --example qmtcode
   > /optimize src/main.rs
   ```

## Built-in Commands

QueryMT includes several built-in commands:

### `/help [command]`

Show available slash commands or detailed help for a specific command.

```bash
# List all commands
> /help

# Show help for specific command
> /help mcp
> /help optimize
```

**Output format:**
```
Available commands:

Built-in:
  /help [command]    Show help information
  /mcp <subcommand>  Manage MCP servers
  /clear             Clear screen
  /exit              Exit application

User (~/.qmt/commands/):
  /optimize <file>   Optimize code for performance
  /review [target]   Review code changes

Project (.qmt/commands/):
  /test [options]    Run project tests
  /deploy [env]      Deploy to environment
```

### `/mcp <subcommand>`

Interact with the MCP (Model Context Protocol) server registry.

#### `/mcp list [--no-cache] [--limit N]`

List available MCP servers:

```bash
> /mcp list

# Skip cache, fetch fresh list
> /mcp list --no-cache

# Limit results
> /mcp list --limit 10
```

**Output:**
```
Available MCP servers:

1. filesystem (v1.2.0)
   Local filesystem access
   Install: npx -y @modelcontextprotocol/server-filesystem

2. github (v2.0.1)
   GitHub API integration
   Install: npx -y @modelcontextprotocol/server-github

3. postgres (v1.0.0)
   PostgreSQL database access
   Install: npx -y @modelcontextprotocol/server-postgres
```

#### `/mcp search <query>`

Search for MCP servers by keyword:

```bash
> /mcp search database
> /mcp search file
> /mcp search github
```

#### `/mcp info <server-id> [version]`

Show detailed information about an MCP server:

```bash
> /mcp info filesystem
> /mcp info github v2.0.1
```

**Output:**
```
MCP Server: filesystem

Version: 1.2.0
Description: Local filesystem access
Author: Anthropic
License: MIT

Installation:
  npx -y @modelcontextprotocol/server-filesystem [root]

Tools provided:
  - read_file: Read file contents
  - write_file: Write file contents
  - list_directory: List directory contents
  - create_directory: Create directory
  - move_file: Move/rename file

Configuration:
  root: Root directory to expose (required)
```

#### `/mcp add <server-id> [version]`

Add an MCP server to your configuration:

```bash
> /mcp add filesystem
> /mcp add github v2.0.1
```

**Behavior:**
- Adds `[[mcp]]` entry to your config file
- Prompts for required configuration
- Validates configuration

### `/clear`

Clear the terminal screen:

```bash
> /clear
```

### `/exit`

Exit the application (alternative to Ctrl+D or typing "exit"):

```bash
> /exit
```

## Custom Commands

### Creating Custom Commands

Custom commands are defined as markdown files in two locations:

#### 1. User-level (Global)

**Location**: `~/.qmt/commands/`

- Available in all projects
- Personal commands
- Not shared with team
- Gitignored by default

```bash
~/.qmt/commands/
├── optimize.md
├── review.md
├── explain.md
└── commit.md
```

#### 2. Project-level (Team-shared)

**Location**: `.qmt/commands/`

- Project-specific commands
- Shared via version control
- Team workflows
- Committed to repository

```bash
project/
├── .qmt/
│   └── commands/
│       ├── test.md
│       ├── deploy.md
│       └── lint.md
└── src/
```

**Add to version control:**
```bash
git add .qmt/commands/
git commit -m "Add team slash commands"
```

### Command Naming

The filename determines the command name:

| Filename | Command | Arguments |
|----------|---------|-----------|
| `optimize.md` | `/optimize` | Yes |
| `review.md` | `/review` | Yes |
| `test.md` | `/test` | Yes |
| `deploy.md` | `/deploy` | Yes |
| `help-me.md` | `/help-me` | Yes |

**Naming rules:**
- Filename (without `.md`) becomes command name
- Hyphens allowed: `code-review.md` → `/code-review`
- Case-insensitive matching
- No spaces in filenames

### Command Discovery

Commands are discovered at startup:

1. **User commands**: `~/.qmt/commands/*.md`
2. **Project commands**: `.qmt/commands/*.md`
3. **Merged**: Project commands override user commands with same name

## Command Format

### Basic Structure

Commands are markdown files with optional YAML frontmatter:

```markdown
---
<frontmatter>
---

<prompt content>
```

### Frontmatter Fields

```yaml
---
description: "Brief description of what the command does"
argument-hint: "<arg1> [arg2]"
tags: ["coding", "review"]
---
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `description` | string | Yes | Brief description shown in help and completion |
| `argument-hint` | string | No | Hint for arguments (e.g., `"<file> [options]"`) |
| `tags` | string[] | No | Tags for filtering and organization |
| `requires-script` | bool | No | Whether command requires script execution (default: false) |
| `script` | object | No | Script definition (for script-backed commands) |

Unknown fields are accepted but ignored by the system.

#### `description`

Brief description shown in command list and help:

```yaml
description: "Analyze and optimize code for performance"
```

**Usage:**
- Required field
- Shown in `/help` output
- Used in ACP command advertising
- Should be concise (< 80 characters)

#### `argument-hint`

Hint for expected arguments:

```yaml
argument-hint: "<file-path> [--fast]"
```

**Format:**
- `<required>` - Required argument
- `[optional]` - Optional argument
- `...` - Variadic arguments

**Examples:**
```yaml
argument-hint: "<file>"                    # Single required file
argument-hint: "[target]"                  # Optional target
argument-hint: "<file> [options...]"       # File with optional options
argument-hint: "<source> <destination>"    # Two required args
```

#### `tags`

Tags for filtering and organization:

```yaml
tags: ["coding", "optimization", "performance"]
```

**Usage:**
- Used for categorization and display
- Optional field

### Prompt Content

The main content of the command is the prompt template:

```markdown
---
description: "Optimize code"
argument-hint: "<file>"
---

Please analyze and optimize the code in $ARGUMENTS.

Focus on:
- Performance improvements
- Memory efficiency
- Readability

Provide specific suggestions with code examples.
```

#### Argument Substitution

Use `$ARGUMENTS` in prompts to insert the text typed after the command name:

| Variable | Description |
|----------|-------------|
| `$ARGUMENTS` | Everything typed after the command name |

**Example:**

Command: `/compare file1.rs file2.rs`

```markdown
---
description: "Compare two files"
argument-hint: "<file1> <file2>"
---

Compare the following files and highlight differences: $ARGUMENTS

Focus on:
- Functional differences
- Performance implications
- Code quality
```

The expansion wraps the command template with context so the model understands it was invoked as a slash command, including the description and instructions.

#### Multi-line Prompts

Commands can have complex, multi-line prompts:

```markdown
---
description: "Generate comprehensive tests"
argument-hint: "<source-file>"
---

You are a testing expert. Analyze the code in $ARGUMENTS and generate comprehensive tests.

## Requirements

1. **Unit tests**: Test individual functions
2. **Integration tests**: Test component interactions
3. **Edge cases**: Test boundary conditions
4. **Error handling**: Test error scenarios

## Output Format

- Use the project's testing framework
- Include setup and teardown
- Add descriptive test names
- Include comments explaining test strategy

## Additional Context

Current directory: $PWD
Project type: Rust (Cargo)
```

## Advanced Features

### Conditional Logic (via LLM)

Commands can instruct the LLM to handle conditions:

```markdown
---
description: "Smart commit"
---

Analyze the current git changes and create an appropriate commit.

## Steps

1. Run `git status` to see changes
2. Run `git diff` to review changes
3. Determine commit type:
   - feat: New feature
   - fix: Bug fix
   - docs: Documentation
   - style: Formatting
   - refactor: Code refactoring
   - test: Adding tests
   - chore: Maintenance
4. Create descriptive commit message
5. Stage and commit changes

If no changes are found, inform the user.
```

### Command Composition

Commands can reference other commands:

```markdown
---
description: "Full development workflow"
---

Run the following workflow:

1. First, review the code: Use /review or read the files
2. Then, run tests: Use /test or run `cargo test`
3. Finally, optimize: Use /optimize if needed

Provide a summary of each step.
```

### Tool-Specific Commands

Create commands that focus on specific tools:

```markdown
---
description: "Search and replace across project"
argument-hint: "<search> <replace>"
---

Search and replace across the project: $ARGUMENTS.

## Steps

1. Use search_text to find all occurrences
2. List affected files
3. Ask for confirmation
4. Use edit or multiedit to make replacements
5. Show summary of changes
```

## Examples

### Example 1: Code Optimization

**File**: `~/.qmt/commands/optimize.md`

```markdown
---
description: "Analyze and optimize code for performance"
argument-hint: "<file-path>"
---

Please analyze and optimize the code in $ARGUMENTS for:

## Analysis Areas

1. **Performance**
   - Algorithmic complexity (Big O)
   - Unnecessary allocations
   - Cache efficiency
   - Parallelization opportunities

2. **Memory**
   - Stack vs heap usage
   - Memory leaks
   - Unnecessary copies
   - Borrow checker optimizations

3. **Readability**
   - Clear variable names
   - Appropriate comments
   - Function decomposition
   - Idiomatic patterns

## Output Format

For each issue found:
- **Issue**: Description
- **Impact**: Performance/memory impact
- **Solution**: Specific code change
- **Example**: Before/after code

Prioritize issues by impact.
```

### Example 2: Code Review

**File**: `.qmt/commands/review.md`

```markdown
---
description: "Review code changes (git diff or specific files)"
argument-hint: "[target]"
---

Review the code changes: $ARGUMENTS

## Review Checklist

### Correctness
- [ ] Logic errors
- [ ] Edge cases handled
- [ ] Error handling complete
- [ ] No race conditions

### Security
- [ ] Input validation
- [ ] No SQL injection
- [ ] No XSS vulnerabilities
- [ ] Secrets not exposed

### Performance
- [ ] No N+1 queries
- [ ] Efficient algorithms
- [ ] Proper caching
- [ ] No memory leaks

### Maintainability
- [ ] Clear naming
- [ ] Appropriate comments
- [ ] DRY principle
- [ ] SOLID principles

### Testing
- [ ] Tests included
- [ ] Edge cases tested
- [ ] Integration tests
- [ ] Documentation updated

## Output Format

Group findings by severity:
1. **Critical**: Must fix before merge
2. **Important**: Should fix
3. **Minor**: Consider fixing
4. **Suggestions**: Optional improvements

For each finding:
- File and line number
- Issue description
- Suggested fix
- Example code (if applicable)
```

### Example 3: Test Generation

**File**: `~/.qmt/commands/test.md`

```markdown
---
description: "Generate comprehensive tests for code"
argument-hint: "<source-file>"
---

Generate comprehensive tests for the code in $ARGUMENTS.

## Test Strategy

1. **Unit Tests**
   - Test each public function
   - Test return values
   - Test error conditions
   - Test edge cases

2. **Integration Tests**
   - Test component interactions
   - Test with real dependencies
   - Test end-to-end flows

3. **Property-Based Tests**
   - Invariant testing
   - Fuzzing where appropriate
   - Random input generation

## Requirements

- Use the project's testing framework (cargo test for Rust)
- Include descriptive test names
- Add setup/teardown as needed
- Include comments explaining test strategy
- Aim for high coverage of critical paths

## Output

Generate complete, runnable test code.
```

### Example 4: Documentation Generation

**File**: `.qmt/commands/docs.md`

```markdown
---
description: "Generate documentation for code"
argument-hint: "<file-or-module>"
---

Generate comprehensive documentation for $ARGUMENTS.

## Documentation Types

1. **Module-level docs**
   - Purpose and overview
   - Usage examples
   - Feature description

2. **Function docs**
   - Description
   - Parameters
   - Return values
   - Errors
   - Examples
   - Safety notes (if unsafe)

3. **Type docs**
   - Description
   - Fields/members
   - Usage examples
   - Invariants

## Style Guide

- Use rustdoc format
- Include code examples
- Add links to related items
- Keep descriptions concise
- Include safety sections for unsafe code

Generate complete documentation comments.
```

### Example 5: Git Commit

**File**: `~/.qmt/commands/commit.md`

```markdown
---
description: "Create semantic commit message and commit"
---

Create a semantic commit for the current changes.

## Steps

1. Run `git status` to see changes
2. Run `git diff --cached` for staged changes
3. If no staged changes, run `git diff` and suggest staging
4. Analyze changes and determine commit type:
   - **feat**: New feature
   - **fix**: Bug fix
   - **docs**: Documentation only
   - **style**: Formatting, missing semicolons, etc.
   - **refactor**: Code change that neither fixes a bug nor adds a feature
   - **test**: Adding missing tests
   - **chore**: Updating build tasks, configs, etc.
   - **perf**: Performance improvement
   - **ci**: CI/CD changes
   - **build**: Build system changes

5. Generate commit message following Conventional Commits:

       <type>[optional scope]: <description>

       [optional body]

       [optional footer(s)]

6. Ask for confirmation before committing
7. Create the commit

## Rules

- Subject line < 72 characters
- Use imperative mood ("Add feature" not "Added feature")
- Body explains what and why, not how
- Reference issues if applicable
```

### Example 6: Project-Specific Deployment

**File**: `.qmt/commands/deploy.md`

```markdown
---
description: "Deploy to specified environment"
argument-hint: "[staging|production]"
---

Deploy to ${1:-staging} environment.

## Pre-deployment Checks

1. Run tests: `cargo test`
2. Check formatting: `cargo fmt --check`
3. Run linter: `cargo clippy`
4. Build release: `cargo build --release`

## Deployment Steps

### Staging

1. Build Docker image
2. Push to registry
3. Update staging deployment
4. Run smoke tests
5. Notify team

### Production

1. **Confirm**: Ask for explicit confirmation
2. Create release tag
3. Build Docker image
4. Push to registry
5. Update production deployment
6. Run smoke tests
7. Monitor for errors
8. Notify team

## Rollback Plan

If issues detected:
1. Identify problem
2. Rollback to previous version
3. Notify team
4. Create incident report

Proceed with deployment to ${1:-staging}? Ask for confirmation.
```

## Best Practices

### Command Design

1. **Single responsibility**: Each command does one thing well
2. **Clear descriptions**: Help users understand what command does
3. **Argument hints**: Show expected arguments
4. **Appropriate tools**: Only enable necessary tools
5. **Model selection**: Use appropriate model for task complexity

### Prompt Engineering

1. **Be specific**: Clear instructions produce better results
2. **Provide context**: Include relevant information
3. **Structure output**: Request specific format
4. **Include examples**: Show desired output
5. **Handle errors**: Instruct how to handle failures

### Team Commands

1. **Version control**: Commit `.qmt/commands/` to repository
2. **Documentation**: Include README explaining commands
3. **Naming conventions**: Use consistent naming
4. **Review process**: Review command changes like code
5. **Testing**: Test commands before committing

### User Commands

1. **Personal workflow**: Customize for your workflow
2. **Keep updated**: Update commands as needs change
3. **Share useful commands**: Consider adding to project
4. **Backup**: Include in dotfiles backup

### Security

1. **No secrets**: Don't include API keys in commands
2. **Input validation**: Instruct LLM to validate inputs
3. **Audit commands**: Review commands from others
4. **Review templates**: Check command templates before sharing

## Troubleshooting

### Command Not Found

**Symptoms:** `/command-name` not recognized

**Solutions:**
1. Check file exists in `~/.qmt/commands/` or `.qmt/commands/`
2. Verify filename ends with `.md`
3. Check filename has no spaces
4. Restart application to reload commands

### Tab Completion Not Working

**Symptoms:** Tab doesn't complete commands

**Solutions:**
1. Ensure you're in interactive mode
2. Type `/` first to enter command mode
3. Check terminal supports completion
4. Try partial match and press Tab

### Arguments Not Substituted

**Symptoms:** `$ARGUMENTS` appears literally in prompt

**Solutions:**
1. Check you provided arguments after command (e.g., `/command some args`)
2. Verify you used `$ARGUMENTS` in the command template
3. Check frontmatter format is correct (YAML between `---` delimiters)

### Command Execution Failed

**Symptoms:** Error when running command

**Solutions:**
1. Check TOML frontmatter syntax
2. Verify model name is correct
3. Ensure tools are available
4. Check for special characters in prompt

### Commands Not Shared with Team

**Symptoms:** Team can't see your commands

**Solutions:**
1. Move to `.qmt/commands/` (project directory)
2. Commit and push to version control
3. Ensure `.qmt/` is not gitignored
4. Pull latest changes from team

## Related Documentation

- [Configuration Guide](configuration.md) - Agent configuration
- [Profiles](profiles.md) - Profile-specific commands
- [Examples](examples.md) - Command examples
- [API Reference](api_reference.md) - Programmatic command creation
