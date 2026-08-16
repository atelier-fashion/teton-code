# REQ-579 — live A/B acceptance record (TASK-158)

**Outcome path: the run happened.** The 17 GiB weights were present and
symlinkable and both daemons loaded and benchmarked the GGUF, so this is a real
A/B against two isolated real-weights daemons — not the `unrun` fallback the
task file licenses.

> **Final state: AC-1 FAILS, 3/3, in both rounds.** Round 1 (§4) is the
> TASK-156 wording; round 2 (§13) is the restructured guide that moved the
> hand-off *inside* step 1 in response to round 1's diagnosis. Neither produces
> a reply that names `/provider setup` on the AC-1 prompt. Both are
> deterministic 3/3.
>
> Round 1 and round 2 are kept whole rather than the first being rewritten:
> round 1 is the evidence for why the restructure exists, and the pair is the
> standing proof that this hand-off does not survive contact with a
> setup-recipe prompt no matter where in the guide it is placed.
>
> **The restructure was not wasted, and it is not a null result.** It cleared
> the `shell` regression (3/3 → 0/3), it cut CLI recitation from 6 mentions to
> 1, and it made the command *reachable*: the direct-question diagnostic that
> failed in round 1 passes in round 2. What it did not do is win the front
> door. Round 2 also flipped ASSUME-008's behaviour, and in doing so produced
> the sharpest single fact in this document: **the model read the
> `teton_docs providers` topic — whose first five lines are "In a session it is
> one line the user types: `/provider setup <vendor> [tier]`" — and then
> answered with `teton provider add` anyway** (§13).
>
> The rest of the REQ is in better shape than AC-1 is: `/help` lists the command
> live (AC-14), the piped degradation prints the exact recipe and asks nothing
> live (AC-9), and AC-2..AC-13 are covered by named tests. AC-1 is the one
> claim only a live run could make, and it is recorded here as failed rather
> than reworded into a pass.

---

## 1. Builds

| Arm | Checkout | Commit | Build | Daemon binary |
|---|---|---|---|---|
| **A — candidate** (REQ-579) | `.worktrees/REQ-579` on `feat/REQ-579-guided-provider-setup` | `4f170e2` (`4f170e2e253785cbd4bb458060a6c3b20d4a4d9f`, TASK-157 complete / tier 4) | `cargo build --release --workspace --features tetond/llama` → `Finished` in **48.55 s** (incremental; llama.cpp objects cached) | `target/release/teton-code`, 12,033,984 bytes |
| **B — baseline** (pre-REQ-579) | installed Homebrew keg `teton 0.1.17`, `/opt/homebrew/opt/teton/bin/` | main @ `05ac398` (`05ac3988aa6c411d7e20b2d4236d895fcb03a709`, the `chore(release): v0.1.17` commit) | not rebuilt — the shipped keg **is** main's tip | `/opt/homebrew/opt/teton/bin/teton-code`, 11,985,392 bytes |

Both binaries really carry the engine — **55** exported `llama_*` symbols in
each — and both daemons loaded and benchmarked the GGUF at startup (a default
build has no inference engine and would silently go remote-only, LESSON-482).
Both report `teton-code 0.1.17 — core 0.1.17, protocol 0.1.17, providers
0.1.17, inference 0.1.17`.

**The baseline identity was verified on the artifact, not inferred from the
version string.** The pre-REQ credential sentence is readable in the shipped
binary:

```
$ strings /opt/homebrew/opt/teton/bin/teton-code | grep "Never ask the user to type an API key"
Never ask the user to type an API key or credential into the conversation: point them at `teton provider add` or `/web setup`, which read it echo-off into the keychain.
```

That is `self_config.md` line 2 exactly as it stands on `main`, with no
`/provider setup`. The candidate binary carries **7** occurrences of the string
`provider setup`; the baseline carries **1**, and that one is prose in the `web`
docs topic ("a separate opt-in from provider setup"), not the command. So the
baseline has no `/provider setup` at all and **cannot** pass AC-1 by
construction — arm B is a control that establishes what the sentence displaced,
not a candidate for the criterion.

**The single variable.** `git diff main -- crates/tetond/src/harness/self_config.md`
is one line — the credential sentence, changed to:

> Never ask the user to type an API key or credential in chat: point them at `/provider setup <vendor> [tier]`, `teton provider add` in a shell, or `/web setup`, which read it echo-off into the keychain.

## 2. Isolation (LESSON-482 method)

```sh
# one base dir per arm; the socket is 26 bytes, far inside SUN_LEN (~104)
XDG_RUNTIME_DIR=/tmp/r579a   # candidate → /tmp/r579a/teton/tetond.sock
XDG_RUNTIME_DIR=/tmp/r579b   # baseline  → /tmp/r579b/teton/tetond.sock

cp  ~/"Library/Application Support/teton/model-selection.toml"  <base>/teton/
ln -s ~/"Library/Application Support/teton/models"              <base>/teton/models
<build>/teton-code --shutdown-policy never
```

- The weights directory is **symlinked**, so both daemons mmap the same
  18,556,689,568-byte inode. Nothing was downloaded and the real file was never
  written to; it was re-checked present and unchanged after teardown.
- `model-selection.toml` is copied so the first-run consent gate has a settled
  decision — the run tests the prompt, not the consent flow.
- **No `config.toml` exists in either base dir.** Default permission level
  (`guarded`), web lookup off, and — the scenario AC-1 names — **no remote
  providers**: `teton doctor` reported `no providers configured` on both arms
  before any round. The user's real `config.toml` was never copied or read.
- `--shutdown-policy never` keeps the daemon alive across an arm's rounds, so
  the model loads once per arm rather than once per round (0.1.14+ otherwise
  exits with the last client).
- **Serialized per REQ-577 F-4**: never more than one of these two daemons
  resident. Arm A ran, was stopped, arm B started; arm B was stopped before arm
  A was restarted for the diagnostics. No Metal OOM occurred.
- The user's own Homebrew daemon (pid 60648, 19.8 GiB RSS) and a pre-existing
  REQ-578 leftover (pid 20612) were resident throughout and **left alone** —
  neither was signalled. Only the three pids this run started (88278, 88612,
  88769) were killed.

Model load to `local model qwen3-coder-30b-a3b ready`: **40 s** (arm A), **50 s**
(arm B) — well inside TASK-158's 3-minute honesty bound, so no `unrun` applies.

### Session working directory

Both arms ran every round in the same cwd, `/tmp/r579proj` — a three-file crate
(`Cargo.toml` at `version = "0.4.2"`, `src/main.rs`, `README.md`) documenting
nothing about Teton, the same neutral environment REQ-577 used. A repository
hunt there cannot succeed, which is the honest test of whether the knowledge
comes from the prompt.

### Driving

One CLI process per round (`teton -y -v`, stdin piped), **one prompt per
session** — a fresh pipe is a fresh session, so no round carries another's
context. Tool calls are counted from the CLI's own ` - <tool> [running]` status
lines: what the session did, not what the prose claimed.

