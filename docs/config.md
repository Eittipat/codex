# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.

## Browsing provider models

The `/model` picker includes **Browse configured providers**. It queries
explicitly configured OpenAI-compatible providers at their `/models` endpoints.
It includes the built-in Bedrock and Google Cloud catalogs only when those
providers are configured or selected. Select a result to start a new chat
with that provider and model; the current chat remains available in history.
The picker caches successful provider catalogs and uses them when a later
request fails. Availability, credentials, and tool support depend on the
provider and model.

For example, add OpenRouter to `~/.codex/config.toml`:

```toml
[model_providers.openrouter]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1"
env_key = "OPENROUTER_API_KEY"
wire_api = "responses"
```

Set `OPENROUTER_API_KEY` in the environment before starting Codex.
