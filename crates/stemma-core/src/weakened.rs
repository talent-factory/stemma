//! Signals of weakened tests, assertions, and lint rules (ADR-004, rule 10; STEMMA-248).
//!
//! [`analyze`] is a pure function over the old and the new blobs of the files of a change. It
//! reads the syntax tree of each `.rs` file with `syn` and the `[lints]` tables of each
//! `Cargo.toml`, and returns one [`Reason`] for each signal that it finds. The policy turns a
//! non-empty list into a level (at least 2, [`crate::policy::Policy::weakening`]); the rule only
//! raises the level of a change and cannot be switched off.
//!
//! # Signals
//!
//! | Signal | Hit |
//! |---|---|
//! | [`Signal::FewerAssertions`] | A function has fewer `assert*!` calls (also `debug_assert*!`) than before. |
//! | [`Signal::AssertionBecameDebug`] | A function has fewer always-on assertions and more `debug_assert*!`. |
//! | [`Signal::TestRemoved`] | A `#[test]` function is gone, or is no test any more. |
//! | [`Signal::TestSkipped`] | `#[ignore]` is added, `#[should_panic]` is added, or its `expected` is dropped. |
//! | [`Signal::TestGated`] | `#[cfg(...)]` is added to a test. |
//! | [`Signal::LintLowered`] | A lint level is lowered in `[lints]` or by an attribute, or `#[allow]` is added. |
//! | [`Signal::Unparsable`] | A `.rs` file does not parse (the fallback). |
//!
//! # What the signals do not see
//!
//! The comparison is syntactic. It does not see a condition that is loosened (`==` becomes `>=`),
//! an assertion that a helper call replaces, a test that a changed production function makes
//! vacuous, or a weakening in a file that is not Rust. Macro bodies are opaque to `syn`: an
//! assertion inside `proptest! { ... }` is not counted, and neither is a macro that wraps
//! `assert!`. A project that has its own assertion macros names them in the policy
//! ([`crate::policy::Policy::assertion_macros`]).
//!
//! # Matching
//!
//! A function is the same function if its path is the same: the modules, the type of an `impl`,
//! and its name. A test that is gone is a rename or a move, and does not count, if the new files
//! of the change hold a function with exactly the same body (the same tokens, whatever the
//! formatting). A `#[ignore]` with a reason that is added to a platform-bound test still hits: the
//! rule cannot tell, so the reviewer justifies it.
//!
//! The size of a file is bounded ([`MAX_PARSE_BYTES`]), because the author of a change chooses
//! it. A file over the bound is as unparsable as one that is not Rust: it hits with the fallback.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use quote::ToTokens;
use syn::visit::{self, Visit};

/// The largest `.rs` file that the signals parse, in bytes. A larger file hits with
/// [`Signal::Unparsable`].
pub const MAX_PARSE_BYTES: usize = 4 * 1024 * 1024;

/// The part of a change that the signals read: one file with its old and its new content. `old` is
/// `None` for a file that is added, `new` is `None` for a file that is deleted.
#[derive(Debug, Clone, Copy)]
pub struct FileChange<'a> {
    /// The path relative to the root, written with `/`.
    pub path: &'a str,
    /// The blob before the change.
    pub old: Option<&'a [u8]>,
    /// The blob after the change.
    pub new: Option<&'a [u8]>,
}

/// The kind of a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Signal {
    /// A function has fewer assertions.
    FewerAssertions,
    /// Assertions that are always on became `debug_assert*!`.
    AssertionBecameDebug,
    /// A test is gone, or is no test any more.
    TestRemoved,
    /// A test is ignored, or its `should_panic` is weaker.
    TestSkipped,
    /// A test is compiled only under a `cfg` now.
    TestGated,
    /// A lint is allowed now or has a lower level.
    LintLowered,
    /// A `.rs` file does not parse, or is too large.
    Unparsable,
}

/// One hit of a signal, with the reason that a reviewer reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    /// Which signal.
    pub signal: Signal,
    /// The file.
    pub path: String,
    /// What was found, in one line.
    pub detail: String,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.detail)
    }
}

/// How an assertion macro counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Checked in every build.
    Always,
    /// Checked only if debug assertions are on.
    Debug,
}

/// The macros that count as assertions: the three of the standard library and their `debug_`
/// variants, with the paths `std::` and `core::`, and the macros that a policy names.
struct Macros<'a> {
    extra: &'a [String],
}

