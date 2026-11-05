//! The commands `stemma worktree add` and `stemma worktree list`, run as a process, as a person or
//! an agent would run them (ADR-009, ADR-010).

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{ok, run, write, Dir, ACTOR};

const ANNA: &str = "human anna";

/// A sandbox with the repository `repo` in it, with `a.txt` committed as "one" on `main`. The
/// worktrees of the tests go next to `repo` in the sandbox: a worktree may not lie in another.
struct Sandbox {
    dir: Dir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = Dir::new();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        ok(&repo, &["init", "--actor", ACTOR]);
        write(&repo, "a.txt", "one");
        ok(&repo, &["track", "a.txt", "--actor", ACTOR]);
        ok(&repo, &["commit", "-m", "first", "--actor", ACTOR]);
        Self { dir }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn at(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

fn read(dir: &Path, path: &str) -> String {
    fs::read_to_string(dir.join(path)).unwrap()
}

/// The real path of an existing directory, as it stands in a file of the repository.
fn real(path: &Path) -> String {
    fs::canonicalize(path).unwrap().display().to_string()
}

/// The real path as the commands print it: escaped like every path in their output, so a
/// backslash of a Windows path is doubled.
fn shown(path: &Path) -> String {
    real(path).replace('\\', "\\\\")
}

#[test]
fn a_worktree_on_a_new_branch_has_the_files_a_pointer_and_its_own_state() {
    let sandbox = Sandbox::new();

    let added = ok(
        &sandbox.repo(),
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );

    let linked = sandbox.at("linked");
    assert!(
        added.stdout.starts_with(&format!(
            "Added the worktree linked in {} on the new branch feature as entry ",
            shown(&linked)
        )),
        "{}",
        added.stdout
    );
    assert!(added.stdout.ends_with(&format!("Owner: {ACTOR}\n")));
    assert_eq!(added.stderr, "");
    assert_eq!(read(&linked, "a.txt"), "one");
    let state = sandbox.repo().join(".stemma/worktrees/linked");
    assert_eq!(
        read(&linked, ".stemma"),
        format!(
            "stemma-worktree {}\n",
            fs::canonicalize(sandbox.repo())
                .unwrap()
                .join(".stemma")
                .join("worktrees")
                .join("linked")
                .display()
        )
    );
    assert_eq!(read(&state, "HEAD"), "branch feature\n");
    assert_eq!(read(&state, "location"), format!("{}\n", real(&linked)));
    assert!(state.join("working-copy").is_file());
    // The shared store has the branch; the main worktree keeps its own HEAD.
    assert_eq!(
        read(&sandbox.repo().join(".stemma"), "HEAD"),
        "branch main\n"
    );
    assert!(sandbox.repo().join(".stemma/refs/heads/feature").is_file());
}

#[test]
fn a_worktree_on_an_existing_branch_checks_out_its_tip() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    ok(&repo, &["branch", "topic", "--actor", ACTOR]);

    let added = ok(
        &repo,
        &[
            "worktree",
            "add",
            "../topic-tree",
            "topic",
            "--actor",
            ACTOR,
        ],
    );

    assert!(
        added.stdout.contains("on the branch topic as entry"),
        "{}",
        added.stdout
    );
    assert_eq!(read(&sandbox.at("topic-tree"), "a.txt"), "one");
    assert_eq!(
        read(&repo.join(".stemma/worktrees/topic-tree"), "HEAD"),
        "branch topic\n"
    );
}

#[test]
fn two_worktrees_with_two_actors_on_two_branches_find_their_own_state() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let linked = sandbox.at("linked");
    ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--owner",
            ANNA,
            "--actor",
            ACTOR,
        ],
    );

    // Anna works in the worktree, the first actor in the main one, at the same time.
    write(&linked, "b.txt", "from anna");
    ok(&linked, &["track", "b.txt", "--actor", ANNA]);
    ok(&linked, &["commit", "-m", "anna", "--actor", ANNA]);
    write(&repo, "c.txt", "from daniel");
    ok(&repo, &["track", "c.txt", "--actor", ACTOR]);
    ok(&repo, &["commit", "-m", "daniel", "--actor", ACTOR]);

    // Each commit is on the branch of its own worktree, and the other branch did not move.
    let from_linked = ok(&linked, &["branch"]).stdout;
    let from_repo = ok(&repo, &["branch"]).stdout;
    let names = |text: &str| -> Vec<String> {
        text.lines()
            .map(|line| line[2..].split_whitespace().next().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        names(&from_linked),
        names(&from_repo),
        "the branches are shared"
    );
    assert!(from_linked.contains("* feature"), "{from_linked}");
    assert!(from_repo.contains("* main"), "{from_repo}");
    let on_feature = ok(&linked, &["log"]).stdout;
    assert!(
        on_feature.contains("\n    anna\n") && !on_feature.contains("\n    daniel\n"),
        "{on_feature}"
    );
    let on_main = ok(&repo, &["log"]).stdout;
    assert!(
        on_main.contains("\n    daniel\n") && !on_main.contains("\n    anna\n"),
        "{on_main}"
    );
    // The files stay apart.
    assert!(linked.join("b.txt").is_file() && !linked.join("c.txt").exists());
    assert!(repo.join("c.txt").is_file() && !repo.join("b.txt").exists());
    // The directories of the worktree below the root find it, too.
    fs::create_dir(linked.join("sub")).unwrap();
    let below = ok(&linked.join("sub"), &["branch"]).stdout;
    assert!(below.contains("* feature"), "{below}");
}

#[test]
fn an_actor_that_does_not_own_the_working_copy_of_the_worktree_is_refused() {
    let sandbox = Sandbox::new();
    ok(
        &sandbox.repo(),
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--owner",
            ANNA,
            "--actor",
            ACTOR,
        ],
    );

    let refused = run(&sandbox.at("linked"), &["status", "--actor", ACTOR]);

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(refused.stderr.contains("anna"), "{}", refused.stderr);
}