Piped stdin means the surface is non-TTY. **This does not confound AC-1**, and
that was checked rather than assumed: `SELF_CONFIG_GUIDE` is pushed
unconditionally into every system prompt at `crates/tetond/src/harness/turn_loop.rs:1228`,
with no surface or TTY gate, so the steer clause was in all six prompts. The
independent proof that the guide reached the model is in the replies themselves
— every round emitted `https://api.moonshot.ai/v1/chat/completions` and
`kimi-k3`, which exist nowhere but the recipe list two lines below the steer
clause in the same file.

## 3. The pass rule, as TASK-158 states it

> Pass = the reply names `/provider setup` (any accepted vendor spelling —
> ADR-2). Naming `teton provider add` / `teton policy set-tier` as the
> **alternative** is fine (BR-1 allows the CLI as the non-interactive answer).
> The fail is reciting the CLI as *the* instruction with no hand-off.

Applied literally below. No round was rescored.

## 4. Round 1 — arm A (candidate, guide at `4f170e2` + TASK-156 wording)

Prompt, three fresh sessions: `set up Kimi for deep reasoning`

**All three replies are byte-identical.** Diffing the three transcripts, the
only differences are the cumulative cost-ledger counters (which accumulate
across sessions sharing a daemon) and the session id. So `n = 3` here is a
check that the temperature-0.2 profile behaves as BUG-168 recorded, not a
sample from a distribution.

### A1 / A2 / A3 — verbatim

The session's first reply, complete:

````
To set up Kimi for deep reasoning, we'll need to configure a provider in Teton. Kimi is an OpenAI-compatible model, so we'll use the `teton provider add` command with the appropriate endpoint and model.

Here's the command to add Kimi as a provider:
```bash
teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1/chat/completions --model kimi-k3
```

After adding the provider, we need to set it as the provider for deep reasoning tasks. We'll use the `teton policy set-tier` command:
```bash
teton policy set-tier think kimi
```

Let's verify the configuration by checking the providers and policy:
```bash
teton provider list
teton policy show
```

This will set up Kimi as the provider for deep reasoning tasks.
````

The model then **attempted to run the command itself**:

```
 - shell: teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1/chat/completions --model kimi-k3 [running]
? permission requested: shell — shell: teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1/chat/completions --model kimi-k3
  allow shell? [y]es / [n]o / [a]llow-always / [d]eny-always:  - shell: … [failed]
```

The driver's stdin was at EOF, so the permission prompt was answered by nobody
and the call failed — no command was run and nothing was written. The model
then produced its second reply:

````
I apologize for the confusion. You're right - I can't execute shell commands directly. Let me help you set up Kimi for deep reasoning by providing the exact steps you need to follow.

First, you'll need to add Kimi as a provider using the Teton CLI. Here's the command you need to run in your terminal:

```bash
teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1/chat/completions --model kimi-k3
```

Then, you'll need to set it as the provider for deep reasoning tasks:

```bash
teton policy set-tier think kimi
```

These commands will configure Kimi as your deep reasoning provider. You'll need to have the Teton CLI installed and configured on your system to run these commands.

If you need help with the Teton CLI setup or have any other questions about configuring Kimi, please let me know.
````

### Round 1 arm A scoring

| Round | `/provider setup` named | `teton provider add` occurrences | Tool calls | Verdict |
|---|---|---|---|---|
| **A1** | **0** | 6 | 1 × `shell` (denied) | **FAIL** |
| **A2** | **0** | 6 | 1 × `shell` (denied) | **FAIL** |
| **A3** | **0** | 6 | 1 × `shell` (denied) | **FAIL** |

Both replies in every round recite the CLI as *the* instruction — "the command
you need to run in your terminal" — with no hand-off named anywhere. That is
the fail condition verbatim.

## 5. Rounds — arm B (baseline, v0.1.17)

Same prompt, same cwd, three fresh sessions. Replies byte-identical across the
three (only the cumulative cost counters differ).

### B1 / B2 / B3 — verbatim

````
To set up Kimi for deep reasoning in Teton, you'll need to configure a provider for Kimi and then assign it to the `think` tier, which handles deep reasoning tasks.

First, let's add Kimi as a provider using the `teton provider add` command. Kimi's API endpoint is `https://api.moonshot.ai/v1/chat/completions`, and we'll use the `kimi-k3` model for deep reasoning.

```bash
teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1/chat/completions --model kimi-k3
```

Next, we'll assign this provider to the `think` tier for deep reasoning tasks:

```bash
teton policy set-tier think kimi
```

These commands will configure Kimi as the provider for deep reasoning tasks in Teton. Make sure to replace `kimi` with the actual API key for Kimi if it's not already configured in your environment.
````

### Arm B scoring

| Round | `/provider setup` named | `teton provider add` occurrences | Tool calls | Verdict |
|---|---|---|---|---|
| **B1** | **0** | 2 | 0 | **FAIL** (expected — command does not exist on this build) |
| **B2** | **0** | 2 | 0 | **FAIL** (expected) |
| **B3** | **0** | 2 | 0 | **FAIL** (expected) |

Arm B is the control. It cannot pass and is not expected to; its value is what
it shows about the delta.

## 6. What the A/B actually shows

**The steer clause changed no observable behaviour on the criterion it was
written for.** Both arms recite the same two CLI commands with the same
endpoint, the same model and the same (correct) `think` tier. AC-1's stated
delta — a reply that names `/provider setup kimi` — did not appear on either
arm.

Two differences the A/B *did* surface, neither of them AC-1:

- **The candidate tries to run the setup itself; the baseline does not.** 3/3
  candidate rounds open a `shell` tool call for `teton provider add …`; 0/3
  baseline rounds make any tool call. This is the "the agent tried to perform
  the setup itself" behaviour REQ-577 §3 D2 recorded — but in REQ-577 it was the
  **baseline** that did it and the candidate that stopped. Here it has moved to
  the candidate. It costs a wasted model call and a permission prompt the user
  must decline, and it is a fresh instance of BUG-168's rule: a byte-level
  prompt change moves behaviour it was not about.
- **The baseline ends with a credential-hygiene defect the candidate does
  not.** B's last sentence — "Make sure to replace `kimi` with the actual API
  key for Kimi" — instructs the user to put a live API key on the command line,
  which is exactly what BR-2 and `teton provider add`'s own echo-off prompt
  exist to prevent. The candidate never says it. That is a real improvement
  from the reworded sentence, and it is not what AC-1 asks about.

**Why the clause loses, as far as this run can tell.** The sentence naming
`/provider setup` is line 2 of the guide; lines 4–6 are a numbered,
recipe-filled procedure whose step 1 *is* `teton provider add` with every flag
spelled out and every vendor's endpoint tabulated. The model composes from the
procedure, not from the hygiene sentence above it — the same texture as
REQ-577's F-1, where the tier slot was filled from the nearest enumeration
rather than from a fact the model demonstrably knew. Diagnostic D4 below is the
sharpest form of the evidence.

