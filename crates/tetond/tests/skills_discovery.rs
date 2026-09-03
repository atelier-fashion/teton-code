//! REQ-585 BR-1/BR-2, ADR-4/ADR-5/ADR-6 — discovery is four directory
//! listings, one level deep, and every entry it declines is named.
//!
//! The suite is fixture-driven against the **real** filesystem, through a
//! recording [`DirLister`] that wraps [`RealFs`] and captures every path handed
//! to `list` and `read`. Two properties need that seam and cannot be had
//! without it:
//!
//! - **Reach.** "Discovery opens only these four directories" is a claim about
//!   what was *not* opened, so the recorded set is compared for **equality**,
//!   never containment: a walker that reached the right files and also crawled
//!   `/` passes a containment assertion.
//! - **Cost.** One file read per candidate, not one per turn (TASK-203 reuses
//!   this seam for that).
//!
//! Every fixture that asserts an absence carries a positive control — a live
//! symlink proving a refusal is the rule's doing and not a dangling link, a
//! registered skill beside a refused one proving discovery did not simply give
//! up (LESSON-479).
//!
//! Fixtures are hand-built under `/tmp` with a short name and a `Drop` cleanup,
//! the `e2e::harness::Workspace` shape.

use std::cell::RefCell;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use teton_protocol::methods::RootKind;
use tetond::skills::{
    discover, DirLister, Entry, ListError, ReadError, RealFs, ShadowedBy, SkillRegistry,
    SkillSource, MAX_ENTRIES_PER_ROOT, SKILL_MAX_BYTES,
};

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

/// A throwaway tree with a `home` and a `repo` in it, removed on drop.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            PathBuf::from("/tmp").join(format!("tsk{:x}{seq:x}", std::process::id() & 0xffff));
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo")).unwrap();
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    /// Write `contents` at `rel` (relative to the fixture root), creating every
    /// parent directory, and return the absolute path.
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn mkdir(&self, rel: &str) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A [`DirLister`] that answers exactly as the real filesystem does and
/// remembers every path it was asked about.
#[derive(Default)]
struct RecordingFs {
    inner: RealFs,
    listed: RefCell<Vec<PathBuf>>,
    read: RefCell<Vec<PathBuf>>,
}

impl DirLister for RecordingFs {
    fn list(&self, dir: &Path) -> Result<Vec<Entry>, ListError> {
        self.listed.borrow_mut().push(dir.to_path_buf());
        self.inner.list(dir)
    }

    fn read(&self, file: &Path) -> Result<String, ReadError> {
        self.read.borrow_mut().push(file.to_path_buf());
        self.inner.read(file)
    }
}

impl RecordingFs {
    fn listed(&self) -> Vec<PathBuf> {
        sorted(&self.listed.borrow())
    }

    fn read_paths(&self) -> Vec<PathBuf> {
        sorted(&self.read.borrow())
    }

    /// Everything opened, either way — for the "nothing under here was touched
    /// at all" assertions.
    fn opened(&self) -> Vec<PathBuf> {
        let mut all = self.listed.borrow().clone();
        all.extend(self.read.borrow().iter().cloned());
        sorted(&all)
    }
}

fn sorted(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted = paths.to_vec();
    sorted.sort();
    sorted
}

/// A minimal well-formed skill file.
fn skill_file(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n")
}

/// The names discovery registered, in registry order.
fn names(registry: &SkillRegistry) -> Vec<&str> {
    registry.skills().iter().map(|s| s.name.as_str()).collect()
}

/// The `(name, display spelling)` pairs a surface would be handed, in registry
/// order — BR-1's "shown relative, never absolute" (BUG-187).
fn displays(registry: &SkillRegistry) -> Vec<(&str, &str)> {
    registry
        .skills()
        .iter()
        .map(|s| (s.name.as_str(), s.path_display.as_str()))
        .collect()
}

