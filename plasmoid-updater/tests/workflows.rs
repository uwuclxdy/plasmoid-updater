//! Runs the release workflow's bash steps the way the Actions runner runs
//! them, against a sandbox workspace shaped like this one, a fake `gh`, and a
//! `cargo` whose `publish` is a fake (every other subcommand is the real
//! cargo, offline). No network, no real remote and no real registry is
//! reached.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod workflow_text;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use workflow_text::{CRATES_STEP, UPLOAD_STEP, job_lines, repo_root, step_script, workflow};

const LIB: &str = "libplasmoid-updater";
const BIN: &str = "plasmoid-updater";

fn host_program(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set"))
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} not found on PATH"))
}

/// `publish` logs its argv to `$FAKE_LOG` and answers per the `FAKE_*`
/// switches; everything else runs the real cargo. The two "already exists"
/// shapes: cargo's own index check (`index`; cargo 1.99.1 prints this text as a
/// warning on a dry run, measured 2026-11-10) and the registry's refusal for a
/// version the index did not list yet (`api`; the text is read from
/// crates.io's source, never measured against a real publish).
const FAKE_CARGO: &str = r#"#!/usr/bin/env bash
args=("$@")
case "${args[0]:-}" in
  publish)
    printf '%s\n' "${args[*]}" >>"$FAKE_LOG"
    pkg=""
    for ((i = 0; i < ${#args[@]}; i++)); do
      if [ "${args[i]}" = -p ]; then pkg="${args[i + 1]}"; fi
    done
    case " ${FAKE_PUBLISH_EXISTS:-} " in
      *" ${pkg} "*)
        if [ "${FAKE_EXISTS_SHAPE:-index}" = index ]; then
          echo "error: crate ${pkg}@${FAKE_VERSION} already exists on crates.io index" >&2
        else
          echo "error: failed to publish ${pkg} v${FAKE_VERSION} to registry at https://crates.io" >&2
          echo >&2
          echo "Caused by:" >&2
          echo "  the remote server responded with an error (status 400 Bad Request): crate version \`${FAKE_VERSION}\` is already uploaded" >&2
        fi
        exit 101
        ;;
    esac
    case " ${FAKE_PUBLISH_FAIL:-} " in
      *" ${pkg} "*)
        echo "error: failed to publish ${pkg} v${FAKE_VERSION} to registry at https://crates.io" >&2
        exit 101
        ;;
    esac
    exit 0
    ;;
esac
# a `cargo` wrapper earlier on the host's PATH resolves `cargo` on PATH
# again, so this fake's own dir leaves PATH first or the wrapper finds it
PATH="${PATH#"$(dirname "$0")":}"
exec "$WORKFLOW_TEST_CARGO" "${args[@]}"
"#;

/// `release view` answers like gh 2.103.3 for a missing release (exit 1,
/// stderr exactly `release not found`) unless `FAKE_GH_RELEASE_EXISTS` is
/// set. `FAKE_GH_VIEW_ERROR` makes it fail another way: exit 1 with that
/// text on stderr (`\n` starts a new line), the shape gh 2.103.3 gave for an
/// unreachable host and for a rejected token (measured 2026-11-09). Every
/// call is logged.
const FAKE_GH: &str = r#"#!/usr/bin/env bash
printf '%s\n' "$*" >>"$FAKE_LOG"
if [ "${1:-} ${2:-}" = "release view" ]; then
  if [ -n "${FAKE_GH_VIEW_ERROR:-}" ]; then
    printf '%b\n' "$FAKE_GH_VIEW_ERROR" >&2
    exit 1
  fi
  if [ -z "${FAKE_GH_RELEASE_EXISTS:-}" ]; then
    echo "release not found" >&2
    exit 1
  fi
fi
exit 0
"#;

/// A workspace shaped like this repo's (a library and a binary member
/// inheriting `[workspace.package] version`), with the fakes first on PATH.
struct Sandbox {
    _root: tempfile::TempDir,
    repo: PathBuf,
    bin: PathBuf,
    log: PathBuf,
}

impl Sandbox {
    fn new(version: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let bin = root.path().join("bin");
        for dir in [&repo, &bin, &repo.join("lib/src"), &repo.join("bin/src")] {
            fs::create_dir_all(dir).unwrap();
        }
        for (name, body) in [("cargo", FAKE_CARGO), ("gh", FAKE_GH)] {
            let path = bin.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let sandbox = Self {
            log: root.path().join("calls.log"),
            _root: root,
            repo,
            bin,
        };
        fs::write(
            sandbox.repo.join("Cargo.toml"),
            format!("[workspace]\nresolver = \"3\"\nmembers = [\"lib\", \"bin\"]\n\n[workspace.package]\nversion = \"{version}\"\nedition = \"2024\"\n\n[workspace.dependencies]\n{LIB} = {{ path = \"lib\", version = \"{version}\" }}\n"),
        )
        .unwrap();
        fs::write(
            sandbox.repo.join("lib/Cargo.toml"),
            format!("[package]\nname = \"{LIB}\"\nversion.workspace = true\nedition.workspace = true\npublish = true\n"),
        )
        .unwrap();
        fs::write(sandbox.repo.join("lib/src/lib.rs"), "pub fn f() {}\n").unwrap();
        fs::write(
            sandbox.repo.join("bin/Cargo.toml"),
            format!("[package]\nname = \"{BIN}\"\nversion.workspace = true\nedition.workspace = true\npublish = true\n\n[dependencies]\n{LIB} = {{ workspace = true }}\n"),
        )
        .unwrap();
        fs::write(sandbox.repo.join("bin/src/main.rs"), "fn main() {}\n").unwrap();
        sandbox
    }

    /// A workflow step's script run the way the Actions runner runs a step
    /// whose job names no shell (`bash -e {0}`: no pipefail unless the script
    /// sets it), from the repo root, with `GITHUB_REF_NAME` set to `tag`.
    fn run_step(&self, script: &str, tag: &str, envs: &[(&str, &str)]) -> Output {
        let path = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap());
        let mut command = Command::new(host_program("bash"));
        command
            .args(["-e", "-c", script])
            .current_dir(&self.repo)
            .env("PATH", path)
            .env("WORKFLOW_TEST_CARGO", host_program("cargo"))
            .env("CARGO_NET_OFFLINE", "true")
            .env("FAKE_LOG", &self.log)
            .env("FAKE_VERSION", tag.trim_start_matches('v'))
            .env("GITHUB_REF_NAME", tag)
            .env("GITHUB_REPOSITORY", "uwuclxdy/plasmoid-updater")
            .env("GITHUB_RUN_ID", "42")
            .stdin(Stdio::null());
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The crate each logged `cargo publish` named, in call order.
    fn published(&self) -> Vec<String> {
        self.calls()
            .iter()
            .filter(|line| line.starts_with("publish"))
            .filter_map(|line| {
                let words: Vec<&str> = line.split_whitespace().collect();
                let at = words.iter().position(|word| *word == "-p")?;
                words.get(at + 1).map(|name| (*name).to_string())
            })
            .collect()
    }
}

fn stdout_lines(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

fn log(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const TAG_FORMAT_STEP: &str = "Validate tag is semver";
const TAG_MANIFEST_STEP: &str = "Refuse a tag the workspace version does not match";

#[test]
fn the_tag_format_step_takes_a_prerelease_and_refuses_a_malformed_version() {
    let sandbox = Sandbox::new("0.2.1");
    let script = step_script(&workflow("release.yml"), TAG_FORMAT_STEP);
    for tag in ["v1.0.0", "v1.0.0-rc.1", "v0.1.0-alpha.2.x"] {
        let out = sandbox.run_step(&script, tag, &[]);
        assert_eq!(out.status.code(), Some(0), "{tag}:\n{}", log(&out));
    }
    for tag in [
        "v1.0",
        "v1.0.0-",
        "v01.0.0",
        "v1.0.0+build",
        "v1.0.0-01",
        "v1.0.0-rc.01",
    ] {
        let out = sandbox.run_step(&script, tag, &[]);
        assert_eq!(out.status.code(), Some(1), "{tag}:\n{}", log(&out));
        assert_eq!(
            stdout_lines(&out),
            [format!(
                "::error::tag '{tag}' is not vX.Y.Z or vX.Y.Z-PRERELEASE"
            )],
            "{tag}"
        );
    }
}

/// Binds only on a host that has the `en_US.UTF-8` locale: without it bash
/// stays in the C locale and refuses these tags whatever the step sets.
#[test]
fn the_tag_format_step_refuses_a_non_ascii_tag_under_a_utf8_locale() {
    let sandbox = Sandbox::new("0.2.1");
    let script = step_script(&workflow("release.yml"), TAG_FORMAT_STEP);
    for tag in ["v1.0.0-é", "v1.0.0-٣"] {
        let out = sandbox.run_step(&script, tag, &[("LC_ALL", "en_US.UTF-8")]);
        assert_eq!(out.status.code(), Some(1), "{tag}:\n{}", log(&out));
        assert_eq!(
            stdout_lines(&out),
            [format!(
                "::error::tag '{tag}' is not vX.Y.Z or vX.Y.Z-PRERELEASE"
            )],
            "{tag}"
        );
    }
}

#[test]
fn validate_refuses_every_tag_when_the_members_disagree_on_the_version() {
    let script = step_script(&workflow("release.yml"), TAG_MANIFEST_STEP);
    let sandbox = Sandbox::new("0.2.1");
    fs::write(
        sandbox.repo.join("bin/Cargo.toml"),
        format!("[package]\nname = \"{BIN}\"\nversion = \"0.2.2\"\nedition.workspace = true\npublish = true\n\n[dependencies]\n{LIB} = {{ workspace = true }}\n"),
    )
    .unwrap();
    for tag in ["v0.2.1", "v0.2.2"] {
        let out = sandbox.run_step(&script, tag, &[]);
        assert_eq!(out.status.code(), Some(1), "{tag}:\n{}", log(&out));
        assert_eq!(
            stdout_lines(&out),
            [format!(
                "::error::tag {tag} does not match the workspace version '0.2.1 0.2.2'; a release bumps the version and tags it together, so a hand-pushed tag must not skip the bump"
            )],
            "{tag}:\n{}",
            log(&out)
        );
    }
}

#[test]
fn validate_refuses_a_tag_the_workspace_version_does_not_carry() {
    let script = step_script(&workflow("release.yml"), TAG_MANIFEST_STEP);
    let sandbox = Sandbox::new("0.2.1");
    let out = sandbox.run_step(&script, "v0.2.2", &[]);
    assert_eq!(out.status.code(), Some(1), "{}", log(&out));
    assert_eq!(
        stdout_lines(&out),
        [
            "::error::tag v0.2.2 does not match the workspace version '0.2.1'; a release bumps the version and tags it together, so a hand-pushed tag must not skip the bump"
        ],
        "{}",
        log(&out)
    );
    for version in ["0.2.1", "1.0.0-rc.1"] {
        let sandbox = Sandbox::new(version);
        let out = sandbox.run_step(&script, &format!("v{version}"), &[]);
        assert_eq!(out.status.code(), Some(0), "{version}:\n{}", log(&out));
    }
}

#[test]
fn validate_refuses_a_tag_that_differs_only_in_its_prerelease_part() {
    let script = step_script(&workflow("release.yml"), TAG_MANIFEST_STEP);
    for (version, tag) in [
        ("1.0.0-rc.1", "v1.0.0-rc.2"),
        ("1.0.0-rc.1", "v1.0.0"),
        ("1.0.0", "v1.0.0-rc.1"),
    ] {
        let sandbox = Sandbox::new(version);
        let out = sandbox.run_step(&script, tag, &[]);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{tag} on {version}:\n{}",
            log(&out)
        );
        assert_eq!(
            stdout_lines(&out),
            [format!(
                "::error::tag {tag} does not match the workspace version '{version}'; a release bumps the version and tags it together, so a hand-pushed tag must not skip the bump"
            )],
            "{tag} on {version}:\n{}",
            log(&out)
        );
    }
}

#[test]
fn validate_runs_only_the_tag_format_and_the_tag_version_checks() {
    let yaml = workflow("release.yml");
    let validate = job_lines(&yaml, "validate");
    let curl: Vec<&str> = validate
        .iter()
        .filter(|line| line.contains("curl"))
        .copied()
        .collect();
    assert!(curl.is_empty(), "validate names curl: {curl:?}");
    let lines: Vec<&str> = validate.iter().copied().map(str::trim).collect();
    let names: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.strip_prefix("- name: "))
        .collect();
    assert_eq!(names, [TAG_FORMAT_STEP, TAG_MANIFEST_STEP], "{validate:?}");
    let scripts = lines
        .iter()
        .filter(|line| line.starts_with("run:") || line.starts_with("- run:"))
        .count();
    assert_eq!(scripts, 2, "a script beside the two checks:\n{validate:?}");
    let actions: Vec<&str> = lines
        .iter()
        .filter(|line| line.starts_with("uses:") || line.starts_with("- uses:"))
        .filter_map(|line| line.split_once("uses: "))
        .filter_map(|(_, action)| action.split('@').next())
        .collect();
    assert_eq!(
        actions,
        ["actions/checkout", "dtolnay/rust-toolchain"],
        "{validate:?}"
    );
}

#[test]
fn the_crates_step_publishes_the_library_before_the_binary() {
    let sandbox = Sandbox::new("0.2.1");
    let out = sandbox.run_step(
        &step_script(&workflow("release.yml"), CRATES_STEP),
        "v0.2.1",
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", log(&out));
    assert_eq!(sandbox.published(), [LIB, BIN], "{}", log(&out));
}

/// The step holds the registry token, so cargo must not build the packaged
/// crates there: that build runs every dependency's build script.
#[test]
fn the_crates_step_publishes_each_package_without_building_it() {
    let sandbox = Sandbox::new("0.2.1");
    let out = sandbox.run_step(
        &step_script(&workflow("release.yml"), CRATES_STEP),
        "v0.2.1",
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", log(&out));
    let publishes: Vec<String> = sandbox
        .calls()
        .into_iter()
        .filter(|line| line.starts_with("publish"))
        .collect();
    assert_eq!(publishes.len(), 2, "{publishes:?}");
    for call in &publishes {
        let words: Vec<&str> = call.split_whitespace().collect();
        assert!(
            words.contains(&"--locked") && words.contains(&"--no-verify"),
            "cargo publish got {call:?}"
        );
    }
}

#[test]
fn the_crates_step_counts_a_version_crates_io_already_has_as_published() {
    let script = step_script(&workflow("release.yml"), CRATES_STEP);
    for (shape, existing) in [("index", LIB), ("api", LIB), ("index", BIN), ("api", BIN)] {
        let sandbox = Sandbox::new("0.2.1");
        let out = sandbox.run_step(
            &script,
            "v0.2.1",
            &[
                ("FAKE_PUBLISH_EXISTS", existing),
                ("FAKE_EXISTS_SHAPE", shape),
            ],
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "{shape} {existing}:\n{}",
            log(&out)
        );
        assert_eq!(
            sandbox.published(),
            [LIB, BIN],
            "{shape} {existing}:\n{}",
            log(&out)
        );
        let counted = format!("{existing} 0.2.1 is already on crates.io; counting it as published");
        assert_eq!(
            stdout_lines(&out)
                .iter()
                .filter(|line| **line == counted)
                .count(),
            1,
            "{shape} {existing}:\n{}",
            log(&out)
        );
    }
}

#[test]
fn the_crates_step_stops_when_the_library_publish_fails() {
    let sandbox = Sandbox::new("0.2.1");
    let out = sandbox.run_step(
        &step_script(&workflow("release.yml"), CRATES_STEP),
        "v0.2.1",
        &[("FAKE_PUBLISH_FAIL", LIB)],
    );
    assert_ne!(out.status.code(), Some(0), "{}", log(&out));
    assert_eq!(sandbox.published(), [LIB], "{}", log(&out));
}

#[test]
fn the_crates_step_fails_a_binary_publish_after_counting_the_library() {
    let sandbox = Sandbox::new("0.2.1");
    let out = sandbox.run_step(
        &step_script(&workflow("release.yml"), CRATES_STEP),
        "v0.2.1",
        &[("FAKE_PUBLISH_EXISTS", LIB), ("FAKE_PUBLISH_FAIL", BIN)],
    );
    assert_ne!(out.status.code(), Some(0), "{}", log(&out));
    assert_eq!(sandbox.published(), [LIB, BIN], "{}", log(&out));
    let counted: Vec<String> = stdout_lines(&out)
        .into_iter()
        .filter(|line| line.ends_with("counting it as published"))
        .collect();
    assert_eq!(
        counted,
        [format!(
            "{LIB} 0.2.1 is already on crates.io; counting it as published"
        )],
        "{}",
        log(&out)
    );
}

#[test]
fn the_upload_step_names_the_resume_for_a_missing_release() {
    let sandbox = Sandbox::new("0.2.2");
    fs::create_dir(sandbox.repo.join("artifacts")).unwrap();
    let out = sandbox.run_step(
        &step_script(&workflow("release.yml"), UPLOAD_STEP),
        "v0.2.2",
        &[],
    );
    assert_eq!(out.status.code(), Some(1), "{}", log(&out));
    assert_eq!(
        stdout_lines(&out),
        [
            "::error::release v0.2.2 does not exist; if the release run that pushed this tag failed before creating it, resume that run with --retry, then re-run this job so the assets attach: gh run rerun 42 --failed — if the tag was pushed by hand, release the next version"
        ],
        "{}",
        log(&out)
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{}", log(&out));
}

#[test]
fn the_upload_step_tells_a_gh_failure_from_a_missing_release() {
    let script = step_script(&workflow("release.yml"), UPLOAD_STEP);
    for (failure, first_line) in [
        (
            "error connecting to api.github.com",
            "error connecting to api.github.com",
        ),
        (
            "HTTP 401: Bad credentials (https://api.github.com/repos/uwuclxdy/plasmoid-updater/releases/tags/v0.2.2)\\nTry authenticating with:  gh auth login -h github.com",
            "HTTP 401: Bad credentials (https://api.github.com/repos/uwuclxdy/plasmoid-updater/releases/tags/v0.2.2)",
        ),
        // gh's missing-release answer is that one line alone
        ("release not found\\nand a second line", "release not found"),
    ] {
        let sandbox = Sandbox::new("0.2.2");
        fs::create_dir(sandbox.repo.join("artifacts")).unwrap();
        let out = sandbox.run_step(&script, "v0.2.2", &[("FAKE_GH_VIEW_ERROR", failure)]);
        assert_eq!(out.status.code(), Some(1), "{failure}:\n{}", log(&out));
        assert_eq!(
            stdout_lines(&out),
            [format!(
                "::error::cannot tell whether release v0.2.2 exists (gh release view: {first_line}); re-run this job once gh answers: gh run rerun 42 --failed"
            )],
            "{failure}:\n{}",
            log(&out)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "",
            "{failure}:\n{}",
            log(&out)
        );
        assert_eq!(
            sandbox.calls(),
            ["release view v0.2.2 --repo uwuclxdy/plasmoid-updater"],
            "{failure}:\n{}",
            log(&out)
        );
    }
}

#[test]
fn the_upload_step_attaches_the_built_files_to_an_existing_release() {
    let sandbox = Sandbox::new("0.2.2");
    let artifacts = sandbox.repo.join("artifacts");
    fs::create_dir(&artifacts).unwrap();
    for name in ["plasmoid-updater-linux-x86_64", "sha256sums.txt"] {
        fs::write(artifacts.join(name), "").unwrap();
    }
    let out = sandbox.run_step(
        &step_script(&workflow("release.yml"), UPLOAD_STEP),
        "v0.2.2",
        &[("FAKE_GH_RELEASE_EXISTS", "1")],
    );
    assert_eq!(out.status.code(), Some(0), "{}", log(&out));
    assert_eq!(
        sandbox.calls(),
        [
            "release view v0.2.2 --repo uwuclxdy/plasmoid-updater",
            "release upload v0.2.2 plasmoid-updater-linux-x86_64 sha256sums.txt --clobber --repo uwuclxdy/plasmoid-updater"
        ],
        "{}",
        log(&out)
    );
}

#[test]
fn both_crates_state_publish_true() {
    for member in ["libplasmoid-updater", "plasmoid-updater"] {
        let manifest = fs::read_to_string(repo_root().join(member).join("Cargo.toml")).unwrap();
        let package: Vec<&str> = manifest
            .lines()
            .skip_while(|line| line.trim() != "[package]")
            .skip(1)
            .take_while(|line| !line.trim_start().starts_with('['))
            .collect();
        assert!(
            package.contains(&"publish = true"),
            "{member}'s [package] has no `publish = true`: {package:?}"
        );
    }
}
