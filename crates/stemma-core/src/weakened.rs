//! Signals of weakened tests, assertions, and lint rules (ADR-004, rule 10; STEMMA-248).
//!
//! [`analyze`] is a pure function over the old and the new blobs of the files of a change. It
//! reads the syntax tree of each `.rs` file with `syn` and the `[lints]` tables of each
//! `Cargo.toml`, and returns one [`Reason`] for each hit. The policy turns a non-empty list into a
//! level (at least 2, [`crate::policy::Policy::weakening`]); the caller takes the maximum of that
//! level and the level of the paths. The rule only raises the level of a change and cannot be
//! switched off.
//!
//! # Signals
//!
//! | Signal | Hit |
//! |---|---|
//! | [`Signal::FewerAssertions`] | A function has fewer `assert*!` calls (also `debug_assert*!`) than before. |
//! | [`Signal::AssertionBecameDebug`] | A function has fewer always-on assertions and more `debug_assert*!`. |
//! | [`Signal::TestRemoved`] | A test (`#[test]`, `#[x::test]`) is gone, or is no test any more. |
//! | [`Signal::TestSkipped`] | `#[ignore]` is added, `#[should_panic]` is added, or its `expected` is dropped. |
//! | [`Signal::TestGated`] | `#[cfg(...)]` or `#[cfg_attr(...)]` is added to a test, or to a module, an `impl`, or the file around it. |
//! | [`Signal::LintLowered`] | A lint level is lowered in `[lints]` or by an attribute, `#[allow]` is added, or `lints.workspace = true` is gone. |
//! | [`Signal::Unparsable`] | A `.rs` file does not parse, in its new or in its old version, or a manifest is not text (the fallback). |
//!
//! # What the signals do not see
//!
//! The comparison is syntactic. It does not see a condition that is loosened (`==` becomes `>=`,
//! `expected = "boom"` becomes `expected = ""`), a test that a changed production function makes
//! vacuous, or a weakening in a file that is not Rust or not a `Cargo.toml` (`.cargo/config.toml`,
//! `clippy.toml`, the flags of a recipe). Macro bodies are opaque to `syn`: an assertion inside
//! `proptest! { ... }` is not counted, and neither is a macro that wraps `assert!`. A project that
//! has its own assertion macros names them in the policy
//! ([`crate::policy::Policy::assertion_macros`]).
//!
//! Known false positives, which a reviewer judges: an assertion that moves into a helper call is a
//! lost assertion, and a `#[cfg]` that a platform-bound test needs is a gate. A `#[ignore]` with a
//! reason that is added to a platform-bound test still hits: the rule cannot tell, so the reviewer
//! justifies it.
//!
//! Known limits of the lint signal: attribute counts are summed per file, so an `#[allow]` that
//! moves from one item to another is no hit, and neither is `#[forbid]` that becomes `#[deny]` and
//! `#[warn]` of the same total. Of a `Cargo.toml`, only the tables `[lints]`, `[lints.TOOL]`,
//! `[lints.TOOL.NAME]`, and the same below `[workspace]` are read, and a value that spreads over
//! several lines counts as `allow`, so the signal errs towards a hit.
//!
//! # Matching
//!
//! A function is the same function if its path is the same: the modules, the traits and types of
//! the `impl`s and traits, the functions around it, and its name. Functions that share a path (the
//! variants of a `cfg`) get a number in the order of their appearance. A test that is gone is a
//! rename or a move, and does not count, if another test of the change that is not matched by its
//! path has exactly the same body (the same tokens, whatever the formatting) and is not weaker:
//! not ignored, not gated by more `cfg`s, and with no new `#[should_panic]`. Each such test excuses
//! one gone test, so deleting one of two tests with the same body hits.
//!
//! The size of a file is bounded ([`MAX_PARSE_BYTES`]), because the author of a change chooses
//! it. A file over the bound is as unparsable as one that is not Rust: it hits with the fallback,
//! whether the old or the new version is the large one. Hits come first for the files that are not
//! readable, then, for each file in order, for its functions and then for its lints.

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
    /// A file does not parse, is too large, or is not text.
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
/// variants, with the paths `std::` and `core::`, and the macros that a policy names. A name of the
/// policy whose last part starts with `debug_` counts as a debug assertion.
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
    /// The `cfg` and `cfg_attr` attributes of the function and of everything around it.
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