#[test]
fn list_shows_the_main_worktree_first_then_the_others_with_branch_and_owner() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--owner",
            ANNA,
            "--actor",
            ACTOR,
        ],
    );
    ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "other",
            "../another",
            "--actor",
            ACTOR,
        ],
    );

    let linked = sandbox.at("linked");
    let from_main = ok(&repo, &["worktree", "list"]).stdout;
    let from_linked = ok(&linked, &["worktree", "list"]).stdout;

    let rows: Vec<Vec<&str>> = from_main
        .lines()
        .map(|line| line.split_whitespace().collect())
        .collect();
    let expected = [
        ("main", shown(&repo), "main", "human daniel"),
        (
            "another",
            shown(&sandbox.at("another")),
            "other",
            "human daniel",
        ),
        ("linked", shown(&linked), "feature", "human anna"),
    ];
    assert_eq!(rows.len(), expected.len(), "{from_main}");
    for (row, (name, path, branch, owner)) in rows.iter().zip(&expected) {
        assert_eq!(row[..3], [*name, path.as_str(), *branch], "{from_main}");
        assert_eq!(row[3..].join(" "), *owner, "{from_main}");
    }
    assert_eq!(from_linked, from_main, "every worktree lists the same rows");
}

#[test]
fn a_worktree_that_was_moved_is_refused() {
    let sandbox = Sandbox::new();
    ok(
        &sandbox.repo(),
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );
    fs::rename(sandbox.at("linked"), sandbox.at("moved")).unwrap();

    let refused = run(&sandbox.at("moved"), &["status", "--actor", ACTOR]);

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("moved or copied"),
        "{}",
        refused.stderr
    );
    assert!(
        refused
            .stderr
            .contains("does not match its state directory"),
        "{}",
        refused.stderr
    );
}

#[test]
fn a_copy_of_a_worktree_is_refused() {
    let sandbox = Sandbox::new();
    ok(
        &sandbox.repo(),
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );
    let copy = sandbox.at("copy");
    fs::create_dir(&copy).unwrap();
    fs::copy(sandbox.at("linked").join(".stemma"), copy.join(".stemma")).unwrap();

    let refused = run(&copy, &["status", "--actor", ACTOR]);

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("moved or copied"),
        "{}",
        refused.stderr
    );
}

#[test]
fn a_worktree_whose_state_directory_is_gone_is_refused() {
    let sandbox = Sandbox::new();
    ok(
        &sandbox.repo(),
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );
    fs::remove_dir_all(sandbox.repo().join(".stemma/worktrees/linked")).unwrap();

    let refused = run(&sandbox.at("linked"), &["status", "--actor", ACTOR]);

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("no state directory"),
        "{}",
        refused.stderr
    );
}

#[test]
fn a_damaged_pointer_file_is_damage_of_the_repository() {
    let sandbox = Sandbox::new();
    ok(
        &sandbox.repo(),
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );
    fs::write(
        sandbox.at("linked").join(".stemma"),
        "stemma-worktree relative/path\n",
    )
    .unwrap();

    let refused = run(&sandbox.at("linked"), &["status", "--actor", ACTOR]);

    assert_eq!(refused.code, 5, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("not in canonical form"),
        "{}",
        refused.stderr
    );
}

#[test]
fn a_branch_that_another_worktree_has_checked_out_is_refused_by_each_command() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let linked = sandbox.at("linked");
    ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );
    ok(&repo, &["branch", "spare", "--actor", ACTOR]);

    // `switch`: the main worktree cannot take `feature`, nor the linked one `main`.
    let switch = run(&repo, &["switch", "feature", "--actor", ACTOR]);
    assert_eq!(switch.code, 1, "{}", switch.stderr);
    assert!(
        switch
            .stderr
            .contains("checked out in the worktree `linked`"),
        "{}",
        switch.stderr
    );
    let back = run(&linked, &["switch", "main", "--actor", ACTOR]);
    assert_eq!(back.code, 1, "{}", back.stderr);
    assert!(
        back.stderr.contains("checked out in the worktree `main`"),
        "{}",
        back.stderr
    );

    // `branch -d` and `branch -m`: the HEAD of the other worktree would be left without a branch.
    let delete = run(&repo, &["branch", "-d", "feature", "--actor", ACTOR]);
    assert_eq!(delete.code, 1, "{}", delete.stderr);
    assert!(
        delete
            .stderr
            .contains("checked out in the worktree `linked`"),
        "{}",
        delete.stderr
    );
    let rename = run(
        &repo,
        &["branch", "-m", "feature", "renamed", "--actor", ACTOR],
    );
    assert_eq!(rename.code, 1, "{}", rename.stderr);
    assert!(
        rename
            .stderr
            .contains("checked out in the worktree `linked`"),
        "{}",
        rename.stderr
    );
    let from_linked = run(&linked, &["branch", "-d", "main", "--actor", ACTOR]);
    assert_eq!(from_linked.code, 1, "{}", from_linked.stderr);
    assert!(
        from_linked
            .stderr
            .contains("checked out in the worktree `main`"),
        "{}",
        from_linked.stderr
    );

    // `worktree add`: neither the branch of the main worktree nor that of a linked one.
    for taken in ["feature", "main"] {
        let add = run(
            &repo,
            &["worktree", "add", "../third", taken, "--actor", ACTOR],
        );
        assert_eq!(add.code, 1, "{taken}: {}", add.stderr);
        assert!(
            add.stderr.contains("checked out in the worktree"),
            "{taken}: {}",
            add.stderr
        );
        assert!(
            !sandbox.at("third").exists(),
            "{taken}: nothing is left behind"
        );
    }

    // A branch that nobody has checked out is free, and a branch is free again once its worktree
    // switched away.
    ok(&linked, &["switch", "spare", "--actor", ACTOR]);
    ok(&repo, &["switch", "feature", "--actor", ACTOR]);
}

#[test]
fn a_path_inside_a_worktree_or_holding_one_is_refused() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );

    let cases = [
        // In the main worktree, and in a linked one.
        (
            repo.join("inside").display().to_string(),
            "lies in the worktree `main`",
        ),
        (
            sandbox.at("linked").join("inside").display().to_string(),
            "lies in the worktree `linked`",
        ),
        // Holding the main worktree, and a linked one: the sandbox holds both, `..` of a linked
        // one holds only itself.
        (
            sandbox.dir.path().display().to_string(),
            "holds the worktree",
        ),
        // The path of a worktree itself.
        (
            sandbox.at("linked").display().to_string(),
            "is the directory of the worktree `linked`",
        ),
        (
            repo.display().to_string(),
            "is the directory of the worktree `main`",
        ),
    ];
    for (path, message) in cases {
        let refused = run(
            &repo,
            &["worktree", "add", "-b", "more", &path, "--actor", ACTOR],
        );
        assert_eq!(refused.code, 1, "{path}: {}", refused.stderr);
        assert!(
            refused.stderr.contains(message),
            "{path}: {}",
            refused.stderr
        );
    }
    // Nothing was made by the refusals: no branch, no extra state directory.
    assert!(!repo.join(".stemma/refs/heads/more").exists());
    assert_eq!(
        fs::read_dir(repo.join(".stemma/worktrees"))
            .unwrap()
            .count(),
        1
    );
    assert!(!repo.join("inside").exists());
}

