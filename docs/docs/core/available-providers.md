# Available Providers

QueryMT ships with 15 providers split across two types: **WASM** (API-based, cloud services) and **Native** (local inference, runs models on your hardware). This page covers how to configure each type, how to pick the right build variant for your hardware, and provides copy-pasteable configuration recipes.

---

## Provider Repository (`repo.query.mt`)

QueryMT maintains an always-up-to-date provider repository at **[`https://repo.query.mt`](https://repo.query.mt)**.

- **[`latest.json`](https://repo.query.mt/latest.json)** — updated on every push to `main` (default)
- **[`stable.json`](https://repo.query.mt/stable.json)** — updated on every tagged release

If no providers config exists at `~/.qmt/providers.toml` (or `.json` / `.yaml`), QueryMT automatically fetches `latest.json` and caches it to `~/.qmt/providers.json` on first run.