## 7. Diagnostics (not AC-1 trials — recorded to characterize the failure and to
bank the live checks that were free while a daemon was up)

**D1 — `/help` (AC-14), candidate.** Live, one line, from the dispatch table:

```
/provider setup  Register a provider and route a tier to it: /provider setup [vendor] [tier] — confirm before anything is written.
```

**PASS live.**

**D2 — piped `/provider setup kimi think` (AC-9 / BR-11), candidate.** Exit 0.
Verbatim:

```
>> `/provider setup` reads an API key without echoing it, which needs a terminal — this session's input is not one, so nothing was read and nothing was changed.
run `teton` in a terminal and type `/provider setup kimi think`, or run these two from a shell:
  teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1/chat/completions --model kimi-k3
  teton policy set-tier think kimi
`teton provider add` reads the key itself, without echoing it, and files it in your OS keychain as `keychain://teton/kimi` — never pass a key on the command line.
```

The line piped *after* the command was not consumed as a flow answer — it
reached the session as an ordinary turn. That is AC-9's "consumed no further
stdin" in the sense that matters: the guided flow asked nothing and swallowed
nothing. **PASS live.** Note the irony worth recording: the degradation text is
the only place in this entire run where the product told the user about
`/provider setup` — and it does so on the one surface where the command cannot
run.

**D3 — "How do I connect Kimi?" (candidate).** Different phrasing, same
failure: `teton provider add` recited, `/provider setup` never named. This
round also **bled two raw guide lines verbatim into the answer** (the "Teton's
own configuration is never inside the repository…" line and the whole `[web]`
paragraph), which is prompt leakage into user-visible output and is not covered
by any AC.

**D4 — "Is there a slash command in this session to set up Kimi?" (candidate).**
The decisive one. Asked *directly* whether a slash command exists, with the
clause naming one sitting in the prompt, the model answered:

> Yes, you can set up Kimi using the `teton provider add` command with the appropriate parameters.

— and never named `/provider setup`. It also misrouted the tier
(`teton policy set-tier build kimi`), the REQ-577 F-1 failure recurring on an
unprompted shape. So the failure is not that the question was ambiguous or that
the hand-off was a subtle ask: the model does not reach that clause even when
the question is precisely about it.

## 8. ASSUME-008 observation

**The assumption holds, in the same direction REQ-577's F-3 recorded.**

`teton_docs` was called **0 times in 0 of the 3 candidate rounds**, and 0 times
across D3 and D4 — five sessions of front-door provider-setup questions, zero
docs-tool calls. The only tool call of any kind in the whole candidate arm was
the `shell` attempt to run `teton provider add` itself. The model answered
entirely from the resident guide: the endpoints and model ids in its replies are
the inline recipes, not a topic read.

This is the "guide-not-tool" pattern ASSUME-008 is about, now with a second
REQ's worth of evidence behind it. It matters for REQ-579 specifically: any
future attempt to fix AC-1 by moving the hand-off knowledge *into* a
`teton_docs` topic would go unread on exactly the shapes AC-1 tests. A fix has
to land in the resident guide, and it has to compete with the recipe procedure
that is already there.

> **Superseded in part by round 2 (§14–15).** The restructured guide **flipped
> this**: the model now calls `teton_docs providers` 1×/round on the same
> front-door prompt, 3/3, completing with no permission prompt. So the tool
> *is* reachable on these shapes after all — the round-1 result was a property
> of that guide wording, not of the shape. The paragraph above is left as
> written because it was true of the build it describes, and because what
> replaced it is worse for ASSUME-008's premise, not better: the topic is now
> read **and disregarded** (§15). "Will the model reach the tool" turns out to
> be the easy half of the assumption; "will it act on what it reads" is the
> half that just failed.

## 9. AC-1..AC-14 accounting

Every criterion accounted for by name. "Covered by test" names the exact test
function as it is written in the source; "pass live" means this run observed
it; "unrun" means neither.

| AC | Status | Evidence |
|---|---|---|
| **AC-1** — reply names `/provider setup kimi`, no CLI recitation; live A/B, 3 rounds, baseline recorded | **FAIL — live, three times** | §4 (round 1, TASK-156 wording): 0/3 name `/provider setup`; 3/3 recite the CLI. §14 (round 2, hand-off moved inside step 1): 0/3, *after* fetching and reading the topic that names the command in its first five lines. §19 (round 3, CLI recipe removed from the guide entirely): 0/3, with the `shell` regression back and model calls doubled. Baseline recorded (§5). Structural halves covered by `harness::turn_loop::tests::the_system_prompt_forbids_asking_for_a_credential_in_the_conversation`, `harness::turn_loop::tests::the_system_prompt_bundles_tetons_own_provider_setup` and (round 3) `the_bundled_guide_carries_no_vendor_recipe_and_refers_to_the_topic` (`crates/tetond/src/harness/turn_loop.rs`) — all green across all three rounds, and all asserting the clause is *in the prompt*, which is exactly the claim these runs confirm is insufficient |
| **AC-2** — TTY walk vendor→model→key→routing→preview→confirm; same session routes `think`→`kimi`, no restart | covered by test | `the_committed_provider_routes_the_next_decision_in_the_same_session` (`crates/tetond/tests/provider_setup_flow.rs`); `provider_setup_ui::tests::a_full_walk_stores_the_key_after_the_confirm_and_sends_only_its_reference` (`crates/teton/src/provider_setup_ui.rs`); `runtime::tests::provider_setup_flow::a_commit_writes_the_previewed_bytes_and_routing_is_live_without_a_restart` (`crates/tetond/src/runtime.rs`). **Not exercised live** — this run had no TTY (piped stdin) |
| **AC-3** — bare `/provider setup` lists the catalog, accepts number or id | covered by test | `provider_setup_ui::tests::a_walk_without_a_vendor_lists_the_catalog_and_takes_a_number_or_a_name`; `provider_setup_ui::tests::an_unresolvable_vendor_argument_falls_back_to_the_catalog`; `provider_setup_ui::tests::no_vendor_argument_resolves_to_none_so_the_caller_lists_the_catalog`; `provider_setup_ui::tests::the_recipe_without_a_vendor_names_every_vendor_the_daemon_sent` (all `crates/teton/src/provider_setup_ui.rs`); `slash::tests::the_provider_setup_command_parses_up_to_two_arguments` (`crates/teton/src/slash.rs`) |
| **AC-4** — key absent from transcript, events, daemon log, cost ledger, config; config carries only `keychain://…` | covered by test **with a named gap** | `provider_setup_ui::tests::the_key_reaches_the_keychain_and_nothing_else`; `provider_setup_ui::tests::a_full_walk_stores_the_key_after_the_confirm_and_sends_only_its_reference` (`crates/teton/src/provider_setup_ui.rs`); `the_committed_provider_routes_the_next_decision_in_the_same_session` (`crates/tetond/tests/provider_setup_flow.rs`, asserts `auth_ref = "keychain://teton/kimi"` in the written bytes); `server::tests::a_provider_setup_commit_announces_only_what_it_applied`; `runtime::tests::provider_setup_flow::a_raw_key_in_place_of_a_reference_is_refused_and_not_echoed`. **Gap: the daemon-log and cost-ledger sweeps are asserted nowhere** — LESSON-519's "assert on the real artifact" is met for transcript/events/config and unmet for those two |
| **AC-5** — client entry and model guide both from one typed catalog, gated both directions | covered by test | `every_recipe_reaches_the_plan_field_for_field` and `the_plan_offers_the_whole_roster_over_a_configured_daemon` (`crates/tetond/tests/provider_setup_contracts.rs`); `provider_recipes::tests::every_recipe_maps_onto_a_wire_entry_field_for_field` (`crates/tetond/src/provider_recipes.rs`); `runtime::tests::provider_setup_flow::the_plan_serves_the_shipped_recipe_catalog_unaltered`; `the_bundled_guide_and_the_recipe_catalog_agree` and `the_providers_topic_and_the_recipe_catalog_agree` (`crates/tetond/tests/web_setup_contracts.rs`). Corroborated live: the endpoint and model in every round of both arms are the catalog's exact values |
| **AC-6** — base URL composed and echoed *before* the key prompt; composed URL persisted; backslash authority refused at the same seam | covered by test | `provider_setup_ui::tests::a_pasted_base_url_is_composed_and_echoed_before_the_key_is_asked_for`; `provider_setup_ui::tests::a_backslash_authority_is_refused_with_the_shell_commands_own_sentence`; `provider_setup_ui::tests::an_anthropic_vendor_is_not_asked_for_an_endpoint` (`crates/teton/src/provider_setup_ui.rs`); `runtime::tests::provider_setup_flow::a_vendor_base_url_is_composed_into_the_url_the_adapter_posts`; `runtime::tests::provider_setup_flow::an_endpoint_whose_authority_carries_a_backslash_is_refused`; `runtime::tests::provider_setup_flow::the_previewed_host_is_the_one_the_request_would_be_dialed_at` (`crates/tetond/src/runtime.rs`) |
| **AC-7** — cancel at each of the five prompts: config byte-identical, no keychain entry | covered by test | `provider_setup_ui::tests::an_abort_at_every_prompt_stores_nothing_and_commits_nothing`; `provider_setup_ui::tests::the_confirm_defaults_to_no_and_only_an_explicit_yes_writes`; `provider_setup_ui::tests::a_refused_preview_asks_for_no_confirmation_and_stores_nothing`; `provider_setup_ui::tests::a_keychain_that_cannot_store_the_key_writes_no_config` (`crates/teton/src/provider_setup_ui.rs`); `a_commit_whose_digest_went_stale_is_refused_and_the_file_is_untouched` (`crates/tetond/tests/provider_setup_flow.rs`, asserts bytes *and* inode) |
| **AC-8** — refused commit deletes a fresh key / restores a rotated one; both reported | covered by test | `provider_setup_ui::tests::a_refused_commit_on_a_fresh_key_deletes_it_and_says_so`; `provider_setup_ui::tests::a_refused_commit_after_a_rotation_puts_the_previous_key_back`; `provider_setup_ui::tests::a_commit_that_never_answered_leaves_the_keychain_alone_and_says_so`; `provider_setup_ui::tests::a_commit_that_applied_nothing_says_the_key_was_still_rotated` (all `crates/teton/src/provider_setup_ui.rs`) |
| **AC-9** — piped `/provider setup kimi` prints the recipe, exits 0, consumes no further stdin | **PASS — live** + covered by test | §7 D2: exit 0, exact recipe printed, next piped line reached the session as an ordinary turn rather than being eaten as a flow answer. Tests: `a_piped_provider_setup_prints_the_recipe_and_asks_nothing` (`crates/teton/tests/cli_e2e.rs`); `provider_setup_ui::tests::a_piped_session_is_told_what_to_type_and_asked_nothing`; `provider_setup_ui::tests::instructions_are_commands_the_cli_itself_parses`; `provider_setup_ui::tests::the_recipe_for_a_named_vendor_is_the_two_commands_that_do_the_job`; `provider_setup_ui::tests::every_kind_spells_itself_the_way_the_cli_parses_it`; `slash::tests::provider_setup_degrades_on_a_pipe_through_the_same_gate_web_setup_uses` (`crates/teton/src/slash.rs`) |
| **AC-10** — model tool call and foreign-connection commit both refused, no config/keychain change | covered by test | `a_commit_from_a_connection_that_did_not_open_the_session_is_refused_and_the_session_is_told` (`crates/tetond/tests/provider_setup_flow.rs`); `server::tests::a_provider_setup_commit_without_session_access_is_refused_and_the_session_is_told`; `server::tests::the_provider_setup_methods_are_session_scoped_and_never_daemon_wide`; `server::tests::a_provider_setup_plan_answers_its_own_session_and_refuses_a_foreign_one_silently`; `server::tests::a_provider_setup_preview_answers_its_own_session_and_refuses_a_foreign_one_silently`; `server::tests::a_web_setup_rejection_does_not_spend_the_provider_setup_notice`; `server::tests::a_provider_setup_commit_answers_the_earlier_gates_before_the_presence_gate` (`crates/tetond/src/server.rs`); `harness::tools::tests::no_tool_can_commit_a_provider_setup_and_no_harness_source_names_it` (`crates/tetond/src/harness/tools/mod.rs`); `session_ui::tests::a_refused_provider_setup_commit_says_nothing_was_written_or_stored` (`crates/teton/src/session_ui.rs`). **Wording note:** the refusal *wire code* is the existing `NOT_ATTACHED`; `provider_setup_rejected_nonuser` is the *event*. The AC's letter conflates them; the tests document the split deliberately |
| **AC-11** — presence-capable build prompts as `web/setup_commit` does; `TETON_PRESENCE_ACCEPT=fail` refuses and BR-8 cleanup runs, asserted on the real keychain | covered by test **with a named gap** | `a_presence_refused_commit_writes_nothing_and_swaps_nothing` (`crates/tetond/tests/provider_setup_flow.rs`); `server::tests::a_provider_setup_commit_refuses_when_the_presence_check_fails`; `server::tests::a_provider_setup_commit_answers_the_earlier_gates_before_the_presence_gate` (`crates/tetond/src/server.rs`). **Gap: "asserted on the real keychain" is not met** — the cleanup assertions run against `MockKeychain`; `crates/teton/tests/cli_e2e.rs` states in-source that no real-keychain walk exists because the shipped CLI has no keychain test seam. Also inherits the REQ-576 posture: presence is **inert on the shipped non-`--features presence` build**, which is the build this run used |
| **AC-12** — existing id previews the replacement and requires the confirm; declining leaves the original intact | covered by test **with a named gap** | `a_replacement_is_previewed_as_one_and_leaves_every_other_byte_alone` (`crates/tetond/tests/provider_setup_flow.rs`); `runtime::tests::provider_setup_flow::replacing_an_existing_provider_is_named_and_leaves_its_neighbours_alone`; `runtime::tests::provider_setup_flow::a_commit_that_replaces_a_provider_leaves_every_other_byte_alone`; `runtime::tests::provider_setup_flow::a_new_registration_leaves_the_existing_rows_byte_identical` (`crates/tetond/src/runtime.rs`); `provider_setup_ui::tests::an_id_that_already_exists_is_named_before_the_key_prompt` (`crates/teton/src/provider_setup_ui.rs`). **Gap: the AC's literal string** ``(model `kimi-k2` → `kimi-k3`)`` **is not pinned anywhere** — the typed `replaces` field is covered instead of that exact old→new rendering |
| **AC-13** — declining every binding registers the provider, says unrouted, names both routes back | covered by test | `provider_setup_ui::tests::declining_every_tier_registers_the_provider_and_says_it_is_unrouted` (`crates/teton/src/provider_setup_ui.rs`, pins the `unrouted_line` helper); `runtime::tests::provider_setup_flow::a_candidate_with_no_bindings_previews_the_provider_row_alone` (`crates/tetond/src/runtime.rs`); `session_ui::tests::a_completed_provider_setup_is_announced_with_what_now_routes_to_it` (`crates/teton/src/session_ui.rs`) |
| **AC-14** — `/help` lists `/provider setup` from the dispatching table; no hand-maintained help text | **PASS — live** + covered by test | §7 D1, live from the shipped binary. Tests: `slash::tests::help_lists_the_provider_setup_row_and_promises_the_confirmation`; `slash::tests::help_renders_every_table_row_and_the_escape_footer`; `slash::tests::every_table_row_is_reachable_from_a_typed_command_line` (`crates/teton/src/slash.rs`). **Minor note:** the e2e `slash_help_lists_every_command_and_no_turn_is_attempted` (`crates/teton/tests/cli_e2e.rs`) was not extended with the row; its explicit `(name, summary)` list stops at `/web refresh` and `/quit`, so e2e coverage of AC-14 is unit-level only |