/// The `(path, rendered reason)` pairs discovery reported, for equality
/// assertions against the words BR-1 promises.
fn diagnostics(registry: &SkillRegistry) -> Vec<(PathBuf, String)> {
    registry
        .skipped()
        .iter()
        .map(|s| (s.path.clone(), s.reason.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// AC-7 — reach
// ---------------------------------------------------------------------------

/// Four listings, one read per candidate, and **nothing else** — compared for
/// equality, because the claim is about what was not opened.
///
/// The fixture plants three things a recursive walker would reach and a
/// one-level lister must not: a `nested/` directory inside a skill directory, a
/// `sub/` directory inside another, and a `sub/` directory inside a `commands/`
/// root.
#[test]
fn discovery_opens_the_four_roots_and_one_file_per_candidate_and_nothing_else() {
    let fixture = Fixture::new();
    fixture.write(
        "home/.claude/skills/alpha/SKILL.md",
        &skill_file("alpha", "the user skill"),
    );
    fixture.write(
        "home/.claude/skills/alpha/nested/SKILL.md",
        &skill_file("nested", "one level too deep"),
    );
    fixture.write("home/.claude/commands/beta.md", "beta body\n");
    fixture.write("home/.claude/commands/notes.txt", "not markdown\n");
    fixture.write(
        "repo/.claude/skills/gamma/SKILL.md",
        &skill_file("gamma", "the project skill"),
    );
    fixture.write(
        "repo/.claude/skills/gamma/sub/SKILL.md",
        &skill_file("sub", "one level too deep"),
    );
    fixture.write("repo/.claude/commands/delta.md", "delta body\n");
    fixture.write("repo/.claude/commands/sub/inner.md", "one level too deep\n");

    let fs = RecordingFs::default();
    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &fs,
    );

    assert_eq!(
        names(&registry),
        vec!["alpha", "beta", "delta", "gamma"],
        "every candidate registered, ordered by name"
    );

    assert_eq!(
        fs.listed(),
        sorted(&[
            fixture.path("home/.claude/commands"),
            fixture.path("home/.claude/skills"),
            fixture.path("repo/.claude/commands"),
            fixture.path("repo/.claude/skills"),
        ]),
        "exactly four directories were listed — no `nested`, no `sub`, no recursion"
    );
    assert_eq!(
        fs.read_paths(),
        sorted(&[
            fixture.path("home/.claude/skills/alpha/SKILL.md"),
            fixture.path("home/.claude/commands/beta.md"),
            fixture.path("repo/.claude/skills/gamma/SKILL.md"),
            fixture.path("repo/.claude/commands/delta.md"),
        ]),
        "one read per candidate: the non-Markdown file was never opened, and \
         neither was anything below a candidate"
    );
}

/// A root that is itself a symlink is **followed** — the dogfood machine's
/// `~/.claude/skills` is one — while an **entry** that is a symlink is refused
/// and named. The fixture's `link → /` is the sharp case: `/` is never listed,
/// and it is never listed because the entry rule refused it, not because a
/// budget stopped a walk that had already started (ADR-4).
#[test]
fn a_symlinked_root_is_followed_and_a_symlinked_entry_is_never_reached() {
    let fixture = Fixture::new();
    fixture.write(
        "elsewhere/alpha/SKILL.md",
        &skill_file("alpha", "reached through a symlinked root"),
    );
    fixture.mkdir("elsewhere/nested/deep");
    fixture.write("elsewhere/nested/deep/SKILL.md", &skill_file("deep", "no"));
    fixture.mkdir("home/.claude");
    symlink(
        fixture.path("elsewhere"),
        fixture.path("home/.claude/skills"),
    )
    .unwrap();
    let link = fixture.path("home/.claude/skills/link");
    symlink("/", &link).unwrap();

    // Positive controls: the root link really resolves (or this test is about a
    // broken link), and so does the entry link (or the refusal below is free).
    assert!(
        fixture
            .path("home/.claude/skills/alpha/SKILL.md")
            .canonicalize()
            .is_ok(),
        "fixture: the symlinked root must actually resolve"
    );
    assert_eq!(
        link.canonicalize().unwrap(),
        Path::new("/"),
        "fixture: the entry link must actually point at the filesystem root"
    );

    let fs = RecordingFs::default();
    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &fs,
    );

    assert_eq!(
        names(&registry),
        vec!["alpha"],
        "the skill behind the symlinked root registered"
    );
    assert_eq!(
        diagnostics(&registry),
        vec![(link.clone(), "symlink not followed".to_owned())],
        "the symlinked entry is named, and it is the only diagnostic: `nested` \
         has no SKILL.md, which is not a skill and not a fault"
    );

    assert_eq!(
        fs.listed(),
        sorted(&[
            fixture.path("home/.claude/commands"),
            fixture.path("home/.claude/skills"),
            fixture.path("repo/.claude/commands"),
            fixture.path("repo/.claude/skills"),
        ]),
        "`/` was never listed, and neither was `nested` or `nested/deep`"
    );
    assert_eq!(
        fs.read_paths(),
        sorted(&[
            fixture.path("home/.claude/skills/alpha/SKILL.md"),
            fixture.path("home/.claude/skills/nested/SKILL.md"),
        ]),
        "a `skills/` directory is asked for its own SKILL.md and nothing else — \
         `nested` is asked, answers no, and is not a diagnostic"
    );
    assert!(
        !fs.opened().iter().any(|p| p.starts_with(&link)),
        "nothing under the symlinked entry was opened"
    );
    assert!(
        !fs.opened().iter().any(|p| p == Path::new("/")),
        "`/` itself was never opened"
    );
}

/// **REQ-587 BR-10.** The follow-the-root exemption belongs to the **home**
/// directory, and a repository does not inherit it.
///
/// The sibling above pins "a symlinked root is followed" — and it exercises only
/// a **user** root, which is the whole of the reason that rule exists: the
/// dogfood machine's `~/.claude/skills` is a symlink into a checked-out toolkit.
/// But `roots()` builds two of the four from `session_root`, so under a blanket
/// exemption `<repo>/.claude/commands` was a **repository-controlled** symlink
/// that discovery followed. Git stores a symlink verbatim, so a cloned repo
/// shipping `.claude/commands -> ../../..` had every lowercase-stemmed `*.md`
/// under the target registered as a *project* skill — in the roster, in the
/// resident prompt, and callable as `skill { name }` with no prompt at `full` —
/// while `read` of the same bytes is refused by REQ-583's jail. That is the
/// second classifier of "what may be read" BR-10 says must not exist.
///
/// Both directions in one fixture, deliberately: a rule stated as an exemption
/// is only meaningful beside the case it does *not* cover, and a fix that simply
/// stopped following every root would pass a one-legged version of this test
/// while breaking the feature's own author.
///
/// **Mutation:** delete the `resolves_under` guard in `discover` and this fails
/// twice over — `pwned` registers, and `outside/` shows up in what was opened.
#[test]
fn a_symlinked_project_root_is_refused_while_a_symlinked_user_root_is_still_followed() {
    let fixture = Fixture::new();

    // The user's own toolkit, reached through a symlinked **user** root. This
    // is the shape the exemption exists for and it must keep working.
    fixture.write(
        "toolkit/mine/SKILL.md",
        &skill_file("mine", "the user's own, through a symlinked home root"),
    );
    fixture.mkdir("home/.claude");
    symlink(fixture.path("toolkit"), fixture.path("home/.claude/skills")).unwrap();

    // The cloned repository's, pointing out of the repo at a tree of ordinary
    // Markdown that would otherwise register under `commands/`'s file-stem rule.
    fixture.write(
        "outside/pwned.md",
        &skill_file("pwned", "not the repository's to offer"),
    );
    fixture.mkdir("repo/.claude");
    symlink(
        fixture.path("outside"),
        fixture.path("repo/.claude/commands"),
    )
    .unwrap();

    // Positive controls (LESSON-479). Both links resolve — otherwise the
    // refusal below would be a dangling link's doing and the follow above would
    // be about an empty directory — and the file behind the project link really
    // is one discovery would register if it followed.
    assert!(
        fixture
            .path("home/.claude/skills/mine/SKILL.md")
            .canonicalize()
            .is_ok(),
        "fixture: the user root link must actually resolve"
    );
    assert_eq!(
        fixture
            .path("repo/.claude/commands")
            .canonicalize()
            .unwrap(),
        fixture.path("outside").canonicalize().unwrap(),
        "fixture: the project root link must actually resolve out of the repo"
    );
    {
        // …and the file behind the link is one discovery genuinely registers
        // when the same bytes sit in a **real** `commands/` directory. Without
        // this leg, "pwned did not register" could be a fact about the fixture's
        // frontmatter or its file stem rather than about the refusal
        // (LESSON-479).
        let control = Fixture::new();
        control.write(
            "repo/.claude/commands/pwned.md",
            &skill_file("pwned", "not the repository's to offer"),
        );
        assert_eq!(
            names(&discover(
                None,
                &control.repo(),
                RootKind::Project,
                &RecordingFs::default(),
            )),
            vec!["pwned"],
            "fixture: the file behind the link is registrable — so its absence \
             below is the root refusal's doing"
        );
    }

    let fs = RecordingFs::default();
    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &fs,
    );

    assert_eq!(
        names(&registry),
        vec!["mine"],
        "the user's symlinked root is still followed, and the repository's is not"
    );
    assert_eq!(
        diagnostics(&registry),
        vec![(
            fixture.path("repo/.claude/commands"),
            "resolves outside the session root".to_owned()
        )],
        "the refusal is named against the root, so a user whose `/help` is short \
         is told why rather than left guessing (BR-1)"
    );

    // The reach claim, and it is the sharp one: the guard runs **before** the
    // listing, because `read_dir` follows and there is no undoing it.
    assert_eq!(
        fs.listed(),
        sorted(&[
            fixture.path("home/.claude/commands"),
            fixture.path("home/.claude/skills"),
            fixture.path("repo/.claude/skills"),
        ]),
        "the escaping root was never listed at all"
    );
    let outside = fixture.path("outside");
    assert!(
        !fs.opened().iter().any(|p| p.starts_with(&outside)),
        "nothing behind the escaping root was opened"
    );
}

