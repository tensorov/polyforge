//! Mechanical enforcement of the theme-only-styling invariant.
//!
//! Every `.rs` file under `src/` EXCEPT `theme.rs` must contain no direct
//! color construction (`Color::Rgb(`) and no hex-style literals
//! (`#RRGGBB`). All styling flows through `crate::theme`, so a stray literal
//! anywhere else is a regression even if it compiles.
//!
//! The walk uses `std::fs::read_dir` recursion rooted at `CARGO_MANIFEST_DIR`
//! and the hex check is hand-rolled (the crate carries no regex dependency):
//! `#` followed by exactly six hex digits and then a non-word byte reproduces
//! the pattern `#[0-9a-fA-F]{6}\b`.

use std::fs;
use std::path::{Path, PathBuf};

fn src_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|err| panic!("read_dir {}: {err}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|err| panic!("readdir entry in {}: {err}", dir.display()));
        let path = entry.path();
        if path.is_dir() {
            src_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// True when `bytes[pos..]` starts at a word boundary: the previous byte is
/// not `[0-9a-zA-Z_]`. Used to reproduce `\b` after the sixth hex digit.
fn word_boundary(bytes: &[u8], pos: usize) -> bool {
    match bytes.get(pos) {
        None => true,
        Some(&b) => !(b.is_ascii_alphanumeric() || b == b'_'),
    }
}

fn is_hex_digit(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || (b'A'..=b'F').contains(&b)
}

/// Find the first hex-style literal (`#RRGGBB` at a word boundary), if any.
fn find_hex_literal(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] != b'#' {
            continue;
        }
        let digits = &bytes[i + 1..];
        if digits.len() >= 6
            && digits[..6].iter().all(|&b| is_hex_digit(b))
            && word_boundary(bytes, i + 7)
        {
            return Some(i);
        }
    }
    None
}

#[test]
fn only_theme_rs_may_construct_colors() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = manifest_dir.join("src");
    assert!(
        src.is_dir(),
        "expected a src directory at {}",
        src.display()
    );

    let mut files = Vec::new();
    src_files(&src, &mut files);
    assert!(
        files.len() > 1,
        "walk found {} rs files under src/, expected several",
        files.len()
    );

    let mut violations: Vec<String> = Vec::new();
    for file in files {
        if file.file_name().is_some_and(|name| name == "theme.rs") {
            continue;
        }
        let content = fs::read_to_string(&file)
            .unwrap_or_else(|err| panic!("read {}: {err}", file.display()));
        for (index, line) in content.lines().enumerate() {
            if line.contains("Color::Rgb(") {
                violations.push(format!(
                    "{}:{}: direct Color::Rgb( is forbidden outside theme.rs: {line}",
                    file.display(),
                    index + 1
                ));
            }
            if let Some(col) = find_hex_literal(line) {
                violations.push(format!(
                    "{}:{}: hex-style color literal at column {}: {line}",
                    file.display(),
                    index + 1,
                    col + 1
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "theme-only-styling invariant violated ({} offender(s)):\n{}",
        violations.len(),
        violations.join("\n")
    );
}
