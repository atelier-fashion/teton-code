# Policy: which model does which work

Teton dispatches on what a call is *for* — classify, summarize, edit, critique
— not on where in the lifecycle it happens. Eleven categories each inherit one
of four tiers, so the usual configuration is four settings rather than eleven.

You cannot run these commands. Print them; the user runs them.

## The four tiers

- `reflex` — sub-second, every turn, never leaves the machine. Latency and
  privacy dominate. Carries `route`, `redact` and `title`.
- `scan` — read a lot, emit a little. Context window and input-token price
  dominate. Carries `digest`, `compact` and `triage`.
- `build` — the agentic loop of read, edit, run, verify. Tool-call fidelity
  dominates. Carries `edit` and `shell`.
- `think` — design, debug, critique, and the once-per-repository draft.
  Reasoning depth dominates. Carries `design`, `debug`, `review` and `draft`.

An unbound `build` or `think` tier falls back to the configured default
provider. `reflex` and `scan` do not: work that was already local stays local
until the user says otherwise.

## Binding

    teton policy set-tier <reflex|scan|build|think> <provider-id> [--fallback <id>]
    teton policy set-category <category> <provider-id> [--fallback <id>]
    teton policy show

`set-tier` is the setting most users want: every category on that tier follows
it. `set-category` binds one category ahead of its tier, for the case where one
kind of work wants a different vendor — some users deliberately route `review`
away from the model that wrote the code.

`--fallback` names the provider used when the primary errors or times out. It
is a second binding, not a retry count.

The ten bindable categories are `title`, `digest`, `compact`, `triage`,
`edit`, `shell`, `design`, `debug`, `review` and `draft`. `route` and `redact` cannot be
bound at all, and this is structural rather than a check: `route` classifies
intent, and a router that called a remote model to decide would have spent what
it was saving; `redact` scans content before egress, so binding it elsewhere
would ship exactly what it exists to hold back.

## Reading the table

`teton policy show` prints the effective routing table: every tier, every
category, and where each one resolves right now. It is the answer to "why did
this call go there" — read it before changing a binding, and read it again
afterwards, because a category with its own override does not follow its tier.

`teton provider list` shows which ids are registered to bind to, and `teton
doctor` shows whether the daemon is running the build you expect.

`teton policy set <phase> <provider>` is retired: routing no longer dispatches
on lifecycle phase, so a phase is no longer something to route.