/// The name under which an attribute that cannot be read is counted as a lint, so that a new one
/// is a hit.
const UNREADABLE_LINT: &str = "<unreadable lint attribute>";

/// Why a file has no inventory.
enum Unparsable {
    TooLarge,
    NotUtf8,
    Syntax(String),
}

impl fmt::Display for Unparsable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "is larger than {MAX_PARSE_BYTES} bytes"),
            Self::NotUtf8 => f.write_str("is not UTF-8 text"),
            Self::Syntax(message) => write!(f, "does not parse as Rust ({message})"),
        }
    }
}

fn parse(bytes: &[u8], macros: &Macros<'_>) -> Result<Inventory, Unparsable> {
    if bytes.len() > MAX_PARSE_BYTES {
        return Err(Unparsable::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Unparsable::NotUtf8)?;
    let file = syn::parse_file(text).map_err(|e| Unparsable::Syntax(e.to_string()))?;
    let mut collector = Collector {
        macros,
        inventory: Inventory::default(),
        stack: Vec::new(),
        seen: HashMap::new(),
        cfg_context: 0,
    };
    collector.visit_file(&file);
    Ok(collector.inventory)
}

struct Collector<'a> {
    macros: &'a Macros<'a>,
    inventory: Inventory,
    /// The names of the modules, traits, types, and functions around the node.
    stack: Vec<String>,
    /// How often a key was used, to tell apart functions that share one (`cfg` variants).
    seen: HashMap<String, usize>,
    /// The `cfg` and `cfg_attr` attributes of the items around the node and of the file.
    cfg_context: usize,
}

/// The number of `cfg` and `cfg_attr` attributes.
fn count_cfgs(attrs: &[syn::Attribute]) -> usize {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("cfg") || a.path().is_ident("cfg_attr"))
        .count()
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
            cfgs: self.cfg_context + count_cfgs(attrs),
            always: counter.always,
            debug: counter.debug,
            body: *blake3::hash(tokens.as_bytes()).as_bytes(),
        };
        for attr in attrs {
            let path = attr.path();
            if path.is_ident("ignore") {
                info.ignored = true;
            } else if path.is_ident("should_panic") {
                info.should_panic = Some(has_expected(attr));
            } else if path.segments.last().is_some_and(|s| s.ident == "test") {
                info.is_test = true;
            }
        }
        self.inventory.fns.push(info);
    }

    /// Runs `body` with the name and the `cfg`s of an item on the stack.
    fn nested<F: FnOnce(&mut Self)>(&mut self, name: String, attrs: &[syn::Attribute], body: F) {
        let before = self.cfg_context;
        self.stack.push(name);
        self.cfg_context += count_cfgs(attrs);
        body(self);
        self.cfg_context = before;
        self.stack.pop();
    }
}