/// **REQ-587 BR-10, the id half.** A project root that is a symlink *within* the
/// repository is permitted — it resolves under the session root — so the
/// provenance id must be minted from where the file actually **is**.
///
/// `ProvenanceId::from_resolved` is a bare `strip_prefix` and is documented as
/// taking a canonical path; `Skill::path` is the path discovery walked to the
/// file, which this shape leaves non-canonical. Minting off the spelling gives
/// one file two identities: the id reads `.claude/skills/x/SKILL.md` for a file
/// at `vendor/skills/x/SKILL.md`, so a `vendor/**` boundary matches nothing and
/// REQ-585 BR-7's *"pins exactly as a `read` would"* is false for exactly the
/// shape that most needs it.
///
/// **Mutation:** drop the `canonicalize` calls in `skills::provenance_of` and
/// this fails — the id comes back as the link's spelling.
#[test]
fn a_skill_reached_through_an_in_repo_symlinked_root_mints_the_id_of_the_real_file() {
    let fixture = Fixture::new();
    fixture.write(
        "repo/vendor/skills/alpha/SKILL.md",
        &skill_file("alpha", "lives under vendor/, reached through .claude/"),
    );
    fixture.mkdir("repo/.claude");
    symlink(
        fixture.path("repo/vendor/skills"),
        fixture.path("repo/.claude/skills"),
    )
    .unwrap();

    let registry = discover(None, &fixture.repo(), RootKind::Project, &RealFs);
    let skill = registry
        .dispatchable_by_user("alpha")
        .expect("an in-repo symlinked project root is permitted and still registers");
    assert_eq!(
        skill.path,
        fixture.path("repo/.claude/skills/alpha/SKILL.md"),
        "the row keeps the spelling the user recognizes"
    );

    let id = tetond::skills::provenance_of(&fixture.repo(), skill)
        .expect("a file under the root has a repo-relative identity");
    assert_eq!(
        id.as_str(),
        "vendor/skills/alpha/SKILL.md",
        "the id names the file, not the link that reached it"
    );

    // Fail-closed, not fall-back: a row whose file is gone mints nothing rather
    // than an id for a path nothing is at.
    let mut moved = skill.clone();
    moved.path = fixture.path("repo/.claude/skills/absent/SKILL.md");
    assert!(
        tetond::skills::provenance_of(&fixture.repo(), &moved).is_none(),
        "a path that does not resolve has no identity"
    );
}