#[test]
fn a_worktree_needs_a_branch() {
    let sandbox = Sandbox::new();

    let none = run(
        &sandbox.repo(),
        &["worktree", "add", "../linked", "--actor", ACTOR],
    );
    let both = run(
        &sandbox.repo(),
        &[
            "worktree",
            "add",
            "../linked",
            "main",
            "-b",
            "x",
            "--actor",
            ACTOR,
        ],
    );

    assert_eq!(none.code, 2, "{}", none.stderr);
    assert_eq!(both.code, 2, "{}", both.stderr);
    assert!(!sandbox.at("linked").exists());
}

#[test]
fn a_directory_that_is_in_the_way_is_refused_unless_it_is_empty() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    write(&sandbox.dir.path().join("full"), "x", "x");
    fs::create_dir(sandbox.at("empty")).unwrap();

    let full = run(
        &repo,
        &["worktree", "add", "-b", "one", "../full", "--actor", ACTOR],
    );
    let empty = run(
        &repo,
        &["worktree", "add", "-b", "two", "../empty", "--actor", ACTOR],
    );

    assert_eq!(full.code, 1, "{}", full.stderr);
    assert!(
        full.stderr.contains("not an empty directory"),
        "{}",
        full.stderr
    );
    assert!(
        sandbox.at("full").join("x").is_file(),
        "the directory is left alone"
    );
    assert_eq!(empty.code, 0, "{}", empty.stderr);
    assert_eq!(read(&sandbox.at("empty"), "a.txt"), "one");
}

#[test]
fn a_new_branch_needs_a_name_that_is_free() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    let taken = run(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "main",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );

    assert_eq!(taken.code, 1, "{}", taken.stderr);
    assert!(
        !sandbox.at("linked").exists(),
        "the directory that the call made goes again"
    );
}

#[test]
fn the_entry_names_the_new_pointers_and_the_branch() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let added = ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );

    let entry = added
        .stdout
        .split("as entry ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap();
    let log = ok(&repo, &["op", "log", "--json", "-n", "1"]).stdout;

    assert!(log.contains(&format!("\"number\":{entry}")), "{log}");
    assert!(log.contains("worktree-add"), "{log}");
    assert!(log.contains("feature"), "{log}");
}

/// The secret that the purge tests commit; its blob ID is what `purge` takes.
const SECRET: &str = "key=hunter2";

fn secret() -> String {
    stemma_core::object::Blob::new(SECRET).id().to_string()
}

/// A sandbox with a linked worktree `linked` on the new branch `feature`.
fn with_linked() -> Sandbox {
    let sandbox = Sandbox::new();
    ok(
        &sandbox.repo(),
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );
    sandbox
}

#[test]
fn a_purge_rewrites_the_working_copy_of_another_worktree_and_keeps_its_files() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    // The secret is in the working-copy commit of the linked worktree, not committed.
    write(&linked, "key", SECRET);
    ok(&linked, &["track", "key", "--actor", ACTOR]);
    let before = fs::read_to_string(linked.join(".stemma")).unwrap();

    let plan = ok(&repo, &["purge", &secret(), "--actor", ACTOR]);
    assert!(
        plan.stdout
            .contains("    working-copy of the worktree linked  "),
        "{}",
        plan.stdout
    );
    let applied = ok(&repo, &["purge", &secret(), "--apply", "--actor", ACTOR]);

    assert!(
        applied
            .stdout
            .contains("working-copy of the worktree linked"),
        "{}",
        applied.stdout
    );
    // The commit of the linked worktree no longer holds the key, and the worktree opens.
    let state = repo.join(".stemma/worktrees/linked");
    let working_copy = read(&state, "working-copy");
    assert!(!working_copy.contains("b3:0000"), "{working_copy}");
    assert_eq!(fs::read_to_string(linked.join(".stemma")).unwrap(), before);
    let status = ok(&linked, &["status", "--actor", ACTOR]);
    assert!(status.stdout.contains("key"), "{}", status.stdout);
    // The purge was run from the main worktree; the same purge again finds nothing to rewrite.
    let again = run(&repo, &["purge", &secret(), "--actor", ACTOR]);
    assert_eq!(again.code, 1, "{}", again.stderr);
}

#[test]
fn a_purge_run_in_a_linked_worktree_moves_the_pointers_of_the_main_worktree() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&repo, "key", SECRET);
    ok(&repo, &["track", "key", "--actor", ACTOR]);
    ok(&repo, &["commit", "-m", "second", "--actor", ACTOR]);

    let applied = ok(&linked, &["purge", &secret(), "--apply", "--actor", ACTOR]);

    assert!(applied.stdout.contains("branch main"), "{}", applied.stdout);
    assert!(
        applied.stdout.contains("working-copy"),
        "{}",
        applied.stdout
    );
    let log = ok(&repo, &["log"]);
    assert!(log.stdout.contains("second"), "{}", log.stdout);
    let verify = ok(&repo, &["op", "verify"]);
    assert_eq!(verify.code, 0);
}

#[test]
fn op_restore_does_not_delete_a_branch_that_another_worktree_has_checked_out() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    ok(&repo, &["branch", "topic", "--actor", ACTOR]);
    let entry = common::numbers(&repo).last().unwrap().to_string();
    ok(
        &repo,
        &["worktree", "add", "../linked", "topic", "--actor", ACTOR],
    );

    let refused = run(&repo, &["op", "restore", &entry, "--actor", ACTOR]);

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused
            .stderr
            .contains("checked out in the worktree `linked`"),
        "{}",
        refused.stderr
    );
    assert!(repo.join(".stemma/refs/heads/topic").is_file());
}

#[test]
fn a_path_with_dot_dot_below_a_missing_directory_is_refused() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    // It would lead back to the main worktree, which no check of the names sees.
    let refused = run(
        &repo,
        &[
            "worktree",
            "add",
            "../nowhere/../repo",
            "-b",
            "x",
            "--actor",
            ACTOR,
        ],
    );

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    // Windows folds the `..` away before the check; the main worktree is refused by name then.
    if cfg!(not(windows)) {
        assert!(refused.stderr.contains(".."), "{}", refused.stderr);
    }
    assert!(!sandbox.at("nowhere").exists());
    assert!(!repo.join(".stemma/refs/heads/x").exists());
}

