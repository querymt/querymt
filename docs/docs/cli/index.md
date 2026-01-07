# QueryMT Command-Line Interface (qmt)

The QueryMT Command-Line Interface (`qmt`) is a powerful and versatile tool for interacting with Large Language Models directly from your terminal. It leverages the full power of the QueryMT library, providing a unified interface for chat, text embedding, tool usage, and configuration management across various LLM providers.

## Key Features

-   **Interactive Chat:** A REPL-style interface for conversational AI.
-   **Piped & Single-Shot Commands:** Seamlessly integrate `qmt` into your shell scripts and workflows.
-   **Unified Provider Management:** Interact with any provider configured in your `providers.toml`, from OpenAI and Anthropic to local models via Ollama.
-   **Text Embeddings:** Generate vector embeddings for text, with support for multiple documents and custom dimensions.
-   **Secure Credential Storage:** Manage API keys and other secrets securely.
-   **Tool & Function Calling:** Connect LLMs to external tools, including those exposed via the Model Context Protocol (MCP).

## Configuration

`qmt` relies on two primary configuration sources, typically located in `~/.qmt/`:

1.  **Provider Configuration (`~/.qmt/providers.toml`):** This file defines all the available LLM provider plugins. You can specify the path to local Wasm or native plugins, or reference them from an OCI registry. This can be overridden with the `--provider-config` flag.
    *   *For more details, see [Plugin Configuration](../plugins/configuration.md).*

2.  **Secret Storage (`~/.qmt/secrets.json`):** This file stores sensitive information like API keys and the default provider setting. It is automatically created and managed by the `qmt` CLI and should not be edited manually.

## Core Commands

### Chatting with LLMs

`qmt` offers several ways to chat with your configured models.

#### Interactive Mode (REPL)

For a conversational experience, simply run `qmt` without any prompt.

```sh
$ qmt
qmt - Interactive Chat
Provider: openai
Type 'exit' to quit
──────────────────────────────────────────────────
:: What is the QueryMT project?
Thinking...
> Assistant: QueryMT is a versatile Rust library designed to provide a unified and extensible interface for interacting with various Large Language Models (LLMs).
──────────────────────────────────────────────────
:: exit
👋 Goodbye!
```

#### Single-Shot Prompt

Pass a prompt directly as an argument for a quick question and answer.

```sh
$ qmt "What is the capital of France?"

> Assistant: The capital of France is Paris.
```

#### Piped Input

Use standard shell pipes to provide context to the LLM. This is ideal for summarizing files, analyzing logs, or processing text and image content.

```sh
# Summarize a text file
$ cat report.txt | qmt "Please provide a 3-sentence summary of the following text."

# Analyze a log file
$ cat server.log | qmt "Are there any 'ERROR' level messages in this log?"

# Analyze an image's content (requires a provider that supports vision)
$ cat my_image.jpg | qmt "Describe what is happening in this image."
```

### Generating Embeddings

The `embed` subcommand generates vector embeddings. It can read from an argument or from stdin, and it outputs the results as a JSON array.

```sh
# Embed a single string
$ qmt embed "This is a test sentence."
[[
  -0.0189,
  0.0234,
  ...
  -0.0056
]]

# Embed a file's content
$ cat document.txt | qmt embed
```

You can also embed multiple documents at once by specifying a separator.

```sh
# Embed two documents separated by "---"
$ echo "First document.---Second document." | qmt embed --separator "---"
[
  [
    -0.01, ...
  ],
  [
    0.02, ...
  ]
]
```

### Managing Configuration & Secrets

`qmt` includes commands to manage your settings without manually editing files.

#### Setting the Default Provider

Set your most-used provider and model combination as the default.

```sh
# Set the default provider to OpenAI's gpt-4-turbo model
$ qmt default openai:gpt-4-turbo

# Check the current default
$ qmt default
Default provider: openai:gpt-4-turbo
```

#### OAuth Authentication

Some providers support OAuth authentication for enhanced security and automatic token management. The `qmt` CLI can handle the OAuth flow for you, including opening your browser and storing credentials securely.