// ---------------------------------------------------------------------------
// AC-3 — the home-root de-dup
// ---------------------------------------------------------------------------

/// A session whose root *is* `$HOME` reaches `~/.claude` through both pairs of
/// globs. `RootKind::Home` skips the project pair, so each skill registers
/// once, as `user`.
///
/// The second half is the control that gives the first half teeth: with the
/// same paths and the kind the de-dup keys off changed, the same file registers
/// twice — two sources, two permission keys, and a name shadowing itself.
#[test]
fn a_home_kind_session_registers_each_skill_once_as_user() {
    let fixture = Fixture::new();
    fixture.write(
        "home/.claude/skills/alpha/SKILL.md",
        &skill_file("alpha", "the only copy"),
    );
    let home = fixture.home();

    let fs = RecordingFs::default();
    let registry = discover(Some(&home), &home, RootKind::Home, &fs);

    assert_eq!(names(&registry), vec!["alpha"], "registered exactly once");
    assert_eq!(registry.skills()[0].source, SkillSource::User);
    assert_eq!(registry.skills()[0].shadowed, None, "it shadows nothing");
    assert_eq!(diagnostics(&registry), vec![], "and nothing was skipped");
    assert_eq!(
        fs.listed(),
        sorted(&[
            fixture.path("home/.claude/commands"),
            fixture.path("home/.claude/skills"),
        ]),
        "project discovery was skipped entirely — two listings, not four"
    );

    let without_the_dedup = discover(
        Some(&home),
        &home,
        RootKind::Project,
        &RecordingFs::default(),
    );
    assert_eq!(
        without_the_dedup.skills().len(),
        2,
        "control: the same tree reached through both pairs registers the same \
         file twice, under two sources and two permission keys"
    );
}

// ---------------------------------------------------------------------------
// BR-2 / ADR-6 — the name contests
// ---------------------------------------------------------------------------

/// Project beats user, and the loser is **listed** rather than dropped: a name
/// that silently does nothing is worse than one marked as taken.
#[test]
fn a_project_skill_beats_a_user_skill_and_the_loser_is_listed_as_shadowed() {
    let fixture = Fixture::new();
    fixture.write(
        "home/.claude/skills/analyze/SKILL.md",
        &skill_file("analyze", "the user copy"),
    );
    fixture.write(
        "repo/.claude/skills/analyze/SKILL.md",
        &skill_file("analyze", "the project copy"),
    );

    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &RecordingFs::default(),
    );

    assert_eq!(
        names(&registry),
        vec!["analyze", "analyze"],
        "both rows are listed"
    );
    let winner = registry
        .dispatchable_by_user("analyze")
        .expect("one row dispatches the name");
    assert_eq!(winner.source, SkillSource::Project);
    assert_eq!(winner.description.as_deref(), Some("the project copy"));

    let loser = registry
        .skills()
        .iter()
        .find(|s| s.source == SkillSource::User)
        .expect("the user row is retained");
    assert_eq!(loser.shadowed, Some(ShadowedBy::ProjectSkill));
    assert!(!loser.is_dispatchable());
    assert_eq!(
        loser.shadow_reason().map(|r| r.to_string()),
        Some("shadowed by a project skill of the same name".to_owned())
    );
}