#[test]
fn a_stray_file_or_a_stopped_add_in_the_state_directories_is_no_worktree() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );
    let states = repo.join(".stemma/worktrees");
    fs::write(states.join(".DS_Store"), "x").unwrap();
    // An add that stopped before it wrote `HEAD`.
    fs::create_dir(states.join("ghost")).unwrap();
    fs::write(states.join("ghost/location"), "/nowhere\n").unwrap();

    let list = ok(&repo, &["worktree", "list"]).stdout;
    let added = run(
        &repo,
        &["worktree", "add", "-b", "more", "../more", "--actor", ACTOR],
    );
    let switched = run(&repo, &["switch", "-c", "free", "--actor", ACTOR]);

    assert_eq!(list.lines().count(), 2, "{list}");
    assert_eq!(added.code, 0, "{}", added.stderr);
    assert_eq!(switched.code, 0, "{}", switched.stderr);
}

#[test]
fn a_new_branch_in_a_repository_without_a_commit_has_no_start() {
    let dir = Dir::new();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    ok(&repo, &["init", "--actor", ACTOR]);

    let refused = run(
        &repo,
        &["worktree", "add", "-b", "x", "../linked", "--actor", ACTOR],
    );

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("no commit yet"),
        "{}",
        refused.stderr
    );
    assert!(!dir.path().join("linked").exists());
}

#[test]
fn a_new_branch_starts_where_head_of_the_calling_worktree_is() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let linked = sandbox.at("linked");
    ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );
    write(&linked, "b.txt", "on feature");
    ok(&linked, &["track", "b.txt", "--actor", ACTOR]);
    ok(&linked, &["commit", "-m", "second", "--actor", ACTOR]);

    ok(
        &linked,
        &[
            "worktree", "add", "-b", "child", "../child", "--actor", ACTOR,
        ],
    );

    // The files of `feature` are there; `main` does not have them.
    assert_eq!(read(&sandbox.at("child"), "b.txt"), "on feature");
    assert!(!repo.join("b.txt").exists());
}

#[test]
fn worktrees_with_the_same_last_part_get_their_own_state_directories() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();

    ok(
        &repo,
        &["worktree", "add", "-b", "one", "../a/x", "--actor", ACTOR],
    );
    ok(
        &repo,
        &["worktree", "add", "-b", "two", "../b/x", "--actor", ACTOR],
    );
    ok(
        &repo,
        &[
            "worktree", "add", "-b", "three", "../main", "--actor", ACTOR,
        ],
    );

    let list = ok(&repo, &["worktree", "list"]).stdout;
    let names: Vec<&str> = list
        .lines()
        .map(|line| line.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(names, ["main", "main-2", "x", "x-2"], "{list}");
    assert_eq!(
        read(&repo.join(".stemma/worktrees/x-2"), "location"),
        format!("{}\n", real(&sandbox.at("b/x")))
    );
}

#[test]
fn two_adds_of_the_same_path_at_once_leave_one_worktree() {
    for round in 0..3 {
        let sandbox = Sandbox::new();
        let repo = sandbox.repo();
        let runs: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|branch| {
                let repo = repo.clone();
                std::thread::spawn(move || {
                    run(
                        &repo,
                        &["worktree", "add", "-b", branch, "../same", "--actor", ACTOR],
                    )
                })
            })
            .collect();
        let runs: Vec<_> = runs.into_iter().map(|run| run.join().unwrap()).collect();

        let won = runs.iter().filter(|run| run.code == 0).count();
        assert_eq!(
            won,
            1,
            "round {round}: {:?}",
            runs.iter().map(|run| &run.stderr).collect::<Vec<_>>()
        );
        let linked = sandbox.at("same");
        assert_eq!(read(&linked, "a.txt"), "one", "round {round}");
        assert!(linked.join(".stemma").is_file(), "round {round}");
        let list = ok(&repo, &["worktree", "list"]).stdout;
        assert_eq!(list.lines().count(), 2, "round {round}: {list}");
    }
}

// ---- worktree remove

#[test]
fn remove_deletes_the_directory_and_the_state_and_keeps_the_branch() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    let shown = shown(&linked);

    let removed = ok(&repo, &["worktree", "remove", "linked", "--actor", ACTOR]);

    assert!(
        removed
            .stdout
            .starts_with(&format!("Removed the worktree linked in {shown} as entry ")),
        "{}",
        removed.stdout
    );
    assert!(!linked.exists());
    assert!(!repo.join(".stemma/worktrees/linked").exists());
    assert!(repo.join(".stemma/refs/heads/feature").is_file());
    let list = ok(&repo, &["worktree", "list"]).stdout;
    assert_eq!(list.lines().count(), 1, "{list}");
    // The branch is free again.
    ok(
        &repo,
        &["worktree", "add", "../again", "feature", "--actor", ACTOR],
    );
}

#[test]
fn remove_names_the_worktree_its_directory_and_the_pointers_in_the_entry() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    let location = real(&linked);
    ok(&repo, &["worktree", "remove", "linked", "--actor", ACTOR]);

    let log = ok(&repo, &["op", "log", "--json", "-n", "1"]).stdout;

    assert!(log.contains("\"operation\":\"worktree-remove\""), "{log}");
    assert!(log.contains("\"worktree\":\"linked\""), "{log}");
    assert!(
        log.contains(&format!("\"location\":{}", stemma_quote(&location))),
        "{log}"
    );
    assert!(log.contains("\"new\":null"), "{log}");
    let text = ok(&repo, &["op", "log", "-n", "1"]).stdout;
    assert!(text.contains("    worktree linked  "), "{text}");
}

/// The path as JSON writes it.
fn stemma_quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\"))
}

#[test]
fn remove_accepts_the_path_of_the_directory() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));

    ok(
        &repo,
        &["worktree", "remove", "../linked", "--actor", ACTOR],
    );

    assert!(!linked.exists());
}

#[test]
fn remove_saves_the_unsaved_files_first() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&linked, "a.txt", "changed");

    let removed = ok(&repo, &["worktree", "remove", "linked", "--actor", ACTOR]);

    assert!(
        removed
            .stdout
            .contains("The files were saved first as entry "),
        "{}",
        removed.stdout
    );
    let log = ok(&repo, &["op", "log", "--json"]).stdout;
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines[0].contains("worktree-remove"), "{log}");
    assert!(lines[1].contains("\"operation\":\"snapshot\""), "{log}");
    assert!(lines[1].contains("\"worktree\":\"linked\""), "{log}");
}

