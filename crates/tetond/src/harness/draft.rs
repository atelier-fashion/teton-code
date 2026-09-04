//! The `draft` duty: writing a repository's `TETON.md` from evidence gathered
//! off its own tree (REQ-613 BR-4, ADR-4).
//!
//! ## Not a tool's duty, and not a turn's either
//!
//! `triage` and `shell` hang off the tool that owns them; `title` and `compact`
//! hang off the session and the context. This one hangs off a *repository*: it
//! runs at most once for a project, before or between turns, and what it
//! produces is a file on disk rather than anything the turn can see. Its call
//! site is the generation pipeline in [`crate::repo_context`], which is also the
//! only thing that knows the evidence was gathered and the gate was answered.
//!
//! ## Why it is bound to `think` and not to `digest`
//!
//! The file is written **once** and then read at the start of every session
//! afterwards, which inverts the usual arithmetic: the expensive model is the
//! cheap choice here, because its cost is amortized over every future turn while
//! a bad draft is re-read forever. `digest` was the obvious host and is the wrong
//! one — its local default is right for digests and wrong for this (REQ-613
//! OQ-2). So [`Category::Draft`] is a category of its own, bound to `think` at
//! compile time and moved by an ordinary policy row
//! (`teton policy set-category draft local`) for a user who wants it local.
//!
//! ## What is `draft`-specific, and what is not
//!
//! Exactly what ADR-3 allows a duty module to own: the one-line resolver in
//! [`crate::runtime`] that names the category literally, [`DRAFT_DUTY`] with its
//! [`DRAFT_OUTPUT_MAX_BYTES`] ceiling, and the prompt builder below with its
//! [`DRAFT_OUTPUT_CONTRACT`]. Routing, egress scoping, ceiling enforcement,
//! `route_decided` emission and the failure shape are the shared seam's, once,
//! for every duty.
//!
//! (On writing about that resolver: describe it, never spell it. The
//! `declared, no call site yet` marker in [`crate::call_sites`] is derived by
//! scanning the daemon's source as **text**, so the spelling of a
//! category-resolving router call inside a doc comment registers as a call site.
//! ADR-9, learned the hard way in REQ-561 TASK-058.)
//!
//! ## One cap, one cutter
//!
//! The ceiling is REQ-612's [`REPO_CONTEXT_MAX_BYTES`] — the same number the
//! loader will measure this file against on the next session, because a draft
//! written past the cap would be silently truncated the moment it is read back,
//! and a file that arrives pre-truncated is worse than a shorter one written on
//! purpose. [`bound_answer`] therefore strips and cuts with REQ-612's own
//! functions rather than spelling a second cutter (LESSON-456): two cutters
//! rounding differently is how "the file fits" stops being true on one of the
//! two surfaces that believe it.

use teton_protocol::Category;

use crate::repo_context::render::{strip_for_prompt, truncate_at_line_boundary};
use crate::repo_context::REPO_CONTEXT_MAX_BYTES;

use super::duty::DutyKind;

/// Byte ceiling on what a `draft` duty may return — REQ-612's cap, read rather
/// than restated (REQ-613 ADR-4).
///
/// The **loosest** of the duty ceilings, and for the one reason that justifies
/// it: every other duty's answer is consumed once and discarded, so its ceiling
/// is a guard against a runaway stream. This one's answer is written to disk and
/// read back into the system prompt at the start of every later session, so its
/// ceiling is the *product* decision about how much of a session's budget
/// repository notes may occupy — which REQ-612 already made, at
/// [`REPO_CONTEXT_MAX_BYTES`].
///
/// Derived from that constant rather than written down beside it: two numbers
/// describing one budget are two numbers that can drift, and the drift here is
/// invisible — a draft written to a larger ceiling loads back truncated, with a
/// marker, on a surface nobody was watching at the time it was written.
pub const DRAFT_OUTPUT_MAX_BYTES: usize = REPO_CONTEXT_MAX_BYTES;

/// The `draft` duty on the shared seam: its category and its output ceiling.
///
/// One `const` per category, stated once and read by every construction site —
/// the resolver in [`crate::runtime`] and the tests.
pub const DRAFT_DUTY: DutyKind = DutyKind::new(Category::Draft, DRAFT_OUTPUT_MAX_BYTES);

