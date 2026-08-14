# Connecting an external provider

You cannot run these commands. Provider registration is deliberately
human-gated, so your job is to hand the user the exact commands and let them
run those in their own shell.

Two steps: register the provider, then route work to it.

    teton provider add <id> --kind <anthropic|openai-compatible> \
      --endpoint <url> --model <model>
    teton policy set-tier <reflex|scan|build|think> <id> [--fallback <id>]

`<id>` is the user's own name for the provider; each recipe below suggests one.
The API key is never typed into this conversation and never written to config:
`provider add` reads it echo-off into the OS keychain, or takes it from
TETON_PROVIDER_KEY. Every remote kind requires all three flags, including a
key — there is no keyless registration.

`--endpoint` is the **full request URL**, posted exactly as given. Nothing
appends a path to it. So it is not the `base_url` a vendor's SDK quickstart
shows: that value is one an OpenAI client appends `/chat/completions` to, and
handing it to Teton registers a provider that validates cleanly and then 404s
on its first turn. When a vendor is not in the list below, take the URL from
the vendor's own `curl` example, not from their `base_url` line.

## Recipes

Every `--model` below is an example, not a recommendation. Substitute whatever
model the vendor serves; `--model` is required for every remote kind. The tier
in each routing line is a suggestion too — see topic `policy` for what the four
tiers mean.

Anthropic. The Messages API, so the path is `/v1/messages` rather than the
`/chat/completions` every other recipe here uses:

    teton provider add anthropic --kind anthropic \
      --endpoint https://api.anthropic.com/v1/messages --model claude-opus-5
    teton policy set-tier think anthropic

OpenAI:

    teton provider add openai --kind openai-compatible \
      --endpoint https://api.openai.com/v1/chat/completions --model gpt-5.6
    teton policy set-tier build openai

Moonshot (Kimi):

    teton provider add kimi --kind openai-compatible \
      --endpoint https://api.moonshot.ai/v1/chat/completions --model kimi-k3
    teton policy set-tier think kimi

DeepSeek. The one path here with no `/v1` segment:

    teton provider add deepseek --kind openai-compatible \
      --endpoint https://api.deepseek.com/chat/completions --model deepseek-v4-pro
    teton policy set-tier build deepseek

Ollama. Local: it serves the models you have pulled and authenticates nothing.
It ignores the key, but `provider add` still asks for one, so any placeholder
does — `TETON_PROVIDER_KEY=ollama` skips the prompt.

    teton provider add ollama --kind openai-compatible \
      --endpoint http://localhost:11434/v1/chat/completions --model llama3.2
    teton policy set-tier scan ollama

Grok (xAI):

    teton provider add grok --kind openai-compatible \
      --endpoint https://api.x.ai/v1/chat/completions --model grok-4.6
    teton policy set-tier build grok

## When the key looks wrong

A 401 or 403 reads as a bad key and often is not one. Check the shape before
the user re-issues anything:

- the wrong `--kind` sends the wrong auth header, so an OpenAI-compatible
  endpoint registered as `anthropic` authenticates against nothing;
- an endpoint that is a base URL rather than a full request URL, or one
  carrying a `/v1` the vendor does not use, reaches a route that answers
  differently — a 404 here is the commoner symptom than a 401;
- the key may sit under a different provider id than the tier is routed to.

`teton provider list` shows which ids exist, `teton policy show` shows where
each tier resolves right now, and `teton doctor` shows whether the daemon is
even the build that was just installed. Web-search credentials are a separate
surface with their own header shapes: topic `web`.