/// Within one source, `skills/` beats `commands/`.
///
/// The pair is legal and the four globs both reach it, and the reason it cannot
/// be left as two rows is in the last assertion: the two files share a
/// permission key, so a remembered grant would authorize whichever one happened
/// to win and would move silently to the other if that ever changed (ADR-6,
/// LESSON-495).
#[test]
fn within_one_source_the_skills_directory_beats_the_commands_file() {
    let fixture = Fixture::new();
    fixture.write(
        "home/.claude/skills/status/SKILL.md",
        &skill_file("status", "the skills copy"),
    );
    fixture.write(
        "home/.claude/commands/status.md",
        "---\ndescription: the commands copy\n---\nbody\n",
    );

    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &RecordingFs::default(),
    );

    assert_eq!(names(&registry), vec!["status", "status"]);
    let winner = registry
        .dispatchable_by_user("status")
        .expect("exactly one row dispatches");
    assert_eq!(
        winner.path,
        fixture.path("home/.claude/skills/status/SKILL.md"),
        "the skills/ entry wins"
    );

    let loser = registry
        .skills()
        .iter()
        .find(|s| s.path == fixture.path("home/.claude/commands/status.md"))
        .expect("the commands/ entry is listed, not dropped");
    assert_eq!(loser.shadowed, Some(ShadowedBy::SkillsDirectory));
    assert_eq!(
        loser.shadow_reason().map(|r| r.to_string()),
        Some("shadowed by the skills/ entry of the same name".to_owned())
    );
    // `permission_key_for`, not a `Skill::permission_key()` — REQ-587 BR-5
    // removed that method, because the key a *grant* is remembered under is
    // `Expansion::grant_key` and a row cannot answer it. What is asserted here
    // is the base key both files would collide on, which is a property of
    // `(source, name)` and of nothing else — which is exactly why the free
    // function is the right spelling for it.
    let key_of = |skill: &tetond::skills::Skill| {
        tetond::skills::permission_key_for(skill.source, &skill.name)
    };
    assert_eq!(
        key_of(winner),
        key_of(loser),
        "the same key for both files is exactly why one of them has to lose"
    );
    assert_eq!(key_of(winner), "skill:user:status");
}

/// The name is where the file lives. A frontmatter `name` that disagrees is a
/// note for `/verbose` and creates no second spelling (BR-2 — one spelling
/// reaches one handler).
#[test]
fn a_frontmatter_name_that_differs_is_a_note_and_never_a_second_spelling() {
    let fixture = Fixture::new();
    fixture.write(
        "home/.claude/commands/deploy.md",
        "---\nname: shipit\ndescription: deploy the thing\n---\nbody\n",
    );

    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &RecordingFs::default(),
    );

    assert_eq!(names(&registry), vec!["deploy"]);
    assert!(
        registry.dispatchable_by_user("shipit").is_none(),
        "the declared name is not a spelling anything dispatches"
    );
    let skill = registry.dispatchable_by_user("deploy").unwrap();
    assert_eq!(
        skill.name_note.as_deref(),
        Some("frontmatter name `shipit` differs; this command dispatches as `/deploy`")
    );
}

// ---------------------------------------------------------------------------
// BR-1 / AC-6 — every declined entry is counted and named
// ---------------------------------------------------------------------------

/// BR-2's naming rule travels **with** the diagnostic instead of being read
/// back off the path.
///
/// Every shape the four globs can produce, including the two that name nothing:
/// a root-level refusal names no single skill, and neither does an entry under
/// `commands/` that could never have been one. The two that matter for BR-10's
/// hint are the invalid spelling (the user typed something; they have to be
/// told why it is not there) and the symlinked `commands/<name>.md`, whose name
/// the path alone cannot give back — discovery refuses it before it is ever a
/// `.md` file that was opened.
#[test]
fn a_skipped_entry_carries_the_name_it_would_have_dispatched_under() {
    let fixture = Fixture::new();
    fixture.write("home/.claude/commands/Deploy Prod.md", "body\n");
    fixture.write(
        "home/.claude/commands/broken.md",
        "---\ntools:\n  - Bash\n---\nbody\n",
    );
    fixture.write(
        "home/.claude/commands/target.md",
        &skill_file("target", "fine"),
    );
    symlink(
        fixture.path("home/.claude/commands/target.md"),
        fixture.path("home/.claude/commands/linked.md"),
    )
    .unwrap();
    fixture.write(
        "home/.claude/skills/Bad Name/SKILL.md",
        &skill_file("bad", "misnamed directory"),
    );

    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &RecordingFs::default(),
    );
    let named = |needle: &str| {
        registry
            .skipped()
            .iter()
            .find(|entry| entry.path.to_string_lossy().contains(needle))
            .unwrap_or_else(|| panic!("no diagnostic mentioning `{needle}`"))
            .name
            .clone()
    };

    // The spelling that is *why* it was skipped is the spelling reported —
    // even though nobody can type it. Dropping it would leave the user who
    // typed `/deploy` with nothing to go on.
    assert_eq!(named("Deploy Prod.md"), Some("Deploy Prod".to_owned()));
    assert_eq!(named("broken.md"), Some("broken".to_owned()));
    assert_eq!(named("Bad Name"), Some("Bad Name".to_owned()));
    // The one the path alone cannot give back.
    assert_eq!(named("linked.md"), Some("linked".to_owned()));
}