/// Whether a `#[should_panic(...)]` has an `expected = ...`. An attribute that does not read as a
/// list of `name = value` counts as having none.
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
    fn visit_file(&mut self, node: &'ast syn::File) {
        self.cfg_context += count_cfgs(&node.attrs);
        visit::visit_file(self, node);
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        self.nested(node.ident.to_string(), &node.attrs, |this| {
            visit::visit_item_mod(this, node)
        });
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let ty = node.self_ty.to_token_stream().to_string().replace(' ', "");
        let name = match &node.trait_ {
            Some((_, path, _)) => {
                let path = path.to_token_stream().to_string().replace(' ', "");
                format!("<{ty} as {path}>")
            }
            None => ty,
        };
        self.nested(name, &node.attrs, |this| visit::visit_item_impl(this, node));
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        self.nested(node.ident.to_string(), &node.attrs, |this| {
            visit::visit_item_trait(this, node)
        });
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.record(&node.sig.ident, &node.attrs, &node.block);
        self.nested(node.sig.ident.to_string(), &node.attrs, |this| {
            visit::visit_item_fn(this, node)
        });
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.record(&node.sig.ident, &node.attrs, &node.block);
        self.nested(node.sig.ident.to_string(), &node.attrs, |this| {
            visit::visit_impl_item_fn(this, node)
        });
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if let Some(block) = &node.default {
            self.record(&node.sig.ident, &node.attrs, block);
        }
        self.nested(node.sig.ident.to_string(), &node.attrs, |this| {
            visit::visit_trait_item_fn(this, node)
        });
    }

    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        let path = node.path();
        if path.is_ident("cfg_attr") {
            // `cfg_attr(PREDICATE, attribute, ...)`: each attribute after the predicate counts as
            // if it stood alone.
            let Ok(metas) = node.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            ) else {
                self.count_lint("allow", UNREADABLE_LINT.to_owned());
                return;
            };
            for meta in metas.iter().skip(1) {
                if let syn::Meta::List(list) = meta {
                    if let Some(level) = list.path.get_ident() {
                        self.read_lints(&level.to_string(), list);
                    }
                }
            }
            return;
        }
        let Some(level) = path.get_ident().map(ToString::to_string) else {
            return;
        };
        if let syn::Meta::List(list) = &node.meta {
            self.read_lints(&level, list);
        }
    }
}

impl Collector<'_> {
    /// Counts the lints of one `allow(...)`, `warn(...)`, `deny(...)`, or `forbid(...)`. A list that
    /// cannot be read counts as one `allow` of [`UNREADABLE_LINT`], so that a new one is a hit.
    fn read_lints(&mut self, level: &str, list: &syn::MetaList) {
        let mut lints = Vec::new();
        let read = list.parse_nested_meta(|meta| {
            if meta.path.is_ident("reason") {
                let _: syn::Expr = meta.value()?.parse()?;
            } else {
                lints.push(path_text(&meta.path));
            }
            Ok(())
        });
        if read.is_err() {
            lints.push(UNREADABLE_LINT.to_owned());
        }
        for lint in lints {
            self.count_lint(level, lint);
        }
    }

    fn count_lint(&mut self, level: &str, lint: String) {
        let count = self.inventory.lints.entry(lint).or_default();
        match level {
            "warn" => count.strict += 1,
            "deny" => count.strict += 2,
            "forbid" => count.strict += 3,
            "allow" => count.allows += 1,
            _ => {}
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

/// The key of the entry `lints.workspace = true`, which a crate uses to inherit the lints of its
/// workspace. It counts as the level `forbid`, so that removing it is a hit.
const WORKSPACE_KEY: &str = "workspace";

/// The levels of the lint tables of one `Cargo.toml`, by table and name: `[lints]`,
/// `[lints.TOOL]`, `[lints.TOOL.NAME]` (with a `level` line), and the same below `[workspace]`.
/// 0 is `allow`, 1 `warn`, 2 `deny`, 3 `forbid`. The forms `name = "level"`, `name = 'level'`, and
/// `name = { level = "level", ... }` on one line are read. Any other value of an entry counts as
/// `allow`, so that it never hides a lowered level.
fn read_lints(text: &str) -> BTreeMap<String, u8> {
    let mut levels = BTreeMap::new();
    // The prefix of the key, and for a table `[lints.TOOL.NAME]` the name of the lint.
    let mut table: Option<(String, Option<String>)> = None;
    for line in text.lines() {
        let line = strip_comment(line).trim();
        if let Some(header) = line.strip_prefix('[') {
            let header: String = header
                .split(']')
                .next()
                .unwrap_or("")
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let workspace = header.starts_with("workspace.");
            let bare = header.strip_prefix("workspace.").unwrap_or(&header);
            let parts: Vec<&str> = bare.split('.').collect();
            table = match parts.as_slice() {
                ["lints"] | ["lints", _] => Some((header.clone(), None)),
                ["lints", tool, name] => {
                    let prefix = if workspace { "workspace." } else { "" };
                    Some((format!("{prefix}lints.{tool}"), Some((*name).to_owned())))
                }
                _ => None,
            };
            continue;
        }
        let Some((prefix, lint)) = &table else {
            continue;
        };
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim().trim_matches(['"', '\'']);
        let value = value.trim();
        match lint {
            // `[lints.TOOL.NAME]`: only the `level` line counts.
            Some(lint) if name == "level" => {
                levels.insert(format!("{prefix} {lint}"), rank(quoted(value)));
            }
            Some(_) => {}
            None if name == WORKSPACE_KEY && value == "true" => {
                levels.insert(format!("{prefix} {WORKSPACE_KEY}"), 3);
            }
            None => {
                let level = if value.starts_with('{') {
                    inline_level(value)
                } else {
                    quoted(value)
                };
                levels.insert(format!("{prefix} {name}"), rank(level));
            }
        }
    }
    levels
}

/// The line without a comment: everything from the first `#` outside of quotes.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (at, c) in line.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if q == c => quote = None,
            (None, '#') => return &line[..at],
            _ => {}
        }
    }
    line
}

