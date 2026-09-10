//! Checks that the documentation still describes this code.
//!
//! These are the checks that caught the last round of drift, kept because
//! reading the docs is exactly how that drift stayed hidden: a table row that
//! had come adrift rendered as a stray line of pipes, a state diagram showed a
//! transition that was never built, and a formfeed byte turned a copy-paste
//! setup command into one that does not exist. None of it is visible to a human
//! skimming for accuracy; all of it is trivial to find mechanically.
//!
//! They live in `tests/` rather than `src/` because they are about the
//! repository rather than the program, and they are `cargo test` rather than a
//! script so that nobody has to remember to run them.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Every markdown file that is part of the documentation.
fn markdown() -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from("README.md")];
    let mut dirs = vec![PathBuf::from("docs")];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).expect("docs/ must exist").flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Lines outside fenced code blocks. Diagrams and TOML samples are not tables,
/// and a `|` inside one is not a column.
fn prose(text: &str) -> Vec<(usize, &str)> {
    let mut fenced = false;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            out.push((i + 1, line));
        }
    }
    out
}

/// The module map must list exactly the files that exist.
///
/// It has twice fallen behind: `doctor.rs` and `transcript.rs` were both absent
/// while the map still described a file as new.
#[test]
fn the_module_map_lists_every_source_file() {
    let actual: BTreeSet<String> = fs::read_dir("src")
        .expect("src/")
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension()? == "rs").then(|| p.file_name()?.to_str().map(String::from))?
        })
        .collect();

    let arch = fs::read_to_string("docs/ARCHITECTURE.md").expect("ARCHITECTURE.md");
    let documented: BTreeSet<String> = arch
        .lines()
        .filter_map(|l| l.strip_prefix("| `"))
        .filter_map(|l| l.split('`').next())
        .filter(|n| n.ends_with(".rs"))
        .map(String::from)
        .collect();

    let missing: Vec<_> = actual.difference(&documented).collect();
    let extra: Vec<_> = documented.difference(&actual).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "docs/ARCHITECTURE.md module map is out of date.\n  \
         in src/ but undocumented: {missing:?}\n  \
         documented but not in src/: {extra:?}"
    );
}

/// Every environment variable the code reads must be in the spec's table.
///
/// The reverse is not required: `RUST_LOG` is read by tracing rather than by
/// IRA, and is documented because a reader needs to know about it.
#[test]
fn every_environment_variable_is_documented() {
    let spec = fs::read_to_string("docs/SPEC.md").expect("SPEC.md");
    let mut undocumented = Vec::new();

    for entry in fs::read_dir("src").expect("src/").flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("readable source");
        // `var_os` as well as `var`: paths.rs reads its directories that way,
        // because a path is not required to be UTF-8, and a check that only
        // knew about `var` would have let every one of them go undocumented.
        for call in ["env::var(\"", "env::var_os(\""] {
            for (_, after) in text.match_indices(call).map(|(i, m)| (i, &text[i + m.len()..])) {
                let Some(name) = after.split('"').next() else {
                    continue;
                };
                // The table writes them as `| \u{60}NAME\u{60} |`.
                if !spec.contains(&format!("`{name}`")) {
                    undocumented.push(format!("{} reads {name}", path.display()));
                }
            }
        }
    }
    undocumented.sort();
    undocumented.dedup();
    assert!(
        undocumented.is_empty(),
        "environment variables missing from docs/SPEC.md:\n  {}",
        undocumented.join("\n  ")
    );
}

/// A table row separated from its table renders as a stray line of pipes.
///
/// The row describing what happens when a background job finishes spent a
/// release like this: present in the source, invisible in the table.
#[test]
fn no_table_row_has_come_adrift() {
    let mut adrift = Vec::new();
    for file in markdown() {
        let text = fs::read_to_string(&file).expect("readable");
        let lines = prose(&text);
        for (idx, (no, line)) in lines.iter().enumerate() {
            if !line.starts_with("| ") {
                continue;
            }
            let before_blank = idx == 0 || lines[idx - 1].1.trim().is_empty();
            let after_pipe = lines.get(idx + 1).is_some_and(|(_, l)| l.starts_with('|'));
            if before_blank && !after_pipe {
                adrift.push(format!("{}:{no}: {}", file.display(), &line[..line.len().min(60)]));
            }
        }
    }
    assert!(
        adrift.is_empty(),
        "table rows detached from their table:\n  {}",
        adrift.join("\n  ")
    );
}

/// A row with the wrong number of columns silently loses a cell.
#[test]
fn tables_keep_their_columns() {
    let mut wrong = Vec::new();
    for file in markdown() {
        let text = fs::read_to_string(&file).expect("readable");
        let mut expected: Option<usize> = None;
        for (no, line) in prose(&text) {
            if line.starts_with('|') {
                let count = line.matches('|').count();
                match expected {
                    None => expected = Some(count),
                    Some(n) if n != count => {
                        wrong.push(format!(
                            "{}:{no}: {count} pipes, table uses {n}",
                            file.display()
                        ));
                    }
                    _ => {}
                }
            } else {
                expected = None;
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "table rows with the wrong number of columns:\n  {}",
        wrong.join("\n  ")
    );
}

/// A control character in a doc is invisible until someone copies the line.
///
/// A formfeed written by a careless `\f` in a patch script turned
/// `.\scripts\fetch-models.ps1` into a command that does not exist, and sat in
/// the README for a fortnight.
#[test]
fn no_document_contains_a_control_character() {
    let mut found = Vec::new();
    for file in markdown() {
        let bytes = fs::read(&file).expect("readable");
        for (i, b) in bytes.iter().enumerate() {
            if matches!(b, 0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F) {
                found.push(format!("{}: byte {i} is {b:#04x}", file.display()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "control characters in documentation:\n  {}",
        found.join("\n  ")
    );
}

/// A link to a file that was renamed or never existed.
#[test]
fn every_relative_link_resolves() {
    let mut broken = Vec::new();
    for file in markdown() {
        let text = fs::read_to_string(&file).expect("readable");
        let dir = file.parent().unwrap_or(Path::new("."));
        for (no, line) in prose(&text) {
            let mut rest = line;
            while let Some(open) = rest.find("](") {
                rest = &rest[open + 2..];
                let Some(close) = rest.find(')') else { break };
                let target = &rest[..close];
                rest = &rest[close..];

                // Strip a fragment; an anchor is not a path.
                let path = target.split('#').next().unwrap_or_default();
                if path.is_empty() || path.starts_with("http") || path.starts_with("mailto:") {
                    continue;
                }
                if !dir.join(path).exists() {
                    broken.push(format!("{}:{no}: {target}", file.display()));
                }
            }
        }
    }
    assert!(
        broken.is_empty(),
        "links pointing at nothing:\n  {}",
        broken.join("\n  ")
    );
}