/// The draft duty's output contract, verbatim: the sentence that says what shape
/// the answer takes, before the evidence it is drawn from.
///
/// Exported for the reason every other duty's contract is: it is how the
/// CI/offline stand-in engine recognizes a duty prompt and answers it *without
/// consuming a scripted turn* — a duty is not a turn (REQ-561 BR-10). The
/// recognition arm itself lands with the call site (REQ-613 TASK-383); until
/// then no scripted session can issue this duty, so there is no scripted block
/// for it to eat.
///
/// A full, distinctive sentence rather than a short phrase, because the
/// recognizer sees the *whole* rendered prompt and this one embeds a repository
/// listing — so a generic phrase could plausibly arrive inside the evidence
/// being summarized.
pub const DRAFT_OUTPUT_CONTRACT: &str =
    "Reply with the finished Markdown file and nothing else — no preamble, no \
     commentary after it, and no code fence around the whole answer.";

/// The five sections a generated file carries, in the order it carries them
/// (REQ-613 OQ-3, resolved *yes*).
///
/// Fixed rather than left to the model so that generated files look alike across
/// repositories: a reader who has met one `TETON.md` knows where to look in the
/// next, and a reviewer comparing two drafts is comparing content rather than
/// arrangement. The order is the order a newcomer needs the answers in — what is
/// this, where is it, how do I run it, what are the rules, where do I start.
///
/// `(heading, what the section is for)`. One array, so the prompt cannot list
/// them in one order and a later check assert another.
const DRAFT_SECTIONS: [(&str, &str); 5] = [
    (
        "Purpose",
        "what this project is and who or what it is for, in two or three sentences",
    ),
    (
        "Layout",
        "the directories that matter and what lives in each — name them, do not describe \
         the whole tree",
    ),
    (
        "Build & test",
        "how the project is built and how its tests are arranged, stated only as far as \
         the evidence below already states it",
    ),
    (
        "Conventions",
        "the rules this repository holds itself to that a newcomer would otherwise break",
    ),
    (
        "Where to look",
        "where to start reading for the two or three commonest kinds of change",
    ),
];

/// The evidence a draft is written from, as this module needs to read it.
///
/// A deliberately minimal, borrowed view rather than the gatherer's own type:
/// this module is built alongside the walker that produces the evidence
/// (REQ-613 TASK-382), and a prompt builder that named the gatherer's struct
/// could not be written or tested until that struct existed. The four fields
/// below are the whole of what the prompt renders, so the adapter at the call
/// site is a projection and not a translation.
pub struct DraftInputs<'a> {
    /// The repository listing, already walked, cut and rendered to text.
    pub tree: &'a str,
    /// `(name, contents)` for each document member — a README and its
    /// neighbours, already bounded by the gatherer.
    pub documents: &'a [(String, String)],
    /// `(name, contents)` for each entry-point member — a manifest, a `main`,
    /// already bounded by the gatherer.
    pub entry_points: &'a [(String, String)],
    /// What the gatherer had to leave out, as a sentence, or `None` when it left
    /// nothing out.
    ///
    /// Rendered into the prompt rather than swallowed (REQ-586 BR-7): a model
    /// told the tree was cut at a depth writes "the crates listed above" instead
    /// of claiming the listing is complete.
    pub cut: Option<String>,
}

/// Build the draft prompt from `inputs` (REQ-613 ADR-4).
///
/// Three things it states and one it does not.
///
/// It states the **section order**, from [`DRAFT_SECTIONS`], because OQ-3 was
/// resolved in favour of files that look alike across repositories. It states
/// the **byte budget**, because a bound the model cannot see is a bound it will
/// overrun and lose its ending to — and the ending is the section a newcomer
/// reads last and needs most. It states the **audience**: this is the file a new
/// contributor opens first, not a summary written for the machine that asked for
/// it.
///
/// What it never asks for is **commands to run**. The model has no tools on this
/// call and nothing it writes is executed; a prompt that solicited "the commands
/// to set this up" would be inviting an answer whose most confident sentences
/// are the ones the evidence supports least. `Build & test` asks for what the
/// repository's own documents already say, which is a reading task rather than a
/// guess.
#[must_use]
pub fn build_prompt(inputs: &DraftInputs<'_>) -> String {
    let mut prompt = String::with_capacity(8_192);
    prompt.push_str(
        "You are writing the repository notes for a project you have just been shown. They \
         are the file a new contributor opens first: someone who has never seen this \
         repository should be able to read them and know what it is, where things are, and \
         where to start.\n\n",
    );
    prompt.push_str(
        "Write the file with exactly these sections, as `## ` headings, in this order and \
         with no others:\n",
    );
    for (n, (heading, purpose)) in DRAFT_SECTIONS.iter().enumerate() {
        prompt.push_str(&format!("{}. `## {heading}` — {purpose}.\n", n + 1));
    }
    prompt.push('\n');
    prompt.push_str(&format!(
        "The finished file must fit in {DRAFT_OUTPUT_MAX_BYTES} bytes, including a one-line \
         header the harness writes above it. Anything past that is cut at a line boundary \
         and lost, so spend the budget on the five sections rather than on an introduction.\n",
    ));
    prompt.push_str(
        "Write only what the evidence below supports. Where it is silent, say less rather \
         than guessing; you cannot see any more of this repository than what follows.\n",
    );
    prompt.push_str(DRAFT_OUTPUT_CONTRACT);
    prompt.push_str("\n\n");

    prompt.push_str("Repository listing:\n");
    prompt.push_str(inputs.tree);
    if !inputs.tree.ends_with('\n') {
        prompt.push('\n');
    }
    if let Some(cut) = &inputs.cut {
        prompt.push_str(cut);
        prompt.push('\n');
    }
    push_members(&mut prompt, "Documents", inputs.documents);
    push_members(&mut prompt, "Entry points", inputs.entry_points);
    prompt
}

