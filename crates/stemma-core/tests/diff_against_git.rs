//! Compares the indent heuristic of `diff_lines` with `git diff` on pseudo-random files.
//!
//! The tests start a process per case and live in their own test binary on purpose: a process
//! that forks while another thread holds a file lock keeps the lock alive for a moment, which
//! would make the lock tests of the library fail at random.

use stemma_core::diff::{diff, diff_byte_lines, diff_lines, Edit};

/// Per line of each side, whether the script changes it.
fn changed_flags(edits: &[Edit], old_len: usize, new_len: usize) -> (Vec<bool>, Vec<bool>) {
    let mut old = vec![false; old_len];
    let mut new = vec![false; new_len];
    for edit in edits {
        match *edit {
            Edit::Equal { .. } => {}
            Edit::Delete { old: i } => old[i] = true,
            Edit::Insert { new: j } => new[j] = true,
        }
    }
    (old, new)
}

/// The changed lines of the output of `git diff` with or without the indent heuristic.
fn git_flags(
    old: &std::path::Path,
    new: &std::path::Path,
    heuristic: bool,
    sizes: (usize, usize),
) -> (Vec<bool>, Vec<bool>) {
    let flag = if heuristic {
        "--indent-heuristic"
    } else {
        "--no-indent-heuristic"
    };
    let output = std::process::Command::new("git")
        .args(["diff", "--no-index", "-U0", "--diff-algorithm=myers", flag])
        .arg(old)
        .arg(new)
        .output()
        .expect("git is installed");
    let text = String::from_utf8(output.stdout).expect("git prints UTF-8");
    let (mut old_flags, mut new_flags) = (vec![false; sizes.0], vec![false; sizes.1]);
    // A hunk header reads `@@ -start[,count] +start[,count] @@`.
    let range = |part: &str| {
        let mut numbers = part[1..].split(',');
        let start: usize = numbers.next().unwrap().parse().unwrap();
        let count: usize = numbers.next().map_or(1, |count| count.parse().unwrap());
        (start, count)
    };
    for header in text.lines().filter(|line| line.starts_with("@@")) {
        let parts: Vec<&str> = header.split_whitespace().collect();
        let (old_start, old_count) = range(parts[1]);
        let (new_start, new_count) = range(parts[2]);
        // With a count of zero the start names the line before the gap, so nothing is filled.
        if old_count > 0 {
            old_flags[old_start - 1..old_start - 1 + old_count].fill(true);
        }
        if new_count > 0 {
            new_flags[new_start - 1..new_start - 1 + new_count].fill(true);
        }
    }
    (old_flags, new_flags)
}

/// Compares `diff_lines` with `git diff` on pseudo-random files over `alphabet`, and returns
/// the mismatches. Only the cases where git's plain diff equals ours are compared, because the
/// heuristic only moves the boundaries of a given diff.
fn compare_with_git(
    alphabet: &[&str],
    max_len: usize,
    cases: usize,
    seed: u64,
) -> Vec<(Vec<String>, Vec<String>)> {
    let mut state = seed;
    let mut next = move |bound: usize| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize % bound
    };
    let dir = std::env::temp_dir().join(format!("stemma-diff-git-{}-{seed}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (old_path, new_path) = (dir.join("old.txt"), dir.join("new.txt"));
    let mut mismatches = Vec::new();
    for _ in 0..cases {
        let (old_len, new_len) = (1 + next(max_len), 1 + next(max_len));
        let old: Vec<&str> = (0..old_len)
            .map(|_| alphabet[next(alphabet.len())])
            .collect();
        let new: Vec<&str> = (0..new_len)
            .map(|_| alphabet[next(alphabet.len())])
            .collect();
        let text = |lines: &[&str]| {
            lines
                .iter()
                .map(|line| format!("{line}\n"))
                .collect::<String>()
        };
        std::fs::write(&old_path, text(&old)).unwrap();
        std::fs::write(&new_path, text(&new)).unwrap();
        // The command compares bytes. Over ASCII lines both functions give the same script.
        let old_bytes: Vec<&[u8]> = old.iter().map(|line| line.as_bytes()).collect();
        let new_bytes: Vec<&[u8]> = new.iter().map(|line| line.as_bytes()).collect();
        assert_eq!(
            diff_byte_lines(&old_bytes, &new_bytes),
            diff_lines(&old, &new),
            "{old:?} against {new:?}"
        );
        let sizes = (old.len(), new.len());
        let plain = changed_flags(&diff(&old, &new), sizes.0, sizes.1);
        if plain != git_flags(&old_path, &new_path, false, sizes) {
            continue;
        }
        let ours = changed_flags(&diff_lines(&old, &new), sizes.0, sizes.1);
        if ours != git_flags(&old_path, &new_path, true, sizes) {
            let owned = |lines: Vec<&str>| lines.into_iter().map(String::from).collect();
            mismatches.push((owned(old), owned(new)));
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
    mismatches
}

#[test]
fn diff_lines_agrees_with_git_on_short_files() {
    let alphabet = ["x", "  x", "    x", "", "}", "  }", "{", "  {"];
    let mismatches = compare_with_git(&alphabet, 9, 1500, 12345);
    assert!(mismatches.is_empty(), "differs from git: {mismatches:?}");
}

#[test]
fn diff_lines_agrees_with_git_on_long_runs_of_equal_lines() {
    // Few distinct lines in long files give change groups that can slide over many lines.
    let alphabet = ["a", "a", "a", "  a", "", "}", "b"];
    let mismatches = compare_with_git(&alphabet, 40, 1500, 777);
    assert!(mismatches.is_empty(), "differs from git: {mismatches:?}");
}

#[test]
fn diff_lines_agrees_with_git_beyond_the_sliding_window() {
    // Mostly equal lines give groups that can slide over more than MAX_SLIDING lines.
    let mut alphabet = vec!["a"; 300];
    alphabet.extend(["  a", "", "b"]);
    let mismatches = compare_with_git(&alphabet, 400, 600, 4242);
    assert!(
        mismatches.is_empty(),
        "differs from git: {} cases",
        mismatches.len()
    );
}