#[test]
fn remove_refuses_files_that_are_not_tracked_unless_forced() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&linked, "notes/todo.txt", "keep me");
    let entries = common::numbers(&repo).len();

    let refused = run(&repo, &["worktree", "remove", "linked", "--actor", ACTOR]);

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("notes/todo.txt") && refused.stderr.contains("--force"),
        "{}",
        refused.stderr
    );
    assert_eq!(read(&linked, "notes/todo.txt"), "keep me");
    assert!(repo.join(".stemma/worktrees/linked/HEAD").is_file());
    assert_eq!(common::numbers(&repo).len(), entries, "nothing was written");

    ok(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );
    assert!(!linked.exists());
}

#[test]
fn remove_refuses_the_main_worktree_and_the_worktree_it_runs_in() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));

    let main = run(&linked, &["worktree", "remove", "main", "--actor", ACTOR]);
    let current = run(&linked, &["worktree", "remove", "linked", "--actor", ACTOR]);
    let unknown = run(&repo, &["worktree", "remove", "nowhere", "--actor", ACTOR]);

    assert_eq!(main.code, 1, "{}", main.stderr);
    assert!(main.stderr.contains("main worktree"), "{}", main.stderr);
    assert_eq!(current.code, 1, "{}", current.stderr);
    assert!(
        current.stderr.contains("another worktree"),
        "{}",
        current.stderr
    );
    assert_eq!(unknown.code, 1, "{}", unknown.stderr);
    assert!(
        unknown.stderr.contains("no linked worktree"),
        "{}",
        unknown.stderr
    );
    assert!(linked.join(".stemma").is_file());
}

#[test]
fn remove_prunes_the_state_of_a_directory_that_was_deleted_outside() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    fs::remove_dir_all(&linked).unwrap();

    let removed = ok(&repo, &["worktree", "remove", "linked", "--actor", ACTOR]);

    assert!(
        removed.stdout.contains("The directory was gone already"),
        "{}",
        removed.stdout
    );
    assert!(!repo.join(".stemma/worktrees/linked").exists());
    assert_eq!(ok(&repo, &["worktree", "list"]).stdout.lines().count(), 1);
}

#[test]
fn remove_does_not_touch_a_directory_that_is_not_the_worktree() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    // The pointer file names another state directory: the directory was replaced.
    fs::write(
        linked.join(".stemma"),
        "stemma-worktree /elsewhere/.stemma/worktrees/x\n",
    )
    .unwrap();
    write(&linked, "precious.txt", "mine");

    let refused = run(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("pointer file"),
        "{}",
        refused.stderr
    );
    assert_eq!(read(&linked, "precious.txt"), "mine");
}

#[test]
fn an_agent_cannot_remove_the_worktree_of_a_human() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&linked, "a.txt", "changed");

    let refused = run(
        &repo,
        &[
            "worktree",
            "remove",
            "linked",
            "--actor",
            "agent bot model=m session=s sponsor=daniel",
        ],
    );

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(linked.join(".stemma").is_file());
    assert_eq!(read(&linked, "a.txt"), "changed");
}

// ---- op restore per worktree

#[test]
fn the_entry_of_a_linked_worktree_names_it_and_the_entry_of_the_main_one_does_not() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&linked, "b.txt", "two");
    ok(&linked, &["track", "b.txt", "--actor", ACTOR]);
    write(&repo, "c.txt", "three");
    ok(&repo, &["track", "c.txt", "--actor", ACTOR]);

    let log = ok(&repo, &["op", "log", "--json"]).stdout;
    let lines: Vec<&str> = log.lines().collect();

    assert!(lines[0].contains("\"worktree\":\"main\""), "{log}");
    assert!(lines[1].contains("\"worktree\":\"linked\""), "{log}");
    let file = common::entry_file(
        &repo,
        common::numbers(&repo)[common::numbers(&repo).len() - 1],
    );
    assert!(!fs::read_to_string(file).unwrap().contains("worktree "));
}

#[test]
fn op_restore_of_an_entry_of_another_worktree_is_refused_and_works_in_that_worktree() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&linked, "b.txt", "two");
    ok(&linked, &["track", "b.txt", "--actor", ACTOR]);
    let entry = common::numbers(&repo).last().unwrap().to_string();

    let refused = run(&repo, &["op", "restore", &entry, "--actor", ACTOR]);

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("worktree `linked`")
            && refused.stderr.contains(&format!("op restore {entry}")),
        "{}",
        refused.stderr
    );
    assert!(!repo.join("b.txt").exists());
    assert_eq!(read(&linked, "b.txt"), "two");

    ok(&linked, &["op", "restore", &entry, "--actor", ACTOR]);

    assert!(!linked.join("b.txt").exists());
    assert!(!repo.join("b.txt").exists());
}

#[test]
fn op_restore_of_the_entry_that_added_or_removed_a_worktree_is_refused() {
    let sandbox = with_linked();
    let repo = sandbox.repo();
    let added = common::numbers(&repo).last().unwrap().to_string();
    let refused = run(&repo, &["op", "restore", &added, "--actor", ACTOR]);
    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("added or removed a worktree"),
        "{}",
        refused.stderr
    );

    ok(&repo, &["worktree", "remove", "linked", "--actor", ACTOR]);
    let removed = common::numbers(&repo).last().unwrap().to_string();
    let refused = run(&repo, &["op", "restore", &removed, "--actor", ACTOR]);
    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("added or removed a worktree"),
        "{}",
        refused.stderr
    );
}

#[test]
fn op_log_judges_the_entries_of_each_worktree_by_its_own_pointers() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&linked, "b.txt", "two");
    ok(&linked, &["track", "b.txt", "--actor", ACTOR]);
    write(&repo, "c.txt", "three");
    ok(&repo, &["track", "c.txt", "--actor", ACTOR]);

    let from_main = ok(&repo, &["op", "log", "--json"]).stdout;
    let from_linked = ok(&linked, &["op", "log", "--json"]).stdout;

    // Each worktree knows its own newest entry; the other one's is not known there.
    let took = |log: &str, worktree: &str| -> String {
        let line = log
            .lines()
            .find(|line| line.contains(&format!("\"worktree\":\"{worktree}\"")))
            .unwrap();
        line.rsplit("\"took_effect\":")
            .next()
            .unwrap()
            .trim_end_matches('}')
            .to_owned()
    };
    assert_eq!(took(&from_main, "main"), "true", "{from_main}");
    assert_eq!(took(&from_linked, "linked"), "true", "{from_linked}");
    assert_eq!(took(&from_main, "linked"), "null", "{from_main}");
    assert_eq!(took(&from_linked, "main"), "null", "{from_linked}");
}