/// Each reason, in the words BR-1 promises, against the file that earned it —
/// and, in the same fixture, the two things that are **normal** and produce no
/// diagnostic at all: a directory with no `SKILL.md`, and a root that is not
/// there.
#[test]
fn every_entry_that_is_not_registered_is_counted_and_named() {
    let fixture = Fixture::new();
    // Registered — the positive control. Discovery must not simply give up.
    fixture.write(
        "home/.claude/commands/alpha.md",
        &skill_file("alpha", "fine"),
    );
    // Oversize.
    let big_bytes = SKILL_MAX_BYTES as usize + 17;
    fixture.write("home/.claude/commands/big.md", &"x".repeat(big_bytes));
    // Not UTF-8.
    std::fs::write(
        fixture.path("home/.claude/commands/binary.md"),
        [0xff, 0xfe, 0x00],
    )
    .unwrap();
    // Malformed frontmatter: an opening delimiter, a nested block, no close.
    fixture.write(
        "home/.claude/commands/broken.md",
        "---\ntools:\n  - Bash\n---\nbody\n",
    );
    // Invalid names, one per shape.
    fixture.write("home/.claude/commands/Deploy Prod.md", "body\n");
    fixture.write(
        "home/.claude/skills/Bad Name/SKILL.md",
        &skill_file("bad", "misnamed directory"),
    );
    // A symlinked entry.
    symlink(
        fixture.path("home/.claude/commands/alpha.md"),
        fixture.path("home/.claude/commands/linked.md"),
    )
    .unwrap();
    // Neither a skill nor a fault: a directory with no SKILL.md (the ADLC
    // toolkit's `agents/`), a dot-directory, and a non-Markdown file.
    fixture.write("home/.claude/skills/agents/README.md", "not a skill\n");
    fixture.write("home/.claude/skills/.git/config", "not a skill\n");
    fixture.write("home/.claude/commands/notes.txt", "not a skill\n");

    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &RecordingFs::default(),
    );

    assert_eq!(
        names(&registry),
        vec!["alpha"],
        "one skill registered; everything else was declined"
    );

    let reported: Vec<(PathBuf, String)> = diagnostics(&registry);
    let expected = vec![
        (
            fixture.path("home/.claude/commands/Deploy Prod.md"),
            "invalid name".to_owned(),
        ),
        (
            fixture.path("home/.claude/commands/big.md"),
            "over 128 KiB (131,089 B)".to_owned(),
        ),
        (
            fixture.path("home/.claude/commands/binary.md"),
            "not UTF-8".to_owned(),
        ),
        (
            fixture.path("home/.claude/commands/broken.md"),
            "malformed frontmatter".to_owned(),
        ),
        (
            fixture.path("home/.claude/commands/linked.md"),
            "symlink not followed".to_owned(),
        ),
        (
            fixture.path("home/.claude/skills/Bad Name/SKILL.md"),
            "invalid name".to_owned(),
        ),
    ];
    assert_eq!(
        sorted_pairs(&reported),
        sorted_pairs(&expected),
        "every declined entry is named, and nothing normal is: no diagnostic \
         for `agents/`, `.git/`, `notes.txt`, or the two absent project roots"
    );
    assert_eq!(
        big_bytes, 131_089,
        "the fixture's size is the figure the reason quotes"
    );
}

fn sorted_pairs(pairs: &[(PathBuf, String)]) -> Vec<(PathBuf, String)> {
    let mut sorted = pairs.to_vec();
    sorted.sort();
    sorted
}

/// Malformed means the file is skipped **whole**. There is no half-parsed
/// registration: the well-formed keys above the break do not survive into a
/// skill (ADR-5).
#[test]
fn a_malformed_file_is_skipped_whole_and_never_half_parsed() {
    let fixture = Fixture::new();
    fixture.write(
        "home/.claude/commands/half.md",
        "---\nname: half\ndescription: this key is perfectly good\n  indented: continuation\n---\nbody\n",
    );

    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &RecordingFs::default(),
    );

    assert!(registry.skills().is_empty(), "nothing was registered");
    assert_eq!(
        diagnostics(&registry),
        vec![(
            fixture.path("home/.claude/commands/half.md"),
            "malformed frontmatter".to_owned()
        )]
    );
}

/// A `commands/` file with no frontmatter at all is the common case, not a
/// failure: the whole file is the body (ADR-5 rule 1).
#[test]
fn a_command_file_with_no_frontmatter_is_all_body() {
    let fixture = Fixture::new();
    let body = "# Deploy\n\nRun the checklist.\n";
    fixture.write("home/.claude/commands/deploy.md", body);

    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &RecordingFs::default(),
    );

    let skill = registry
        .dispatchable_by_user("deploy")
        .expect("it registered");
    assert_eq!(skill.body, body);
    assert_eq!(skill.description, None);
    assert!(skill.ignored_keys.is_empty());
    assert_eq!(diagnostics(&registry), vec![]);
}

// ---------------------------------------------------------------------------
// LESSON-540 — order is decided here, not by the filesystem
// ---------------------------------------------------------------------------

/// Entries are sorted **before** the cap applies, so which entries survive a
/// truncated root is a property of their names and not of APFS's hash order or
/// ext4's.
///
/// The fixture is created in **reverse** name order on purpose: a filesystem
/// that lists in creation order (tmpfs does) would otherwise keep the *last*
/// 512 names, and the test would pass without the sort on Linux while failing
/// on macOS — the LESSON-540 shape exactly.
#[test]
fn entries_are_sorted_before_the_cap_applies() {
    let fixture = Fixture::new();
    let planted = MAX_ENTRIES_PER_ROOT + 88;
    fixture.mkdir("home/.claude/commands");
    for index in (0..planted).rev() {
        fixture.write(&format!("home/.claude/commands/s{index:03}.md"), "body\n");
    }

    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &RecordingFs::default(),
    );

    let expected: Vec<String> = (0..MAX_ENTRIES_PER_ROOT)
        .map(|index| format!("s{index:03}"))
        .collect();
    assert_eq!(
        names(&registry),
        expected.iter().map(String::as_str).collect::<Vec<_>>(),
        "the alphabetically first 512 names survived, on any filesystem"
    );
    assert_eq!(
        diagnostics(&registry),
        vec![(
            fixture.path("home/.claude/commands"),
            "root truncated at 512 entries".to_owned()
        )],
        "truncation is named once, against the root — never silent"
    );
}