```sh
# Login to Anthropic via OAuth (using Max mode by default)
$ qmt auth login anthropic
=== Anthropic OAuth Authentication ===

Starting OAuth flow for Anthropic...

🔐 Please visit this URL to authorize:
https://console.anthropic.com/oauth/authorize?...

✓ Browser opened automatically

# Login to Anthropic via OAuth (using Console mode for API key generation)
$ qmt auth login anthropic --mode console

# Login to OpenAI via OAuth
$ qmt auth login openai

# Check authentication status for all providers
$ qmt auth status
OAuth Authentication Status
===========================

anthropic: Valid ✓
  Access token expires: 2026-02-15 14:30:00 UTC
  Refresh token available

openai: Not authenticated
  Run 'qmt auth login openai' to authenticate

# Check status for a specific provider (with automatic refresh disabled)
$ qmt auth status anthropic --no-refresh

# Logout from a provider
$ qmt auth logout anthropic
✓ Logged out from anthropic
```

#### Storing API Keys

Securely store API keys and other secrets. The `key` should match the `api_key_name` defined by the provider plugin (e.g., `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`).

```sh
# Store your OpenAI API key
$ qmt secrets set OPENAI_API_KEY "sk-..."
✓ Secret 'OPENAI_API_KEY' has been set.

# Retrieve a stored key (for verification)
$ qmt secrets get OPENAI_API_KEY
OPENAI_API_KEY: sk-...

# Delete a key
$ qmt secrets delete OPENAI_API_KEY
✓ Secret 'OPENAI_API_KEY' has been deleted.
```

### Discovering Providers & Models

List the providers and models that `qmt` has loaded from your configuration.

```sh
# List all configured provider plugins
$ qmt providers
- openai
- anthropic
- ollama-plugin

# List all available models for each provider
$ qmt models
openai:
  - openai:gpt-4-turbo
  - openai:gpt-3.5-turbo
anthropic:
  - anthropic:claude-3-opus-20240229
  - anthropic:claude-3-sonnet-20240229
ollama-plugin:
  - ollama-plugin:llama3
  - ollama-plugin:mistral
```

## Examples & Advanced Usage

Combine flags to customize LLM interactions.

#### Specifying a Provider and Model

Use the `--provider` (`-p`) and `--model` flags, or combine them in the format `provider:model`.

```sh
# Use the anthropic provider with the claude-3-sonnet model
$ qmt -p anthropic --model claude-3-sonnet-20240229 "Explain the concept of emergence."

# A more concise way to do the same
$ qmt -p anthropic:claude-3-sonnet-20240229 "Explain the concept of emergence."
```

#### Using a Local Model via a Proxy/Local Server

Override the `--base-url` to point to a local service like Ollama.

```sh
# Assuming you have an 'ollama' provider configured that works with OpenAI's API format
$ qmt -p ollama:llama3 --base-url http://localhost:11434 "Who are you?"
```

#### Setting a System Prompt

Guide the model's behavior with a system prompt using `--system` (`-s`).

```sh
$ qmt -s "You are a helpful assistant that only speaks in rhyme." "What is a computer?"
```

#### Controlling Generation Parameters

Adjust temperature, max tokens, and other parameters for more creative or controlled output.

```sh
$ qmt --temperature 1.2 --max-tokens 100 "Write a short, creative opening line for a fantasy novel."
```

#### Passing Custom Provider-Specific Options

Use the `--options` (`-o`) flag to pass arbitrary key-value pairs that a specific plugin might support. Values are parsed as JSON if possible, otherwise as strings.

```sh
$ qmt -p some-custom-provider -o vision_mode=true -o response_format='{"type": "json_object"}' "Analyze this image"
```

#### Embedding with Specific Dimensions

Some embedding models support custom dimensions.

```sh
$ qmt embed "This is a test" -p openai:text-embedding-3-small --dimensions 512
```

#### Using Tools via MCP
Create a file named `mcp-config.toml`. This file tells QueryMT how to launch and communicate with the tool server.

```toml
# mcp-config.toml
[[mcp]]
name = "mem_server"
protocol = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-memory"]
```

When you ask a question that requires a tool, the LLM will request to use it, and `qmt` will handle the communication.