#[test]
fn an_agent_cannot_prune_the_state_of_a_human_even_if_the_directory_is_gone() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    fs::remove_dir_all(&linked).unwrap();

    let refused = run(
        &repo,
        &[
            "worktree",
            "remove",
            "linked",
            "--actor",
            "agent bot model=m session=s sponsor=daniel",
        ],
    );

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(repo.join(".stemma/worktrees/linked/HEAD").is_file());
}

#[test]
fn remove_with_force_names_the_files_that_no_commit_holds() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&linked, "notes/todo.txt", "gone");

    let removed = ok(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );

    assert!(
        removed
            .stdout
            .contains("Also removed 1 file that no commit holds:\n    notes/todo.txt\n"),
        "{}",
        removed.stdout
    );
}

#[test]
fn op_restore_of_an_entry_of_a_removed_worktree_is_refused() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&linked, "b.txt", "two");
    ok(&linked, &["track", "b.txt", "--actor", ACTOR]);
    let entry = common::numbers(&repo).last().unwrap().to_string();
    ok(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );

    let refused = run(&repo, &["op", "restore", &entry, "--actor", ACTOR]);

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("removed or added again"),
        "{}",
        refused.stderr
    );
}

#[test]
fn op_restore_of_a_branch_only_entry_of_a_removed_worktree_works_from_main() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    ok(&linked, &["branch", "topic", "--actor", ACTOR]);
    let entry = common::numbers(&repo).last().unwrap().to_string();
    ok(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );

    let restored = run(&repo, &["op", "restore", &entry, "--actor", ACTOR]);

    // Branches are shared, so the name of the removed worktree does not matter for this entry.
    assert_eq!(restored.code, 0, "{}", restored.stderr);
}

#[test]
fn a_remove_that_cannot_delete_the_directory_keeps_the_worktree_and_a_retry_finishes() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&linked, "b.txt", "two");
    ok(&linked, &["track", "b.txt", "--actor", ACTOR]);
    let older = common::numbers(&repo).last().unwrap().to_string();

    // The directory `linked` is a name directly in the sandbox: its removal is refused.
    let failed = common::run_failing(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
        &[("write", sandbox.dir.path())],
    );

    assert_eq!(failed.code, 3, "{}", failed.stderr);
    assert!(linked.join("b.txt").exists(), "the files are still there");
    let listed = ok(&repo, &["worktree", "list"]).stdout;
    assert!(listed.contains("linked"), "{listed}");
    // The removal did not take effect, so an older entry of the worktree is still restorable.
    let restored = run(&linked, &["op", "restore", &older, "--actor", ACTOR]);
    assert!(
        !restored.stderr.contains("removed or added again"),
        "{}",
        restored.stderr
    );

    // Run again without the failure: the state and the directory go.
    ok(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );
    assert!(!linked.exists());
    assert!(!ok(&repo, &["worktree", "list"]).stdout.contains("linked"));
    // The directory went through a hidden name; nothing of it is left.
    let left: Vec<_> = fs::read_dir(sandbox.dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".stemma-removing-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_damaged_worktree_does_not_stop_the_removal_of_another() {
    let sandbox = with_linked();
    let repo = sandbox.repo();
    ok(
        &repo,
        &[
            "worktree", "add", "-b", "other", "../other", "--actor", ACTOR,
        ],
    );
    let state = repo.join(".stemma").join("worktrees").join("other");
    fs::remove_file(state.join("location")).unwrap();

    let removed = run(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );
    let damaged = run(
        &repo,
        &["worktree", "remove", "other", "--force", "--actor", ACTOR],
    );

    assert_eq!(removed.code, 0, "{}", removed.stderr);
    assert!(!sandbox.at("linked").exists());
    // The damaged worktree itself is named, with what to do, and nothing of it was touched.
    assert_ne!(damaged.code, 0);
    assert!(
        damaged.stderr.contains("worktree `other` cannot be read"),
        "{}",
        damaged.stderr
    );
    assert!(sandbox.at("other").exists() && state.exists());
}

#[test]
fn a_state_directory_without_head_is_explained_by_remove_and_taken_over_by_add() {
    let sandbox = with_linked();
    let repo = sandbox.repo();
    // What a removal that stopped after the first step of the state leaves.
    let state = repo.join(".stemma").join("worktrees").join("linked");
    fs::remove_file(state.join("HEAD")).unwrap();

    let removed = run(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );

    assert_eq!(removed.code, 1, "{}", removed.stderr);
    assert!(
        removed.stderr.contains("holds no worktree"),
        "{}",
        removed.stderr
    );
    // An add in a directory with the same last part takes the name over and cleans the rest.
    fs::create_dir(sandbox.at("nested")).unwrap();
    ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "again",
            "../nested/linked",
            "--actor",
            ACTOR,
        ],
    );
    assert!(state.join("HEAD").exists());
    let listed = ok(&repo, &["worktree", "list"]).stdout;
    assert!(listed.contains("nested"), "{listed}");
}

// STEMMA-196: the refusals of `op restore`, in the order of ADR-013, one test each.

#[test]
fn op_restore_of_an_entry_of_the_main_worktree_is_refused_in_a_linked_one() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&repo, "c.txt", "three");
    ok(&repo, &["track", "c.txt", "--actor", ACTOR]);
    let entry = common::numbers(&repo).last().unwrap().to_string();

    let refused = run(&linked, &["op", "restore", &entry, "--actor", ACTOR]);

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("worktree `main`")
            && refused.stderr.contains(&format!("op restore {entry}")),
        "{}",
        refused.stderr
    );
    assert!(repo.join("c.txt").exists(), "nothing was restored");
    assert!(!linked.join("c.txt").exists());

    ok(&repo, &["op", "restore", &entry, "--actor", ACTOR]);

    assert!(!repo.join("c.txt").exists());
}

