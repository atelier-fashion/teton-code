# Connecting an external provider

In a session it is one line the user types:

    /provider setup <vendor> [tier]

It asks vendor, id, model, key (never typed in chat — read echo-off into the
keychain) and routing, previews the config, and writes provider and tier
together. "Kimi for deep reasoning" is `/provider setup kimi think`; with no
vendor it lists the ones below. Name it first; the commands here are the shell
form, and what `/provider setup` prints with no terminal to ask on. You cannot
run either — hand the user the exact commands to run themselves.

Two steps: register the provider, then route work to it.

    teton provider add <id> --kind <anthropic|openai-compatible> \
      --endpoint <url> --model <model>
    teton policy set-tier <reflex|scan|build|think> <id> [--fallback <id>]

`<id>` is the user's own name for the provider; each recipe below suggests one,
and `/provider setup` keys the same keychain entry. The key is never written to
config: `provider add` reads it echo-off, or takes it from TETON_PROVIDER_KEY.
Every remote kind requires all three flags, including a key — there is no
keyless registration.

`--endpoint` is the **full request URL**, posted exactly as given; nothing
appends a path to it. It is not a vendor's `base_url`: an OpenAI client appends
`/chat/completions` to that, and handing it to Teton registers a provider that
validates cleanly and then 404s on its first turn. For a vendor not listed
below, take the URL from their own `curl` example, not their `base_url` line.

## Recipes

Every `--model` below is an example, not a recommendation — substitute whatever
the vendor serves; `--model` is required for every remote kind. The tier in each
routing line is a suggestion too; topic `policy` says what the four mean.

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
It ignores the key, but `provider add` still asks for one — enter any
placeholder at the (echo-off) prompt. Never put a real key on a command line;
it lands in shell history and the process list.

    teton provider add ollama --kind openai-compatible \
      --endpoint http://localhost:11434/v1/chat/completions --model llama3.2
    teton policy set-tier scan ollama

Grok (xAI):

    teton provider add grok --kind openai-compatible \
      --endpoint https://api.x.ai/v1/chat/completions --model grok-4.6
    teton policy set-tier build grok

## When the key looks wrong

A 401 or 403 reads as a bad key and often is not. Check the shape before the
user re-issues anything:

- the wrong `--kind` sends the wrong auth header, so an OpenAI-compatible
  endpoint registered as `anthropic` authenticates against nothing;
- a base URL rather than a full request URL, or a `/v1` the vendor does not
  use, reaches a different route — a 404 is the commoner symptom than a 401;
- the key may sit under a different provider id than the tier is routed to.

`teton provider list` shows which ids exist, `teton policy show` where each
tier resolves now, `teton doctor` whether the daemon is the build just
installed. Web-search credentials have their own header shapes: topic `web`.
