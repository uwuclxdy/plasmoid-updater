//! The workflow-text readers both workflow test crates share.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

pub const CRATES_STEP: &str = "Publish the crates to crates.io";
pub const UPLOAD_STEP: &str = "Upload assets to the release";

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

pub fn workflow(name: &str) -> String {
    fs::read_to_string(repo_root().join(".github/workflows").join(name)).unwrap()
}

/// The lines of the job named `job` in a workflow's text: from its `  <job>:`
/// header to the next two-space-indented key.
pub(crate) fn job_lines<'a>(yaml: &'a str, job: &str) -> Vec<&'a str> {
    let header = format!("  {job}:");
    let mut lines = yaml.lines().skip_while(|line| *line != header);
    assert!(lines.next().is_some(), "no job named {job:?}");
    lines
        .take_while(|line| {
            let key = line.starts_with("  ")
                && !line.starts_with("   ")
                && !line.trim_start().starts_with('#');
            !key
        })
        .collect()
}

/// What follows the `-` of a line that opens a sequence entry, the line's
/// indentation already cut: the dash stands alone or before a blank, so a
/// script's `--flag` opens none.
pub fn entry(line: &str) -> Option<&str> {
    line.strip_prefix('-')
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        .map(str::trim_start)
}

/// The `run:` script (a `|` block or an inline command) of the step named
/// `step_name` in a workflow's text.
pub fn step_script(yaml: &str, step_name: &str) -> String {
    let marker = format!("- name: {step_name}");
    let mut lines = yaml.lines();
    lines
        .find(|line| line.trim() == marker)
        .unwrap_or_else(|| panic!("no step named {step_name:?}"));
    let run = lines
        .by_ref()
        .find(|line| {
            assert!(
                entry(line.trim_start()).is_none(),
                "step {step_name:?} has no run"
            );
            line.trim_start().starts_with("run:")
        })
        .unwrap_or_else(|| panic!("step {step_name:?} has no run"));
    let inline = run.trim_start().trim_start_matches("run:").trim();
    if inline != "|" {
        return format!("{inline}\n");
    }
    let body_indent = run.len() - run.trim_start().len() + 2;
    let mut script = String::new();
    for line in lines {
        if line.trim().is_empty() {
            script.push('\n');
        } else if line.len() - line.trim_start().len() >= body_indent {
            script.push_str(&line[body_indent..]);
            script.push('\n');
        } else {
            break;
        }
    }
    script
}