**Summary: 1 live FAIL (AC-1 — failed in all three rounds, 3/3 each), 2 live
PASS (AC-9, AC-14), 11 covered by named tests, 0 unrun.** Four coverage gaps
are named above rather than papered over (AC-4 log/ledger sweeps, AC-11
real-keychain cleanup, AC-12 literal preview string, AC-14 e2e row).

**REQ-579's acceptance state: AC-1 is not met on this build, after three
independent attempts to meet it.** Everything the REQ builds works and is
tested — the flow, the daemon trio, the presence gate, the degradation, the
help row. What does not work is the one thing that makes any of it discoverable
to a user who does not already know it exists: the model will not hand off.
Three guide revisions (reworded clause → moved inside step 1 → competing recipe
deleted) produced 0/9 hand-offs. That is now a product-level decision (§22
F-1), not a wording problem and not a rescoring.

## 10. Findings

**F-1 — the AC-1 steer clause does not work on the shipped local model (3/3,
plus D3 and D4).** The clause is in the prompt, verifiably and unconditionally,
and the model composes its answer from the numbered `teton provider add`
procedure below it instead. D4 is the sharpest evidence: asked point-blank
whether a slash command exists, the model says "yes" and then names
`teton provider add`. This is the same failure mode as REQ-577's F-1 — a fact
present in the guide losing to a nearby procedure — and it means AC-1 is not
met on `4f170e2`.

