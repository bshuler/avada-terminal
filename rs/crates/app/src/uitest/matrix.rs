//! Track V5: docs/feature-test-matrix.md lists every Command and callback.
//!
//! The matrix is generated, and a generated file that nothing checks is a file that is
//! wrong within a fortnight. This module recomputes it from the sources and fails if the
//! two disagree, so adding a callback, a `Command` or a keybinding without regenerating
//! the doc breaks the build rather than quietly making the doc a lie.
//!
//! Three parser traps this file has already paid for — the doc's generator carries the
//! same notes, and they are the reason this is hand-rolled rather than a grep:
//!
//! 1. `command.rs` is prose-heavy and its `///` lines contain stray `(` and `{`, so brace
//!    depth must be counted *after* stripping comments.
//! 2. `command.rs` holds other enums whose variants are indistinguishable from `Command`'s
//!    line by line; only a depth-tracked slice of `pub enum Command` can be trusted.
//! 3. Keybinding ids are camelCase after the dot (`pane.focusLeft`). Nothing here parses
//!    them: [`default_bindings`] returns real data, and parsing what you can execute is a
//!    bug waiting to happen.
//!
//! A uitest file may also declare, in a `//! e2e: name, name` header, callbacks its
//! tests genuinely drive through a production wiring helper without ever writing the
//! callback's name. See the check below for what that claim has to survive.
//!
//! Deliberately, this file names no callback and no `Command`. The "proven" column means
//! "some test under `src/uitest/` mentions it", and a guard that listed them all would
//! make every one of them look proven by its own existence.
#![allow(unused_imports)]

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The crate root, found through cargo rather than the working directory: `cargo test`
/// does not promise where it runs a test binary from, but it does promise this.
fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every file under `dir` with the given extension, concatenated, sorted by path so the
/// result does not depend on the order the filesystem hands them back.
fn concat(dir: &Path, ext: &str) -> String {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap_or_else(|e| panic!("read {}: {e}", d.display())) {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == ext) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
        .iter()
        .map(|p| std::fs::read_to_string(p).expect("source file is readable"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `foo-bar` is `foo_bar` on the Rust side; Slint allows both spellings.
fn snake(name: &str) -> String {
    name.replace('-', "_")
}

/// Every `callback` declared in `ui/*.slint`, mapped to the file that declares it.
///
/// A declaration carrying `<=>` is an alias — a child's callback re-exported by its
/// parent — not a second place anything can be handled, so it is skipped.
fn callbacks() -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    let dir = crate_dir().join("ui");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("ui/ exists")
        .filter_map(|e| {
            let p = e.expect("dir entry").path();
            p.extension().is_some_and(|x| x == "slint").then_some(p)
        })
        .collect();
    files.sort();
    for path in files {
        let name = path
            .file_name()
            .expect("a file has a name")
            .to_string_lossy()
            .into_owned();
        for line in std::fs::read_to_string(&path)
            .expect("slint is readable")
            .lines()
        {
            let rest = line.trim_start();
            let rest = rest.strip_prefix("pure ").unwrap_or(rest);
            let Some(rest) = rest.strip_prefix("callback ") else {
                continue;
            };
            if line.contains("<=>") {
                continue;
            }
            let id: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if !id.is_empty() {
                found.entry(snake(&id)).or_insert_with(|| name.clone());
            }
        }
    }
    found
}

/// Whether Rust anywhere installs a handler for `callback`, i.e. `.on_<name>(`.
fn is_bound(rust: &str, callback: &str) -> bool {
    let needle = format!(".on_{callback}");
    rust.match_indices(&needle)
        .any(|(i, _)| rust[i + needle.len()..].trim_start().starts_with('('))
}

/// The variants of `pub enum Command`, and nothing else in `command.rs`.
fn command_variants() -> Vec<String> {
    let src = std::fs::read_to_string(crate_dir().join("src/command.rs")).expect("command.rs");
    let head = src
        .find("pub enum Command")
        .expect("the enum is still called that");
    let open = src[head..].find('{').expect("an enum has a body") + head;
    let bytes: Vec<char> = src.chars().collect();
    // Byte and char indices coincide only for ASCII, and this file is not ASCII.
    let open = src[..open].chars().count();
    let mut depth = 0i32;
    let mut end = open;
    for (i, c) in bytes.iter().enumerate().skip(open) {
        depth += i32::from(*c == '{') - i32::from(*c == '}');
        if depth == 0 && *c == '}' {
            end = i;
            break;
        }
    }
    let body: String = bytes[open + 1..end].iter().collect();
    let mut variants = Vec::new();
    let mut depth = 0i32;
    for line in body.lines() {
        let code = line.split("//").next().unwrap_or("");
        if depth == 0 {
            let s = code.trim_start();
            if s.starts_with(|c: char| c.is_ascii_uppercase()) {
                let id: String = s
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                let after = s[id.len()..].trim_start();
                if after.is_empty() || after.starts_with(['(', '{', ',']) {
                    variants.push(id);
                }
            }
        }
        for c in code.chars() {
            depth += i32::from(c == '{' || c == '(') - i32::from(c == '}' || c == ')');
        }
    }
    variants.sort();
    variants
}

/// One row of a markdown table, split on `|` with the empty edges dropped and the
/// backticks the doc renders names in removed.
fn cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim().trim_matches('`').trim().to_string())
        .collect()
}

/// The doc's `## <heading>` section, without the heading itself.
fn section<'a>(doc: &'a str, heading: &str) -> &'a str {
    let start = doc
        .find(&format!("\n## {heading}\n"))
        .unwrap_or_else(|| panic!("the matrix has no `## {heading}` section"));
    let rest = &doc[start + 1..];
    let body = &rest[rest.find('\n').expect("a heading ends") + 1..];
    match body.find("\n## ") {
        Some(next) => &body[..next],
        None => body,
    }
}

