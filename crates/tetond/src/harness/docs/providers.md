# Connecting an external provider

You cannot run these commands. Provider registration is deliberately
human-gated, so your job is to hand the user the exact commands and let them
run those in their own shell.

Two steps: register the provider, then route work to it.

    teton provider add <id> --kind <anthropic|openai-compatible> \
      [--endpoint <url>] --model <model>
    teton policy set-tier <reflex|scan|build|think> <id> [--fallback <id>]

`<id>` is the user's own name for the provider; each recipe below suggests one.
The API key is never typed into this conversation and never written to config:
`provider add` reads it echo-off into the OS keychain, or takes it from
TETON_PROVIDER_KEY.

## Recipes

Every `--model` below is an example, not a recommendation. Substitute whatever
model the vendor serves; `--model` is required for every remote kind. The tier
in each routing line is a suggestion too — see topic `policy` for what the four
tiers mean.

Anthropic. The `anthropic` kind knows its own address, so pass no `--endpoint`:

    teton provider add anthropic --kind anthropic --model claude-opus-5
    teton policy set-tier think anthropic

OpenAI:

    teton provider add openai --kind openai-compatible \
      --endpoint https://api.openai.com/v1 --model gpt-5.6
    teton policy set-tier build openai

Moonshot (Kimi):

    teton provider add kimi --kind openai-compatible \
      --endpoint https://api.moonshot.ai/v1 --model kimi-k3
    teton policy set-tier think kimi

DeepSeek. The base URL takes no `/v1` suffix:

    teton provider add deepseek --kind openai-compatible \
      --endpoint https://api.deepseek.com --model deepseek-v4-pro
    teton policy set-tier build deepseek

Ollama. Local and keyless: it serves the models you have pulled and
authenticates nothing, so there is no key step at all.

    teton provider add ollama --kind openai-compatible \
      --endpoint http://localhost:11434/v1 --model llama3.2
    teton policy set-tier scan ollama

Grok (xAI):

    teton provider add grok --kind openai-compatible \
      --endpoint https://api.x.ai/v1 --model grok-4.6
    teton policy set-tier build grok

## When the key looks wrong

A 401 or 403 reads as a bad key and often is not one. Check the shape before
the user re-issues anything:

- the wrong `--kind` sends the wrong auth header, so an OpenAI-compatible
  endpoint registered as `anthropic` authenticates against nothing;
- an endpoint carrying a `/v1` the vendor does not use, or missing one it
  does, reaches a route that answers differently;
- the key may sit under a different provider id than the tier is routed to.

`teton provider list` shows which ids exist, `teton policy show` shows where
each tier resolves right now, and `teton doctor` shows whether the daemon is
even the build that was just installed. Web-search credentials are a separate
surface with their own header shapes: topic `web`.
