//! Scriptable REPL test harness.
//!
//! Each `tests/repl/*.repl` file is a self-contained debugger test:
//!
//! ```text
//! begin
//!     push.3 push.4 add
//! end
//! ---
//! continue
//! # CHECK: Program terminated successfully
//! stack
//! # CHECK: Operand Stack (1 elements)
//! # CHECK-NEXT: > [0] 7
//! ```
//!
//! The file has two sections separated by a line containing only `---`:
//!
//! 1. **Program** — inline Miden Assembly, assembled on the fly.
//! 2. **Script** — REPL commands interleaved with `# CHECK` directives.
//!
//! The commands are executed against the program via [`miden_debug::run_script`],
//! producing a transcript (prompts + echoed commands + output). The directives
//! are then matched against that transcript, lit/FileCheck style:
//!
//! * `# CHECK: <text>`      — `<text>` must appear on some line at or after the
//!                            previous match (substring match).
//! * `# CHECK-NEXT: <text>` — `<text>` must appear on the immediately following
//!                            line.
//! * `# CHECK-NOT: <text>`  — `<text>` must NOT appear between the previous
//!                            match and the next positive match.
//!
//! Lines starting with `#` that are not directives are comments. Blank lines are
//! ignored. A `quit` command (or end of script) ends execution.
//!
//! Set `REPL_DUMP=1` to print every transcript (useful when authoring tests or
//! updating expected output).
//!
//! This test requires the `repl` feature:
//!
//! ```sh
//! cargo test --features repl --test repl_harness
//! ```

use std::{fs, path::Path};

#[derive(Debug)]
enum Directive {
    /// Match a substring somewhere at or after the cursor.
    Check(String),
    /// Match a substring on the immediately following line.
    CheckNext(String),
    /// Assert a substring is absent until the next positive match.
    CheckNot(String),
}

struct ReplTest {
    masm: String,
    commands: Vec<String>,
    directives: Vec<Directive>,
}

/// Parse a `.repl` file into its program, commands, and CHECK directives.
fn parse(contents: &str) -> Result<ReplTest, String> {
    let (masm, script) = contents
        .split_once("\n---\n")
        .or_else(|| split_on_separator_line(contents))
        .ok_or("missing `---` separator between program and script sections")?;

    let mut commands = Vec::new();
    let mut directives = Vec::new();

    for line in script.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix('#') {
            let rest = rest.trim_start();
            if let Some(pat) = rest.strip_prefix("CHECK-NEXT:") {
                directives.push(Directive::CheckNext(pat.trim().to_string()));
            } else if let Some(pat) = rest.strip_prefix("CHECK-NOT:") {
                directives.push(Directive::CheckNot(pat.trim().to_string()));
            } else if let Some(pat) = rest.strip_prefix("CHECK:") {
                directives.push(Directive::Check(pat.trim().to_string()));
            }
            // Any other `#` line is a comment.
            continue;
        }

        commands.push(trimmed.to_string());
    }

    Ok(ReplTest {
        masm: masm.to_string(),
        commands,
        directives,
    })
}

/// Fallback separator detection for files whose `---` line has trailing
/// whitespace or uses CRLF endings.
fn split_on_separator_line(contents: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for line in contents.split_inclusive('\n') {
        if line.trim() == "---" {
            let before = &contents[..offset];
            let after = &contents[offset + line.len()..];
            return Some((before, after));
        }
        offset += line.len();
    }
    None
}

/// Match `directives` against `transcript`, returning a human-readable error on
/// the first failure.
fn check(transcript: &str, directives: &[Directive]) -> Result<(), String> {
    let lines: Vec<&str> = transcript.lines().collect();
    let mut cursor = 0usize;
    let mut pending_nots: Vec<&str> = Vec::new();

    let verify_nots = |nots: &[&str], from: usize, to: usize| -> Result<(), String> {
        for not in nots {
            for line in &lines[from..to] {
                if line.contains(*not) {
                    return Err(format!("CHECK-NOT matched (but should not): `{not}`"));
                }
            }
        }
        Ok(())
    };

    for directive in directives {
        match directive {
            Directive::Check(pat) => {
                let found = (cursor..lines.len()).find(|&j| lines[j].contains(pat));
                match found {
                    Some(j) => {
                        verify_nots(&pending_nots, cursor, j)?;
                        pending_nots.clear();
                        cursor = j + 1;
                    }
                    None => {
                        return Err(format!("CHECK not found: `{pat}` (searched from line {cursor})"));
                    }
                }
            }
            Directive::CheckNext(pat) => {
                if cursor >= lines.len() {
                    return Err(format!("CHECK-NEXT past end of output: `{pat}`"));
                }
                if !lines[cursor].contains(pat) {
                    return Err(format!(
                        "CHECK-NEXT failed: `{pat}`\n  actual next line: `{}`",
                        lines[cursor]
                    ));
                }
                pending_nots.clear();
                cursor += 1;
            }
            Directive::CheckNot(pat) => {
                pending_nots.push(pat);
            }
        }
    }

    verify_nots(&pending_nots, cursor, lines.len())?;
    Ok(())
}

#[test]
fn run_all_repl_scripts() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/repl");
    let dump = std::env::var_os("REPL_DUMP").is_some();

    let mut entries: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "repl"))
        .collect();
    entries.sort();

    assert!(!entries.is_empty(), "no .repl test files found in {}", dir.display());

    let mut failures = Vec::new();

    for path in &entries {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let contents = fs::read_to_string(path).expect("failed to read .repl file");

        let test = match parse(&contents) {
            Ok(t) => t,
            Err(e) => {
                failures.push(format!("[{name}] parse error: {e}"));
                continue;
            }
        };

        let command_refs: Vec<&str> = test.commands.iter().map(String::as_str).collect();
        let transcript = match miden_debug::run_script(&test.masm, &command_refs) {
            Ok(t) => t,
            Err(e) => {
                failures.push(format!("[{name}] run_script failed: {e}"));
                continue;
            }
        };

        if dump {
            println!("===== {name} =====\n{transcript}\n=====");
        }

        if let Err(e) = check(&transcript, &test.directives) {
            failures.push(format!(
                "[{name}] {e}\n\n--- transcript ---\n{transcript}------------------"
            ));
        } else {
            println!("ok: {name}");
        }
    }

    assert!(
        failures.is_empty(),
        "\n{} REPL script test(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