fn matrix() -> String {
    let path = crate_dir().join("../../../docs/feature-test-matrix.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e} — regenerate the matrix", path.display()))
}

/// The heart of it: the doc and the sources must agree, name for name.
///
/// One test rather than five, because they share the whole parse and because a mismatch in
/// any column has the same fix — regenerate the matrix — so splitting them would only make
/// one edit report five failures.
#[test]
fn the_feature_matrix_still_describes_this_app() {
    let doc = matrix();
    let declared = callbacks();
    let rust = concat(&crate_dir().join("src"), "rs");
    let uitest = concat(&crate_dir().join("src/uitest"), "rs");

    let mut rows = BTreeMap::new();
    for line in section(&doc, "Callbacks").lines() {
        if !line.starts_with("| ") || line.starts_with("|---") || line.starts_with("| Callback") {
            continue;
        }
        let c = cells(line);
        assert_eq!(c.len(), 3, "a callback row has three columns: {line}");
        rows.insert(c[0].clone(), (c[1].clone(), c[2].clone()));
    }

    let listed: BTreeSet<&String> = rows.keys().collect();
    let real: BTreeSet<&String> = declared.keys().collect();
    assert_eq!(
        listed, real,
        "the matrix lists a different set of callbacks than ui/*.slint declares — regenerate it"
    );

    let (mut bound, mut proven) = (0usize, 0usize);
    for (name, file) in &declared {
        let status = if !is_bound(&rust, name) {
            "ui-local"
        } else {
            bound += 1;
            if uitest.contains(name.as_str()) {
                proven += 1;
                "e2e"
            } else {
                "unproven"
            }
        };
        let (doc_file, doc_status) = &rows[name];
        assert_eq!(
            doc_file, file,
            "the matrix files `{name}` in the wrong .slint"
        );
        assert_eq!(
            doc_status, status,
            "the matrix calls `{name}` {doc_status}; it is {status} — regenerate it"
        );
    }

    // A test that drives a callback through a production `wire()` helper never writes the
    // callback's own name, so the substring measure calls it unproven even though a real
    // click really does reach the real Rust. A `//! e2e: a, b` header in a uitest file
    // declares such a case: the name lands in the file, so the measure counts it, and the
    // assertions here keep the claim honest — every declared name must be a real,
    // currently-bound callback, and no two files may claim the same one. A rename or a
    // deletion therefore breaks the build instead of leaving a stale boast behind.
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    for line in uitest.lines() {
        let Some(rest) = line.trim_start().strip_prefix("//! e2e:") else {
            continue;
        };
        for name in rest.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            assert!(
                declared.contains_key(name),
                "an `//! e2e:` line claims `{name}`, which no ui/*.slint declares"
            );
            assert!(
                is_bound(&rust, name),
                "an `//! e2e:` line claims `{name}`, which is not bound to Rust"
            );
            assert!(
                claimed.insert(name.to_string()),
                "`{name}` is claimed by two `//! e2e:` lines"
            );
        }
    }

    let variants = command_variants();
    let bindings = crate::keybindings::default_bindings();
    let keyed: BTreeSet<String> = bindings
        .iter()
        .map(|b| format!("{:?}", b.command))
        .map(|d| {
            d.split(['(', ' '])
                .next()
                .expect("a debug name")
                .to_string()
        })
        .collect();
    assert!(
        keyed.iter().all(|k| variants.contains(k)),
        "a keybinding names a Command that no longer exists"
    );

    let mut listed_cmds = BTreeMap::new();
    for line in section(&doc, "Commands").lines() {
        if !line.starts_with("| `") {
            continue;
        }
        let c = cells(line);
        assert_eq!(c.len(), 2, "a command row has two columns: {line}");
        listed_cmds.insert(c[0].clone(), c[1].clone());
    }
    assert_eq!(
        listed_cmds.keys().cloned().collect::<Vec<_>>(),
        variants,
        "the matrix lists a different set of Commands than command.rs defines — regenerate it"
    );
    for v in &variants {
        let want = if keyed.contains(v) { "key" } else { "—" };
        assert_eq!(
            &listed_cmds[v], want,
            "the matrix has `{v}` on a key: {}; it is on a key: {}",
            listed_cmds[v], want
        );
    }

    let listed_ids: Vec<String> = section(&doc, "Keybindings")
        .lines()
        .filter(|l| l.starts_with("| `"))
        .map(|l| cells(l).remove(0))
        .collect();
    let real_ids: Vec<String> = bindings.iter().map(|b| b.id.to_string()).collect();
    assert_eq!(
        listed_ids, real_ids,
        "the matrix lists different keybindings than default_bindings() returns"
    );

    let counts = [
        ("callbacks declared", declared.len()),
        ("…bound to Rust", bound),
        ("…proven end-to-end", proven),
        ("…bound but unproven", bound - proven),
        ("…ui-local or dead", declared.len() - bound),
        ("Command variants", variants.len()),
        ("…reachable by a default keybinding", keyed.len()),
        ("keybindings in default_bindings()", real_ids.len()),
    ];
    let mut doc_counts = BTreeMap::new();
    for line in section(&doc, "Counts").lines() {
        if !line.starts_with("| ") || line.starts_with("|---") {
            continue;
        }
        let c = cells(line);
        if let Ok(n) = c[1].parse::<usize>() {
            doc_counts.insert(c[0].clone(), n);
        }
    }
    for (label, n) in counts {
        let got = doc_counts
            .get(label)
            .unwrap_or_else(|| panic!("the Counts table has no `{label}` row"));
        assert_eq!(*got, n, "the matrix says {label} = {got}; it is {n}");
    }
}