impl Macros<'_> {
    fn kind_of(&self, path: &syn::Path) -> Option<Kind> {
        let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        let name = segments.last()?.as_str();
        let plain = match segments.as_slice() {
            [_] => true,
            [root, _] => root == "std" || root == "core",
            _ => false,
        };
        if plain {
            match name {
                "assert" | "assert_eq" | "assert_ne" => return Some(Kind::Always),
                "debug_assert" | "debug_assert_eq" | "debug_assert_ne" => return Some(Kind::Debug),
                _ => {}
            }
        }
        let written = segments.join("::");
        if self.extra.contains(&written) {
            Some(if name.starts_with("debug_") {
                Kind::Debug
            } else {
                Kind::Always
            })
        } else {
            None
        }
    }
}

/// What the signals know about one function.
struct FnInfo {
    /// The path of the function, made unique in its file.
    key: String,
    is_test: bool,
    ignored: bool,
    /// `None` without `#[should_panic]`, else whether it has an `expected`.
    should_panic: Option<bool>,
    cfgs: usize,
    always: usize,
    debug: usize,
    body: [u8; 32],
}

/// The functions and the lint attributes of one file.
#[derive(Default)]
struct Inventory {
    fns: Vec<FnInfo>,
    lints: BTreeMap<String, LintCount>,
}

/// What the attributes of a file say about one lint.
#[derive(Default, Clone, Copy)]
struct LintCount {
    /// The sum of the strictness of the `warn`, `deny`, and `forbid` attributes (1, 2, 3).
    strict: u32,
    /// The number of `allow` attributes. An `expect` is not counted: it reports itself when the
    /// lint no longer fires, so only the change from `expect` to `allow` is a signal.
    allows: u32,
}

/// Why a file has no inventory.
struct Unparsable;

fn parse(bytes: &[u8], macros: &Macros<'_>) -> Result<Inventory, Unparsable> {
    if bytes.len() > MAX_PARSE_BYTES {
        return Err(Unparsable);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Unparsable)?;
    let file = syn::parse_file(text).map_err(|_| Unparsable)?;
    let mut collector = Collector {
        macros,
        inventory: Inventory::default(),
        stack: Vec::new(),
        seen: HashMap::new(),
    };
    collector.visit_file(&file);
    Ok(collector.inventory)
}

struct Collector<'a> {
    macros: &'a Macros<'a>,
    inventory: Inventory,
    /// The names of the modules, types, and functions around the node.
    stack: Vec<String>,
    /// How often a key was used, to tell apart functions that share one (`cfg` variants).
    seen: HashMap<String, usize>,
}

impl Collector<'_> {
    fn record(&mut self, name: &syn::Ident, attrs: &[syn::Attribute], block: &syn::Block) {
        let name = name.to_string();
        let path = self
            .stack
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(name.as_str()))
            .collect::<Vec<_>>()
            .join("::");
        let count = self.seen.entry(path.clone()).or_insert(0);
        *count += 1;
        let key = if *count == 1 {
            path
        } else {
            format!("{path}#{count}")
        };
        let mut counter = Counter {
            macros: self.macros,
            always: 0,
            debug: 0,
        };
        counter.visit_block(block);
        let tokens = block.to_token_stream().to_string();
        let mut info = FnInfo {
            key,
            is_test: false,
            ignored: false,
            should_panic: None,
            cfgs: 0,
            always: counter.always,
            debug: counter.debug,
            body: *blake3::hash(tokens.as_bytes()).as_bytes(),
        };
        for attr in attrs {
            let path = attr.path();
            if path.is_ident("ignore") {
                info.ignored = true;
            } else if path.is_ident("cfg") {
                info.cfgs += 1;
            } else if path.is_ident("should_panic") {
                info.should_panic = Some(has_expected(attr));
            } else if path.segments.last().is_some_and(|s| s.ident == "test") {
                info.is_test = true;
            }
        }
        self.inventory.fns.push(info);
    }

    fn nested<F: FnOnce(&mut Self)>(&mut self, name: String, body: F) {
        self.stack.push(name);
        body(self);
        self.stack.pop();
    }
}

/// Whether a `#[should_panic(...)]` has an `expected = ...`.
fn has_expected(attr: &syn::Attribute) -> bool {
    let mut found = false;
    let _ = attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("expected") {
            found = true;
            let _: syn::Expr = meta.value()?.parse()?;
        }
        Ok(())
    });
    found
}

impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        self.nested(node.ident.to_string(), |this| {
            visit::visit_item_mod(this, node)
        });
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let name = node.self_ty.to_token_stream().to_string().replace(' ', "");
        self.nested(name, |this| visit::visit_item_impl(this, node));
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        self.nested(node.ident.to_string(), |this| {
            visit::visit_item_trait(this, node)
        });
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.record(&node.sig.ident, &node.attrs, &node.block);
        self.nested(node.sig.ident.to_string(), |this| {
            visit::visit_item_fn(this, node)
        });
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.record(&node.sig.ident, &node.attrs, &node.block);
        self.nested(node.sig.ident.to_string(), |this| {
            visit::visit_impl_item_fn(this, node)
        });
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if let Some(block) = &node.default {
            self.record(&node.sig.ident, &node.attrs, block);
        }
        self.nested(node.sig.ident.to_string(), |this| {
            visit::visit_trait_item_fn(this, node)
        });
    }

    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        let path = node.path();
        let strictness = ["warn", "deny", "forbid"]
            .iter()
            .position(|name| path.is_ident(name))
            .map(|at| at as u32 + 1);
        let allows = path.is_ident("allow");
        if strictness.is_none() && !allows {
            return;
        }
        let mut lints = Vec::new();
        let _ = node.parse_nested_meta(|meta| {
            if meta.path.is_ident("reason") {
                let _: syn::Lit = meta.value()?.parse()?;
            } else {
                lints.push(path_text(&meta.path));
            }
            Ok(())
        });
        for lint in lints {
            let count = self.inventory.lints.entry(lint).or_default();
            match strictness {
                Some(strictness) => count.strict += strictness,
                None => count.allows += 1,
            }
        }
    }
}

fn path_text(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// Counts the assertions in a function body, without the functions nested in it (they are
/// functions of their own).
struct Counter<'a> {
    macros: &'a Macros<'a>,
    always: usize,
    debug: usize,
}

impl<'ast> Visit<'ast> for Counter<'_> {
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        match self.macros.kind_of(&node.path) {
            Some(Kind::Always) => self.always += 1,
            Some(Kind::Debug) => self.debug += 1,
            None => {}
        }
        visit::visit_macro(self, node);
    }

    fn visit_item(&mut self, _: &'ast syn::Item) {}
}

/// The levels of the tables `[lints]`, `[lints.rust]`, `[lints.clippy]`, and the same below
/// `[workspace.lints]`, of one `Cargo.toml`, by table and name. 0 is `allow`, 1 `warn`, 2 `deny`,
/// 3 `forbid`. Only the forms `name = "level"` and `name = { level = "level", ... }` on one line are
/// read; the rest is ignored.
fn read_lints(text: &str) -> BTreeMap<String, u8> {
    let mut levels = BTreeMap::new();
    let mut table: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix('[') {
            let header = header.trim_end_matches(']').trim();
            let header = header.strip_prefix("workspace.").unwrap_or(header);
            let is_lints = header == "lints"
                || header
                    .strip_prefix("lints.")
                    .is_some_and(|tool| !tool.contains('.'));
            table = is_lints.then(|| line.to_owned());
            continue;
        }
        let Some(table) = &table else { continue };
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim().trim_matches('"');
        let value = value.trim();
        let level = if let Some(rest) = value.strip_prefix('"') {
            rest.split('"').next()
        } else if value.starts_with('{') {
            inline_level(value)
        } else {
            None
        };
        let rank = match level {
            Some("allow") => 0,
            Some("warn") => 1,
            Some("deny") => 2,
            Some("forbid") => 3,
            _ => continue,
        };
        levels.insert(format!("{table} {name}"), rank);
    }
    levels
}

/// The `level` of an inline table `{ level = "warn", priority = -1 }`.
fn inline_level(value: &str) -> Option<&str> {
    let rest = value.split_once("level")?.1.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start().strip_prefix('"')?;
    rest.split('"').next()
}

