# MCP Integration

QueryMT includes support for integrating with **Model Context Protocol (MCP)** servers, leveraging the `rmcp` crate. In the context of QueryMT, MCP servers typically act as providers of tools or services that Large Language Models (LLMs) can interact with. This allows you to define complex functionalities or access external systems through a standardized MCP interface, and then make these capabilities available to your LLMs as tools.

Key components for MCP integration are found in the `crates/querymt/src/mcp/` directory.

## Core Concepts

*   **MCP Server:** An external process or service implementing the MCP protocol (as defined by `rmcp`). It exposes a set of "tools" (in the MCP sense, which are similar to functions) that can be invoked.
*   **`querymt::mcp::config::McpServerConfig`**: A configuration struct in QueryMT that specifies how to connect to an MCP server.
    *   `name`: A unique name for this MCP server configuration.
    *   `transport`: An `enum@querymt::mcp::config::McpServerTransportConfig` that defines the communication protocol and parameters. Supported transports include:
        *   **`Http`**: Connects to an MCP server over standard HTTP. Requires a `url` and optional `token`.
        *   **`Sse` (Server-Sent Events):** Connects to an MCP server over HTTP using SSE. Requires a `url` and optional `token`.
        *   **`Stdio`:** Spawns an MCP server as a child process and communicates with it over `stdin`/`stdout`. Requires the `command` to execute, optional `args`, and `envs`.
*   **`rmcp::RoleClient`**: The `rmcp` client used by QueryMT to communicate with an MCP server.
*   **`querymt::mcp::adapter::McpToolAdapter`**: A crucial adapter that bridges the gap between an MCP tool (`rmcp::model::Tool`) and QueryMT's tool system (`struct@querymt::chat::Tool` and `trait@querymt::tool_decorator::CallFunctionTool`).
    *   It converts the MCP tool's input schema into a `serde_json::Value` that QueryMT's LLMs can understand.
    *   It implements `CallFunctionTool`, so an MCP tool can be registered with `LLMBuilder` just like any other native Rust tool. When the LLM decides to call this tool, the `McpToolAdapter` forwards the call to the actual MCP server via the `rmcp` client.

## Workflow

1.  **Configure MCP Servers:**
    *   You define your MCP server connections in a configuration file (e.g., `mcp_config.toml`) that QueryMT can load using `mcp::config::Config::load()`. This file lists each MCP server and its transport details.

    ```toml
    # Example mcp_config.toml
    [[mcp]]
    name = "my_calculator_service"
    protocol = "stdio" # or "sse" or "http"
    command = "/path/to/mcp_calculator_server_binary"
    # args = ["--port", "8080"] # if needed

    [[mcp]]
    name = "external_data_api"
    protocol = "sse"
    url = "https://api.example.com/mcp_endpoint"
    token = "some-secret-token"
    ```

2.  **Start MCP Clients:**
    *   Your application uses `mcp::config::Config::create_mcp_clients()` to establish connections to all configured MCP servers. This returns a map of server names to `RunningService<RoleClient, Box<dyn DynService<RoleClient>>>` instances.

3.  **Discover Tools from MCP Server:**
    *   Once connected, you can query an MCP server for the list of tools it provides. The `rmcp` client would typically have a method like `list_tools()`. Each tool returned will be an `rmcp::model::Tool`.

4.  **Adapt MCP Tools for QueryMT:**
    *   For each `rmcp::model::Tool` you want to make available to your LLMs:
        *   Create an `McpToolAdapter` instance using `McpToolAdapter::try_new(mcp_tool, server_sink)`, where `server_sink` is the `ServerSink` from the `RunningService` for that MCP server. This adapter converts the MCP tool's schema and handles the call forwarding.

5.  **Register Adapted Tools with `LLMBuilder`:**
    *   Add the `McpToolAdapter` instances to your `LLMBuilder` using the `add_tool()` method.

    ```rust
    // Conceptual Code
    use querymt::builder::LLMBuilder;
    use querymt::mcp::{config::Config as McpConfig, adapter::McpToolAdapter};
    // ... other imports ...

    let mcp_config = McpConfig::load("mcp_config.toml").await?;
    let mcp_clients = mcp_config.create_mcp_clients().await?;

    let calculator_client_service = mcp_clients.get("my_calculator_service").unwrap();
    let mcp_calc_tool_description = calculator_client_service.client().list_tools().await?.into_iter().find(|t| t.name == "add").unwrap();

    let adapted_calc_tool = McpToolAdapter::try_new(
        mcp_calc_tool_description,
        calculator_client_service.client().sink().clone()
    )?;

    let llm = LLMBuilder::new()
        .provider("some_provider")
        // ... other configs ...
        .add_tool(adapted_calc_tool) // Register the MCP tool adapter
        .build(&provider_registry)?;
    ```

6.  **LLM Interaction:**
    *   When the LLM (configured with the adapted MCP tools) decides to use one of these tools:
        *   QueryMT's `ToolEnabledProvider` will invoke the `call()` method on the corresponding `McpToolAdapter`.
        *   The adapter will then use its `ServerSink` to send a `CallToolRequestParam` to the target MCP server.
        *   The MCP server executes its internal logic for that tool and returns a result.
        *   The `McpToolAdapter` receives this result and passes it back (as a JSON string) into the QueryMT tool-calling flow.
        *   The LLM receives this result and continues the conversation.

## Benefits of MCP Integration

*   **Decoupling:** Keep complex tool logic or integrations with external systems separate from your main LLM application code, managed within dedicated MCP servers.
*   **Standardization:** Use the MCP protocol as a standard way for LLMs to discover and invoke external capabilities.
*   **Reusability:** MCP servers and their tools can potentially be reused across multiple LLM applications or by other systems.
*   **Language Independence (for MCP servers):** MCP servers can be written in any language, as long as they implement the MCP protocol.

By integrating with MCP servers, QueryMT allows LLMs to leverage a broader ecosystem of tools and services in a structured and maintainable way.