#[test]
fn op_restore_of_an_entry_of_a_worktree_that_was_added_again_is_refused() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&linked, "b.txt", "two");
    ok(&linked, &["track", "b.txt", "--actor", ACTOR]);
    let entry = common::numbers(&repo).last().unwrap().to_string();
    ok(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );
    // The same name and the same place stand for another worktree now.
    ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "second",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );

    let refused = run(
        &sandbox.at("linked"),
        &["op", "restore", &entry, "--actor", ACTOR],
    );

    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("worktree `linked`")
            && refused.stderr.contains("removed or added again"),
        "{}",
        refused.stderr
    );
    // `--force` does not lift it: the name may stand for another worktree.
    let forced = run(
        &sandbox.at("linked"),
        &["op", "restore", &entry, "--force", "--actor", ACTOR],
    );
    assert_eq!(forced.code, 1, "{}", forced.stderr);
    assert!(
        forced.stderr.contains("removed or added again"),
        "{}",
        forced.stderr
    );
}

#[test]
fn the_refusals_of_op_restore_come_in_the_order_of_adr_013() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    let added = common::numbers(&repo).last().unwrap().to_string();
    write(&linked, "b.txt", "two");
    ok(&linked, &["track", "b.txt", "--actor", ACTOR]);
    let tracked = common::numbers(&repo).last().unwrap().to_string();
    ok(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );
    let removed = common::numbers(&repo).last().unwrap().to_string();

    // The entries of `worktree add` and `worktree remove` come first, also for a worktree that
    // is gone and is not the one where the restore runs.
    for entry in [&added, &removed] {
        let refused = run(&repo, &["op", "restore", entry, "--actor", ACTOR]);
        assert_eq!(refused.code, 1, "{}", refused.stderr);
        assert!(
            refused.stderr.contains("added or removed a worktree"),
            "{entry}: {}",
            refused.stderr
        );
    }
    // A worktree that is gone is named before the worktree where the entry would have to run.
    let gone = run(&repo, &["op", "restore", &tracked, "--actor", ACTOR]);
    assert_eq!(gone.code, 1, "{}", gone.stderr);
    assert!(
        gone.stderr.contains("removed or added again"),
        "{}",
        gone.stderr
    );
    assert!(!gone.stderr.contains("run `op restore"), "{}", gone.stderr);
}

/// A repository with the secret committed, and a linked worktree made from that history, in which
/// a file is tracked. Returns the sandbox and the number of the entry of that file.
fn with_secret_in_linked() -> (Sandbox, String) {
    let sandbox = Sandbox::new();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&repo, "key", SECRET);
    ok(&repo, &["track", "key", "--actor", ACTOR]);
    ok(&repo, &["commit", "-m", "second", "--actor", ACTOR]);
    ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "../linked",
            "--actor",
            ACTOR,
        ],
    );
    write(&linked, "b.txt", "two");
    ok(&linked, &["track", "b.txt", "--actor", ACTOR]);
    let entry = common::numbers(&repo).last().unwrap().to_string();
    (sandbox, entry)
}

#[test]
fn op_restore_of_an_entry_of_another_worktree_is_refused_by_the_journal_of_a_purge_first() {
    let (sandbox, entry) = with_secret_in_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    let oplog = repo.join(".stemma/oplog");
    let stopped = common::run_failing(
        &repo,
        &["purge", &secret(), "--apply", "--actor", ACTOR],
        &[("write", &oplog)],
    );
    assert_eq!(stopped.code, 3, "{}", stopped.stderr);
    assert!(repo.join(".stemma/purge-pending").exists());

    // The journal keeps every writer out, in every worktree, before the entry is looked at: the
    // refusal does not depend on whether the entry belongs to the worktree where it runs.
    for dir in [&repo, &linked] {
        let refused = run(dir, &["op", "restore", &entry, "--actor", ACTOR]);
        assert_eq!(refused.code, 1, "{}", refused.stderr);
        assert!(
            refused.stderr.contains("a purge has not finished")
                && refused.stderr.contains("stemma purge --resume"),
            "{}",
            refused.stderr
        );
    }
    assert_eq!(read(&linked, "b.txt"), "two", "nothing was restored");

    ok(&repo, &["purge", "--resume", "--actor", ACTOR]);
    assert!(!repo.join(".stemma/purge-pending").exists());
}

// STEMMA-196: hardening.

#[test]
fn remove_with_force_lists_at_most_twenty_files_and_counts_the_others() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    for number in 0..25 {
        write(&linked, &format!("junk/{number:02}.txt"), "gone");
    }

    let removed = ok(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );

    assert!(
        removed
            .stdout
            .contains("Also removed 25 files that no commit holds:\n"),
        "{}",
        removed.stdout
    );
    assert!(
        removed.stdout.contains("    junk/19.txt\n"),
        "{}",
        removed.stdout
    );
    assert!(
        !removed.stdout.contains("junk/20.txt"),
        "{}",
        removed.stdout
    );
    assert!(
        removed.stdout.contains("    and 5 more\n"),
        "{}",
        removed.stdout
    );
    assert!(!linked.exists());
}

#[test]
fn remove_with_force_lists_all_files_up_to_twenty() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    for number in 0..20 {
        write(&linked, &format!("junk/{number:02}.txt"), "gone");
    }

    let removed = ok(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );

    assert!(
        removed.stdout.contains("    junk/19.txt\n"),
        "{}",
        removed.stdout
    );
    assert!(!removed.stdout.contains("more\n"), "{}", removed.stdout);
}

/// Makes the state directory `linked` also exist under `name`, as a copy or a hand-made directory
/// would.
fn with_bad_state_name(sandbox: &Sandbox, name: &str) {
    let states = sandbox.repo().join(".stemma/worktrees");
    fs::create_dir(states.join(name)).unwrap();
    for file in ["HEAD", "working-copy", "location"] {
        fs::copy(
            states.join("linked").join(file),
            states.join(name).join(file),
        )
        .unwrap();
    }
}