/// The text of a string in `"` or `'`.
fn quoted(value: &str) -> Option<&str> {
    let quote = value.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    value[1..].split(quote).next()
}

/// The number of a level. A value that is no level counts as `allow`.
fn rank(level: Option<&str>) -> u8 {
    match level {
        Some("warn") => 1,
        Some("deny") => 2,
        Some("forbid") => 3,
        _ => 0,
    }
}

/// The `level` of an inline table `{ level = "warn", priority = -1 }`.
fn inline_level(value: &str) -> Option<&str> {
    let rest = value.split_once("level")?.1.trim_start();
    quoted(rest.strip_prefix('=')?.trim_start())
}

/// A test that no other test of the same file matches by its path, as a candidate for the other
/// side of a rename or a move.
struct Candidate {
    is_test: bool,
    ignored: bool,
    should_panic: Option<bool>,
    cfgs: usize,
}

type Pool = HashMap<[u8; 32], Vec<Candidate>>;

/// Takes the test that excuses `gone`, if there is one: a test with the same body that is no weaker.
fn take_moved(pool: &mut Pool, gone: &FnInfo) -> bool {
    let Some(candidates) = pool.get_mut(&gone.body) else {
        return false;
    };
    let weaker_panic = |now: Option<bool>| {
        matches!(
            (gone.should_panic, now),
            (None, Some(_)) | (Some(true), Some(false))
        )
    };
    let at = candidates.iter().position(|c| {
        c.is_test
            && (!c.ignored || gone.ignored)
            && c.cfgs <= gone.cfgs
            && !weaker_panic(c.should_panic)
    });
    match at {
        Some(at) => {
            candidates.remove(at);
            true
        }
        None => false,
    }
}