// ---------------------------------------------------------------------------
// BR-1 — EPERM is a reason, not a crash and not silence
// ---------------------------------------------------------------------------

/// A refused directory is named at both levels it can be refused at — the root
/// itself (macOS's TCC gate in front of `~/Documents`, which a `~/.claude`
/// symlink can lead into) and one skill directory under a readable root — and
/// neither one stops the rest of discovery.
#[test]
fn a_refused_root_and_a_refused_skill_directory_are_each_named_unreadable() {
    // SAFETY: `geteuid` reads the calling process's effective uid; no
    // arguments, no side effects.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!(
            "skipped: running as root, where mode 000 denies nothing — the \
             fixture cannot pose the question this test asks"
        );
        return;
    }

    let fixture = Fixture::new();
    // The positive control: a readable skill beside the refused ones.
    fixture.write(
        "home/.claude/skills/alpha/SKILL.md",
        &skill_file("alpha", "readable"),
    );
    let locked_dir = fixture.path("home/.claude/skills/locked");
    fixture.write(
        "home/.claude/skills/locked/SKILL.md",
        &skill_file("locked", "unreachable"),
    );
    let locked_root = fixture.mkdir("home/.claude/commands");
    fixture.write("home/.claude/commands/beta.md", "body\n");

    chmod(&locked_dir, 0o000);
    chmod(&locked_root, 0o000);

    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &RecordingFs::default(),
    );

    assert_eq!(
        names(&registry),
        vec!["alpha"],
        "the readable skill still registered — a refusal is not a crash and \
         does not abandon the root it happened under"
    );
    assert_eq!(
        sorted_pairs(&diagnostics(&registry)),
        sorted_pairs(&[
            (
                locked_root.clone(),
                "unreadable (permission denied)".to_owned()
            ),
            (
                locked_dir.join("SKILL.md"),
                "unreadable (permission denied)".to_owned()
            ),
        ]),
        "both refusals are named, with the path each happened at"
    );

    // Restore before `Drop`, or the fixture cannot be removed.
    chmod(&locked_dir, 0o755);
    chmod(&locked_root, 0o755);
}

fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

// ---------------------------------------------------------------------------
// the empty cases
// ---------------------------------------------------------------------------

/// A machine with no `~/.claude` and a repo with no `.claude` yields an empty
/// registry, four listing attempts and no diagnostics — the state most sessions
/// are in, and it must cost nothing and say nothing.
#[test]
fn absent_roots_are_free_and_silent() {
    let fixture = Fixture::new();
    let fs = RecordingFs::default();
    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &fs,
    );

    assert!(registry.is_empty());
    assert_eq!(diagnostics(&registry), vec![]);
    assert_eq!(fs.read_paths(), Vec::<PathBuf>::new(), "nothing was read");
    assert_eq!(
        fs.listed().len(),
        4,
        "the four roots were asked, and that is all"
    );

    // No home at all (a daemon started without `HOME`): the user pair is not
    // reached, and that is not a diagnostic either.
    let no_home = RecordingFs::default();
    let registry = discover(None, &fixture.repo(), RootKind::Project, &no_home);
    assert!(registry.is_empty());
    assert_eq!(no_home.listed().len(), 2);
}

/// **BR-2's third shadowing case, daemon-side.**
///
/// A skill can never take a built-in's name — and that has to be true *here*,
/// not only in the client. `SkillRegistry::dispatchable` used to answer for a
/// skill named `cost`, so a client carrying no command table could dispatch a
/// repo-supplied `.claude/skills/cost/SKILL.md` by name over the wire. The
/// spec's own Assumptions say a project skill may be authored by someone other
/// than the user, so that is a repo choosing what `/cost` means for any client
/// that is not `teton`.
///
/// The list is `teton_protocol::methods::RESERVED_SKILL_NAMES`, which `teton`
/// asserts is exactly its own derivation from `COMMANDS`.
#[test]
fn a_skill_named_after_a_built_in_is_shadowed_by_the_daemon_not_only_by_the_client() {
    let fixture = Fixture::new();
    for name in ["cost", "provider", "teton", "help"] {
        fixture.write(
            &format!("repo/.claude/skills/{name}/SKILL.md"),
            &skill_file(&format!("shadow {name}"), "Body.\n"),
        );
    }
    // The control: a name no row claims still dispatches, so this is a test
    // about the reserved set and not about project skills in general.
    fixture.write(
        "repo/.claude/skills/deploy/SKILL.md",
        &skill_file("deploy something", "Body.\n"),
    );

    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &RecordingFs::default(),
    );

    for name in ["cost", "provider", "teton", "help"] {
        let skill = registry
            .skills()
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("`{name}` was dropped rather than listed"));
        assert!(
            !skill.is_dispatchable(),
            "`/{name}` dispatches a file over the wire, whatever table the client has",
        );
    }
    assert!(
        registry
            .skills()
            .iter()
            .any(|s| s.name == "deploy" && s.is_dispatchable()),
        "the control name stopped dispatching, so this test says nothing",
    );
}

