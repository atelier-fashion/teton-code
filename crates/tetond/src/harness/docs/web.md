# Web lookup

Web lookup is off by default and is a separate opt-in from provider setup. You
cannot enable it. Point the user at `/web setup`, which asks its questions in
order and writes nothing until the last answer.

## Tiers, cumulative

`[web] tier` takes one of:

- `off` — the default. No lookup of any kind.
- `fetch_user_url` — fetch a URL the user pasted into the session.
- `fetch_any_url` — also fetch a URL the model composed.
- `search` — also search through a backend the user names.

Each tier includes the ones before it. Enabling a tier is not consenting to a
lookup: a lookup asks before anything leaves the machine **unless** that tier
has already been granted — for the session, or permanently via `[web]
permission_allow` — and a fresh cache hit performs no egress and asks nothing.
The three tiers are consented separately, so an answer about one is never an
answer about the others. `search` additionally needs the
local model, because every query is scanned before it leaves — a machine with
no local model can fetch but cannot search, and the tier menu marks it
unavailable there rather than writing a tier that would refuse every query.

## The key and the header it rides in

The key goes into the OS keychain and never into config. Config carries only
the reference: `search_key_ref = "keychain://teton/web-search"`, naming an
entry under the same `teton` service every Teton credential is filed under.

`search_auth` is the header shape, with `{key}` marking where the secret is
substituted. Backends disagree about it, and the wrong shape answers 401 —
which reads exactly like a bad key and is not one:

- a backend Teton does not name, and the default: `Authorization: Bearer {key}`
- Brave Search API: `X-Subscription-Token: {key}`
- Kagi Search API: `Authorization: Bot {key}`
- self-hosted SearxNG: keyless, no header at all. Its endpoint has to end
  `/search?format=json`, or the instance answers a web page instead of JSON
  and the parse finds no results.

## The rest of the table

`permission_allow` is the durable half of the consent prompt: it lists the
tiers whose "enable permanently" answer was given, one entry each, and defaults
to empty. Removing a tier from it restores asking for that tier and nothing
else. It cannot widen `tier` — the ceiling is checked before any prompt exists,
so a tier listed here that `tier` does not reach simply never comes up.

`allowed_domains` constrains model-composed destinations only: absent means
unrestricted, present but empty means nothing is allowed, and a URL the user
pasted is exempt either way. `cache_ttl_secs` is the freshness window in
seconds and defaults to 900; 0 disables caching. `tier = "search"` with no
`search_endpoint` is the one combination the daemon refuses to start on.

## When a change takes effect

`/web setup` is live immediately: that session and every other open session
pick the capability up on their next turn, with no restart. A `config.toml`
edited by hand is read when the daemon next starts, so a hand-written `[web]`
table does nothing until then. Restart with `brew services restart teton`, or
stop the running `teton-code` and let the next `teton` command start a fresh
one.