/// Looks for the signals in the files of a change.
///
/// `extra_macros` are the assertion macros that the policy names, as written in a call
/// (`pretty_assertions::assert_eq`). The reasons come in the order of the files, and for each file
/// in the order of its functions, so the same change gives the same list.
pub fn analyze(files: &[FileChange<'_>], extra_macros: &[String]) -> Vec<Reason> {
    let macros = Macros {
        extra: extra_macros,
    };
    let mut reasons = Vec::new();
    let reason = |signal, path: &str, detail: String| Reason {
        signal,
        path: path.to_owned(),
        detail,
    };

    // The inventories of the `.rs` files first: a test that moved to another file of the change
    // is found by the bodies of all new files.
    let mut parsed: Vec<Option<(Inventory, Inventory)>> = Vec::new();
    let mut new_bodies: HashSet<[u8; 32]> = HashSet::new();
    for file in files {
        if !file.path.ends_with(".rs") {
            parsed.push(None);
            continue;
        }
        let old = file
            .old
            .map_or(Ok(Inventory::default()), |b| parse(b, &macros));
        let new = file
            .new
            .map_or(Ok(Inventory::default()), |b| parse(b, &macros));
        match (old, new) {
            (_, Err(Unparsable)) => {
                reasons.push(reason(
                    Signal::Unparsable,
                    file.path,
                    "the file does not parse as Rust, or is too large to read".to_owned(),
                ));
                parsed.push(None);
            }
            // The old file does not parse: nothing to compare with.
            (Err(Unparsable), Ok(new)) => {
                new_bodies.extend(new.fns.iter().map(|f| f.body));
                parsed.push(None);
            }
            (Ok(old), Ok(new)) => {
                new_bodies.extend(new.fns.iter().map(|f| f.body));
                parsed.push(Some((old, new)));
            }
        }
    }

    for (file, pair) in files.iter().zip(&parsed) {
        if let Some((old, new)) = pair {
            compare(file.path, old, new, &new_bodies, &mut reasons);
        } else if file.path.rsplit('/').next() == Some("Cargo.toml") {
            compare_manifest(file, &mut reasons);
        }
    }
    reasons
}

fn compare(
    path: &str,
    old: &Inventory,
    new: &Inventory,
    new_bodies: &HashSet<[u8; 32]>,
    reasons: &mut Vec<Reason>,
) {
    let mut hit = |signal, detail: String| {
        reasons.push(Reason {
            signal,
            path: path.to_owned(),
            detail,
        })
    };
    let by_key: HashMap<&str, &FnInfo> = new.fns.iter().map(|f| (f.key.as_str(), f)).collect();
    for before in &old.fns {
        let key = &before.key;
        let Some(after) = by_key.get(key.as_str()) else {
            if before.is_test && !new_bodies.contains(&before.body) {
                hit(
                    Signal::TestRemoved,
                    format!("the test `{key}` is gone, and no function has its body"),
                );
            }
            continue;
        };
        if before.is_test && !after.is_test {
            hit(
                Signal::TestRemoved,
                format!("`{key}` is no test any more: it lost its `#[test]`"),
            );
        }
        let (was, is) = (before.always + before.debug, after.always + after.debug);
        if is < was {
            hit(
                Signal::FewerAssertions,
                format!("`{key}` has fewer assertions ({was} before, {is} now)"),
            );
        }
        if after.always < before.always && after.debug > before.debug {
            hit(
                Signal::AssertionBecameDebug,
                format!(
                    "`{key}` checks fewer assertions in a release build ({} before, {} now): \
                     `debug_assert*!` replaces `assert*!`",
                    before.always, after.always
                ),
            );
        }
        if !before.ignored && after.ignored {
            hit(Signal::TestSkipped, format!("`{key}` is `#[ignore]`d now"));
        }
        match (before.should_panic, after.should_panic) {
            (None, Some(_)) => hit(
                Signal::TestSkipped,
                format!("`{key}` has `#[should_panic]` now: any panic passes"),
            ),
            (Some(true), Some(false)) => hit(
                Signal::TestSkipped,
                format!("`#[should_panic]` of `{key}` lost its `expected`"),
            ),
            _ => {}
        }
        if before.is_test && after.cfgs > before.cfgs {
            hit(
                Signal::TestGated,
                format!("`{key}` has a `#[cfg(...)]` now: it may not be compiled everywhere"),
            );
        }
    }
    for (lint, after) in &new.lints {
        let before = old.lints.get(lint).copied().unwrap_or_default();
        if after.allows > before.allows {
            hit(
                Signal::LintLowered,
                format!("`{lint}` is allowed once more"),
            );
        } else if after.strict < before.strict {
            hit(
                Signal::LintLowered,
                format!("the level of `{lint}` is lower"),
            );
        }
    }
    for (lint, before) in &old.lints {
        if !new.lints.contains_key(lint) && before.strict > 0 {
            hit(
                Signal::LintLowered,
                format!("the level of `{lint}` is gone"),
            );
        }
    }
}

fn compare_manifest(file: &FileChange<'_>, reasons: &mut Vec<Reason>) {
    let read = |bytes: Option<&[u8]>| {
        bytes
            .and_then(|b| std::str::from_utf8(b).ok())
            .map(read_lints)
            .unwrap_or_default()
    };
    let (old, new) = (read(file.old), read(file.new));
    let mut hit = |detail: String| {
        reasons.push(Reason {
            signal: Signal::LintLowered,
            path: file.path.to_owned(),
            detail,
        })
    };
    for (key, before) in &old {
        match new.get(key) {
            Some(after) if after < before => hit(format!("`{key}` has a lower level")),
            None if *before >= 2 => hit(format!("`{key}` is removed")),
            _ => {}
        }
    }
    for (key, after) in &new {
        if *after == 0 && !old.contains_key(key) {
            hit(format!("`{key}` is set to `allow`"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(path: &str, old: &str, new: &str) -> Vec<Reason> {
        analyze(
            &[FileChange {
                path,
                old: Some(old.as_bytes()),
                new: Some(new.as_bytes()),
            }],
            &[],
        )
    }

    fn signals(path: &str, old: &str, new: &str) -> Vec<Signal> {
        run(path, old, new).into_iter().map(|r| r.signal).collect()
    }

    fn rs(old: &str, new: &str) -> Vec<Signal> {
        signals("src/lib.rs", old, new)
    }

    #[test]
    fn unchanged_code_has_no_signal() {
        let code = "#[test] fn t() { assert_eq!(1, 1); }";
        assert_eq!(rs(code, code), []);
    }

    #[test]
    fn a_removed_assertion_hits() {
        let old = "#[test] fn t() { assert!(a()); assert_eq!(1, 1); }";
        let new = "#[test] fn t() { assert!(a()); }";
        let reasons = run("src/lib.rs", old, new);
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].signal, Signal::FewerAssertions);
        assert!(
            reasons[0].detail.contains("2 before, 1 now"),
            "{}",
            reasons[0]
        );
    }

    #[test]
    fn more_or_equal_assertions_do_not_hit() {
        let old = "fn f() { assert!(a); }";
        assert_eq!(rs(old, "fn f() { assert!(a); assert_ne!(1, 2); }"), []);
        // A reformatted or reworded assertion is the same count.
        assert_eq!(
            rs(
                old,
                "fn f() {\n    assert!(\n        b,\n        \"text\"\n    );\n}"
            ),
            []
        );
    }

    #[test]
    fn std_paths_count_as_assertions() {
        let old = "fn f() { std::assert!(a); core::assert_eq!(1, 1); }";
        assert_eq!(
            rs(old, "fn f() { std::assert!(a); }"),
            [Signal::FewerAssertions]
        );
    }

    #[test]
    fn an_assertion_moved_into_a_helper_hits_as_a_false_positive() {
        // Named limit: a helper call that replaces an assertion is a drop of the count.
        let old = "fn f() { assert!(a); }";
        assert_eq!(rs(old, "fn f() { check(); }"), [Signal::FewerAssertions]);
    }

    #[test]
    fn assert_becoming_debug_assert_hits_both_signals_only_if_the_total_drops() {
        let old = "fn f() { assert!(a); assert_eq!(1, 1); }";
        // The total stays, the always-on count drops.
        assert_eq!(
            rs(old, "fn f() { assert!(a); debug_assert_eq!(1, 1); }"),
            [Signal::AssertionBecameDebug]
        );
        assert_eq!(
            rs(old, "fn f() { debug_assert!(a); }"),
            [Signal::FewerAssertions, Signal::AssertionBecameDebug]
        );
    }

    #[test]
    fn debug_assert_replaced_by_assert_does_not_hit() {
        assert_eq!(
            rs("fn f() { debug_assert!(a); }", "fn f() { assert!(a); }"),
            []
        );
    }

    #[test]
    fn a_removed_test_hits() {
        let old = "#[test] fn a() { assert!(x); } #[test] fn b() { assert!(y); }";
        let new = "#[test] fn a() { assert!(x); }";
        let reasons = run("src/lib.rs", old, new);
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].signal, Signal::TestRemoved);
        assert!(reasons[0].detail.contains("`b`"));
    }

    #[test]
    fn a_renamed_test_does_not_hit() {
        let old = "#[test] fn old_name() { assert!(x); }";
        let new = "#[test] fn new_name() { assert!(x); }";
        assert_eq!(rs(old, new), []);
        // The formatting of the body does not matter.
        assert_eq!(rs(old, "#[test] fn n() {\n assert!(\n x\n );\n}"), []);
    }

    #[test]
    fn a_test_that_moves_to_another_file_does_not_hit() {
        let files = [
            FileChange {
                path: "src/a.rs",
                old: Some(b"#[test] fn t() { assert!(x); }"),
                new: Some(b""),
            },
            FileChange {
                path: "src/b.rs",
                old: None,
                new: Some(b"mod tests { #[test] fn t() { assert!(x); } }"),
            },
        ];
        assert_eq!(analyze(&files, &[]), []);
        // Without the other file, it is a deletion.
        let alone = &files[..1];
        assert_eq!(analyze(alone, &[])[0].signal, Signal::TestRemoved);
    }

    #[test]
    fn a_deleted_file_with_tests_hits() {
        let files = [FileChange {
            path: "tests/it.rs",
            old: Some(b"#[test] fn t() { assert!(x); }"),
            new: None,
        }];
        assert_eq!(analyze(&files, &[])[0].signal, Signal::TestRemoved);
    }

    #[test]
    fn a_new_file_with_tests_does_not_hit() {
        let files = [FileChange {
            path: "tests/it.rs",
            old: None,
            new: Some(b"#[test] #[ignore] fn t() { assert!(x); }"),
        }];
        assert_eq!(analyze(&files, &[]), []);
    }

    #[test]
    fn a_function_that_loses_its_test_attribute_hits() {
        assert_eq!(
            rs("#[test] fn t() { assert!(x); }", "fn t() { assert!(x); }"),
            [Signal::TestRemoved]
        );
        // A path such as `tokio::test` is a test attribute as well.
        assert_eq!(
            rs(
                "#[tokio::test] async fn t() { assert!(x); }",
                "async fn t() { assert!(x); }"
            ),
            [Signal::TestRemoved]
        );
    }

    #[test]
    fn a_removed_function_that_is_no_test_does_not_hit() {
        assert_eq!(rs("fn helper() { assert!(x); }", ""), []);
    }

    #[test]
    fn ignore_added_hits_with_and_without_a_reason() {
        let old = "#[test] fn t() { assert!(x); }";
        assert_eq!(
            rs(old, "#[test] #[ignore] fn t() { assert!(x); }"),
            [Signal::TestSkipped]
        );
        // A platform-bound test with a reason still hits: the reviewer justifies it.
        assert_eq!(
            rs(
                old,
                "#[test] #[ignore = \"needs a TTY\"] fn t() { assert!(x); }"
            ),
            [Signal::TestSkipped]
        );
        // An ignore that was there before is no change.
        let ignored = "#[test] #[ignore] fn t() { assert!(x); }";
        assert_eq!(rs(ignored, ignored), []);
        // Removing an ignore strengthens the suite.
        assert_eq!(rs(ignored, old), []);
    }

    #[test]
    fn should_panic_added_or_without_expected_hits() {
        let plain = "#[test] fn t() { f(); }";
        let any = "#[test] #[should_panic] fn t() { f(); }";
        let precise = "#[test] #[should_panic(expected = \"boom\")] fn t() { f(); }";
        assert_eq!(rs(plain, any), [Signal::TestSkipped]);
        assert_eq!(rs(plain, precise), [Signal::TestSkipped]);
        assert_eq!(rs(precise, any), [Signal::TestSkipped]);
        // A message that is reworded or kept is not a hit.
        assert_eq!(
            rs(
                precise,
                "#[test] #[should_panic(expected = \"bang\")] fn t() { f(); }"
            ),
            []
        );
        assert_eq!(rs(precise, precise), []);
        assert_eq!(rs(any, precise), []);
    }

    #[test]
    fn cfg_added_to_a_test_hits_but_not_to_other_functions() {
        let old = "#[test] fn t() { assert!(x); }";
        assert_eq!(
            rs(old, "#[test] #[cfg(unix)] fn t() { assert!(x); }"),
            [Signal::TestGated]
        );
        assert_eq!(rs("fn f() {}", "#[cfg(unix)] fn f() {}"), []);
        let gated = "#[test] #[cfg(unix)] fn t() { assert!(x); }";
        assert_eq!(rs(gated, gated), []);
    }

    #[test]
    fn allow_added_by_attribute_hits() {
        let old = "fn f() {}";
        assert_eq!(
            rs(old, "#[allow(dead_code)] fn f() {}"),
            [Signal::LintLowered]
        );
        assert_eq!(
            rs(old, "#![allow(clippy::all)]\nfn f() {}"),
            [Signal::LintLowered]
        );
        assert_eq!(
            rs(old, "#[allow(dead_code, reason = \"later\")] fn f() {}"),
            [Signal::LintLowered]
        );
    }

    #[test]
    fn expect_turned_into_allow_hits_but_not_the_other_way() {
        let expect = "#[expect(dead_code)] fn f() {}";
        let allow = "#[allow(dead_code)] fn f() {}";
        assert_eq!(rs(expect, allow), [Signal::LintLowered]);
        assert_eq!(rs(allow, expect), []);
    }

    #[test]
    fn lowering_a_lint_attribute_hits() {
        assert_eq!(
            rs("#![deny(unsafe_code)]", "#![warn(unsafe_code)]"),
            [Signal::LintLowered]
        );
        assert_eq!(
            rs("#![forbid(unsafe_code)]", "#![deny(unsafe_code)]"),
            [Signal::LintLowered]
        );
        assert_eq!(rs("#![deny(unsafe_code)]", ""), [Signal::LintLowered]);
        // Raising a level, or moving an allowance, is no hit.
        assert_eq!(rs("#![warn(unsafe_code)]", "#![deny(unsafe_code)]"), []);
        assert_eq!(
            rs(
                "#[allow(dead_code)] fn a() {}",
                "fn b() {} #[allow(dead_code)] fn c() {}"
            ),
            []
        );
    }

    #[test]
    fn a_lint_in_a_nested_item_is_seen() {
        let old = "mod m { fn f() {} }";
        assert_eq!(
            rs(
                old,
                "mod m { #[allow(unused)] fn f() { #[allow(x)] let _a = 1; } }"
            )
            .len(),
            2
        );
    }

    #[test]
    fn a_file_that_does_not_parse_falls_back() {
        let reasons = run("src/lib.rs", "fn f() {}", "fn f( {");
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].signal, Signal::Unparsable);
        // Not UTF-8, and too large, are no Rust as well.
        assert_eq!(
            analyze(
                &[FileChange {
                    path: "a.rs",
                    old: None,
                    new: Some(&[0xff, 0xfe])
                }],
                &[]
            )[0]
            .signal,
            Signal::Unparsable
        );
        let big = vec![b' '; MAX_PARSE_BYTES + 1];
        assert_eq!(
            analyze(
                &[FileChange {
                    path: "a.rs",
                    old: None,
                    new: Some(&big)
                }],
                &[]
            )[0]
            .signal,
            Signal::Unparsable
        );
        // A file that is fixed has no hit, and neither has a file that is not Rust.
        assert_eq!(rs("fn f( {", "fn f() {}"), []);
        assert_eq!(signals("README.md", "x", "fn f( {"), []);
    }

    #[test]
    fn functions_in_modules_and_impls_are_told_apart() {
        let old = "mod a { struct S; impl S { #[test] fn t() { assert!(x); assert!(y); } } }\n\
                   mod b { struct S; impl S { #[test] fn t() { assert!(x); } } }";
        let new = "mod a { struct S; impl S { #[test] fn t() { assert!(x); } } }\n\
                   mod b { struct S; impl S { #[test] fn t() { assert!(x); } } }";
        let reasons = run("src/lib.rs", old, new);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].detail.contains("`a::S::t`"), "{}", reasons[0]);
    }

    #[test]
    fn extra_macros_of_the_policy_count() {
        let old = "fn f() { pretty_assertions::assert_eq!(1, 1); }";
        let new = "fn f() {}";
        let file = FileChange {
            path: "src/lib.rs",
            old: Some(old.as_bytes()),
            new: Some(new.as_bytes()),
        };
        assert_eq!(analyze(&[file], &[]), []);
        let extra = ["pretty_assertions::assert_eq".to_owned()];
        assert_eq!(analyze(&[file], &extra)[0].signal, Signal::FewerAssertions);
        // A name with `debug_` counts as a debug assertion.
        let extra = ["my::debug_check".to_owned()];
        let old = "fn f() { my::debug_check!(a); assert!(b); }";
        let new = "fn f() { assert!(b); }";
        let file = FileChange {
            path: "src/lib.rs",
            old: Some(old.as_bytes()),
            new: Some(new.as_bytes()),
        };
        assert_eq!(analyze(&[file], &extra)[0].signal, Signal::FewerAssertions);
    }

    #[test]
    fn nested_functions_are_counted_for_themselves() {
        let old = "fn outer() { fn inner() { assert!(a); } inner(); }";
        let new = "fn outer() { fn inner() { assert!(a); } inner(); assert!(b); }";
        assert_eq!(rs(old, new), []);
    }

    fn cargo(old: &str, new: &str) -> Vec<Reason> {
        run("crates/x/Cargo.toml", old, new)
    }

    #[test]
    fn lowered_lint_level_in_cargo_toml_hits() {
        let old =
            "[lints.clippy]\nall = \"deny\"\nunwrap_used = { level = \"forbid\", priority = 1 }\n";
        assert_eq!(cargo(old, old), []);
        let reasons = cargo(
            old,
            "[lints.clippy]\nall = \"warn\"\nunwrap_used = { level = \"forbid\" }\n",
        );
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].signal, Signal::LintLowered);
        assert!(reasons[0].detail.contains("all"));
        assert_eq!(
            cargo(
                old,
                "[lints.clippy]\nall = \"deny\"\nunwrap_used = { level = \"deny\" }\n"
            )
            .len(),
            1
        );
        // Removed, and set to allow.
        assert_eq!(
            cargo(
                old,
                "[lints.clippy]\nunwrap_used = { level = \"forbid\" }\n"
            )
            .len(),
            1
        );
        assert_eq!(cargo(old, &format!("{old}pedantic = \"allow\"\n")).len(), 1);
    }

    #[test]
    fn raised_or_foreign_levels_in_cargo_toml_do_not_hit() {
        let old = "[lints.rust]\nunused = \"warn\"\n[package]\nname = \"x\"\n";
        assert_eq!(
            cargo(
                old,
                "[lints.rust]\nunused = \"deny\"\n[package]\nname = \"x\"\n"
            ),
            []
        );
        assert_eq!(
            cargo(
                old,
                "[lints.rust]\nunused = \"warn\"\nnew_lint = \"warn\"\n[package]\nname = \"y\"\n"
            ),
            []
        );
        // A level in another table is no lint level.
        assert_eq!(
            cargo(
                "[profile.dev]\nlevel = \"deny\"\n",
                "[profile.dev]\nlevel = \"allow\"\n"
            ),
            []
        );
        assert_eq!(
            cargo("[lints]\nworkspace = true\n", "[lints]\nworkspace = true\n"),
            []
        );
    }

    #[test]
    fn workspace_lints_are_read() {
        let old = "[workspace.lints.rust]\nunsafe_code = \"forbid\"\n";
        assert_eq!(
            cargo(old, "[workspace.lints.rust]\nunsafe_code = \"warn\"\n").len(),
            1
        );
        assert_eq!(cargo(old, "").len(), 1);
    }

    #[test]
    fn a_manifest_that_is_not_text_is_read_as_empty() {
        let file = FileChange {
            path: "Cargo.toml",
            old: Some(&[0xff]),
            new: Some(b"[lints.rust]\nx = \"allow\"\n"),
        };
        assert_eq!(analyze(&[file], &[]).len(), 1);
    }

    #[test]
    fn reasons_have_a_stable_order_and_text() {
        let old = "#[test] fn a() { assert!(x); }\n#[test] fn b() { assert!(y); }";
        let new = "#[test] #[ignore] fn a() { }";
        let reasons: Vec<String> = run("src/t.rs", old, new)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            reasons,
            [
                "src/t.rs: `a` has fewer assertions (1 before, 0 now)",
                "src/t.rs: `a` is `#[ignore]`d now",
                "src/t.rs: the test `b` is gone, and no function has its body",
            ]
        );
    }
}