```sh
# Ask the LLM to use the 'set' tool from our stdio server
$ qmt --provider openai:gpt-4o-mini --mcp-config ./mcp-config.toml
qmt - Interactive Chat
Provider: openai
Type 'exit' to quit
──────────────────────────────────────────────────
:: create two entities: qmt and awesome and connect qmt with an isa relation to awesome
┌─ create_entities
└─ calling...
┌─ create_entities
└─ generated
> Assistant: The entities "qmt" and "awesome" have been successfully created, and the relation "isa" has been established from "qmt" to "awesome." If you need any further modifications or additional actions, just let me know!
──────────────────────────────────────────────────
:: what is qmt?
┌─ open_nodes
└─ generated
> Assistant: The entity "qmt" is of type "type1" and currently has no associated observations or relations other than the relation to "awesome" established earlier. If you need more information or to add observations, feel free to ask!
──────────────────────────────────────────────────
::
👋 Goodbye!
```

## Command-Line Reference

### Global Options

These options can be used with the main chat command.

| Flag                       | Alias | Description                                                               |
| -------------------------- | ----- | ------------------------------------------------------------------------- |
| `--provider <NAME>`        | `-p`  | The name of the provider to use (e.g., `openai`, `anthropic:claude-3`).     |
| `--model <NAME>`           |       | The specific model to use (e.g., `gpt-4-turbo`).                          |
| `--system <PROMPT>`        | `-s`  | The system prompt to set the context for the conversation.                |
| `--api-key <KEY>`          |       | Directly provide an API key, overriding stored secrets or environment vars. |
| `--base-url <URL>`         |       | The base URL for the LLM API, for use with proxies or local models.       |
| `--temperature <FLOAT>`    |       | Controls randomness (e.g., `0.7`).                                        |
| `--top-p <FLOAT>`          |       | Nucleus sampling parameter.                                               |
| `--top-k <INT>`            |       | Top-k sampling parameter.                                                 |
| `--max-tokens <INT>`       |       | The maximum number of tokens to generate in the response.                 |
| `--mcp-config <PATH>`      |       | Path to a TOML configuration file for MCP tool servers.                   |
| `--provider-config <PATH>` |       | Path to the provider plugins configuration file (e.g., `plugins.toml`).   |
| `--options <KEY=VALUE>`    | `-o`  | Set a provider-specific parameter. Can be used multiple times.            |

### Subcommands

| Command                                | Description                                                          |
| -------------------------------------- | -------------------------------------------------------------------- |
| `qmt auth login <PROVIDER>`            | Login to a provider using OAuth authentication.                      |
| &nbsp;&nbsp;`--mode <MODE>`            | OAuth mode (provider-specific, e.g., "max" or "console" for Anthropic). Defaults to "max". |
| `qmt auth logout <PROVIDER>`           | Logout from an OAuth provider (removes stored tokens).               |
| `qmt auth status [PROVIDER]`           | Check OAuth authentication status for all or a specific provider.    |
| &nbsp;&nbsp;`--no-refresh`             | Skip automatic token refresh (show raw stored status).               |
| `qmt secrets set <KEY> <VALUE>`        | Set a secret key-value pair in the secure store.                     |
| `qmt secrets get <KEY>`                | Get a secret value by its key.                                       |
| `qmt secrets delete <KEY>`             | Delete a secret by its key.                                          |
| `qmt providers`                        | List all available provider plugins loaded from the configuration.   |
| `qmt models`                           | List all available models for each provider.                         |
| `qmt default [PROVIDER]`               | Get or set the default provider (e.g., `qmt default openai:gpt-4`).    |
| `qmt embed [TEXT]`                     | Generate embeddings for the given text or for text from stdin.       |
| &nbsp;&nbsp;`--separator <STR>`        | Document separator for embedding multiple texts from a single stream. |
| &nbsp;&nbsp;`--dimensions <INT>`       | Request a specific number of dimensions for the embedding vector.      |
| &nbsp;&nbsp;`--encoding-format <FMT>`  | Specify the embedding encoding format (e.g., `float`, `base64`).     |
| &nbsp;&nbsp;`--provider <NAME>`        | Override the provider for this embedding task.                       |
| &nbsp;&nbsp;`--model <NAME>`           | Override the model for this embedding task.                          |
| `qmt update`                           | Update provider plugins.                                             |
| `qmt completion <SHELL>`               | Generate shell completions for the specified shell.                  |