/// Render one labelled group of evidence members, or a line saying there were
/// none.
///
/// The empty case is stated rather than omitted: a prompt that simply lacks an
/// "Entry points" block reads, to the model, like a repository whose entry
/// points were not looked for — and the answer it produces then hedges about
/// something the walker is certain of.
fn push_members(prompt: &mut String, label: &str, members: &[(String, String)]) {
    prompt.push('\n');
    prompt.push_str(label);
    if members.is_empty() {
        prompt.push_str(": none were found.\n");
        return;
    }
    prompt.push_str(":\n");
    for (name, body) in members {
        prompt.push_str("\n--- ");
        prompt.push_str(name);
        prompt.push_str(" ---\n");
        prompt.push_str(body);
        if !body.ends_with('\n') {
            prompt.push('\n');
        }
    }
}

/// The model's answer as the bytes that go to disk: `header`, then as much of
/// the stripped answer as fits under [`DRAFT_OUTPUT_MAX_BYTES`] (REQ-613 AC-8).
///
/// ## The header comes first and is charged to the budget
///
/// ADR-5's header line says which tier wrote the file, when, and what the walk
/// had to leave out — so it is the first thing a reader meets and the last thing
/// that may be dropped. It is written first and its bytes come out of the cap,
/// which is what makes "the file is at most 8,192 bytes" a statement about the
/// file rather than about the part of it the model produced.
///
/// ## Both passes are REQ-612's own
///
/// [`strip_for_prompt`] removes the control and bidi characters that cost bytes
/// and render as nothing, and [`truncate_at_line_boundary`] cuts whole lines.
/// Neither is re-spelled here (LESSON-456). The order matters and matches the
/// renderer's: strip first, so the cap is measured on the bytes that survive,
/// and cut last, so the cap is a bound on the result rather than on an
/// intermediate.
///
/// Cutting at a line boundary is what keeps a truncated draft *readable*: a file
/// that ends mid-sentence reads as corrupt, while one that ends after a whole
/// line reads as one that ran out of room.
///
/// A `header` that does not end in a newline gets one, because the alternative
/// is a header welded to the draft's first heading; the added byte is charged to
/// the budget like every other.
#[must_use]
pub fn bound_answer(answer: &str, header: &str) -> String {
    let mut out = String::with_capacity(DRAFT_OUTPUT_MAX_BYTES);
    out.push_str(header);
    if !header.is_empty() && !header.ends_with('\n') {
        out.push('\n');
    }
    let stripped = strip_for_prompt(answer);
    let room = DRAFT_OUTPUT_MAX_BYTES.saturating_sub(out.len());
    out.push_str(truncate_at_line_boundary(&stripped, room));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header of exactly `bytes` bytes, ending in a newline.
    fn header(bytes: usize) -> String {
        let mut h = "> Generated by Teton on 2026-09-03 (think tier). ".to_owned();
        assert!(h.len() < bytes, "the fixture header is already too long");
        h.push_str(&"h".repeat(bytes - h.len() - 1));
        h.push('\n');
        assert_eq!(h.len(), bytes);
        h
    }

    /// An answer of exactly `bytes` bytes made of `width`-byte lines, so a
    /// line-boundary cut can be predicted rather than discovered.
    fn lines_of(bytes: usize, width: usize) -> String {
        let mut text = String::with_capacity(bytes);
        while text.len() + width <= bytes {
            text.push_str(&"m".repeat(width - 1));
            text.push('\n');
        }
        text.push_str(&"m".repeat(bytes - text.len()));
        assert_eq!(text.len(), bytes);
        text
    }

    fn inputs<'a>(
        tree: &'a str,
        documents: &'a [(String, String)],
        entry_points: &'a [(String, String)],
    ) -> DraftInputs<'a> {
        DraftInputs {
            tree,
            documents,
            entry_points,
            cut: None,
        }
    }

    /// **AC-8.** A model answer of cap + 2,000 bytes, with a 120-byte header, is
    /// written at exactly the cap — header first, cut at a line boundary.
    ///
    /// The fixture is built so the landing is *exact* rather than approximate:
    /// the answer's lines are 8 bytes wide and `cap - 120` is a multiple of 8,
    /// so the last newline at or under the room available sits exactly at the
    /// room's last byte. An off-by-one in either direction — charging the header
    /// to the wrong side, or cutting at `cap` instead of `cap - header` — moves
    /// the result off that number.
    ///
    /// # Mutations
    ///
    /// Dropping the header from the budget (`saturating_sub(0)`) yields
    /// `cap + 120` and fails the length assertion; cutting with a second,
    /// naive `&stripped[..room]` instead of [`truncate_at_line_boundary`] passes
    /// the length assertion here but fails the line-boundary one on the ragged
    /// fixture below; writing the header last fails the prefix assertion.
    #[test]
    fn bound_answer_lands_exactly_at_the_cap_with_the_header_first() {
        let head = header(120);
        let room = DRAFT_OUTPUT_MAX_BYTES - head.len();
        assert_eq!(room % 8, 0, "the fixture's arithmetic assumes whole lines");
        let answer = lines_of(DRAFT_OUTPUT_MAX_BYTES + 2_000, 8);

        let bounded = bound_answer(&answer, &head);

        assert_eq!(
            bounded.len(),
            DRAFT_OUTPUT_MAX_BYTES,
            "the bounded draft must land exactly on REQ-612's cap"
        );
        assert!(
            bounded.starts_with(&head),
            "the header must be the first thing in the file"
        );
        assert!(
            bounded.ends_with('\n'),
            "the cut must land on a line boundary, not mid-line"
        );

        // And on a fixture whose lines do *not* divide the room, the cut is at
        // the last whole line under it rather than at the cap: shorter than the
        // cap, still whole lines, never mid-line.
        let ragged = lines_of(DRAFT_OUTPUT_MAX_BYTES + 2_000, 63);
        let bounded = bound_answer(&ragged, &head);
        assert!(bounded.len() <= DRAFT_OUTPUT_MAX_BYTES, "{}", bounded.len());
        assert!(bounded.len() > DRAFT_OUTPUT_MAX_BYTES - 63);
        assert!(bounded.ends_with('\n'));
        assert!(bounded.starts_with(&head));
    }

    /// The two smaller promises `bound_answer` makes: an answer that already
    /// fits is untouched, and the control characters REQ-612 strips are gone
    /// before the cap is measured — so a hostile draft cannot spend the budget
    /// on bytes that render as nothing.
    ///
    /// Mutation: dropping the `strip_for_prompt` call leaves the NULs in and
    /// fails both the content and the length assertion.
    #[test]
    fn a_short_answer_is_kept_whole_and_invisible_bytes_never_reach_disk() {
        let head = header(120);
        let short = "## Purpose\nA daemon and a CLI.\n";
        let bounded = bound_answer(short, &head);
        assert_eq!(bounded, format!("{head}{short}"));

        let hostile = format!("## Purpose\n{}A daemon.\n", "\0".repeat(500));
        let bounded = bound_answer(&hostile, &head);
        assert!(!bounded.contains('\0'), "a stripped byte reached the file");
        assert_eq!(bounded.len(), head.len() + hostile.len() - 500);
    }

    /// A header with no trailing newline is given one rather than welded to the
    /// draft's first heading — and the byte is charged to the budget, so the
    /// file still fits.
    #[test]
    fn a_header_without_its_newline_is_still_a_line_of_its_own() {
        let bounded = bound_answer("## Purpose\nA daemon.\n", "> Generated by Teton");
        assert!(bounded.starts_with("> Generated by Teton\n## Purpose\n"));

        let head = "> Generated by Teton";
        let over = lines_of(DRAFT_OUTPUT_MAX_BYTES + 2_000, 64);
        assert!(bound_answer(&over, head).len() <= DRAFT_OUTPUT_MAX_BYTES);

        // An empty header buys no stray blank line and spends nothing.
        assert_eq!(bound_answer("body\n", ""), "body\n");
    }

    /// **The prompt golden** (REQ-613 ADR-4, OQ-3): the five sections are named
    /// in order, the byte budget is stated, the audience is stated, and no
    /// command is asked for.
    ///
    /// Asserted as *positions* rather than as a `contains` per heading, because
    /// the claim OQ-3 resolved is about the **order**: a prompt that lists all
    /// five in the wrong sequence produces files that do not look alike, which
    /// is the whole thing the fixed order buys.
    ///
    /// # Mutations
    ///
    /// Reordering any two entries of `DRAFT_SECTIONS` fails the position chain;
    /// replacing the budget sentence with a bare "keep it short" fails the
    /// budget assertion; adding "list the commands to run" to the `Build & test`
    /// purpose fails the last one.
    #[test]
    fn the_prompt_names_the_five_sections_in_order_and_states_the_budget() {
        let docs = [(
            "README.md".to_owned(),
            "# Teton\nA coding agent.\n".to_owned(),
        )];
        let entries = [("Cargo.toml".to_owned(), "[workspace]\n".to_owned())];
        let prompt = build_prompt(&inputs("crates/\n  tetond/\n  teton/\n", &docs, &entries));

        let mut at = 0usize;
        for (heading, _) in DRAFT_SECTIONS {
            let found = prompt[at..]
                .find(&format!("`## {heading}`"))
                .unwrap_or_else(|| panic!("`{heading}` is missing or out of order:\n{prompt}"));
            at += found + heading.len();
        }
        assert_eq!(
            DRAFT_SECTIONS.map(|(h, _)| h),
            [
                "Purpose",
                "Layout",
                "Build & test",
                "Conventions",
                "Where to look"
            ],
            "the section order is the OQ-3 decision and changing it changes every \
             generated file"
        );

        // The budget, as a number the model can act on.
        assert!(
            prompt.contains(&format!("{DRAFT_OUTPUT_MAX_BYTES} bytes")),
            "the prompt must state the byte budget:\n{prompt}"
        );
        // The audience.
        assert!(prompt.contains("new contributor"), "{prompt}");
        // The output contract, verbatim, so the offline engine can recognize it.
        assert!(prompt.contains(DRAFT_OUTPUT_CONTRACT), "{prompt}");

        // And never a request for commands to run. The prompt talks about how a
        // project *is* built, never about what the reader should execute.
        for solicitation in ["commands to run", "command to run", "run the following"] {
            assert!(
                !prompt.contains(solicitation),
                "the draft prompt asks for commands: `{solicitation}`"
            );
        }
    }

    /// The evidence is rendered under stated labels, an absent group says so,
    /// and a walk that was cut says that too rather than letting the model
    /// believe the listing is complete (REQ-586 BR-7).
    #[test]
    fn the_evidence_is_labelled_and_an_absent_group_is_stated() {
        let docs = [("README.md".to_owned(), "# Teton".to_owned())];
        let prompt = build_prompt(&DraftInputs {
            tree: "crates/",
            documents: &docs,
            entry_points: &[],
            cut: Some("The listing was cut at depth 6.".to_owned()),
        });
        assert!(prompt.contains("--- README.md ---\n# Teton\n"), "{prompt}");
        assert!(
            prompt.contains("Entry points: none were found."),
            "{prompt}"
        );
        assert!(
            prompt.contains("The listing was cut at depth 6."),
            "{prompt}"
        );
        // A tree with no trailing newline does not run into the next label.
        assert!(prompt.contains("crates/\n"), "{prompt}");
    }

    /// The duty declares the category it routes on and the ceiling it is bound
    /// to, and the ceiling **is** REQ-612's cap rather than a number that looks
    /// like it (REQ-613 ADR-4).
    ///
    /// Mutation: writing `8_192` in place of `REPO_CONTEXT_MAX_BYTES` passes
    /// today and fails here the moment REQ-612's cap moves — which is the drift
    /// this assertion exists to catch, since a draft written over the loader's
    /// cap comes back truncated on a surface nobody was watching.
    #[test]
    fn the_duty_is_bound_to_draft_and_to_req_612s_cap() {
        assert_eq!(DRAFT_DUTY.category(), Category::Draft);
        assert_eq!(DRAFT_DUTY.ceiling_bytes(), DRAFT_OUTPUT_MAX_BYTES);
        assert_eq!(DRAFT_OUTPUT_MAX_BYTES, REPO_CONTEXT_MAX_BYTES);
        assert!(DRAFT_DUTY.max_tokens() > 0);
    }
}