// ---------------------------------------------------------------------------
// BUG-187 — the display spelling, for a root that is not under `$HOME`
// ---------------------------------------------------------------------------

/// **A path this registry hands to a surface is never an absolute one.**
///
/// BR-1's entity table says a skill path is *shown* relative, never absolute,
/// because an absolute path carries a username or the location of the user's
/// working tree into a transcript and onto a remote payload. `display_for`
/// alone could only deliver that for a file under `$HOME`, so a project skill
/// in a checkout anywhere else — `/tmp`, an external volume, a CI workspace,
/// this fixture — reached the wire as
/// `/tmp/tsk4e9f0/repo/.claude/skills/gamma/SKILL.md` (BUG-187). The rule now
/// has two halves, chosen by **source**: the session root for a project skill,
/// the home folder for a user skill.
///
/// The fixture is the sharp one on purpose: `home` and `repo` are siblings, so
/// the project root is outside `$HOME` exactly as a real checkout on another
/// volume is.
#[test]
fn a_project_skill_outside_home_is_spelled_relative_to_the_session_root() {
    let fixture = Fixture::new();
    fixture.write(
        "repo/.claude/skills/gamma/SKILL.md",
        &skill_file("gamma", "the project skill"),
    );
    fixture.write("repo/.claude/commands/delta.md", "delta body\n");
    fixture.write(
        "home/.claude/skills/alpha/SKILL.md",
        &skill_file("alpha", "the user skill"),
    );
    fixture.write("home/.claude/commands/beta.md", "beta body\n");

    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &RecordingFs::default(),
    );

    assert_eq!(
        displays(&registry),
        vec![
            ("alpha", "~/.claude/skills/alpha/SKILL.md"),
            ("beta", "~/.claude/commands/beta.md"),
            ("delta", ".claude/commands/delta.md"),
            ("gamma", ".claude/skills/gamma/SKILL.md"),
        ],
        "a project skill is spelled relative to the session root, a user skill \
         relative to `$HOME` — and the project root here is under neither the \
         home folder nor any ancestor of it"
    );
    assert!(
        registry
            .skills()
            .iter()
            .all(|skill| !skill.path_display.starts_with('/')),
        "no row's display spelling is an absolute path"
    );
    assert!(
        registry
            .skills()
            .iter()
            .all(|skill| skill.path.is_absolute()),
        "…while `path` itself stays absolute: it is the local-only fact the \
         expander and the provenance mint need (BR-7)"
    );
}

/// **A skipped row is spelled by the same rule.** `/help`'s diagnostic list is
/// a user-visible surface — the one a screenshot reaches — and a refusal that
/// named `/tmp/ci-4f2a/repo/.claude/skills/broken/SKILL.md` would put the
/// working tree's location there. The wire contract in
/// `skills_list_contracts.rs` covers a *user* skip; this covers the project
/// half.
#[test]
fn a_skipped_project_entry_is_spelled_relative_to_the_session_root() {
    let fixture = Fixture::new();
    fixture.write(
        "repo/.claude/skills/broken/SKILL.md",
        "---\nname: broken\ndescription: unterminated frontmatter\n",
    );
    fixture.write(
        "repo/.claude/skills/ok/SKILL.md",
        &skill_file("ok", "the positive control"),
    );

    let registry = discover(
        Some(&fixture.home()),
        &fixture.repo(),
        RootKind::Project,
        &RecordingFs::default(),
    );

    assert_eq!(
        names(&registry),
        vec!["ok"],
        "the positive control registered, so the skip below is the rule's \
         doing and not an empty root"
    );
    assert_eq!(
        registry
            .skipped()
            .iter()
            .map(|entry| (entry.path_display.as_str(), entry.reason.to_string()))
            .collect::<Vec<_>>(),
        vec![(
            ".claude/skills/broken/SKILL.md",
            "malformed frontmatter".to_owned()
        )],
        "the diagnostic names the file relative to the session root"
    );
}

/// **A user skill keeps `~/…` even when the session root is an ancestor of the
/// home folder.** A session rooted at `/` or `/Users` reaches
/// `~/.claude/skills` through the *user* pair, and spelling it against the root
/// would produce `Users/someone/.claude/skills/x/SKILL.md` — the username the
/// whole rule exists to keep off a surface. Which base applies is decided by
/// the skill's **source**, not by whichever prefix happens to match.
#[test]
fn a_user_skill_under_a_session_root_that_contains_home_is_still_spelled_from_home() {
    let fixture = Fixture::new();
    fixture.write(
        "home/.claude/skills/alpha/SKILL.md",
        &skill_file("alpha", "the user skill"),
    );
    // The fixture root is an ancestor of `home` — the `/Users`-as-session-root
    // shape, without having to be `/Users`.
    fixture.write("Cargo.toml", "[package]\n");

    let registry = discover(
        Some(&fixture.home()),
        &fixture.path(""),
        RootKind::Project,
        &RecordingFs::default(),
    );

    assert_eq!(
        displays(&registry),
        vec![("alpha", "~/.claude/skills/alpha/SKILL.md")],
        "the user pair's rule is `$HOME`, whatever the session root contains"
    );
}