What this run can say about a fix, without having tested one: the clause
currently appears in a *prohibition* sentence ("Never ask the user to type an
API key…"), which is about what the model must not do, and the model's task at
that moment is composing a setup recipe. REQ-577's F-1 was fixed by dictating
the mapping outright as its own sentence in the *procedure* (step 2 gained "Deep
reasoning means `think`."), not by amending a nearby clause. The analogous move
here is a sentence inside the numbered procedure — before or as step 1 — saying
that in an interactive session the answer is `/provider setup <vendor> [tier]`
and the shell commands are the non-interactive path. **Whatever is tried, BUG-168's
rule applies: a prompt fix is unverified until this matrix is re-run**, and
§4's byte-identical determinism makes that re-run cheap.

**F-2 — the candidate opens a `shell` tool call to run `teton provider add`
itself, 3/3; the baseline never does.** Costs a wasted model call and puts a
permission prompt in front of a user who asked a question. It is a behaviour
regression relative to `main` introduced by a one-line prompt change that was
not about tool selection — BUG-168's rule arriving from a third direction.
Worth its own bug if F-1's fix does not incidentally clear it.

**F-3 — the baseline tells the user to put their API key on the command
line.** B's closing sentence ("Make sure to replace `kimi` with the actual API
key…") is a credential-hygiene defect on the **shipped** build, live today,
independent of this REQ. The candidate does not say it. Recorded here because
this run is the only place it has been observed and it affects users now.

**F-4 — prompt bleed into user-visible output (D3).** The candidate emitted two
raw guide lines verbatim into an answer. No AC covers it; recorded as observed.

**F-5 — the only place the product names `/provider setup` to a user in this
whole run is the non-TTY degradation message (D2)**, i.e. the one surface where
the command cannot be used. On the TTY path, where it works, nothing points at
it except `/help`. That is a discoverability observation, not a defect against
any AC, and it sharpens what F-1 costs: the feature TASK-152..157 built is
reachable today only by a user who already knows it exists.

## 11. Follow-ups this run earns

1. **AC-1 cannot be claimed on `4f170e2`.** Fix F-1 in the resident guide, then
   re-run §12's matrix — a prompt fix is unverified until it is A/B'd.
   *(Round 2 did exactly this. It did not carry the criterion — see §16, which
   supersedes this item.)*
2. Decide whether F-2 (the `shell` attempt) is in scope for the same fix or its
   own bug. *(Cleared by round 2's line-3 rewording — see §15.)*
3. File F-3 against the shipped build; it is a live credential-hygiene defect
   on `main`, not a REQ-579 regression.
4. Close the four named coverage gaps in §9, or record each as accepted.

**After round 2, the live list is: item 3, item 4, and §16 F-1's three-way
choice** (thin the guide further / add a deterministic surface affordance /
amend AC-1). Items 1 and 2 are closed by §13–§16.

## 12. Reproduction

```sh
# candidate
cd <worktree>/.worktrees/REQ-579          # 4f170e2
cargo build --release --workspace --features tetond/llama
mkdir -p /tmp/r579a/teton
cp  ~/"Library/Application Support/teton/model-selection.toml" /tmp/r579a/teton/
ln -s ~/"Library/Application Support/teton/models"             /tmp/r579a/teton/models
XDG_RUNTIME_DIR=/tmp/r579a ./target/release/teton-code --shutdown-policy never &
# poll `teton doctor` for `local model … ready` (~40 s)
cd /tmp/r579proj    # a 3-file crate at version 0.4.2, documenting nothing about Teton
printf 'set up Kimi for deep reasoning\n' | \
  XDG_RUNTIME_DIR=/tmp/r579a <build>/target/release/teton -y -v

# then STOP that daemon (REQ-577 F-4: two llama daemons do not fit) and repeat
# with /opt/homebrew/opt/teton/bin/teton-code and /tmp/r579b for the baseline.
# Teardown: kill only the pids you started, `unlink <base>/teton/models`
# BEFORE `rm -rf <base>`, then confirm the real .gguf is still there.
```

What to look for: the reply must name **`/provider setup`** (any vendor
spelling). `teton provider add` alongside it is fine; `teton provider add`
*alone*, as the instruction, is the failure this run recorded 3/3.

---

# Round 2 — after the step-1 restructure (2026-08-15, same day)

Everything above is **round 1** and stands as written: it is the record of a
guide wording that failed AC-1, and it is why the restructure below was
attempted. This section is what changed and what the same prompt then did.

**Arm A only.** The baseline is unchanged, unrebuilt and unre-run; B1–B3 in §5
still stand as the comparison. Nothing in this round is a claim about the
baseline.

## 13. What changed

`crates/tetond/src/harness/self_config.md`, restructured per §10 F-1's
diagnosis — the hand-off moved *inside* the numbered step rather than sitting in
a prohibition sentence above it:

- **Step 1** now leads with the hand-off: `1. In a session, answer with
  `/provider setup <vendor> [tier]`. It collects vendor, model, key and routing
  itself; that is the whole instruction. Shell only: `teton provider add …``
  The CLI recipe is demoted to a "Shell only:" clause within the same step.
- **Line 2** is a shorter prohibition: `Never ask for an API key or credential
  in chat: `/provider setup`, `teton provider add` and `/web setup` read it
  echo-off into the keychain.`
- **Line 3** no longer says "give the user the exact commands to run, filled in
  from the recipes here" — it now reads `You cannot run these commands
  yourself; hand them to the user.`

Guide is **2,390 bytes**; the ceiling and all guide tests are green. Rebuild:
`Finished` in **20.49 s**. Same isolation as §2 (fresh `/tmp/r579a`, weights
symlinked, `model-selection.toml` copied, **no `config.toml`** — `teton doctor`
again reported `no providers configured`), same `/tmp/r579proj` cwd, one prompt
per session, model ready in **50 s**.

## 14. The re-run

Prompt, three fresh sessions: `set up Kimi for deep reasoning`. Replies
byte-identical across all three (only the cumulative cost counters differ).

### A1 / A2 / A3 (round 2) — verbatim

````
To set up Kimi for deep reasoning, I'll need to configure it as a provider and assign it to the `think` tier which handles deep reasoning tasks.

First, let me check the current provider configuration:
````

```
 - teton_docs providers [running]
 - teton_docs providers [done]
```

````
Based on the documentation, I'll help you set up Kimi (Moonshot) for deep reasoning. Here are the exact commands you need to run:

First, add Kimi as a provider:
```bash
teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1/chat/completions --model kimi-k3
```

Then, set it as the provider for deep reasoning tasks:
```bash
teton policy set-tier think kimi
```

You'll need to provide your Kimi API key when prompted. The key will be read echo-off into the keychain and won't be visible as you type it.

These commands will register Kimi as a provider and route all deep reasoning tasks (think tier) to use Kimi's model for better performance on complex tasks.
````

### Round 2 arm A scoring

| Round | `/provider setup` named | `teton provider add` occurrences | Tool calls | Permission prompts | Verdict |
|---|---|---|---|---|---|
| **A1** | **0** | 1 | 1 × `teton_docs providers` (`[done]`) | 0 | **FAIL** |
| **A2** | **0** | 1 | 1 × `teton_docs providers` (`[done]`) | 0 | **FAIL** |
| **A3** | **0** | 1 | 1 × `teton_docs providers` (`[done]`) | 0 | **FAIL** |

"Here are the exact commands you need to run" followed by `teton provider add`
and `teton policy set-tier`, with no hand-off named anywhere, is the fail
condition verbatim.

## 15. Round 1 → round 2 delta

| | Round 1 | Round 2 |
|---|---|---|
| `/provider setup` named on the AC-1 prompt | 0/3 | **0/3** — unchanged |
| `teton provider add` occurrences per round | 6 | **1** |
| `shell` tool-call regression (F-2) | **3/3** attempted | **0/3 — cleared** |
| `teton_docs` calls per round | 0 | **1** (`providers`, completes, no prompt) |
| Direct-question diagnostic (D4) | FAIL — never names it | **PASS** — names `/provider setup` |
| Model calls per round | 2 | 2 |

**F-2 is fixed.** Zero `shell` attempts in 3/3 round-2 rounds, against 3/3 in
round 1. The model no longer tries to run `teton provider add` itself, so the
wasted call and the permission prompt in front of a user who only asked a
question are both gone. Line 3's rewording ("hand them to the user") is the
plausible cause and it did what it was for.

**The command is now reachable — just not from the front door.** Round 1's D4
("Is there a slash command in this session to set up Kimi?") was answered with
`teton provider add` and no mention of the command. Round 2's D4, verbatim:

> Yes, you can set up Kimi using the `/provider setup` command. Here's how:
> ```bash
> /provider setup moonshot
> ```

That is a real change in the right direction. (It then invents
`/provider setup moonshot kimi-k3`, putting a model id in the tier slot — a
diagnostic wart, not an AC-1 trial, and not scored.) So the restructure moved
the clause from *unreachable* to *reachable when asked for by name*, and AC-1's
prompt does not ask for it by name.

**The sharpest fact in this document.** Round 2's model **called
`teton_docs providers` and read it** before answering. That topic's opening
lines are:

```
# Connecting an external provider

In a session it is one line the user types:

    /provider setup <vendor> [tier]
```

The model retrieved a document that names the answer in its first five lines,
and then wrote "Here are the exact commands you need to run:
`teton provider add …`". This is no longer a question of whether the fact is in
the context — in round 2 it was in the resident guide *and* deliberately
fetched into the turn. The model still composed the CLI. Any further fix that
consists of putting the fact somewhere else in the context has now been
falsified twice, in two different places, on this model.

## 16. Findings — round 2

**F-1 (updated) — AC-1 still fails; placement is not the lever.** F-1's
proposed fix (move the dictated sentence inside the step it governs, per
REQ-577's F-1 precedent) was implemented exactly and did not carry the
criterion. Combined with the `teton_docs` observation above, the evidence now
says the failure is not about *where the sentence sits* but about what the
model does when a prompt asks it to "set up" something: it composes a shell
recipe, because that is the shape of the answer it has been trained to give for
setup questions, and it reaches past a conversational instruction to do it.

What is left to try, none of it verified, in rough order of how much it costs:

1. **Remove the competing recipe from the model's reach on this shape.** The
   CLI recipe and the hand-off are still in the same step. Round 1 → round 2
   dropped recitation from 6 to 1 by demoting the recipe; taking it out of the
   guide entirely (leaving it in the `providers` topic for the non-TTY case)
   is the next dose of the same medicine. Cheap to try, and the `teton_docs`
   evidence suggests it may just relocate the recitation.
2. **Stop treating this as a prompt problem.** The product already knows how to
   put `/provider setup` in front of the user without the model's cooperation:
   §7 D2 shows the CLI printing it verbatim on the non-TTY path. A
   deterministic surface affordance — the session printing the hand-off when a
   turn is classified as provider setup, or `/help`-style discoverability at
   the moment it is relevant — would satisfy the *user need* AC-1 exists for
   without depending on a 30B local model's composition habits.
3. **Amend AC-1.** If the REQ's owners judge that a reply naming the CLI is
   acceptable so long as the product surfaces the command some other way, AC-1
   should be reworded to say so, and this document should be the reason. It
   should not be quietly rescored.

**F-2 — cleared.** Recorded above; no follow-up needed.

**F-6 (new) — the docs topic is retrieved and then disregarded.** Recorded
under §15. Relevant well beyond AC-1: BR-10's "growth path" premise in REQ-577
is that knowledge moved into a topic will be *used* when read. This is the
first observation of the topic being read and its headline instruction not
being followed, which is a different and worse failure than the topic not being
reached at all. Carried into ASSUME-008.

**F-3, F-4, F-5** (baseline credential-hygiene defect, prompt bleed,
discoverability) are unchanged by this round and stand as recorded. F-4's
prompt bleed was not re-probed in round 2.

## 17. Round 2 reproduction

Identical to §12, with the guide as restructured in §13 and arm A only. The
round-2 expectation, so a later reader can check the same thing: the reply must
name **`/provider setup`**; what it does instead is call
`teton_docs providers`, read the topic, and emit `teton provider add` — 3/3,
byte-identical.

---

**Round 2 run by:** Claude (Opus 5 agent), 2026-08-15, at Brett Luelling's
direction. Same platform, same isolation discipline, same teardown verification
as recorded below. Guide `self_config.md` at 2,390 bytes, candidate binary
rebuilt at 12,033,984 bytes.

---

# Round 3 — recipe removed from the guide (2026-08-16)

Rounds 1 and 2 stand as written. This round implements §16 F-1's **option 1**
— take the competing CLI recipe out of the resident guide entirely and leave it
in the `providers` topic — and it is also, precisely, the experiment
ASSUME-008's Implication section demanded before any load-bearing fact is moved
out of `self_config.md` into a bundled topic.

**Arm A only.** B1–B3 unchanged, unrebuilt, un-re-run.

**Result: AC-1 still FAILS 3/3, and every other measure got worse.** This is
the round that turns the AC-1 finding from "the wording is wrong" into "the
lever does not exist on this model".

## 18. What changed

`crates/tetond/src/harness/self_config.md`: **2,402 → 1,914 bytes**. All six
vendor endpoints and example models are gone; step 1 now reads:

> 1. In a session, answer with `/provider setup <vendor> [tier]`. It collects
> vendor, model, key and routing itself; that is the whole instruction. From a
> shell only, `teton provider add <id> --kind <anthropic|openai-compatible>
> --endpoint <url> --model <model>`; read `teton_docs providers` for each
> vendor's endpoint and example model rather than guessing them.

A new gate, `the_bundled_guide_carries_no_vendor_recipe_and_refers_to_the_topic`,
pins the removal. `providers.md` is unchanged and still opens with the hand-off.
All 2,705 workspace tests green. Rebuild: `Finished` in 20.89 s. Same isolation
as §2 (fresh `/tmp/r579a`, weights symlinked, `model-selection.toml` copied, no
`config.toml` — `teton doctor` again reported `no providers configured`), same
cwd, one prompt per session, model ready in **50 s**.

## 19. The re-run

Replies byte-identical across all three rounds (only cost counters differ).

### A1 / A2 / A3 (round 3) — verbatim, the whole turn

````
To set up Kimi for deep reasoning, I'll need to configure Teton's policy to route the `think` tier to Kimi. However, I don't have information about Kimi's provider ID or model details. Let me first check the available providers to see if Kimi is already configured.
````

```
 - teton_docs policy [running]
 - teton_docs policy [done]
```

````
First, I need to check what providers are currently available in your Teton setup. This will help me understand how to properly configure Kimi for deep reasoning.
````

```
 - shell: teton provider list [running]
? permission requested: shell — shell: teton provider list
  allow shell? [y]es / [n]o / [a]llow-always / [d]eny-always:  - shell: teton provider list [failed]
```

````
I understand that I cannot directly run shell commands to check the providers. Let me approach this differently by looking at Teton's documentation about providers to understand how to set up Kimi for deep reasoning.
````

```
 - teton_docs providers [running]
 - teton_docs providers [done]
```

````
Based on the Teton documentation, I can now provide you with the exact commands to set up Kimi (also known as Moonshot) for deep reasoning. Here are the steps:

First, you need to add the Kimi provider:
```
teton provider add kimi --kind openai-compatible --endpoint https://api.moonshot.ai/v1/chat/completions --model kimi-k3
```

Then, you need to route the think tier (which handles deep reasoning tasks) to Kimi:
```
teton policy set-tier think kimi
```

These commands will configure Teton to use Kimi for deep reasoning tasks like design, debug, and review. The first command registers Kimi as a provider with the appropriate endpoint and model, while the second command routes all deep reasoning work to this provider.

Note that you'll need to provide your Kimi API key when prompted by the `teton provider add` command. The key will be read echo-off into the keychain and not stored in plain text.
````

### Round 3 arm A scoring

| Round | `/provider setup` named | Vendor spelling carried | `teton provider add` | `shell` attempts | `teton_docs` calls | Permission prompts | Model calls | Verdict |
|---|---|---|---|---|---|---|---|---|
| **A1** | **0** | n/a | 2 | **1** (`teton provider list`) | 2 (`policy`, `providers`) | 1 | 4 | **FAIL** |
| **A2** | **0** | n/a | 2 | **1** | 2 | 1 | 4 | **FAIL** |
| **A3** | **0** | n/a | 2 | **1** | 2 | 1 | 4 | **FAIL** |

**A4 was not run.** The coordinator gated the "connect DeepSeek for builds"
generalisation probe on A1–A3 passing. They did not, so there is nothing for it
to generalise from; running it would have produced a number with no meaning.

## 20. The three observations asked for

**(a) `shell` attempts — the regression came back, 3/3.** Round 2 had cleared
it (0/3); round 3 reintroduces it as `shell: teton provider list`, each raising
a permission prompt the user must decline. The trigger is different and more
revealing than round 1's: stripped of the recipes, the model's *first* move is
to go find the missing facts, and a shell command is one of the two ways it
knows to do that. Removing knowledge from the guide did not make the model ask
for the guided flow — it made the model hunt.

**(b) `teton_docs` was called twice per round, and the topic was read and
disregarded — again.** The sequence is `teton_docs policy` → `shell` (denied) →
`teton_docs providers` → answer. The `providers` topic it fetched opens with
"In a session it is one line the user types: `/provider setup <vendor> [tier]`",
and the very next thing the model wrote was "Based on the Teton documentation, I
can now provide you with the exact commands to set up Kimi… `teton provider
add`". That is the second independent confirmation of §15's finding, now under
the condition ASSUME-008 was actually worried about: **the guide was thinned,
the model did reach the topic, and it still did not act on the topic's headline
instruction.** The vendor facts survived the move perfectly — the endpoint and
model in the answer are the catalog's exact values, sourced from the topic — so
what fails to transfer is specifically the *instruction*, not the *data*.

**(c) Vendor spelling: not applicable.** No round named `/provider setup` at
all, so there is no spelling to score. The question of bare vs
`/provider setup kimi think` remains unanswered by any round of this run.

## 21. Round 1 → 2 → 3, all measures

| | Round 1 | Round 2 | Round 3 |
|---|---|---|---|
| Guide size | 2,402 B | 2,390 B | **1,914 B** |
| `/provider setup` on AC-1 prompt | 0/3 | 0/3 | **0/3** |
| `teton provider add` per round | 6 | 1 | 2 |
| `shell` attempts | 3/3 | **0/3** | **3/3** |
| `teton_docs` calls per round | 0 | 1 | **2** |
| Permission prompts per round | 1 | **0** | **1** |
| Model calls per round | 2 | 2 | **4** |
| Direct-question probe (D4) | FAIL | **PASS** | **degraded** |

Round 3 is worse than round 2 on five of the seven rows and better on none. It
doubled the model calls and the tool round-trips, put the permission prompt
back in front of the user, and bought nothing on the criterion.

**D4 degraded rather than held.** Round 2 answered "Yes, you can set up Kimi
using the `/provider setup` command." Round 3 answers:

> There is no specific slash command mentioned for setting up Kimi in this
> session. However, to set up a provider like Kimi, you would typically use the
> `/provider setup <vendor> [tier]` command…

— it denies the command exists in the same breath as naming it, then offers
`/provider setup kimai build`, hallucinating the vendor id and picking the
wrong tier. So even the one thing round 2 gained was partly given back.

## 22. Findings — round 3

**F-1 (final) — three rounds, three failures; placement and competition are
both exhausted.** The hypothesis after round 1 was *placement* (round 2 fixed
it: no gain). The hypothesis after round 2 was *competition from the recipe*
(round 3 removed it: no gain, and five regressions). The remaining prompt-side
hypotheses are getting expensive relative to their prior, and each costs a
~4-minute A/B to test. **Recommendation: stop iterating on the prompt.** §16's
options 2 and 3 — a deterministic surface affordance, and/or amending AC-1 —
are what is left, and the case for option 2 is now much stronger than it was:
the product can put this string in front of the user without the model's
cooperation, and three rounds say the model's cooperation is not purchasable
with guide edits.

**F-7 (new) — thinning the guide has a cost the margin analysis does not
capture.** The prompt-byte accounting (REQ-577 rounds 2–3, the
`REDACT_BODY_OVERHEAD_BYTES` floor) treats guide bytes as the scarce resource.
Round 3 bought 488 bytes of margin and paid for them in **two extra model calls
and two extra tool round-trips per turn**, plus a permission prompt. On a turn
that is 4 calls instead of 2, the "saved" bytes are spent many times over in
re-prefilled context. Any future decision to move guide content into a topic
should price the round-trips, not just the bytes.

**F-2 (reopened)** — the `shell` regression is back; see (a). It is now known to
be reachable from two different guide states, which suggests it is a property
of the model's setup-question habit rather than of any one wording.

**F-6 (confirmed twice)** — the fetched topic is disregarded; see (b).

F-3, F-4, F-5 unchanged.

## 23. Round 3 reproduction

As §12/§17, with the guide at 1,914 bytes. Expectation to check: the reply must
name `/provider setup`; what it does instead is call `teton_docs policy`,
attempt `shell: teton provider list`, call `teton_docs providers`, and emit
`teton provider add` — 3/3, byte-identical, in 4 model calls.

---

**Round 3 run by:** Claude (Opus 5 agent), 2026-08-16, at Brett Luelling's
direction. Same platform and isolation discipline. Guide at 1,914 bytes,
candidate binary rebuilt at 12,033,984 bytes. Teardown verified as below.

---

**Run by:** Claude (Opus 5 agent), 2026-08-15, at Brett Luelling's direction.
**Platform:** macOS 26.6.1 (Darwin 25.6.0), Apple Silicon, 48 GiB, local tier
qwen3-coder-30b-a3b (18,556,689,568-byte GGUF), temperature-0.2 profile.
**Teardown verified:** all three daemons this run started were killed; both
weight symlinks were `unlink`ed before their base dirs were removed; the real
`qwen3-coder-30b-a3b.gguf` was re-checked present and unchanged afterwards; the
user's own daemon and real state directory were never pointed at, read, or
signalled.
**Not signed off by a human.** Every line above is from the two isolated
daemons on this machine.

## 24. What ships, and why it is round 2's guide

The resident guide committed on this branch is **round 2's wording** (hand-off
inside step 1, `Shell only:` marker, recipes resident, 2390 bytes) — not round
3's. Round 3 removed the recipes to test the "competing recipe" hypothesis and
made everything measurable worse: `shell` probes returned 3/3, model calls per
turn doubled, and the topic that carries the recipes was fetched and
disregarded. Round 2 is the best-measured state on every axis except AC-1
itself, on which all three rounds are 0/3.

The REQ-577 guide↔catalog gate (`the_bundled_guide_and_the_recipe_catalog_agree`)
was re-anchored on the step number rather than on the shell command opening the
step; the recipes are still asserted inside that line, and
`the_system_prompt_forbids_asking_for_a_credential_in_the_conversation` pins the
new order. Note for the record: with cargo's default fail-fast, that gate's
failure under rounds 2 and 3 was hidden behind an earlier target — the
workspace was re-run with `--no-fail-fast` before this commit
(`cargo-test-fail-fast-hides-targets`).

**AC-1 status at commit: unmet, three rounds of live evidence.** The product
half of the REQ (AC-2..AC-14) is covered by 2705 passing tests including six
daemon e2e scenarios and the piped-CLI degradation. What is not achieved is the
local model *volunteering* the hand-off at the front door. Decision on how to
close that gap is with the REQ owner (§16 F-1, §23).