/// Looks for the signals in the files of a change.
///
/// `extra_macros` are the assertion macros that the policy names, as written in a call
/// (`pretty_assertions::assert_eq`). The reasons come in a fixed order (see the module), so the
/// same change gives the same list.
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
    // is found among the tests of all new files that no path matches.
    let mut parsed: Vec<Option<(Inventory, Inventory)>> = Vec::new();
    let mut pool: Pool = HashMap::new();
    let mut add_to_pool = |old_keys: &HashSet<&str>, new: &Inventory| {
        for f in new
            .fns
            .iter()
            .filter(|f| !old_keys.contains(f.key.as_str()))
        {
            pool.entry(f.body).or_default().push(Candidate {
                is_test: f.is_test,
                ignored: f.ignored,
                should_panic: f.should_panic,
                cfgs: f.cfgs,
            });
        }
    };
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
            (_, Err(why)) => {
                reasons.push(reason(
                    Signal::Unparsable,
                    file.path,
                    format!("the new version {why}"),
                ));
                parsed.push(None);
            }
            // Nothing to compare with: report it, so that a repair cannot hide a weakening.
            (Err(why), Ok(new)) => {
                reasons.push(reason(
                    Signal::Unparsable,
                    file.path,
                    format!("the old version {why}, so the change cannot be compared"),
                ));
                add_to_pool(&HashSet::new(), &new);
                parsed.push(None);
            }
            (Ok(old), Ok(new)) => {
                let keys: HashSet<&str> = old.fns.iter().map(|f| f.key.as_str()).collect();
                add_to_pool(&keys, &new);
                parsed.push(Some((old, new)));
            }
        }
    }

    for (file, pair) in files.iter().zip(&parsed) {
        if let Some((old, new)) = pair {
            compare(file.path, old, new, &mut pool, &mut reasons);
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
    pool: &mut Pool,
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
            if before.is_test && !take_moved(pool, before) {
                hit(
                    Signal::TestRemoved,
                    format!(
                        "the test `{key}` is gone, and no other test of the change has its body \
                         without being weaker"
                    ),
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
            (None, Some(expected)) => hit(
                Signal::TestSkipped,
                if expected {
                    format!("`{key}` has `#[should_panic]` now")
                } else {
                    format!("`{key}` has `#[should_panic]` now: any panic passes")
                },
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
                format!(
                    "`{key}` has a `#[cfg(...)]` or `#[cfg_attr(...)]` more now, on it or around \
                     it: it may not be compiled everywhere"
                ),
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
    let mut hit = |signal, detail: String| {
        reasons.push(Reason {
            signal,
            path: file.path.to_owned(),
            detail,
        })
    };
    let mut read = |bytes: Option<&[u8]>, side: &str| match bytes {
        None => Some(BTreeMap::new()),
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => Some(read_lints(text)),
            Err(_) => {
                hit(
                    Signal::Unparsable,
                    format!("the {side} version is not UTF-8 text"),
                );
                None
            }
        },
    };
    let (Some(old), Some(new)) = (read(file.old, "old"), read(file.new, "new")) else {
        return;
    };
    for (key, before) in &old {
        match new.get(key) {
            Some(after) if after < before => {
                hit(Signal::LintLowered, format!("`{key}` has a lower level"))
            }
            None if *before >= 1 => hit(Signal::LintLowered, format!("`{key}` is removed")),
            _ => {}
        }
    }
    for (key, after) in &new {
        if *after == 0 && !old.contains_key(key) {
            hit(Signal::LintLowered, format!("`{key}` is set to `allow`"));
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
        // The old version that does not parse is no way to hide a weakening: it hits as well.
        let reasons = run("src/lib.rs", "fn f( {", "fn f() {}");
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].signal, Signal::Unparsable);
        assert!(
            reasons[0].detail.starts_with("the old version"),
            "{}",
            reasons[0]
        );
        assert_eq!(signals("src/lib.rs", "fn f( {", ""), [Signal::Unparsable]);
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
    fn a_manifest_that_is_not_text_hits() {
        let file = FileChange {
            path: "Cargo.toml",
            old: Some(&[0xff]),
            new: Some(b"[lints.rust]\nx = \"warn\"\n"),
        };
        let reasons = analyze(&[file], &[]);
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].signal, Signal::Unparsable);
        assert!(reasons[0].detail.contains("old version"));
        let file = FileChange {
            old: Some(b"[lints.rust]\nx = \"warn\"\n"),
            new: Some(&[0xff]),
            ..file
        };
        assert_eq!(analyze(&[file], &[])[0].signal, Signal::Unparsable);
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
                "src/t.rs: the test `b` is gone, and no other test of the change has its body \
                 without being weaker",
            ]
        );
    }

    #[test]
    fn a_test_renamed_into_a_function_that_is_no_test_hits() {
        assert_eq!(
            rs(
                "#[test] fn t() { assert!(x); }",
                "fn helper() { assert!(x); }"
            ),
            [Signal::TestRemoved]
        );
        // An empty test is not excused by any empty function.
        assert_eq!(
            rs("#[test] fn t() {}", "fn helper() {}"),
            [Signal::TestRemoved]
        );
    }

    #[test]
    fn a_rename_that_also_weakens_the_test_hits() {
        let old = "#[test] fn a() { assert!(x); }";
        for new in [
            "#[test] #[ignore] fn b() { assert!(x); }",
            "#[test] #[should_panic] fn b() { assert!(x); }",
            "#[test] #[cfg(unix)] fn b() { assert!(x); }",
        ] {
            assert_eq!(rs(old, new), [Signal::TestRemoved], "{new}");
        }
        // A test that was ignored before may be renamed with its ignore.
        let ignored = "#[test] #[ignore] fn a() { assert!(x); }";
        assert_eq!(rs(ignored, "#[test] #[ignore] fn b() { assert!(x); }"), []);
    }

    #[test]
    fn deleting_one_of_two_tests_with_the_same_body_hits() {
        let old = "#[test] fn a() { assert!(x); } #[test] fn b() { assert!(x); }";
        assert_eq!(
            rs(old, "#[test] fn a() { assert!(x); }"),
            [Signal::TestRemoved]
        );
        // Both renamed: both are excused.
        assert_eq!(
            rs(
                old,
                "#[test] fn c() { assert!(x); } #[test] fn d() { assert!(x); }"
            ),
            []
        );
    }

    #[test]
    fn a_test_moved_into_a_cfg_test_module_does_not_hit() {
        let old = "#[cfg(test)] mod old_tests { #[test] fn t() { assert!(x); } }";
        let new = "#[cfg(test)] mod tests { #[test] fn t() { assert!(x); } }";
        assert_eq!(rs(old, new), []);
    }

    #[test]
    fn a_cfg_around_a_test_hits() {
        let old = "mod tests { #[test] fn t() { assert!(x); } }";
        assert_eq!(
            rs(
                old,
                "#[cfg(any())] mod tests { #[test] fn t() { assert!(x); } }"
            ),
            [Signal::TestGated]
        );
        assert_eq!(
            rs(
                old,
                "mod tests { #![cfg(any())] #[test] fn t() { assert!(x); } }"
            ),
            [Signal::TestGated]
        );
        assert_eq!(
            rs(
                "#[test] fn t() { assert!(x); }",
                "#![cfg(any())]\n#[test] fn t() { assert!(x); }"
            ),
            [Signal::TestGated]
        );
        assert_eq!(
            rs(
                "impl S { #[test] fn t() { assert!(x); } }",
                "#[cfg(any())] impl S { #[test] fn t() { assert!(x); } }"
            ),
            [Signal::TestGated]
        );
        // An existing gate around the test is no change.
        let gated = "#[cfg(unix)] mod tests { #[test] fn t() { assert!(x); } }";
        assert_eq!(rs(gated, gated), []);
    }

    #[test]
    fn cfg_attr_on_a_test_hits() {
        assert_eq!(
            rs(
                "#[test] fn t() { assert!(x); }",
                "#[test] #[cfg_attr(all(), ignore)] fn t() { assert!(x); }"
            ),
            [Signal::TestGated]
        );
    }

    #[test]
    fn cfg_attr_with_a_lint_level_is_read() {
        assert_eq!(
            rs(
                "fn f() {}",
                "#![cfg_attr(all(), allow(clippy::all))]\nfn f() {}"
            ),
            [Signal::LintLowered]
        );
        assert_eq!(
            rs(
                "#![cfg_attr(all(), deny(unsafe_code))]",
                "#![cfg_attr(all(), warn(unsafe_code))]"
            ),
            [Signal::LintLowered]
        );
        // A `cfg_attr` that is not a list is unreadable and counts as an allowance.
        assert_eq!(rs("fn f() {}", "#![cfg_attr]\nfn f() {}").len(), 1);
    }

    #[test]
    fn a_reason_that_is_a_macro_does_not_hide_the_lint() {
        assert_eq!(
            rs(
                "fn f() {}",
                "#[allow(reason = concat!(\"a\", \"b\"), dead_code)] fn f() {}"
            ),
            [Signal::LintLowered]
        );
        // An attribute that cannot be read counts as an allowance as well.
        assert!(!rs("fn f() {}", "#[allow(dead_code = )] fn f() {}").is_empty());
    }

    #[test]
    fn impls_of_different_traits_are_told_apart() {
        let old = "impl Debug for S { fn fmt(&self) { a(); } }\n\
                   impl Display for S { fn fmt(&self) { assert!(ok); } }";
        let new = "impl Binary for S { fn fmt(&self) { b(); } }\n\
                   impl Debug for S { fn fmt(&self) { a(); } }\n\
                   impl Display for S { fn fmt(&self) { assert!(ok); } }";
        assert_eq!(rs(old, new), []);
        let weaker = "impl Debug for S { fn fmt(&self) { a(); } }\n\
                      impl Display for S { fn fmt(&self) {} }";
        let reasons = run("src/lib.rs", old, weaker);
        assert_eq!(reasons.len(), 1);
        assert!(
            reasons[0].detail.contains("<S as Display>::fmt"),
            "{}",
            reasons[0]
        );
    }

    #[test]
    fn should_panic_with_expected_says_so() {
        let reasons = run(
            "src/lib.rs",
            "#[test] fn t() { f(); }",
            "#[test] #[should_panic(expected = \"boom\")] fn t() { f(); }",
        );
        assert!(!reasons[0].detail.contains("any panic"), "{}", reasons[0]);
        let reasons = run(
            "src/lib.rs",
            "#[test] fn t() { f(); }",
            "#[test] #[should_panic] fn t() { f(); }",
        );
        assert!(reasons[0].detail.contains("any panic"), "{}", reasons[0]);
    }

    #[test]
    fn a_large_old_file_does_not_hide_a_weakening() {
        let big = format!(
            "{}#[test] fn t() {{ assert!(x); }}",
            " ".repeat(MAX_PARSE_BYTES)
        );
        let files = [FileChange {
            path: "a.rs",
            old: Some(big.as_bytes()),
            new: Some(b"fn t() {}"),
        }];
        let reasons = analyze(&files, &[]);
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].signal, Signal::Unparsable);
        assert!(reasons[0].detail.contains("larger than"), "{}", reasons[0]);
    }

    #[test]
    fn removing_the_workspace_lints_of_a_crate_hits() {
        let old = "[lints]\nworkspace = true\n";
        assert_eq!(cargo(old, "[package]\nname = \"x\"\n").len(), 1);
        assert_eq!(cargo(old, "[lints]\nworkspace = false\n").len(), 1);
        assert_eq!(cargo(old, old), []);
        assert_eq!(cargo("[lints.rust]\nx = \"warn\"\n", "").len(), 1);
    }

    #[test]
    fn other_spellings_of_a_lowered_lint_hit() {
        let old = "[lints.clippy]\npedantic = \"warn\"\n";
        for new in [
            "[lints.clippy] # relaxed\npedantic = \"allow\"\n",
            "[lints.clippy]\npedantic = 'allow'\n",
            "[lints.clippy]\n\"pedantic\" = \"allow\"\n",
            "[lints.clippy.pedantic]\nlevel = \"allow\"\npriority = -1\n",
            "[ lints.clippy ]\npedantic = { level = 'allow', priority = -1 }\n",
            "[lints.clippy]\npedantic = {\n    level = \"allow\",\n}\n",
        ] {
            assert!(!cargo(old, new).is_empty(), "{new}");
        }
        // The same level in another spelling is no change.
        for new in [
            "[lints.clippy] # same\npedantic = 'warn'\n",
            "[lints.clippy.pedantic]\nlevel = \"warn\"\n",
            "[lints.clippy]\npedantic = { level = \"warn\", priority = -1 } # comment\n",
        ] {
            assert_eq!(cargo(old, new), [], "{new}");
        }
        // A hash inside a string is no comment.
        assert_eq!(
            cargo(
                "[lints.rust]\nx = \"deny\"\n",
                "[lints.rust]\nx = \"deny\" # ok\n"
            ),
            []
        );
    }

    #[test]
    fn removing_a_warn_level_hits() {
        assert_eq!(
            cargo("[lints.rust]\nunused = \"warn\"\n", "[lints.rust]\n").len(),
            1
        );
        // Removing an entry that was `allow` is no weakening.
        assert_eq!(
            cargo("[lints.rust]\nunused = \"allow\"\n", "[lints.rust]\n"),
            []
        );
    }
}
