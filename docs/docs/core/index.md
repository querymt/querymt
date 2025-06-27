# Core Concepts Overview

QueryMT is built around a set of core concepts that enable flexible and powerful interactions with Large Language Models. This section provides an overview of these fundamental ideas. Each concept is explored in more detail in its dedicated page.

*   **[LLM Providers](./providers.md):**
    The abstraction layer for different LLM services (e.g., OpenAI, Anthropic, local models). QueryMT allows you to interact with them through a unified interface.

*   **[Chat Interactions](./chat.md):**
    Engage in conversational AI. This involves sending sequences of messages (from users, assistants, or even tool calls) and receiving responses from the LLM.

*   **[Text Completion](./completion.md):**
    Generate text based on a given prompt. Useful for tasks like summarization, translation, or content creation.

*   **[Embeddings](./embeddings.md):**
    Convert text into numerical vector representations (embeddings). These are essential for semantic search, clustering, and other machine learning applications.

*   **[Tools & Function Calling](./tools.md):**
    Empower LLMs to interact with external systems. You can define tools (functions) that the LLM can choose to call, allowing it to access real-time information or perform actions.

*   **[Building Providers (LLMBuilder)](./builder.md):**
    A fluent builder pattern (`LLMBuilder`) simplifies the configuration and instantiation of LLM provider instances, allowing you to specify models, API keys, behavior parameters, and tools.

*   **[Chaining Prompts](./chaining.md):**
    Orchestrate complex workflows by linking multiple LLM calls in sequence. The output of one step can be used as input for the next, enabling sophisticated reasoning and task decomposition.

Understanding these concepts will provide a solid foundation for using QueryMT effectively in your projects.