#[test]
fn a_state_directory_with_a_name_no_worktree_can_have_is_damage_and_stops_a_purge_before_its_journal(
) {
    // One name for each way `valid_worktree_name` refuses that a directory can have: a space, a
    // dot first, and `main`. (A name of more than 255 bytes cannot exist on a disk.)
    for bad in ["bad name", ".hidden", "main"] {
        let sandbox = Sandbox::new();
        let repo = sandbox.repo();
        write(&repo, "key", SECRET);
        ok(&repo, &["track", "key", "--actor", ACTOR]);
        ok(&repo, &["commit", "-m", "second", "--actor", ACTOR]);
        ok(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                "../linked",
                "--actor",
                ACTOR,
            ],
        );
        ok(&repo, &["branch", "spare", "--actor", ACTOR]);
        with_bad_state_name(&sandbox, bad);

        let purge = run(&repo, &["purge", &secret(), "--apply", "--actor", ACTOR]);
        let list = run(&repo, &["worktree", "list"]);

        // No journal that the reader of the journal would refuse: nothing is written.
        assert_eq!(purge.code, 5, "{bad}: {}", purge.stderr);
        assert!(!repo.join(".stemma/purge-pending").exists(), "{bad}");
        assert_eq!(list.code, 5, "{bad}: {}", list.stderr);
        // The commands that ask which worktree has a branch checked out stop at it as well.
        let delete = run(&repo, &["branch", "-d", "spare", "--actor", ACTOR]);
        let switch = run(&repo, &["switch", "spare", "--actor", ACTOR]);
        assert_eq!(delete.code, 5, "{bad}: {}", delete.stderr);
        assert_eq!(switch.code, 5, "{bad}: {}", switch.stderr);
        assert!(
            list.stderr.contains("none that a worktree can have"),
            "{bad}: {}",
            list.stderr
        );
        // The damaged name does not hide the others: the real worktree is removed.
        let removed = run(
            &repo,
            &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
        );
        assert_eq!(removed.code, 0, "{bad}: {}", removed.stderr);
    }
}

#[test]
fn a_snapshot_that_is_not_flushed_makes_a_refused_removal_code_3_and_the_message_says_so() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&linked, "a.txt", "changed");
    write(&linked, "loose.txt", "three");
    // The snapshot moves the working-copy commit of the worktree, and the flush of its state
    // directory fails; the refusal for the untracked file follows.
    let state = repo.join(".stemma/worktrees/linked");

    let refused = common::run_failing(
        &repo,
        &["worktree", "remove", "linked", "--actor", ACTOR],
        &[("read", &state)],
    );

    assert_eq!(refused.code, 3, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("saved first as entry")
            && refused.stderr.contains("loose.txt")
            && refused.stderr.contains("may not survive a crash"),
        "{}",
        refused.stderr
    );
    assert!(linked.join("loose.txt").exists(), "nothing was removed");
}

// Windows refuses a line break and ESC in the name of a directory.
#[cfg(unix)]
#[test]
fn a_line_break_in_a_path_cannot_forge_a_line_of_an_error() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    // A directory that is not empty is refused, and the message names its path as it is on disk.
    let crowded = sandbox.at("a\n\u{1b}[31mstemma: forged\u{202e}");
    fs::create_dir(&crowded).unwrap();
    fs::write(crowded.join("file"), "x").unwrap();

    let failed = run(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "x",
            crowded.to_str().unwrap(),
            "--actor",
            ACTOR,
        ],
    );

    assert_eq!(failed.code, 1, "{}", failed.stderr);
    assert_eq!(failed.stderr.lines().count(), 1, "{:?}", failed.stderr);
    assert!(
        failed.stderr.contains("is not an empty directory"),
        "{:?}",
        failed.stderr
    );
    assert!(
        !failed.stderr.contains('\u{1b}') && !failed.stderr.contains('\u{202e}'),
        "{:?}",
        failed.stderr
    );
}

#[test]
fn a_refused_removal_after_a_snapshot_names_the_entry_and_ends_with_code_1() {
    let sandbox = with_linked();
    let (repo, linked) = (sandbox.repo(), sandbox.at("linked"));
    write(&linked, "a.txt", "changed");
    write(&linked, "loose.txt", "three");

    let refused = run(&repo, &["worktree", "remove", "linked", "--actor", ACTOR]);

    // The snapshot wrote an entry, the worktree is as it was, and the same call with `--force` is
    // safe: the code is the one of the refusal (ADR-018), and the message names the entry.
    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("saved first as entry") && refused.stderr.contains("loose.txt"),
        "{}",
        refused.stderr
    );
    assert!(linked.join("loose.txt").exists());
    ok(
        &repo,
        &["worktree", "remove", "linked", "--force", "--actor", ACTOR],
    );
    assert!(!linked.exists());
}

#[test]
fn an_empty_directory_with_a_strange_name_among_the_states_is_no_worktree_and_no_damage() {
    let sandbox = with_linked();
    let repo = sandbox.repo();
    write(&repo, "key", SECRET);
    ok(&repo, &["track", "key", "--actor", ACTOR]);
    ok(&repo, &["commit", "-m", "second", "--actor", ACTOR]);
    fs::create_dir(repo.join(".stemma/worktrees/tmp dir")).unwrap();
    fs::create_dir(repo.join(".stemma/worktrees/.hidden")).unwrap();

    let list = run(&repo, &["worktree", "list"]);
    let purge = run(&repo, &["purge", &secret(), "--apply", "--actor", ACTOR]);
    let added = run(
        &repo,
        &[
            "worktree", "add", "-b", "other", "../other", "--actor", ACTOR,
        ],
    );

    assert_eq!(list.code, 0, "{}", list.stderr);
    assert_eq!(purge.code, 0, "{}", purge.stderr);
    assert!(purge.stdout.contains("branch main"), "{}", purge.stdout);
    assert!(!repo.join(".stemma/purge-pending").exists());
    ok(&repo, &["op", "verify"]);
    assert!(list.stdout.contains("linked") && !list.stdout.contains("tmp dir"));
    assert_eq!(added.code, 0, "{}", added.stderr);
}

#[cfg(unix)]
#[test]
fn a_head_that_cannot_be_inspected_is_an_error_and_not_a_worktree_that_is_missing() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = with_linked();
    let repo = sandbox.repo();
    let state = repo.join(".stemma/worktrees/linked");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o000)).unwrap();
    // A user who may read it anyway (root) cannot make the situation.
    let unreadable = fs::symlink_metadata(state.join("HEAD")).is_err();

    let list = run(&repo, &["worktree", "list"]);
    let remove = run(&repo, &["worktree", "remove", "linked", "--actor", ACTOR]);
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    if !unreadable {
        return;
    }

    assert_eq!(list.code, 1, "{}", list.stderr);
    assert!(list.stderr.contains("inspect"), "{}", list.stderr);
    assert_ne!(remove.code, 0, "{}", remove.stderr);
    assert!(
        !remove.stderr.contains("no worktree") && !remove.stderr.contains("unfinished"),
        "{}",
        remove.stderr
    );
}
