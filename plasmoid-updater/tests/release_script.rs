//! Drives `release.sh` and `scripts/semver-guard.sh` against a sandbox tree: a
//! throwaway git repo whose `origin` is a local bare repo, with fake `gh`,
//! `cargo` and `cargo-semver-checks` first on PATH (`fixtures/release/`). The
//! real `gh` is never reached and no real remote is touched.
//!
//! The sandbox starts at the previous release: an initial commit tagged
//! `v0.2.1` (matching the manifest version) pushed to the bare remote, so the
//! script's API check has a baseline. The semver check runs before any
//! mutation, so a refused run must leave the tree, refs, config and remote
//! untouched; `snapshot` pins that.
//!
//! Linux-only like the repo's other sandboxed suites: `release.sh` uses GNU
//! `sed -i`, the fake commands sit on a `/usr/bin:/bin` PATH, and the fixture
//! installs them with unix permissions.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const ORIGIN_URL: &str = "https://github.com/uwuclxdy/plasmoid-updater";
const BASE_PATH: &str = "/usr/bin:/bin";
const VERSION: &str = "0.2.2";
const PREV_VERSION: &str = "0.2.1";
const SEMVER_PACKAGE: &str = "libplasmoid-updater";
const WORKSPACE_ARGS: [&str; 3] = ["--workspace", "--notes", "docs/RELEASE_NOTES.md"];

fn host_program(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set"))
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

fn system_program(name: &str) -> PathBuf {
    host_program(name).unwrap_or_else(|| panic!("{name} not found on PATH"))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// This checkout's script by default. `PLASMOID_UPDATER_RELEASE_SH` overrides
/// only when `PLASMOID_UPDATER_RELEASE_SH_OVERRIDE` is also set, so a lone
/// exported variable in a developer's shell never retargets the gate at
/// another script.
fn release_script() -> PathBuf {
    if std::env::var_os("PLASMOID_UPDATER_RELEASE_SH_OVERRIDE").is_some()
        && let Some(path) = std::env::var_os("PLASMOID_UPDATER_RELEASE_SH")
    {
        return PathBuf::from(path);
    }
    repo_root().join("release.sh")
}

/// The shared semver-guard script release.sh sources and the workflows run.
fn guard_script() -> PathBuf {
    repo_root().join("scripts").join("semver-guard.sh")
}

fn install(from: &Path, to: &Path) {
    fs::copy(from, to).unwrap();
    fs::set_permissions(to, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The production workspace manifest: members inherit the version, and the
/// root `[workspace.dependencies]` pins the lib by path (the one line the bump
/// and the reset both touch).
fn workspace_manifest(version: &str) -> String {
    format!(
        "[workspace]\nmembers = [\"lib\", \"bin\"]\n\n[workspace.package]\nversion = \"{version}\"\nedition = \"2021\"\n\n[workspace.dependencies]\nlibplasmoid-updater = {{ path = \"lib\", version = \"{version}\", default-features = false }}\n"
    )
}

enum Layout {
    /// A single `[package]` crate rolling a tracked `CHANGELOG.md`.
    Single,
    /// The production shape: a virtual workspace whose members inherit the
    /// version, the root `[workspace.dependencies]` pins the lib by path, and
    /// a gitignored changelog rolls.
    Workspace,
}

struct Fixture {
    _root: tempfile::TempDir,
    repo: PathBuf,
    remote: PathBuf,
    bin: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
    gh_log: PathBuf,
    semver_log: PathBuf,
    cargo_log: PathBuf,
    rustup_log: PathBuf,
    publish_log: PathBuf,
    path_with_tool: String,
    path_without_tool: String,
    prev_tag: String,
}

impl Fixture {
    fn new() -> Self {
        Self::with_layout(Layout::Single, PREV_VERSION)
    }

    fn workspace() -> Self {
        Self::with_layout(Layout::Workspace, PREV_VERSION)
    }

    fn workspace_at(prev_version: &str) -> Self {
        Self::with_layout(Layout::Workspace, prev_version)
    }

    /// Several tags around the answer, so only the baseline rule picks it:
    /// below the requested version are `v0.1.9`, `v0.2.0` (lightweight),
    /// `v0.2.1`, `v0.2.9` and `v0.2.10` (annotated, which sorts above `v0.2.9`
    /// only numerically); `v0.2.11` sits on a maintenance commit `main` does
    /// not contain; `v1.0.0` is above any 0.x release.
    fn crowded() -> Self {
        let fixture = Self::workspace();
        for name in ["v0.1.9", "v0.2.0", "v1.0.0"] {
            fixture.git(&["tag", name]);
        }
        for name in ["v0.2.9", "v0.2.10"] {
            fixture.git(&["tag", name, "-m", "release"]);
        }
        fixture.git(&["switch", "--quiet", "--create", "maint"]);
        fixture.git(&[
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "maintenance fix",
        ]);
        fixture.git(&["tag", "v0.2.11", "-m", "maintenance"]);
        fixture.git(&["switch", "--quiet", "main"]);
        fixture.git(&["branch", "--quiet", "-D", "maint"]);
        fixture.git(&["push", "--quiet", "origin", "--tags"]);
        fixture
    }

    /// A tag that exists on origin only: its commit was pushed from another
    /// clone and never fetched here.
    fn with_remote_only_tag(name: &str) -> Self {
        let fixture = Self::workspace();
        let remote = fixture.remote.display().to_string();
        let other = fixture.base().join("other");
        fixture.git_at(fixture.base(), &["clone", "--quiet", &remote, "other"]);
        fixture.git_at(&other, &["config", "user.name", "Other"]);
        fixture.git_at(&other, &["config", "user.email", "other@example.invalid"]);
        fixture.git_at(
            &other,
            &["commit", "--quiet", "--allow-empty", "-m", "other"],
        );
        fixture.git_at(&other, &["tag", name, "-m", "other release"]);
        fixture.git_at(&other, &["push", "--quiet", "origin", name]);
        fixture
    }

    /// A tag that exists on origin only, on a commit this clone already has
    /// (the initial one): reachable, but not a tag here.
    fn with_remote_tag_on_known_commit(name: &str) -> Self {
        let fixture = Self::workspace();
        let remote = fixture.remote.display().to_string();
        let other = fixture.base().join("other");
        fixture.git_at(fixture.base(), &["clone", "--quiet", &remote, "other"]);
        fixture.git_at(&other, &["config", "user.name", "Other"]);
        fixture.git_at(&other, &["config", "user.email", "other@example.invalid"]);
        fixture.git_at(&other, &["tag", name, "-m", "other release"]);
        fixture.git_at(&other, &["push", "--quiet", "origin", name]);
        fixture
    }

    fn with_layout(layout: Layout, prev_version: &str) -> Self {
        let prev_tag = format!("v{prev_version}");
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        let repo = base.join("repo");
        let remote = base.join("remote.git");
        let bin = base.join("bin");
        let tool_bin = base.join("tool-bin");
        let home = base.join("home");
        let tmp = base.join("tmp");
        for dir in [&repo, &remote, &bin, &tool_bin, &home, &tmp] {
            fs::create_dir_all(dir).unwrap();
        }
        let gh_log = base.join("gh.log");
        let semver_log = base.join("semver.log");
        let cargo_log = base.join("cargo.log");
        let rustup_log = base.join("rustup.log");
        let publish_log = base.join("publish.log");

        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/release");
        install(&fixtures.join("fake-gh"), &bin.join("gh"));
        install(&fixtures.join("fake-cargo"), &bin.join("cargo"));
        install(&fixtures.join("fake-git"), &bin.join("git"));
        install(&fixtures.join("fake-rustup"), &bin.join("rustup"));
        install(&fixtures.join("fake-curl"), &bin.join("curl"));
        install(
            &fixtures.join("fake-cargo-semver-checks"),
            &tool_bin.join("cargo-semver-checks"),
        );

        fs::write(home.join(".gitconfig"), "").unwrap();

        let fixture = Self {
            _root: root,
            repo,
            remote: remote.clone(),
            bin: bin.clone(),
            home,
            tmp,
            gh_log,
            semver_log,
            cargo_log,
            rustup_log,
            publish_log,
            path_with_tool: format!("{}:{}:{BASE_PATH}", bin.display(), tool_bin.display()),
            path_without_tool: format!("{}:{BASE_PATH}", bin.display()),
            prev_tag,
        };

        fixture.git(&["init", "--quiet", "--initial-branch=main"]);
        fixture.git(&["config", "user.email", "release-fixture@example.invalid"]);
        fixture.git(&["config", "user.name", "Release Fixture"]);
        fixture.git(&[
            "config",
            "--add",
            &format!("url.file://{}.insteadOf", remote.display()),
            ORIGIN_URL,
        ]);
        fixture.git(&["remote", "add", "origin", ORIGIN_URL]);
        fixture.write_layout(&layout, prev_version);
        fixture.git(&["add", "-A"]);
        fixture.git(&["commit", "--quiet", "-m", "initial"]);
        // The previous release: the tag the API check uses as its baseline.
        fixture.git(&["tag", fixture.prev_tag.as_str(), "-m", "release"]);
        fixture.git_at(
            &remote,
            &["init", "--quiet", "--bare", "--initial-branch=main"],
        );
        fixture.git(&["push", "--quiet", "-u", "origin", "main"]);
        fixture.git(&["push", "--quiet", "origin", fixture.prev_tag.as_str()]);
        fixture
    }

    fn base(&self) -> &Path {
        self._root.path()
    }

    fn write_layout(&self, layout: &Layout, prev_version: &str) {
        fs::write(self.repo.join("Cargo.lock"), "# sandbox lockfile\n").unwrap();
        match layout {
            Layout::Single => {
                fs::write(
                    self.repo.join("Cargo.toml"),
                    format!("[package]\nname = \"{SEMVER_PACKAGE}\"\nversion = \"{prev_version}\"\nedition = \"2021\"\n"),
                )
                .unwrap();
                fs::write(
                    self.repo.join("CHANGELOG.md"),
                    "# Changelog\n\n## Unreleased\n\n- Add a thing (#12)\n",
                )
                .unwrap();
            }
            Layout::Workspace => {
                fs::write(
                    self.repo.join("Cargo.toml"),
                    workspace_manifest(prev_version),
                )
                .unwrap();
                fs::create_dir_all(self.repo.join("lib")).unwrap();
                fs::create_dir_all(self.repo.join("bin")).unwrap();
                fs::write(
                    self.repo.join("lib/Cargo.toml"),
                    "[package]\nname = \"libplasmoid-updater\"\nversion.workspace = true\nedition.workspace = true\n",
                )
                .unwrap();
                fs::write(
                    self.repo.join("bin/Cargo.toml"),
                    "[package]\nname = \"plasmoid-updater\"\nversion.workspace = true\nedition.workspace = true\n\n[dependencies]\nlibplasmoid-updater = { workspace = true }\n",
                )
                .unwrap();
                // The production changelog is gitignored (`/docs` in the main
                // checkout's .git/info/exclude), so neither the initial commit
                // nor the release commit can carry it.
                fs::write(self.repo.join(".git/info/exclude"), "/docs\n").unwrap();
                fs::create_dir_all(self.repo.join("docs")).unwrap();
                fs::write(
                    self.repo.join("docs/RELEASE_NOTES.md"),
                    "# Release notes\n\n## Unreleased\n\n- Add a thing (#12)\n",
                )
                .unwrap();
            }
        }
    }

    fn set_manifest_version(&self, version: &str) {
        fs::write(self.repo.join("Cargo.toml"), workspace_manifest(version)).unwrap();
    }

    /// Puts a copy of the guard where the workflows call it from
    /// (`scripts/semver-guard.sh`, untracked).
    fn install_guard(&self) {
        let dir = self.repo.join("scripts");
        fs::create_dir_all(&dir).unwrap();
        install(&guard_script(), &dir.join("semver-guard.sh"));
        install(
            &repo_root().join("scripts").join(CRATES_IO_COPY),
            &dir.join(CRATES_IO_COPY),
        );
    }

    fn git_at(&self, dir: &Path, args: &[&str]) -> Output {
        let out = Command::new(system_program("git"))
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_clear()
            .env("PATH", BASE_PATH)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", self.home.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed: {}", text(&out));
        out
    }

    fn git(&self, args: &[&str]) -> Output {
        self.git_at(&self.repo, args)
    }

    fn git_stdout(&self, dir: &Path, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.git_at(dir, args).stdout).to_string()
    }

    fn latest_commit(&self) -> String {
        String::from_utf8_lossy(&self.git(&["log", "-1", "--format=%s"]).stdout)
            .trim()
            .to_string()
    }

    fn rev_parse(&self, rev: &str) -> String {
        String::from_utf8_lossy(&self.git(&["rev-parse", rev]).stdout)
            .trim()
            .to_string()
    }

    fn head(&self) -> String {
        self.rev_parse("HEAD")
    }

    fn remote_head(&self) -> String {
        String::from_utf8_lossy(&self.git_at(&self.remote, &["rev-parse", "main"]).stdout)
            .trim()
            .to_string()
    }

    fn tags(&self, dir: &Path) -> String {
        String::from_utf8_lossy(&self.git_at(dir, &["tag", "--list"]).stdout)
            .trim()
            .to_string()
    }

    fn delete_previous_tag(&self) {
        self.git(&["tag", "-d", self.prev_tag.as_str()]);
        self.git(&[
            "push",
            "--quiet",
            "origin",
            "--delete",
            self.prev_tag.as_str(),
        ]);
    }

    fn read(&self, path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    /// The one argv the fake tool recorded (a single check invocation).
    fn semver_argv(&self) -> String {
        self.read(&self.semver_log).trim().to_string()
    }

    /// Every argv line the fake tool recorded, in order (the patch-then-minor
    /// re-check records two).
    fn semver_argv_lines(&self) -> Vec<String> {
        self.read(&self.semver_log)
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The `+toolchain` each `cargo semver-checks` call carried, in order
    /// (an empty entry for a call without one).
    fn semver_toolchains(&self) -> Vec<String> {
        self.read(&self.cargo_log)
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Every rustup call recorded, in order.
    fn rustup_calls(&self) -> Vec<String> {
        self.read(&self.rustup_log)
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A command in the sandbox: a fake-first PATH, a sandboxed HOME/TMPDIR and
    /// git config, and no access to the real remote.
    fn sandbox(&self, program: PathBuf) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.repo)
            .env_clear()
            .env("HOME", &self.home)
            .env("TMPDIR", &self.tmp)
            .env("GIT_CONFIG_GLOBAL", self.home.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("FAKE_GH_LOG", &self.gh_log)
            .env("FAKE_SEMVER_LOG", &self.semver_log)
            .env("FAKE_CARGO_LOG", &self.cargo_log)
            .env("FAKE_RUSTUP_LOG", &self.rustup_log)
            .env("FAKE_PUBLISH_LOG", &self.publish_log);
        command
    }

    fn bash(&self, path: &str, script: &str) -> Output {
        self.sandbox(system_program("bash"))
            .arg("-c")
            .arg(script)
            .env("PATH", path)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn run(&self, path: &str, args: &[&str], semver_exit: u32) -> Output {
        self.run_with_env(path, args, &[("FAKE_SEMVER_EXIT", semver_exit.to_string())])
    }

    fn run_with_env(&self, path: &str, args: &[&str], envs: &[(&str, String)]) -> Output {
        let mut command = self.sandbox(system_program("bash"));
        command
            .arg(release_script())
            .args(args)
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        child.stdin.as_mut().unwrap().write_all(b"y\n").unwrap();
        child.wait_with_output().unwrap()
    }

    /// Runs a command line a refusal printed, verbatim, the way an operator
    /// pastes it (answering the plan's prompt with `y`).
    fn run_printed(&self, line: &str, envs: &[(&str, String)]) -> Output {
        let mut command = self.sandbox(system_program("bash"));
        command
            .arg("-c")
            .arg(line)
            .env("PATH", &self.path_with_tool)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        child.stdin.as_mut().unwrap().write_all(b"y\n").unwrap();
        child.wait_with_output().unwrap()
    }

    /// The guard run directly, in the repo, with the fake tool on PATH.
    fn guard(&self, args: &[&str], envs: &[(&str, String)]) -> Output {
        self.guard_in(&self.repo, args, envs)
    }

    fn guard_in(&self, dir: &Path, args: &[&str], envs: &[(&str, String)]) -> Output {
        let mut command = self.sandbox(system_program("bash"));
        command
            .arg(guard_script())
            .args(args)
            .current_dir(dir)
            .env("PATH", &self.path_with_tool)
            .stdin(Stdio::null());
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    /// A workflow step's script run the way the Actions runner runs a bash
    /// step, in the repo with the guard installed where the step calls it.
    fn run_step(&self, script: &str, envs: &[(&str, String)]) -> Output {
        self.install_guard();
        let mut command = self.sandbox(system_program("bash"));
        command
            .args(["--noprofile", "--norc", "-eo", "pipefail", "-c", script])
            .env("PATH", &self.path_with_tool)
            .stdin(Stdio::null());
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    /// Everything a refusal must leave alone: the worktree files (ignored ones
    /// too), the index, HEAD, every local ref, `.git/config`, the sandbox's
    /// global git config and the remote's refs.
    fn snapshot(&self) -> String {
        let mut files = Vec::new();
        collect_files(&self.repo, &self.repo, &mut files);
        files.sort();
        let mut out = String::new();
        for (path, bytes) in files {
            out += &format!("file {path}\n{}\n", String::from_utf8_lossy(&bytes));
        }
        out += &format!(
            "status\n{}stash\n{}head {}\nsymbolic {}index\n{}refs\n{}remote refs\n{}config\n{}global\n{}",
            self.git_stdout(&self.repo, &["status", "--porcelain"]),
            self.git_stdout(&self.repo, &["stash", "list"]),
            self.head(),
            self.git_stdout(&self.repo, &["symbolic-ref", "HEAD"]),
            self.git_stdout(&self.repo, &["ls-files", "--stage"]),
            self.git_stdout(&self.repo, &["for-each-ref"]),
            self.git_stdout(&self.remote, &["for-each-ref"]),
            self.read(&self.repo.join(".git/config")),
            self.read(&self.home.join(".gitconfig")),
        );
        out
    }
}

fn collect_files(dir: &Path, root: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().unwrap() == ".git" {
            continue;
        }
        if path.is_dir() {
            collect_files(&path, root, out);
        } else {
            out.push((
                path.strip_prefix(root).unwrap().display().to_string(),
                fs::read(&path).unwrap(),
            ));
        }
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Runs the re-run line a refusal printed, verbatim and with a check that
/// passes, and asserts it finishes the release of `version`.
fn assert_rerun_releases(fixture: &Fixture, line: &str, version: &str) {
    let out = fixture.run_printed(line, &[env("FAKE_SEMVER_EXIT", "0")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the printed re-run {line:?} failed:\n{}",
        text(&out)
    );
    assert!(
        text(&out).contains(&format!("Done: released v{version}")),
        "the printed re-run {line:?} did not release v{version}:\n{}",
        text(&out)
    );
}

/// The recovery commands a refusal prints to stderr, in order: the indented
/// lines that are not shell comments.
fn printed_commands(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|line| line.starts_with("  ") && !line.trim_start().starts_with('#'))
        .map(|line| line.trim().to_string())
        .collect()
}

/// The `Error:` lines of a stderr text, in order.
fn error_lines(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|line| line.starts_with("Error:"))
        .map(str::to_string)
        .collect()
}

fn porcelain(fixture: &Fixture) -> String {
    String::from_utf8_lossy(&fixture.git(&["status", "--porcelain"]).stdout)
        .trim()
        .to_string()
}

fn env(key: &'static str, value: &str) -> (&'static str, String) {
    (key, value.to_string())
}

/// The `run:` script (a `|` block or an inline command) of the step named
/// `step_name` in a workflow's text.
fn step_script(yaml: &str, step_name: &str) -> String {
    let marker = format!("- name: {step_name}");
    let mut lines = yaml.lines();
    lines
        .find(|line| line.trim() == marker)
        .unwrap_or_else(|| panic!("no step named {step_name:?}"));
    let run = lines
        .by_ref()
        .find(|line| {
            assert!(
                !line.trim_start().starts_with("- "),
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

fn workflow_step(workflow: &str, step_name: &str) -> String {
    let yaml = fs::read_to_string(repo_root().join(".github/workflows").join(workflow)).unwrap();
    let script = step_script(&yaml, step_name);
    assert!(
        script.contains("scripts/semver-guard.sh"),
        "{workflow} step {step_name:?} no longer calls the shared guard:\n{script}"
    );
    script
}

const VALIDATE_STEP: &str = "Check the library's API against its previous release tag";
const REPORT_STEP: &str = "release type the change needs";

/// Runs scripts/semver-guard.sh directly for the modes that need no repo.
fn run_guard(args: &[&str]) -> Output {
    Command::new(system_program("bash"))
        .arg(guard_script())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn a_compatible_release_proceeds() {
    let fixture = Fixture::new();
    let gh = fixture.bash(&fixture.path_with_tool, "command -v gh");
    assert_eq!(
        String::from_utf8_lossy(&gh.stdout).trim(),
        fixture.bin.join("gh").display().to_string(),
        "PATH does not resolve gh to the fixture"
    );

    let out = fixture.run(
        &fixture.path_with_tool,
        &["--notes", "CHANGELOG.md", VERSION],
        0,
    );
    let log = text(&out);

    assert_eq!(out.status.code(), Some(0), "the release failed:\n{log}");
    assert!(
        log.contains("Done: released v0.2.2"),
        "no completion line:\n{log}"
    );
    assert_eq!(fixture.latest_commit(), "chore: release v0.2.2");
    assert_eq!(fixture.tags(&fixture.repo), "v0.2.1\nv0.2.2");
    assert_eq!(fixture.tags(&fixture.remote), "v0.2.1\nv0.2.2");
    assert!(
        fixture
            .read(&fixture.gh_log)
            .contains("release create v0.2.2"),
        "no release was created:\n{log}"
    );
    assert_eq!(
        fixture.semver_argv(),
        "check-release -p libplasmoid-updater --baseline-rev v0.2.1 --release-type minor",
        "the check must run against the previous release tag with the type derived from 0.2.1 -> 0.2.2:\n{log}"
    );
    assert!(
        stdout_text(&out).lines().any(|line| line.trim_start_matches("Proceed? [y/N] ")
            == "Checking libplasmoid-updater's API against v0.2.1 as a minor release (a patch bump while below 1.0)"),
        "a 0.x patch bump is a minor release and the line must say why:\n{log}"
    );
}

#[test]
fn a_breaking_change_stops_before_any_change() {
    let fixture = Fixture::workspace();
    let notes = fixture.repo.join("docs/RELEASE_NOTES.md");
    assert!(
        String::from_utf8_lossy(&fixture.git(&["ls-files", "docs/RELEASE_NOTES.md"]).stdout)
            .is_empty(),
        "the fixture changelog must be untracked for this case to cover an ignored changelog left untouched by a refusal"
    );
    let before_head = fixture.head();
    let before_remote_head = fixture.remote_head();
    let before_changelog = fixture.read(&notes);
    let before = fixture.snapshot();

    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        100,
    );
    let log = text(&out);
    let stderr = stderr_text(&out);

    assert_ne!(
        out.status.code(),
        Some(0),
        "the release proceeded despite a deny-level semver violation:\n{log}"
    );
    assert!(
        stderr.contains("major version bump"),
        "the refusal does not name the needed release type:\n{log}"
    );
    assert_eq!(
        error_lines(&stderr),
        vec![
            "Error: libplasmoid-updater's public API change needs a major version bump (a minor one while below 1.0)"
                .to_string()
        ],
        "{log}"
    );

    let script = release_script().display().to_string();
    let commands = printed_commands(&stderr);
    assert_eq!(
        commands,
        vec![format!(
            "{script} --workspace --notes docs/RELEASE_NOTES.md 0.3.0"
        )],
        "a refused check must print only the re-run line with the version that announces the break, never a reset or undo command:\n{log}"
    );

    assert_eq!(
        porcelain(&fixture),
        "",
        "the check refused after mutating the tree:\n{log}"
    );
    assert_eq!(fixture.head(), before_head, "HEAD moved:\n{log}");
    assert_eq!(
        fixture.tags(&fixture.repo),
        "v0.2.1",
        "a local ref changed:\n{log}"
    );
    assert_eq!(
        fixture.tags(&fixture.remote),
        "v0.2.1",
        "the bare remote changed:\n{log}"
    );
    assert_eq!(
        fixture.remote_head(),
        before_remote_head,
        "the bare remote moved:\n{log}"
    );
    assert_eq!(
        fixture.read(&notes),
        before_changelog,
        "the gitignored changelog changed during a refused run"
    );
    assert_eq!(
        fixture.semver_argv(),
        "check-release -p libplasmoid-updater --baseline-rev v0.2.1 --release-type minor",
        "the check must run against the previous release tag:\n{log}"
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal changed the tree, a ref, .git/config or the remote:\n{log}"
    );
    assert_rerun_releases(&fixture, &commands[0], "0.3.0");
}

#[test]
fn a_could_not_complete_check_stops_and_reruns_the_same_command() {
    let fixture = Fixture::workspace();
    let before_head = fixture.head();
    let before_remote_head = fixture.remote_head();
    let before = fixture.snapshot();

    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        101,
    );
    let log = text(&out);
    let stderr = stderr_text(&out);

    assert_ne!(
        out.status.code(),
        Some(0),
        "the release proceeded despite a check that could not complete:\n{log}"
    );
    assert!(
        stderr.contains("could not complete"),
        "the refusal does not say the check could not complete:\n{log}"
    );
    assert!(
        stderr.contains("cargo-semver-checks exited 101"),
        "the refusal does not name the exit code:\n{log}"
    );
    assert!(
        stderr.contains("did not start"),
        "the refusal does not say the release did not start:\n{log}"
    );

    let script = release_script().display().to_string();
    let commands = printed_commands(&stderr);
    assert_eq!(
        commands,
        vec![format!(
            "{script} --workspace --notes docs/RELEASE_NOTES.md 0.2.2"
        )],
        "a non-100 exit must print the same command to re-run (same version), never a different version:\n{log}"
    );

    assert_eq!(porcelain(&fixture), "", "the tree changed:\n{log}");
    assert_eq!(fixture.head(), before_head, "HEAD moved:\n{log}");
    assert_eq!(
        fixture.tags(&fixture.repo),
        "v0.2.1",
        "a local ref changed:\n{log}"
    );
    assert_eq!(
        fixture.tags(&fixture.remote),
        "v0.2.1",
        "the bare remote changed:\n{log}"
    );
    assert_eq!(
        fixture.remote_head(),
        before_remote_head,
        "remote moved:\n{log}"
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal changed the tree, a ref, .git/config or the remote:\n{log}"
    );
    assert_rerun_releases(&fixture, &commands[0], VERSION);
}

#[test]
fn a_missing_semver_tool_refuses_before_any_change() {
    let fixture = Fixture::new();
    let before_head = fixture.head();
    let before = fixture.snapshot();

    let out = fixture.run(
        &fixture.path_without_tool,
        &["--notes", "CHANGELOG.md", VERSION],
        0,
    );
    let log = text(&out);

    assert_ne!(
        out.status.code(),
        Some(0),
        "the release proceeded without the tool:\n{log}"
    );
    assert!(
        log.contains("cargo install cargo-semver-checks --version 0.51.0 --locked"),
        "the refusal does not pin the install command to CI's version:\n{log}"
    );
    assert_eq!(porcelain(&fixture), "", "the tree changed:\n{log}");
    assert_eq!(fixture.head(), before_head, "HEAD moved:\n{log}");
    assert_eq!(
        fixture.tags(&fixture.repo),
        "v0.2.1",
        "a local ref changed:\n{log}"
    );
    assert_eq!(fixture.tags(&fixture.remote), "v0.2.1");
    assert!(!fixture.semver_log.exists(), "the tool was invoked:\n{log}");
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );
}

#[test]
fn a_retry_after_a_tag_push_runs_the_check_and_proceeds() {
    let fixture = Fixture::workspace();
    // The state release.yml's failure advice points operators at: the tag is on
    // origin but the release creation never finished.
    fs::write(fixture.repo.join("Cargo.toml"), workspace_manifest(VERSION)).unwrap();
    fs::write(
        fixture.repo.join("docs/RELEASE_NOTES.md"),
        "# Release notes\n\n## 0.2.2\n\n- Add a thing (#12)\n",
    )
    .unwrap();
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "--quiet", "-m", "chore: release v0.2.2"]);
    fixture.git(&["push", "--quiet", "-u", "origin", "main"]);
    fixture.git(&["tag", "v0.2.2", "-m", "release v0.2.2"]);
    fixture.git(&["push", "--quiet", "origin", "v0.2.2"]);

    let out = fixture.run(
        &fixture.path_with_tool,
        &[
            "--retry",
            "--workspace",
            "--notes",
            "docs/RELEASE_NOTES.md",
            VERSION,
        ],
        0,
    );
    let log = text(&out);

    assert_eq!(
        out.status.code(),
        Some(0),
        "the retry did not complete the release:\n{log}"
    );
    assert!(
        !log.contains("Skipping libplasmoid-updater's API check"),
        "the check must not be skipped for a published tag; --retry re-runs it:\n{log}"
    );
    assert_eq!(
        fixture.semver_argv(),
        "check-release -p libplasmoid-updater --baseline-rev v0.2.1 --release-type minor",
        "the retry must re-run the check against the same baseline:\n{log}"
    );
    assert!(
        log.contains("Done: released v0.2.2"),
        "the release did not finish after the re-run check:\n{log}"
    );
}

#[test]
fn a_missing_previous_tag_refuses() {
    let fixture = Fixture::workspace();
    fixture.delete_previous_tag();
    let before_head = fixture.head();
    let before = fixture.snapshot();

    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    let log = text(&out);

    assert_ne!(
        out.status.code(),
        Some(0),
        "the release proceeded without a baseline:\n{log}"
    );
    assert!(
        log.contains("no previous v* release tag"),
        "the refusal does not name the missing baseline:\n{log}"
    );
    assert_eq!(porcelain(&fixture), "", "the tree changed:\n{log}");
    assert_eq!(fixture.head(), before_head, "HEAD moved:\n{log}");
    assert!(!fixture.semver_log.exists(), "the tool was invoked:\n{log}");
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );
}

#[test]
fn a_one_point_x_patch_release_checks_as_patch() {
    let fixture = Fixture::workspace_at("1.4.2");
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", "1.4.3"],
        0,
    );
    let log = text(&out);
    assert_eq!(out.status.code(), Some(0), "the release failed:\n{log}");
    assert_eq!(
        fixture.semver_argv(),
        "check-release -p libplasmoid-updater --baseline-rev v1.4.2 --release-type patch",
        "a 1.x patch release must be checked as a patch:\n{log}"
    );
}

#[test]
fn a_one_point_x_minor_release_checks_as_minor() {
    let fixture = Fixture::workspace_at("1.4.2");
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", "1.5.0"],
        0,
    );
    let log = text(&out);
    assert_eq!(out.status.code(), Some(0), "the release failed:\n{log}");
    assert_eq!(
        fixture.semver_argv(),
        "check-release -p libplasmoid-updater --baseline-rev v1.4.2 --release-type minor",
        "a 1.x minor release must be checked as a minor:\n{log}"
    );
    assert!(
        stdout_text(&out)
            .lines()
            .any(|line| line.trim_start_matches("Proceed? [y/N] ")
                == "Checking libplasmoid-updater's API against v1.4.2 as a minor release"),
        "above 1.0 a minor release needs no gloss:\n{log}"
    );
}

#[test]
fn a_one_point_x_major_release_checks_as_major() {
    let fixture = Fixture::workspace_at("1.4.2");
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", "2.0.0"],
        0,
    );
    let log = text(&out);
    assert_eq!(out.status.code(), Some(0), "the release failed:\n{log}");
    assert_eq!(
        fixture.semver_argv(),
        "check-release -p libplasmoid-updater --baseline-rev v1.4.2 --release-type major",
        "a 1.x major release must be checked as a major:\n{log}"
    );
}

#[test]
fn a_patch_release_needing_a_minor_bump_prints_the_next_minor() {
    let fixture = Fixture::workspace_at("1.4.2");
    let before_head = fixture.head();
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", "1.4.3"],
        &[
            ("FAKE_SEMVER_EXIT", "100".to_string()),
            ("FAKE_SEMVER_EXIT_PATCH", "100".to_string()),
            ("FAKE_SEMVER_EXIT_MINOR", "0".to_string()),
        ],
    );
    let log = text(&out);
    let stderr = stderr_text(&out);
    assert_ne!(
        out.status.code(),
        Some(0),
        "the release proceeded despite the deny-level violation:\n{log}"
    );
    assert!(
        stderr.contains("needs a minor version bump"),
        "the refusal does not name a minor bump:\n{log}"
    );
    assert_eq!(
        error_lines(&stderr),
        vec![
            "Error: libplasmoid-updater's public API change needs a minor version bump".to_string()
        ],
        "{log}"
    );
    let script = release_script().display().to_string();
    assert_eq!(
        printed_commands(&stderr),
        vec![format!(
            "{script} --workspace --notes docs/RELEASE_NOTES.md 1.5.0"
        )],
        "the re-run line must carry the next minor, not the next major:\n{log}"
    );
    assert_eq!(porcelain(&fixture), "", "the tree changed:\n{log}");
    assert_eq!(fixture.head(), before_head, "HEAD moved:\n{log}");
    assert_eq!(
        fixture.semver_argv_lines(),
        vec![
            "check-release -p libplasmoid-updater --baseline-rev v1.4.2 --release-type patch"
                .to_string(),
            "check-release -p libplasmoid-updater --baseline-rev v1.4.2 --release-type minor"
                .to_string(),
        ],
        "the two-step must re-check as a minor against the same baseline:\n{log}"
    );
    assert_rerun_releases(&fixture, &printed_commands(&stderr)[0], "1.5.0");
}

#[test]
fn a_patch_release_needing_a_major_bump_prints_the_next_major() {
    let fixture = Fixture::workspace_at("1.4.2");
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", "1.4.3"],
        &[
            ("FAKE_SEMVER_EXIT", "100".to_string()),
            ("FAKE_SEMVER_EXIT_PATCH", "100".to_string()),
            ("FAKE_SEMVER_EXIT_MINOR", "100".to_string()),
        ],
    );
    let log = text(&out);
    let stderr = stderr_text(&out);
    assert_ne!(out.status.code(), Some(0), "the release proceeded:\n{log}");
    assert!(
        stderr.contains("needs a major version bump"),
        "the refusal does not name a major bump:\n{log}"
    );
    assert_eq!(
        error_lines(&stderr),
        vec![
            "Error: libplasmoid-updater's public API change needs a major version bump".to_string()
        ],
        "{log}"
    );
    let script = release_script().display().to_string();
    assert_eq!(
        printed_commands(&stderr),
        vec![format!(
            "{script} --workspace --notes docs/RELEASE_NOTES.md 2.0.0"
        )],
        "the re-run line must carry the next major:\n{log}"
    );
    assert_eq!(
        fixture.semver_argv_lines(),
        vec![
            "check-release -p libplasmoid-updater --baseline-rev v1.4.2 --release-type patch"
                .to_string(),
            "check-release -p libplasmoid-updater --baseline-rev v1.4.2 --release-type minor"
                .to_string(),
        ],
        "the two-step must re-check as a minor:\n{log}"
    );
    assert_rerun_releases(&fixture, &printed_commands(&stderr)[0], "2.0.0");
}

#[test]
fn a_patch_release_whose_minor_recheck_could_not_complete_reruns_the_same_command() {
    let fixture = Fixture::workspace_at("1.4.2");
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", "1.4.3"],
        &[
            ("FAKE_SEMVER_EXIT", "100".to_string()),
            ("FAKE_SEMVER_EXIT_PATCH", "100".to_string()),
            ("FAKE_SEMVER_EXIT_MINOR", "101".to_string()),
        ],
    );
    let log = text(&out);
    let stderr = stderr_text(&out);
    assert_ne!(out.status.code(), Some(0), "the release proceeded:\n{log}");
    assert!(
        stderr.contains("could not complete") && stderr.contains("cargo-semver-checks exited 101"),
        "the refusal does not name the incomplete re-check:\n{log}"
    );
    let script = release_script().display().to_string();
    assert_eq!(
        printed_commands(&stderr),
        vec![format!(
            "{script} --workspace --notes docs/RELEASE_NOTES.md 1.4.3"
        )],
        "an incomplete re-check must re-run the same command:\n{log}"
    );
    assert_rerun_releases(&fixture, &printed_commands(&stderr)[0], "1.4.3");
}

#[test]
fn a_baseline_tag_without_the_crate_names_the_baseline() {
    let fixture = Fixture::workspace();
    // Re-point the baseline tag at a tree without the lib crate: the check
    // cannot run against it, and the refusal must say so instead of advising a
    // re-run that can never pass.
    fixture.git(&["rm", "--quiet", "lib/Cargo.toml"]);
    fixture.git(&["commit", "--quiet", "-m", "drop the lib crate"]);
    fixture.git(&["tag", "--force", fixture.prev_tag.as_str(), "-m", "release"]);
    fixture.git(&[
        "push",
        "--quiet",
        "--force",
        "origin",
        fixture.prev_tag.as_str(),
    ]);
    let before = fixture.snapshot();
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    let log = text(&out);
    assert_ne!(out.status.code(), Some(0), "the release proceeded:\n{log}");
    assert!(
        log.contains("has no libplasmoid-updater manifest"),
        "the refusal does not name the baseline lacking the crate:\n{log}"
    );
    assert_eq!(porcelain(&fixture), "", "the tree changed:\n{log}");
    assert!(!fixture.semver_log.exists(), "the check ran:\n{log}");
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );
}

#[test]
fn a_lockfile_naming_the_crate_is_not_a_manifest() {
    let fixture = Fixture::workspace();
    // The crate is only a locked dependency at the baseline: the lockfile names
    // it, no Cargo.toml does.
    fixture.git(&["rm", "--quiet", "lib/Cargo.toml"]);
    fs::write(
        fixture.repo.join("Cargo.lock"),
        "[[package]]\nname = \"libplasmoid-updater\"\nversion = \"0.2.1\"\n",
    )
    .unwrap();
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "--quiet", "-m", "crate only in the lockfile"]);
    fixture.git(&["tag", "--force", fixture.prev_tag.as_str(), "-m", "release"]);
    fixture.git(&[
        "push",
        "--quiet",
        "--force",
        "origin",
        fixture.prev_tag.as_str(),
    ]);
    let before = fixture.snapshot();
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    let log = text(&out);
    assert_ne!(out.status.code(), Some(0), "the release proceeded:\n{log}");
    assert_eq!(
        error_lines(&stderr_text(&out)),
        vec![
            "Error: the baseline tag v0.2.1 has no libplasmoid-updater manifest (the crate did not exist at that tag); the API check cannot run against it"
                .to_string()
        ],
        "a lockfile entry must not satisfy the manifest question:\n{log}"
    );
    assert!(!fixture.semver_log.exists(), "the check ran:\n{log}");
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );
}

#[test]
fn a_git_error_reading_the_baseline_tree_is_not_a_missing_crate() {
    let fixture = Fixture::workspace();
    let before = fixture.snapshot();
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &WORKSPACE_ARGS
            .iter()
            .copied()
            .chain([VERSION])
            .collect::<Vec<_>>(),
        &[
            env("FAKE_SEMVER_EXIT", "0"),
            env("FAKE_GIT_FAIL_CMD", "grep"),
            env("FAKE_GIT_FAIL_RC", "128"),
        ],
    );
    let log = text(&out);
    assert_ne!(out.status.code(), Some(0), "the release proceeded:\n{log}");
    assert_eq!(
        error_lines(&stderr_text(&out)),
        vec![
            "Error: could not read the tree of the baseline tag v0.2.1 (git grep exited 128); cannot tell whether it has a libplasmoid-updater manifest"
                .to_string()
        ],
        "a git failure must not read as a crate that did not exist:\n{log}"
    );
    assert!(!fixture.semver_log.exists(), "the check ran:\n{log}");
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );
}

#[test]
fn a_failing_origin_tag_listing_stops_with_one_message() {
    let fixture = Fixture::workspace();
    let before = fixture.snapshot();
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &WORKSPACE_ARGS
            .iter()
            .copied()
            .chain([VERSION])
            .collect::<Vec<_>>(),
        &[
            env("FAKE_SEMVER_EXIT", "0"),
            env("FAKE_GIT_FAIL_CMD", "ls-remote"),
        ],
    );
    let log = text(&out);
    assert_ne!(out.status.code(), Some(0), "the release proceeded:\n{log}");
    assert_eq!(
        error_lines(&stderr_text(&out)),
        vec![
            "Error: git ls-remote --tags origin failed; cannot tell whether v0.2.2 is already tagged, so the release did not start; nothing was pushed"
                .to_string()
        ],
        "a failing origin listing must stop with exactly one message, never read as no previous tag:\n{log}"
    );
    assert!(
        !stdout_text(&out).contains("Release plan"),
        "the refusal must come before the plan and its prompt:\n{log}"
    );
    assert!(!fixture.semver_log.exists(), "the tool was invoked:\n{log}");
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );
}

#[test]
fn a_retry_whose_origin_tag_listing_fails_stops_with_its_own_message() {
    let fixture = Fixture::workspace();
    let before = fixture.snapshot();
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &[
            "--retry",
            "--workspace",
            "--notes",
            "docs/RELEASE_NOTES.md",
            VERSION,
        ],
        &[
            env("FAKE_SEMVER_EXIT", "0"),
            env("FAKE_GIT_FAIL_CMD", "ls-remote"),
        ],
    );
    let log = text(&out);
    assert_eq!(out.status.code(), Some(1), "{log}");
    assert_eq!(
        error_lines(&stderr_text(&out)),
        vec![
            "Error: git ls-remote --tags origin failed; cannot tell whether v0.2.2 names HEAD, so --retry did not start; nothing was pushed"
                .to_string()
        ],
        "{log}"
    );
    assert!(
        !stdout_text(&out).contains("Release plan"),
        "the refusal must come before the plan and its prompt:\n{log}"
    );
    assert!(!fixture.semver_log.exists(), "the tool was invoked:\n{log}");
    assert_eq!(fixture.snapshot(), before, "{log}");
}

#[test]
fn a_failing_ancestry_probe_stops_with_one_message() {
    let fixture = Fixture::workspace();
    let before = fixture.snapshot();
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &WORKSPACE_ARGS
            .iter()
            .copied()
            .chain([VERSION])
            .collect::<Vec<_>>(),
        &[
            env("FAKE_SEMVER_EXIT", "0"),
            env("FAKE_GIT_FAIL_CMD", "merge-base"),
            env("FAKE_GIT_FAIL_RC", "128"),
        ],
    );
    let log = text(&out);
    assert_ne!(out.status.code(), Some(0), "the release proceeded:\n{log}");
    assert_eq!(
        error_lines(&stderr_text(&out)),
        vec![
            "Error: git merge-base failed (exit 128) for tag v0.2.1; cannot determine the API check's baseline"
                .to_string()
        ],
        "an ancestry error (not the plain 'not an ancestor' status) must stop the run:\n{log}"
    );
    assert!(!fixture.semver_log.exists(), "the tool was invoked:\n{log}");
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );
}

#[test]
fn the_release_check_runs_against_the_newest_reachable_origin_tag_below_the_version() {
    let fixture = Fixture::crowded();
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", "0.3.0"],
        0,
    );
    let log = text(&out);
    assert_eq!(out.status.code(), Some(0), "the release failed:\n{log}");
    assert_eq!(
        fixture.semver_argv(),
        "check-release -p libplasmoid-updater --baseline-rev v0.2.10 --release-type major",
        "the baseline must be the newest tag below 0.3.0 that HEAD contains (not the oldest, not the lexically largest, not v0.2.11 on the maintenance commit, not v1.0.0):\n{log}"
    );
}

#[test]
fn release_sh_and_the_release_workflow_pick_the_same_baseline() {
    let script_side = Fixture::crowded();
    let out = script_side.run(
        &script_side.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", "0.3.0"],
        0,
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the release failed:\n{}",
        text(&out)
    );

    let workflow_side = Fixture::crowded();
    workflow_side.set_manifest_version("0.3.0");
    let step = workflow_step("release.yml", VALIDATE_STEP);
    let out = workflow_side.run_step(
        &step,
        &[
            env("GITHUB_REF_NAME", "v0.3.0"),
            env("FAKE_SEMVER_EXIT", "0"),
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the validate step failed:\n{}",
        text(&out)
    );

    assert_eq!(
        workflow_side.semver_argv(),
        script_side.semver_argv(),
        "release.sh and the workflow must run the check against the same baseline with the same type"
    );
    assert_eq!(
        workflow_side.semver_argv(),
        "check-release -p libplasmoid-updater --baseline-rev v0.2.10 --release-type major"
    );
}

#[test]
fn a_baseline_commit_missing_locally_refuses_and_names_the_fetch() {
    let fixture = Fixture::with_remote_only_tag("v0.2.5");
    let before = fixture.snapshot();
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", "0.3.0"],
        0,
    );
    let log = text(&out);
    let stderr = stderr_text(&out);
    assert_ne!(out.status.code(), Some(0), "the release proceeded:\n{log}");
    assert_eq!(
        error_lines(&stderr),
        vec![
            "Error: origin's tag v0.2.5 points at a commit this clone does not have, so whether it is reachable from HEAD cannot be told; fetch the tags, then re-run:"
                .to_string()
        ],
        "the refusal must name the tag whose commit is absent:\n{log}"
    );
    let script = release_script().display().to_string();
    let commands = printed_commands(&stderr);
    assert_eq!(
        commands,
        vec![
            "git fetch --tags origin".to_string(),
            format!("{script} --workspace --notes docs/RELEASE_NOTES.md 0.3.0"),
        ],
        "the refusal must print the fetch and then the same command:\n{log}"
    );
    assert!(!fixture.semver_log.exists(), "the tool was invoked:\n{log}");
    assert_eq!(
        fixture.snapshot(),
        before,
        "the script fetched or changed something itself:\n{log}"
    );

    let fetch = fixture.run_printed(&commands[0], &[]);
    assert_eq!(
        fetch.status.code(),
        Some(0),
        "the printed fetch failed:\n{}",
        text(&fetch)
    );
    let rerun = fixture.run_printed(&commands[1], &[env("FAKE_SEMVER_EXIT", "0")]);
    assert_eq!(
        rerun.status.code(),
        Some(0),
        "the printed re-run failed:\n{}",
        text(&rerun)
    );
    assert_eq!(
        fixture.semver_argv(),
        "check-release -p libplasmoid-updater --baseline-rev v0.2.1 --release-type major",
        "after the fetch the other clone's commit is known and unmerged, so v0.2.1 is the baseline:\n{}",
        text(&rerun)
    );
}

#[test]
fn a_reachable_origin_tag_missing_locally_refuses_and_names_the_fetch() {
    let fixture = Fixture::with_remote_tag_on_known_commit("v0.2.5");
    let before = fixture.snapshot();
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", "0.3.0"],
        0,
    );
    let log = text(&out);
    let stderr = stderr_text(&out);
    assert_ne!(out.status.code(), Some(0), "the release proceeded:\n{log}");
    assert_eq!(
        error_lines(&stderr),
        vec![
            "Error: origin's tag v0.2.5 is not a tag in this clone, and the API check resolves the baseline by tag name; fetch the tags, then re-run:"
                .to_string()
        ],
        "{log}"
    );
    let script = release_script().display().to_string();
    let commands = printed_commands(&stderr);
    assert_eq!(
        commands,
        vec![
            "git fetch --tags origin".to_string(),
            format!("{script} --workspace --notes docs/RELEASE_NOTES.md 0.3.0"),
        ],
        "{log}"
    );
    assert!(!fixture.semver_log.exists(), "the tool was invoked:\n{log}");
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );

    let fetch = fixture.run_printed(&commands[0], &[]);
    assert_eq!(fetch.status.code(), Some(0), "{}", text(&fetch));
    let rerun = fixture.run_printed(&commands[1], &[env("FAKE_SEMVER_EXIT", "0")]);
    assert_eq!(rerun.status.code(), Some(0), "{}", text(&rerun));
    assert_eq!(
        fixture.semver_argv(),
        "check-release -p libplasmoid-updater --baseline-rev v0.2.5 --release-type major",
        "{}",
        text(&rerun)
    );
}

#[test]
fn a_local_tag_naming_another_commit_refuses_and_names_the_forced_fetch() {
    let fixture = Fixture::workspace();
    fixture.git(&["tag", "v0.2.5", "-m", "release"]);
    fixture.git(&["push", "--quiet", "origin", "v0.2.5"]);
    fixture.git(&["commit", "--quiet", "--allow-empty", "-m", "second"]);
    fixture.git(&["tag", "--force", "v0.2.5", "-m", "moved"]);
    let before = fixture.snapshot();
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", "0.3.0"],
        0,
    );
    let log = text(&out);
    let stderr = stderr_text(&out);
    assert_ne!(out.status.code(), Some(0), "the release proceeded:\n{log}");
    assert_eq!(
        error_lines(&stderr),
        vec![
            "Error: this clone's tag v0.2.5 points at a different commit than origin's, and the API check resolves the baseline by tag name; take origin's, then re-run:"
                .to_string()
        ],
        "{log}"
    );
    let script = release_script().display().to_string();
    let commands = printed_commands(&stderr);
    assert_eq!(
        commands,
        vec![
            "git fetch --force --tags origin".to_string(),
            format!("{script} --workspace --notes docs/RELEASE_NOTES.md 0.3.0"),
        ],
        "{log}"
    );
    assert!(!fixture.semver_log.exists(), "the tool was invoked:\n{log}");
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );

    let fetch = fixture.run_printed(&commands[0], &[]);
    assert_eq!(fetch.status.code(), Some(0), "{}", text(&fetch));
    let rerun = fixture.run_printed(&commands[1], &[env("FAKE_SEMVER_EXIT", "0")]);
    assert_eq!(rerun.status.code(), Some(0), "{}", text(&rerun));
    assert_eq!(
        fixture.semver_argv(),
        "check-release -p libplasmoid-updater --baseline-rev v0.2.5 --release-type major",
        "{}",
        text(&rerun)
    );
}

#[test]
fn a_shallow_clone_refuses_and_names_the_deepening_fetch() {
    let fixture = Fixture::workspace();
    fixture.git(&["commit", "--quiet", "--allow-empty", "-m", "second"]);
    fixture.git(&["push", "--quiet", "origin", "main"]);
    let url = format!("file://{}", fixture.remote.display());
    fixture.git_at(
        fixture.base(),
        &["clone", "--quiet", "--depth", "1", &url, "shallow"],
    );
    let shallow = fixture.base().join("shallow");

    let out = fixture.guard_in(&shallow, &["baseline", VERSION], &[]);
    let stderr = stderr_text(&out);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert_eq!(
        printed_commands(&stderr),
        vec!["git fetch --unshallow --tags origin".to_string()],
        "the refusal must name the fetch that deepens the clone:\n{}",
        text(&out)
    );

    let fetch = fixture.bash(
        &fixture.path_with_tool,
        &format!(
            "cd {} && git fetch --unshallow --tags origin",
            shallow.display()
        ),
    );
    assert_eq!(fetch.status.code(), Some(0), "{}", text(&fetch));
    let out = fixture.guard_in(&shallow, &["baseline", VERSION], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(stdout_text(&out).trim(), "v0.2.1");
}

#[test]
fn the_shared_baseline_rule_selects_by_version_and_reachability() {
    let fixture = Fixture::crowded();
    let cases = [
        ("0.3.0", "below", "v0.2.10"),
        ("0.2.10", "below", "v0.2.9"),
        ("0.2.10", "upto", "v0.2.10"),
        ("0.2.12", "upto", "v0.2.10"),
        ("0.2.1", "below", "v0.2.0"),
        ("0.2.1", "upto", "v0.2.1"),
        ("0.2.0", "upto", "v0.2.0"),
        ("9.0.0", "below", "v1.0.0"),
    ];
    for (requested, bound, expected) in cases {
        let out = fixture.guard(&["baseline", requested, bound], &[]);
        assert_eq!(
            stdout_text(&out).trim(),
            expected,
            "baseline {requested} {bound}:\n{}",
            text(&out)
        );
        assert_eq!(out.status.code(), Some(0), "baseline {requested} {bound}");
    }
    let none = fixture.guard(&["baseline", "0.1.9"], &[]);
    assert_eq!(none.status.code(), Some(4), "{}", text(&none));
    assert_eq!(stdout_text(&none), "");
    assert_eq!(
        error_lines(&stderr_text(&none)),
        vec![
            "Error: no previous v* release tag on origin below v0.1.9 is reachable from HEAD; the API check has no baseline, and a release without one is refused"
                .to_string()
        ]
    );
}

#[test]
fn the_check_mode_forwards_the_tool_status() {
    let fixture = Fixture::workspace();
    for exit in [0, 100, 101] {
        let out = fixture.guard(
            &["check", "v0.2.1", "minor"],
            &[env("FAKE_SEMVER_EXIT", &exit.to_string())],
        );
        assert_eq!(
            out.status.code(),
            Some(exit),
            "check must forward the tool's exit status:\n{}",
            text(&out)
        );
    }
    assert_eq!(
        fixture.semver_argv_lines(),
        vec![
            "check-release -p libplasmoid-updater --baseline-rev v0.2.1 --release-type minor"
                .to_string();
            3
        ],
        "every run must reach the tool with the baseline and the type"
    );
}

#[test]
fn a_retry_refusal_for_a_bigger_bump_prints_a_line_that_passes_the_existing_release_guard() {
    let fixture = Fixture::workspace();
    let before = fixture.snapshot();
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &[
            "--retry",
            "--workspace",
            "--notes",
            "docs/RELEASE_NOTES.md",
            VERSION,
        ],
        &[
            env("FAKE_SEMVER_EXIT", "100"),
            env("FAKE_SEMVER_EXIT_MAJOR", "0"),
        ],
    );
    let log = text(&out);
    assert_ne!(out.status.code(), Some(0), "the release proceeded:\n{log}");
    let script = release_script().display().to_string();
    let commands = printed_commands(&stderr_text(&out));
    assert_eq!(
        commands,
        vec![format!(
            "{script} --workspace --notes docs/RELEASE_NOTES.md 0.3.0"
        )],
        "a different version is a different release: the line must drop --retry:\n{log}"
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );

    let refused = fixture.run_printed(&commands[0], &[env("FAKE_GH_RELEASES", "v0.3.0")]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused));
    assert!(
        text(&refused).contains("github release v0.3.0 already exists"),
        "the printed line must reach the existing-release guard:\n{}",
        text(&refused)
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "the guard refusal mutated state"
    );

    let done = fixture.run_printed(&commands[0], &[env("FAKE_SEMVER_EXIT", "0")]);
    assert_eq!(done.status.code(), Some(0), "{}", text(&done));
    assert!(
        text(&done).contains("Done: released v0.3.0"),
        "{}",
        text(&done)
    );
}

const DIRTY_HEADER: &str =
    "# Error: working tree or index not clean; the release commit stages the whole index:\n";
const DIRTY_ADVICE: &str = "# Inspect them:\n  git diff HEAD\n# Commit what you want to keep (notes written in place included), discard the rest yourself, then re-run:\n";

/// Git subcommands that discard, hide or move work aside.
const DISCARDING_SUBCOMMANDS: [&str; 5] = ["checkout", "stash", "reset", "clean", "restore"];

/// The printed lines that run a discarding git subcommand.
fn discarding_commands(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| {
            let mut words = line.split_whitespace();
            words.next() == Some("git") && words.any(|word| DISCARDING_SUBCOMMANDS.contains(&word))
        })
        .cloned()
        .collect()
}

#[test]
fn the_discarding_command_scan_flags_what_it_must() {
    let flagged = [
        "git checkout HEAD -- Cargo.toml",
        "git stash",
        "git reset --hard",
        "git clean -fd",
        "git restore .",
    ]
    .map(str::to_string);
    assert_eq!(discarding_commands(&flagged), flagged);
    let clean = [
        "git diff HEAD",
        "git fetch --tags origin",
        "M Cargo.toml",
        "/x/release.sh --retry --workspace 0.2.2",
    ]
    .map(str::to_string);
    assert_eq!(discarding_commands(&clean), Vec::<String>::new());
}

#[test]
fn a_dirty_bump_refuses_with_inspect_and_commit_advice_and_follows_it() {
    let fixture = Fixture::workspace();
    // The state a failure between the bump and the commit leaves.
    fixture.set_manifest_version(VERSION);
    let before = fixture.snapshot();
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    let log = text(&out);
    assert_eq!(out.status.code(), Some(1), "{log}");
    let script = release_script().display().to_string();
    let typed = format!("{script} --workspace --notes docs/RELEASE_NOTES.md 0.2.2");
    let commands = printed_commands(&stderr_text(&out));
    assert_eq!(
        commands.last(),
        Some(&typed),
        "the re-run line is the invocation as typed, never with --retry added:\n{log}"
    );
    assert_eq!(
        stderr_text(&out),
        format!("{DIRTY_HEADER}  # Cargo.toml\n{DIRTY_ADVICE}  {typed}\n"),
        "{log}"
    );
    assert_eq!(discarding_commands(&commands), Vec::<String>::new());
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );

    // follow the advice: inspect, keep the bump by committing it, re-run; the
    // typed line meets the "already X" error, which names --retry
    let diff = fixture.run_printed(&commands[0], &[]);
    assert_eq!(diff.status.code(), Some(0), "{}", text(&diff));
    fixture.git(&["commit", "--quiet", "--all", "-m", "keep the bump"]);
    let committed = fixture.snapshot();
    let refused = fixture.run_printed(&commands[1], &[env("FAKE_SEMVER_EXIT", "0")]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused));
    assert_eq!(
        error_lines(&stdout_text(&refused)),
        vec!["Error: version is already 0.2.2; bump to a higher version, or re-run with --retry if an earlier run of 0.2.2 stopped part-way".to_string()],
        "{}",
        text(&refused)
    );
    assert_eq!(
        fixture.snapshot(),
        committed,
        "the refusal mutated state:\n{}",
        text(&refused)
    );
    assert_rerun_releases(
        &fixture,
        &format!("{script} --retry --workspace --notes docs/RELEASE_NOTES.md 0.2.2"),
        VERSION,
    );
}

#[test]
fn a_dirty_tree_after_a_released_version_prints_the_typed_line_and_never_publishes_the_edit() {
    let fixture = Fixture::workspace();
    let args = ["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION];
    let released = fixture.run(&fixture.path_with_tool, &args, 0);
    assert_eq!(released.status.code(), Some(0), "{}", text(&released));
    // the tag is on origin, no github release is visible, and an edit is left
    assert_eq!(fixture.tags(&fixture.remote), "v0.2.1\nv0.2.2");
    fs::write(
        fixture.repo.join("Cargo.lock"),
        "# sandbox lockfile\n# wip\n",
    )
    .unwrap();
    let before = fixture.snapshot();
    let remote_head = fixture.remote_head();

    let out = fixture.run(&fixture.path_with_tool, &args, 0);
    let log = text(&out);
    assert_eq!(out.status.code(), Some(1), "{log}");
    let script = release_script().display().to_string();
    let typed = format!("{script} --workspace --notes docs/RELEASE_NOTES.md 0.2.2");
    let commands = printed_commands(&stderr_text(&out));
    assert_eq!(
        commands.last(),
        Some(&typed),
        "the re-run line is the invocation as typed, never with --retry added:\n{log}"
    );
    assert_eq!(
        stderr_text(&out),
        format!("{DIRTY_HEADER}  # Cargo.lock\n{DIRTY_ADVICE}  {typed}\n"),
        "{log}"
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );

    // keep the edit by committing it, then run the printed line: it must
    // refuse and leave the edit unpublished
    fixture.git(&["commit", "--quiet", "--all", "-m", "wip"]);
    let committed = fixture.snapshot();
    let rerun = fixture.run_printed(&commands[1], &[env("FAKE_SEMVER_EXIT", "0")]);
    assert_eq!(rerun.status.code(), Some(1), "{}", text(&rerun));
    assert!(
        text(&rerun).contains("version is already 0.2.2"),
        "{}",
        text(&rerun)
    );
    assert_eq!(fixture.remote_head(), remote_head, "the edit was pushed");
    assert_eq!(fixture.tags(&fixture.remote), "v0.2.1\nv0.2.2");
    assert_eq!(fixture.snapshot(), committed, "{}", text(&rerun));
}

#[test]
fn a_retry_flag_after_the_version_is_dropped_from_a_bigger_bump_line() {
    let fixture = Fixture::workspace();
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &[
            "--workspace",
            "--notes",
            "docs/RELEASE_NOTES.md",
            VERSION,
            "--retry",
        ],
        &[
            env("FAKE_SEMVER_EXIT", "100"),
            env("FAKE_SEMVER_EXIT_MAJOR", "0"),
        ],
    );
    let script = release_script().display().to_string();
    assert_eq!(
        printed_commands(&stderr_text(&out)),
        vec![format!(
            "{script} --workspace --notes docs/RELEASE_NOTES.md 0.3.0"
        )],
        "a different version is a different release, wherever --retry sits:\n{}",
        text(&out)
    );
}

/// The simple commands bash ran while sourcing `block`, one per line, as a
/// DEBUG trap saw them (functrace so the sourced file is traced too).
fn pasted_commands(fixture: &Fixture, block: &Path) -> Vec<String> {
    let log = fixture.base().join("pasted.log");
    fs::write(&log, "").unwrap();
    let script = format!(
        "set -T; trap 'printf \"%s\\n\" \"$BASH_COMMAND\" >>\"{}\"' DEBUG; . \"{}\"",
        log.display(),
        block.display()
    );
    let out = fixture.bash(&fixture.path_with_tool, &script);
    assert!(
        out.status.code().is_some(),
        "the pasted block was killed:\n{}",
        text(&out)
    );
    fixture.read(&log).lines().map(str::to_string).collect()
}

/// Feeds `block` to an interactive zsh on stdin, the way a paste arrives;
/// zsh does not treat `#` as a comment there. None when zsh is not installed.
fn paste_into_zsh(fixture: &Fixture, block: &Path) -> Option<Output> {
    let zsh = host_program("zsh")?;
    let mut command = fixture.sandbox(zsh);
    command
        .args(["-f", "-i"])
        .env("PATH", &fixture.path_with_tool)
        .stdin(fs::File::open(block).unwrap());
    Some(command.output().unwrap())
}

#[test]
fn pasting_the_whole_dirty_refusal_block_runs_only_its_two_commands() {
    let fixture = Fixture::workspace();
    fs::write(fixture.repo.join("a"), "keep a\n").unwrap();
    fixture.git(&["add", "a"]);
    fixture.git(&["commit", "--quiet", "-m", "add a"]);
    // a staged rename whose target is a file, a tracked edit, and untracked
    // files named after a command, a substitution, a newline and a redirect
    fixture.git(&["mv", "a", "b"]);
    fs::write(
        fixture.repo.join("Cargo.lock"),
        "# sandbox lockfile\n# wip\n",
    )
    .unwrap();
    for name in ["x;touch pwned", "$(touch pwned2)", "two\nlines", "to>file"] {
        fs::write(fixture.repo.join(name), "stray\n").unwrap();
    }
    let before = fixture.snapshot();

    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let stderr = stderr_text(&out);
    let script = release_script().display().to_string();
    let typed = format!("{script} --workspace --notes docs/RELEASE_NOTES.md 0.2.2");
    let untracked = |path: &str| {
        format!(
            "# untracked {path} blocks the release: commit it, ignore it, or move it out of the tree (a --notes file can also live outside the worktree)\n"
        )
    };
    assert_eq!(
        stderr,
        format!(
            "{DIRTY_HEADER}  # Cargo.lock\n  # a b\n  # \\$\\(touch\\ pwned2\\)\n  # to\\>file\n  # $'two\\nlines'\n  # x\\;touch\\ pwned\n{}{}{}{}{DIRTY_ADVICE}  {typed}\n",
            untracked("\\$\\(touch\\ pwned2\\)"),
            untracked("to\\>file"),
            untracked("$'two\\nlines'"),
            untracked("x\\;touch\\ pwned"),
        ),
        "every path is one comment line of quoted words"
    );

    let block = fixture.base().join("block.txt");
    fs::write(&block, &stderr).unwrap();
    let ran = pasted_commands(&fixture, &block);
    assert_eq!(
        ran,
        vec![
            format!(". \"{}\"", block.display()),
            "git diff HEAD".to_string(),
            typed,
        ],
        "the pasted block must run `git diff HEAD` and the re-run line, nothing else"
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "pasting the block into bash changed a file, the index, a ref or the stash"
    );

    match paste_into_zsh(&fixture, &block) {
        Some(zsh) => {
            assert!(zsh.status.code().is_some(), "{}", text(&zsh));
            assert_eq!(
                fixture.snapshot(),
                before,
                "pasting the block into zsh changed a file, the index, a ref or the stash:\n{}",
                text(&zsh)
            );
        }
        None => eprintln!("skipping the zsh leg: zsh is not on PATH"),
    }
}

#[test]
fn an_unrelated_dirty_edit_gets_the_same_advice_and_keeps_retry_as_invoked() {
    let fixture = Fixture::workspace();
    fs::write(fixture.repo.join("extra.txt"), "one\n").unwrap();
    fixture.git(&["add", "extra.txt"]);
    fixture.git(&["commit", "--quiet", "-m", "add extra"]);
    fs::write(fixture.repo.join("extra.txt"), "two\n").unwrap();
    let script = release_script().display().to_string();

    let unstaged = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    assert_eq!(unstaged.status.code(), Some(1), "{}", text(&unstaged));
    assert_eq!(
        stderr_text(&unstaged),
        format!(
            "{DIRTY_HEADER}  # extra.txt\n{DIRTY_ADVICE}  {script} --workspace --notes docs/RELEASE_NOTES.md 0.2.2\n"
        ),
        "a fresh release's re-run line carries no --retry"
    );

    fixture.git(&["add", "extra.txt"]);
    let before = fixture.snapshot();
    let staged = fixture.run(
        &fixture.path_with_tool,
        &[
            "--retry",
            "--workspace",
            "--notes",
            "docs/RELEASE_NOTES.md",
            VERSION,
        ],
        0,
    );
    assert_eq!(staged.status.code(), Some(1), "{}", text(&staged));
    assert_eq!(
        stderr_text(&staged),
        format!(
            "{DIRTY_HEADER}  # extra.txt\n{DIRTY_ADVICE}  {script} --retry --workspace --notes docs/RELEASE_NOTES.md 0.2.2\n"
        ),
        "an invocation that passed --retry keeps it"
    );
    assert_eq!(
        discarding_commands(&printed_commands(&stderr_text(&staged))),
        Vec::<String>::new()
    );
    assert_eq!(fixture.snapshot(), before, "the refusal mutated state");

    // discard the rest by hand, then run the printed line
    fixture.git(&["reset", "--quiet", "--hard"]);
    assert_rerun_releases(
        &fixture,
        &printed_commands(&stderr_text(&staged))[1],
        VERSION,
    );
}

#[test]
fn an_untracked_file_refuses_even_when_git_hides_untracked_files() {
    let fixture = Fixture::workspace();
    fixture.git(&["config", "status.showUntrackedFiles", "no"]);
    fs::write(fixture.repo.join("stray.txt"), "stray\n").unwrap();
    assert_eq!(
        String::from_utf8_lossy(&fixture.git(&["status", "--porcelain"]).stdout),
        "",
        "the fixture's config must hide the stray file from a plain status"
    );
    let before = fixture.snapshot();
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    let log = text(&out);
    assert_eq!(out.status.code(), Some(1), "{log}");
    let script = release_script().display().to_string();
    assert_eq!(
        stderr_text(&out),
        format!(
            "{DIRTY_HEADER}  # stray.txt\n# untracked stray.txt blocks the release: commit it, ignore it, or move it out of the tree (a --notes file can also live outside the worktree)\n{DIRTY_ADVICE}  {script} --workspace --notes docs/RELEASE_NOTES.md 0.2.2\n"
        ),
        "{log}"
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );
    assert!(!fixture.semver_log.exists(), "the check ran:\n{log}");

    // keep the file by committing it, then run the printed line
    fixture.git(&["add", "stray.txt"]);
    fixture.git(&["commit", "--quiet", "-m", "keep the stray file"]);
    assert_rerun_releases(&fixture, &printed_commands(&stderr_text(&out))[1], VERSION);
}

const DEPENDENCY_TABLES: &str = "\n[dependencies.serde]\nversion = \"1\"\nfeatures = [\"derive\"]\n\n[dev-dependencies.tempfile]\nversion = \"3\"\n";

#[test]
fn a_single_crate_bump_leaves_a_dependency_tables_version_alone() {
    let fixture = Fixture::new();
    let manifest = |version: &str| {
        format!(
            "[package]\nname = \"{SEMVER_PACKAGE}\"\nversion = \"{version}\"\nedition = \"2021\"\n{DEPENDENCY_TABLES}"
        )
    };
    fs::write(fixture.repo.join("Cargo.toml"), manifest(PREV_VERSION)).unwrap();
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "--quiet", "-m", "add dependency tables"]);

    let out = fixture.run(
        &fixture.path_with_tool,
        &["--notes", "CHANGELOG.md", VERSION],
        0,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        fixture.read(&fixture.repo.join("Cargo.toml")),
        manifest(VERSION),
        "only the [package] version may change; a dependency table's version is the crate requirement"
    );
}

#[test]
fn a_workspace_bump_leaves_dependency_tables_alone() {
    let fixture = Fixture::workspace();
    let root = fixture.repo.join("Cargo.toml");
    let member = fixture.repo.join("lib/Cargo.toml");
    let root_before = format!(
        "{}\n[workspace.dependencies.serde]\nversion = \"1\"\n",
        workspace_manifest(PREV_VERSION)
    );
    let member_before = format!("{}{DEPENDENCY_TABLES}", fixture.read(&member));
    fs::write(&root, &root_before).unwrap();
    fs::write(&member, &member_before).unwrap();
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "--quiet", "-m", "add dependency tables"]);

    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        fixture.read(&root),
        format!(
            "{}\n[workspace.dependencies.serde]\nversion = \"1\"\n",
            workspace_manifest(VERSION)
        )
    );
    assert_eq!(
        fixture.read(&member),
        member_before,
        "a member's dependency tables must not change"
    );
}

#[test]
fn a_baseline_version_too_long_to_compute_refuses_without_a_rerun_line() {
    let fixture = Fixture::workspace_at("0.1234567890123456789.0");
    let before = fixture.snapshot();
    let out = fixture.run(
        &fixture.path_with_tool,
        &[
            "--workspace",
            "--notes",
            "docs/RELEASE_NOTES.md",
            "0.1234567890123456789.1",
        ],
        100,
    );
    let log = text(&out);
    let stderr = stderr_text(&out);
    assert_eq!(out.status.code(), Some(1), "{log}");
    assert_eq!(
        error_lines(&stderr),
        vec![
            "Error: libplasmoid-updater's public API change needs a major version bump (a minor one while below 1.0)"
                .to_string(),
            "Error: version 0.1234567890123456789.0 has a part over 18 digits; the next version cannot be computed from it"
                .to_string(),
        ],
        "{log}"
    );
    assert_eq!(
        printed_commands(&stderr),
        Vec::<String>::new(),
        "a re-run line would repeat the refused version:\n{log}"
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "the refusal mutated state:\n{log}"
    );
}

#[test]
fn the_plan_lists_the_steps_in_the_order_they_run() {
    let fixture = Fixture::workspace();
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    let log = text(&out);
    let check = log
        .find("- check libplasmoid-updater's API against its previous release tag")
        .expect("plan lacks the check line");
    let bump = log.find("- bump ").expect("plan lacks the bump line");
    let refresh = log
        .find("- refresh Cargo.lock")
        .expect("plan lacks the lock refresh line");
    assert!(
        check < bump,
        "the check must be listed before the bump:\n{log}"
    );
    assert!(
        bump < refresh,
        "the bump must be listed before the lock refresh:\n{log}"
    );
    let credential = log
        .find("- set gh as the git credential helper")
        .expect("plan lacks the credential helper line");
    let push = log.find("- push ").expect("plan lacks the push line");
    assert!(
        credential < push,
        "the credential helper is set before the push and must be listed before it:\n{log}"
    );
}

#[test]
fn the_plan_describes_the_check_without_fixing_its_release_type() {
    let fixture = Fixture::workspace();
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    let line = stdout_text(&out)
        .lines()
        .find(|line| line.starts_with("  - check "))
        .expect("plan lacks the check line")
        .to_string();
    assert_eq!(
        line,
        "  - check libplasmoid-updater's API against its previous release tag (an API change needs the bump that announces it)",
        "the needed release type comes from the tool, not from 'breaking needs major'"
    );
}

#[test]
fn the_shared_guard_computes_release_types() {
    let cases = [
        ("0.2.1", "0.2.2", "minor"),
        ("0.2.1", "0.3.0", "major"),
        ("0.0.1", "0.0.2", "major"),
        ("1.4.2", "1.4.3", "patch"),
        ("1.4.2", "1.5.0", "minor"),
        ("1.4.2", "2.0.0", "major"),
    ];
    for (base, cur, expected) in cases {
        let out = run_guard(&["release-type", base, cur]);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            expected,
            "{base} -> {cur} must be a {expected} release:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn the_release_workflow_step_passes_a_compatible_tag_and_fails_a_denied_one() {
    let step = workflow_step("release.yml", VALIDATE_STEP);
    let fixture = Fixture::workspace();
    fixture.set_manifest_version(VERSION);
    let envs = [env("GITHUB_REF_NAME", "v0.2.2")];

    let ok = fixture.run_step(&step, &[envs[0].clone(), env("FAKE_SEMVER_EXIT", "0")]);
    assert_eq!(ok.status.code(), Some(0), "{}", text(&ok));
    assert_eq!(
        fixture.semver_argv(),
        "check-release -p libplasmoid-updater --baseline-rev v0.2.1 --release-type minor"
    );

    let denied = fixture.run_step(&step, &[envs[0].clone(), env("FAKE_SEMVER_EXIT", "100")]);
    assert_eq!(
        denied.status.code(),
        Some(100),
        "a denied check must fail the step with the tool's status:\n{}",
        text(&denied)
    );
}

#[test]
fn the_release_workflow_step_refuses_a_tag_that_does_not_match_the_manifest() {
    let step = workflow_step("release.yml", VALIDATE_STEP);
    let fixture = Fixture::workspace();
    let out = fixture.run_step(
        &step,
        &[
            env("GITHUB_REF_NAME", "v0.2.3"),
            env("FAKE_SEMVER_EXIT", "0"),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        stdout_text(&out).contains("tag v0.2.3 does not match the workspace version '0.2.1'"),
        "{}",
        text(&out)
    );
    assert!(!fixture.semver_log.exists(), "the tool was invoked");
}

#[test]
fn the_release_workflow_step_names_a_baseline_without_the_crate() {
    let step = workflow_step("release.yml", VALIDATE_STEP);
    let fixture = Fixture::workspace();
    fixture.git(&["rm", "--quiet", "lib/Cargo.toml"]);
    fixture.git(&["commit", "--quiet", "-m", "drop the lib crate"]);
    fixture.git(&["tag", "--force", fixture.prev_tag.as_str(), "-m", "release"]);
    fixture.git(&[
        "push",
        "--quiet",
        "--force",
        "origin",
        fixture.prev_tag.as_str(),
    ]);
    fixture.set_manifest_version(VERSION);
    let out = fixture.run_step(
        &step,
        &[
            env("GITHUB_REF_NAME", "v0.2.2"),
            env("FAKE_SEMVER_EXIT", "0"),
        ],
    );
    assert_eq!(out.status.code(), Some(5), "{}", text(&out));
    assert!(
        stderr_text(&out).contains("has no libplasmoid-updater manifest"),
        "a crate-less baseline must be named in CI too, not left to the tool's bare 101:\n{}",
        text(&out)
    );
    assert!(!fixture.semver_log.exists(), "the tool was invoked");
}

#[test]
fn the_step_extractor_reads_the_run_block_of_the_named_step() {
    let yaml = "jobs:\n  a:\n    steps:\n      - name: first\n        run: echo one\n      - name: second\n        # a comment\n        run: |\n          set -e\n\n          echo two\n            nested\n      - name: third\n        run: |\n          echo three\n";
    assert_eq!(
        step_script(yaml, "second"),
        "set -e\n\necho two\n  nested\n"
    );
    assert_eq!(step_script(yaml, "third"), "echo three\n");
    assert_eq!(step_script(yaml, "first"), "echo one\n");
    let no_run = "steps:\n  - name: a\n    uses: x\n  - name: b\n    run: echo b\n";
    let missing = std::panic::catch_unwind(|| step_script(no_run, "a"));
    assert!(
        missing.is_err(),
        "a step without a run must not borrow the next step's"
    );
}

/// The lines of the job named `job` in a workflow's text: from its `  <job>:`
/// header to the next two-space-indented key.
fn job_lines<'a>(yaml: &'a str, job: &str) -> Vec<&'a str> {
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

/// The value of a job-level `key:` line.
fn job_key(job: &[&str], key: &str) -> Option<String> {
    let prefix = format!("    {key}: ");
    job.iter()
        .find_map(|line| line.strip_prefix(prefix.as_str()).map(str::to_string))
}

/// The toolchain the job's `dtolnay/rust-toolchain` step is pinned to (None
/// when it floats on the channel) and the version its
/// `cargo-semver-checks@<version>` install is pinned to.
fn semver_pins(job: &[&str]) -> (Option<String>, Option<String>) {
    let toolchain = job
        .iter()
        .position(|line| line.trim().starts_with("- uses: dtolnay/rust-toolchain@"))
        .and_then(|start| {
            job[start + 1..]
                .iter()
                .take_while(|line| !line.trim_start().starts_with("- "))
                .find_map(|line| line.trim().strip_prefix("toolchain: "))
                .map(|value| value.trim_matches('"').to_string())
        });
    let tool = job.iter().find_map(|line| {
        line.trim()
            .strip_prefix("tool: cargo-semver-checks@")
            .map(str::to_string)
    });
    (toolchain, tool)
}

#[test]
fn the_job_readers_find_the_pins_and_keys_they_are_given() {
    let yaml = "jobs:\n  a:\n    if: x\n    steps:\n      - uses: dtolnay/rust-toolchain@stable\n        with:\n          toolchain: \"1.2.3\"\n      - uses: taiki-e/install-action@v2\n        with:\n          tool: cargo-semver-checks@4.5.6\n  # note\n  b:\n    if: y\n    steps:\n      - uses: dtolnay/rust-toolchain@stable\n      - uses: other\n        with:\n          toolchain: \"9.9.9\"\n";
    let a = job_lines(yaml, "a");
    assert_eq!(job_key(&a, "if").as_deref(), Some("x"));
    assert_eq!(
        semver_pins(&a),
        (Some("1.2.3".to_string()), Some("4.5.6".to_string()))
    );
    let b = job_lines(yaml, "b");
    assert_eq!(job_key(&b, "if").as_deref(), Some("y"));
    assert_eq!(
        semver_pins(&b),
        (None, None),
        "a floating toolchain step must not borrow the next step's pin"
    );
}

#[test]
fn both_semver_jobs_pin_the_toolchain_beside_the_tool() {
    let workflows = repo_root().join(".github/workflows");
    let read = |name: &str| fs::read_to_string(workflows.join(name)).unwrap();
    let release = read("release.yml");
    let ci = read("ci.yml");
    let pins = [
        semver_pins(&job_lines(&release, "validate")),
        semver_pins(&job_lines(&ci, "semver")),
    ];
    for (toolchain, tool) in &pins {
        let toolchain = toolchain
            .as_deref()
            .expect("a semver job builds rustdoc on a floating toolchain");
        assert!(
            toolchain.split('.').count() == 3
                && toolchain
                    .split('.')
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())),
            "the toolchain must be one explicit X.Y.Z, got {toolchain:?}"
        );
        assert!(tool.is_some(), "a semver job lost its tool pin");
    }
    assert_eq!(
        pins[0], pins[1],
        "release.yml and ci.yml must pin the same toolchain and tool"
    );
}

#[test]
fn publish_runs_only_after_a_successful_validate() {
    let yaml = fs::read_to_string(repo_root().join(".github/workflows/release.yml")).unwrap();
    let publish = job_lines(&yaml, "publish");
    assert_eq!(
        job_key(&publish, "needs").as_deref(),
        Some("[validate, build]")
    );
    assert_eq!(
        job_key(&publish, "if").as_deref(),
        Some(
            "always() && needs.validate.result == 'success' && startsWith(github.ref, 'refs/tags/')"
        ),
        "a failed validate skips every build leg, so publish must not run to fail on the missing artifacts"
    );
}

const CRATES_JOB: &str = "crates";
const CRATES_STEP: &str = "Publish the crates to crates.io";

fn release_yaml() -> String {
    fs::read_to_string(repo_root().join(".github/workflows/release.yml")).unwrap()
}

/// The package each recorded `cargo publish` call named, in call order.
fn published_packages(fixture: &Fixture) -> Vec<String> {
    let log = fs::read_to_string(&fixture.publish_log).unwrap_or_default();
    log.lines()
        .map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            words
                .iter()
                .position(|word| *word == "-p" || *word == "--package")
                .map(|at| words[at + 1].to_string())
                .or_else(|| {
                    words
                        .iter()
                        .find_map(|word| word.strip_prefix("--package="))
                        .map(str::to_string)
                })
                .unwrap_or_else(|| panic!("publish call names no package: {line}"))
        })
        .collect()
}

/// The shared script release.sh and both release.yml jobs ask about a crate
/// version's copy on crates.io.
const CRATES_IO_COPY: &str = "crates-io-copy.sh";
/// The validate step that stops a tag whose version crates.io holds from
/// another commit before anything is built or uploaded.
const CRATES_GUARD_STEP: &str = "Refuse a version crates.io holds from another commit";
const OTHER_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// A crate version already on crates.io: the commit its `.cargo_vcs_info.json`
/// names (`None`: a `.crate` carrying no such file), and whether cargo marked
/// the packed tree dirty.
struct Published {
    name: &'static str,
    version: &'static str,
    sha1: Option<String>,
    dirty: bool,
}

fn published(name: &'static str, version: &'static str, sha1: Option<&str>) -> Published {
    Published {
        name,
        version,
        sha1: sha1.map(str::to_string),
        dirty: false,
    }
}

/// Packs `registry` as crates.io's `.crate` files for the fake `curl` and lists
/// them for the fake `cargo`, returning the env both read.
fn fake_registry(fixture: &Fixture, registry: &[Published]) -> Vec<(&'static str, String)> {
    let crates_dir = fixture.base().join("crates-io");
    fs::create_dir_all(&crates_dir).unwrap();
    for entry in registry {
        let stem = format!("{}-{}", entry.name, entry.version);
        let tree = fixture.base().join("crate-src").join(&stem);
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("Cargo.toml"), "[package]\n").unwrap();
        if let Some(sha1) = &entry.sha1 {
            let dirty = if entry.dirty {
                r#", "dirty": true"#
            } else {
                ""
            };
            fs::write(
                tree.join(".cargo_vcs_info.json"),
                format!(r#"{{"git": {{"sha1": "{sha1}"{dirty}}}, "path_in_vcs": ""}}"#),
            )
            .unwrap();
        }
        let status = Command::new(system_program("tar"))
            .arg("-czf")
            .arg(crates_dir.join(format!("{stem}.crate")))
            .arg("-C")
            .arg(tree.parent().unwrap())
            .arg(&stem)
            .status()
            .unwrap();
        assert!(status.success(), "packing the fake {stem}.crate failed");
    }
    let listed: Vec<String> = registry
        .iter()
        .map(|entry| format!("{}@{}", entry.name, entry.version))
        .collect();
    vec![
        env("FAKE_CRATES_ON_REGISTRY", &listed.join(" ")),
        env("FAKE_CRATES_DIR", &crates_dir.display().to_string()),
    ]
}

/// Runs a release.yml step with `registry` already on crates.io and the tag
/// at the fixture's version.
fn run_registry_step(
    fixture: &Fixture,
    step: &str,
    registry: &[Published],
    envs: &[(&'static str, String)],
) -> Output {
    fixture.set_manifest_version(VERSION);
    let mut all = vec![
        env("GITHUB_REF_NAME", &format!("v{VERSION}")),
        env("CARGO_REGISTRY_TOKEN", "fake-trusted-publishing-token"),
    ];
    all.extend(fake_registry(fixture, registry));
    all.extend_from_slice(envs);
    fixture.run_step(&step_script(&release_yaml(), step), &all)
}

fn run_crates_step(
    fixture: &Fixture,
    registry: &[Published],
    envs: &[(&'static str, String)],
) -> Output {
    run_registry_step(fixture, CRATES_STEP, registry, envs)
}

/// The shared script's one-line answer about `<name>@<VERSION>` on crates.io.
fn crates_io_copy(
    fixture: &Fixture,
    registry: &[Published],
    envs: &[(&'static str, String)],
) -> Output {
    let mut all = fake_registry(fixture, registry);
    all.extend_from_slice(envs);
    fixture.run_step(
        &format!("bash scripts/{CRATES_IO_COPY} libplasmoid-updater {VERSION}\n"),
        &all,
    )
}

#[test]
fn the_crates_io_copy_script_tells_each_state_apart() {
    // Each case runs in its own fixture, whose commit sha embeds its creation
    // time, so a case's registry is built from that fixture's own HEAD.
    type Registry = fn(&str) -> Vec<Published>;
    type Case = (
        &'static str,
        Registry,
        Option<(&'static str, &'static str)>,
        String,
    );
    let cases: [Case; 10] = [
        ("not published", |_| vec![], None, "absent".to_string()),
        (
            "a 404",
            |_| vec![],
            Some(("FAKE_CURL_HTTP", "404")),
            "absent".to_string(),
        ),
        (
            "a CDN server error proves nothing",
            |_| vec![],
            Some(("FAKE_CURL_HTTP", "503")),
            "unknown".to_string(),
        ),
        (
            "packed from a dirty tree at another commit",
            |_| {
                vec![Published {
                    dirty: true,
                    ..published("libplasmoid-updater", VERSION, Some(OTHER_COMMIT))
                }]
            },
            None,
            format!("other {OTHER_COMMIT}"),
        ),
        (
            "published from this commit",
            |head| vec![published("libplasmoid-updater", VERSION, Some(head))],
            None,
            "same".to_string(),
        ),
        (
            "published from another commit",
            |_| {
                vec![published(
                    "libplasmoid-updater",
                    VERSION,
                    Some(OTHER_COMMIT),
                )]
            },
            None,
            format!("other {OTHER_COMMIT}"),
        ),
        (
            "packed from a dirty tree at this commit",
            |head| {
                vec![Published {
                    dirty: true,
                    ..published("libplasmoid-updater", VERSION, Some(head))
                }]
            },
            None,
            "unknown".to_string(),
        ),
        (
            "no readable commit",
            |_| vec![published("libplasmoid-updater", VERSION, None)],
            None,
            "unknown".to_string(),
        ),
        (
            "a commit field that is no object id",
            |_| vec![published("libplasmoid-updater", VERSION, Some("v0.2.2"))],
            None,
            "unknown".to_string(),
        ),
        (
            "download failed for another reason",
            |head| vec![published("libplasmoid-updater", VERSION, Some(head))],
            Some(("FAKE_CURL_EXIT", "7")),
            "unknown".to_string(),
        ),
    ];
    for (case, registry, extra, expected) in cases {
        let fixture = Fixture::workspace();
        let registry = registry(&fixture.head());
        let envs: Vec<(&'static str, String)> = extra
            .map(|(key, value)| env(key, value))
            .into_iter()
            .collect();
        let out = crates_io_copy(&fixture, &registry, &envs);
        assert_eq!(out.status.code(), Some(0), "{case}: {}", text(&out));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let answer = stdout.trim_end();
        if expected == "unknown" {
            assert!(
                answer == "unknown" || answer.starts_with("unknown "),
                "{case}: expected unknown, got {answer:?}"
            );
        } else {
            assert_eq!(answer, expected, "{case}");
        }
    }
    let fixture = Fixture::workspace();
    let out = crates_io_copy(&fixture, &[], &[env("FAKE_CURL_EXIT", "7")]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("curl: (7)"),
        "a failed download shows curl's own reason: {}",
        text(&out)
    );
}

#[test]
fn validate_refuses_a_version_crates_io_holds_from_another_commit() {
    let fixture = Fixture::workspace();
    let head = fixture.head();
    let out = run_registry_step(
        &fixture,
        CRATES_GUARD_STEP,
        &[published("plasmoid-updater", VERSION, Some(OTHER_COMMIT))],
        &[],
    );
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(OTHER_COMMIT) && stderr.contains(&head),
        "the error names both commits:\n{stderr}"
    );
    let error = format!(
        "::error::plasmoid-updater {VERSION} is already on crates.io but was packed from {OTHER_COMMIT}, not from this tag's {head}; crates.io versions are immutable, so this tag can never publish it: bump to a new version (delete the GitHub release, if release.sh made one, and the tag, then release the next version)"
    );
    assert!(
        stderr.lines().any(|line| line == error),
        "the one remedy is a new version:\n{stderr}"
    );
    assert_eq!(
        stderr,
        format!("curl: (22) The requested URL returned error: 403\n{error}\n"),
        "the error is the step's whole stderr past curl's own line: no second line may offer a do-over"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("::error::"),
        "{}",
        text(&out)
    );

    let fixture = Fixture::workspace();
    let out = run_registry_step(&fixture, CRATES_GUARD_STEP, &[], &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "not yet published: {}",
        text(&out)
    );

    let fixture = Fixture::workspace();
    let head = fixture.head();
    let registry = [
        published("libplasmoid-updater", VERSION, Some(&head)),
        published("plasmoid-updater", VERSION, Some(&head)),
    ];
    let out = run_registry_step(&fixture, CRATES_GUARD_STEP, &registry, &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "a re-run of this commit: {}",
        text(&out)
    );

    let yaml = release_yaml();
    let validate = job_lines(&yaml, "validate");
    assert!(
        validate
            .iter()
            .any(|line| line.trim() == format!("- name: {CRATES_GUARD_STEP}")),
        "the guard runs in validate, before any build or upload"
    );
}

#[test]
fn release_refuses_a_version_crates_io_holds_from_another_commit() {
    let fixture = Fixture::workspace();
    let head = fixture.head();
    let before = fixture.snapshot();
    let mut envs = vec![env("FAKE_SEMVER_EXIT", "0")];
    envs.extend(fake_registry(
        &fixture,
        &[published(
            "libplasmoid-updater",
            VERSION,
            Some(OTHER_COMMIT),
        )],
    ));
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        &envs,
    );
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    let stderr = stderr_text(&out);
    assert!(
        stderr.contains(OTHER_COMMIT) && stderr.contains(&head),
        "the refusal names both commits:\n{stderr}"
    );
    let refusal = format!(
        "Error: libplasmoid-updater {VERSION} is already on crates.io but was packed from {OTHER_COMMIT}, not from this tree's {head}; crates.io versions are immutable, so v{VERSION} can never be released from this tree: bump to a new version; the release did not start"
    );
    assert!(
        stderr.lines().any(|line| line == refusal),
        "the one remedy is a new version:\n{stderr}"
    );
    assert_eq!(
        stderr,
        format!("{refusal}\n"),
        "the refusal is the whole stderr: no second line may offer a do-over"
    );
    assert_eq!(
        printed_commands(&stderr),
        Vec::<String>::new(),
        "no re-run line: it would only repeat the refused version"
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "a refused release changes nothing"
    );
}

/// The publish job's missing-release error never advises redoing the same
/// version: a hand-pushed tag is superseded by the next release.
#[test]
fn the_publish_job_never_advises_redoing_a_hand_pushed_tag() {
    let fixture = Fixture::workspace();
    fs::create_dir_all(fixture.repo.join("artifacts")).unwrap();
    let out = fixture.run_step(
        &step_script(&release_yaml(), "Upload assets to the release"),
        &[
            env("GITHUB_REF_NAME", &format!("v{VERSION}")),
            env("GITHUB_REPOSITORY", "owner/repo"),
            env("GITHUB_RUN_ID", "42"),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let error = format!(
        "::error::release v{VERSION} does not exist; if release.sh's create step failed, run from the repo root: echo y | ./release.sh --retry --workspace --notes CHANGELOG_FILE {VERSION}, then re-run this job so the assets attach: gh run rerun 42 --failed — if the tag was hand-pushed, release the next version with release.sh"
    );
    assert!(
        text(&out).lines().any(|line| line == error),
        "{}",
        text(&out)
    );
    assert_eq!(
        stdout_text(&out),
        format!("{error}\n"),
        "the error is the step's whole output: no second line may offer a do-over"
    );
    assert_eq!(stderr_text(&out), "", "{}", text(&out));
}

/// A release whose tag went out is resumed or superseded, never redone: its
/// first workflow run may still publish the version from the first commit.
#[test]
fn an_existing_release_is_resumed_or_superseded_never_redone() {
    let fixture = Fixture::workspace();
    let before = fixture.snapshot();
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        &[env("FAKE_GH_RELEASES", &format!("v{VERSION}"))],
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let refusal = format!(
        "Error: github release v{VERSION} already exists; rerun with --retry to resume, or release the next version"
    );
    assert!(
        stderr_text(&out).lines().any(|line| line == refusal),
        "{}",
        text(&out)
    );
    assert_eq!(
        stderr_text(&out),
        format!("{refusal}\n"),
        "the refusal is the whole stderr: no second line may offer a do-over"
    );
    assert_eq!(stdout_text(&out), "", "{}", text(&out));
    assert_eq!(fixture.snapshot(), before, "the refusal changed state");
}

/// A crates.io answer the check cannot read (here a failed connection) stops
/// the release before any change, the way a semver check that could not
/// complete does: a version it cannot see might be another commit's.
#[test]
fn release_refuses_when_the_crates_io_check_cannot_complete() {
    let fixture = Fixture::workspace();
    let before = fixture.snapshot();
    let mut envs = vec![env("FAKE_SEMVER_EXIT", "0"), env("FAKE_CURL_EXIT", "7")];
    envs.extend(fake_registry(&fixture, &[]));
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        &envs,
    );
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        stderr_text(&out).contains("curl: (7)"),
        "the refusal shows why the check could not complete:\n{}",
        text(&out)
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "a refused release changes nothing"
    );
}

/// On the tag, an unreadable crates.io answer does not block the release (the
/// crates job asks again, and crates.io refuses a real duplicate), but the
/// validate log says what it saw.
#[test]
fn validate_passes_an_unreadable_crates_io_answer_and_logs_it() {
    let fixture = Fixture::workspace();
    let out = run_registry_step(
        &fixture,
        CRATES_GUARD_STEP,
        &[],
        &[env("FAKE_CURL_HTTP", "503")],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("unknown"),
        "validate logs the check's answer:\n{}",
        text(&out)
    );
}

/// Both crates are checked: whichever one crates.io holds from another commit
/// stops `release.sh` and validate alike.
#[test]
fn either_crate_from_another_commit_stops_the_release() {
    for crate_name in ["libplasmoid-updater", "plasmoid-updater"] {
        let fixture = Fixture::workspace();
        let before = fixture.snapshot();
        let mut envs = vec![env("FAKE_SEMVER_EXIT", "0")];
        envs.extend(fake_registry(
            &fixture,
            &[published(crate_name, VERSION, Some(OTHER_COMMIT))],
        ));
        let out = fixture.run_with_env(
            &fixture.path_with_tool,
            &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
            &envs,
        );
        assert_ne!(out.status.code(), Some(0), "{crate_name}: {}", text(&out));
        assert_eq!(fixture.snapshot(), before, "{crate_name}: changed the tree");

        let fixture = Fixture::workspace();
        let out = run_registry_step(
            &fixture,
            CRATES_GUARD_STEP,
            &[published(crate_name, VERSION, Some(OTHER_COMMIT))],
            &[],
        );
        assert_ne!(
            out.status.code(),
            Some(0),
            "validate, {crate_name}: {}",
            text(&out)
        );
    }
}

/// A version crates.io already holds from the tagged commit is the same
/// release: a `--retry` resume at that commit goes on after the crates job
/// published.
#[test]
fn release_proceeds_over_a_version_crates_io_holds_from_this_commit() {
    let fixture = Fixture::workspace();
    let args = ["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION];
    let released = fixture.run(&fixture.path_with_tool, &args, 0);
    assert_eq!(released.status.code(), Some(0), "{}", text(&released));
    let tagged = fixture.rev_parse(&format!("v{VERSION}^{{commit}}"));
    let mut envs = vec![env("FAKE_SEMVER_EXIT", "0")];
    envs.extend(fake_registry(
        &fixture,
        &[
            published("libplasmoid-updater", VERSION, Some(&tagged)),
            published("plasmoid-updater", VERSION, Some(&tagged)),
        ],
    ));
    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &[
            "--retry",
            "--workspace",
            "--notes",
            "docs/RELEASE_NOTES.md",
            VERSION,
        ],
        &envs,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    for crate_name in ["libplasmoid-updater", "plasmoid-updater"] {
        let line = format!("crates.io's {crate_name} {VERSION}: same");
        assert!(
            stdout_text(&out).lines().any(|l| l == line),
            "missing {line:?}:\n{}",
            text(&out)
        );
    }
}

#[test]
fn validate_logs_every_crates_io_answer() {
    let fixture = Fixture::workspace();
    let head = fixture.head();
    let out = run_registry_step(
        &fixture,
        CRATES_GUARD_STEP,
        &[published("plasmoid-updater", VERSION, Some(&head))],
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let log = text(&out);
    for line in [
        format!("crates.io's libplasmoid-updater {VERSION}: absent"),
        format!("crates.io's plasmoid-updater {VERSION}: same"),
    ] {
        assert!(log.lines().any(|l| l == line), "missing {line:?}:\n{log}");
    }
}

#[test]
fn the_plan_lists_the_crates_io_check_between_the_api_check_and_the_bump() {
    let fixture = Fixture::workspace();
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    let log = stdout_text(&out);
    let lines: Vec<&str> = log.lines().collect();
    let at = |prefix: &str| {
        lines
            .iter()
            .position(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("plan lacks {prefix:?}:\n{log}"))
    };
    let copy = at("  - check crates.io ");
    assert_eq!(
        lines[copy],
        format!(
            "  - check crates.io holds no {VERSION} from another commit, stopping if it cannot tell"
        )
    );
    assert!(at("  - check libplasmoid-updater's API") < copy, "{log}");
    assert!(copy < at("  - bump "), "{log}");
}

#[test]
fn the_crates_step_never_skips_a_crate_packed_from_a_dirty_tree() {
    let fixture = Fixture::workspace();
    let head = fixture.head();
    let out = run_crates_step(
        &fixture,
        &[Published {
            dirty: true,
            ..published("libplasmoid-updater", VERSION, Some(&head))
        }],
        &[],
    );
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(published_packages(&fixture), ["libplasmoid-updater"]);
}

#[test]
fn the_crates_step_publishes_the_library_before_the_binary() {
    let fixture = Fixture::workspace();
    let out = run_crates_step(&fixture, &[], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        published_packages(&fixture),
        ["libplasmoid-updater", "plasmoid-updater"],
        "the binary depends on the library by version, so the library publishes first"
    );
    let log = fs::read_to_string(&fixture.publish_log).unwrap();
    for line in log.lines() {
        assert!(
            line.split_whitespace().any(|word| word == "--locked"),
            "every publish verifies against the committed Cargo.lock: {line}"
        );
        assert!(
            !line.split_whitespace().any(|word| word == "--no-verify"),
            "every publish builds the packaged crate before upload: {line}"
        );
        assert!(
            line.ends_with(" token=set"),
            "every publish carries the registry token: {line}"
        );
    }
}

#[test]
fn the_crates_step_skips_a_crate_already_on_crates_io() {
    let other = "0123456789abcdef0123456789abcdef01234567";
    let fixture = Fixture::workspace();
    let head = fixture.head();
    let rerun = run_crates_step(
        &fixture,
        &[
            published("libplasmoid-updater", PREV_VERSION, Some(other)),
            published("libplasmoid-updater", VERSION, Some(&head)),
        ],
        &[],
    );
    assert_eq!(rerun.status.code(), Some(0), "{}", text(&rerun));
    assert_eq!(
        published_packages(&fixture),
        ["plasmoid-updater"],
        "a re-run after the library went up from this commit publishes only the binary"
    );

    let fixture = Fixture::workspace();
    let older_only = run_crates_step(
        &fixture,
        &[
            published("libplasmoid-updater", PREV_VERSION, Some(other)),
            published("plasmoid-updater", PREV_VERSION, Some(other)),
        ],
        &[],
    );
    assert_eq!(older_only.status.code(), Some(0), "{}", text(&older_only));
    assert_eq!(
        published_packages(&fixture),
        ["libplasmoid-updater", "plasmoid-updater"],
        "only the version under release counts as already published"
    );
}

/// crates.io's copy of the version came from another commit (a tag deleted and
/// pushed again elsewhere while the first run was still publishing, which
/// validate cannot see yet): the step fails, naming
/// both commits, and publishes nothing, since a crate version is immutable.
#[test]
fn the_crates_step_refuses_a_version_published_from_another_commit() {
    let other = "0123456789abcdef0123456789abcdef01234567";
    let fixture = Fixture::workspace();
    let head = fixture.head();
    let out = run_crates_step(
        &fixture,
        &[published("libplasmoid-updater", VERSION, Some(other))],
        &[],
    );
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        published_packages(&fixture).is_empty(),
        "nothing publishes past a mismatched version: {:?}",
        published_packages(&fixture)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(other) && stderr.contains(&head),
        "the error names both commits:\n{stderr}"
    );
    let error = format!(
        "::error::libplasmoid-updater {VERSION} is already on crates.io but was packed from {other}, not from this tag's {head}; crates.io versions are immutable, so this tag cannot publish it: bump to a new version (delete the GitHub release and the tag, then release the next version)"
    );
    assert!(
        stderr.lines().any(|line| line == error),
        "the one remedy is a new version:\n{stderr}"
    );
    assert_eq!(
        stderr,
        format!("{error}\n"),
        "the error is the step's whole stderr: no second line may offer a do-over"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("::error::"),
        "{}",
        text(&out)
    );
}

/// A published `.crate` whose commit cannot be read proves nothing: the step
/// tries the publish, and crates.io's duplicate refusal fails it loudly.
#[test]
fn the_crates_step_never_skips_a_version_whose_commit_it_cannot_read() {
    let fixture = Fixture::workspace();
    let out = run_crates_step(
        &fixture,
        &[published("libplasmoid-updater", VERSION, None)],
        &[],
    );
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(published_packages(&fixture), ["libplasmoid-updater"]);
}

#[test]
fn the_crates_step_stops_when_the_library_publish_fails() {
    let fixture = Fixture::workspace();
    let out = run_crates_step(
        &fixture,
        &[],
        &[env("FAKE_PUBLISH_FAIL", "libplasmoid-updater")],
    );
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        published_packages(&fixture),
        ["libplasmoid-updater"],
        "the binary never publishes against a library version crates.io lacks"
    );
}

#[test]
fn the_crates_job_publishes_through_trusted_publishing_after_the_build() {
    let yaml = release_yaml();
    let job = job_lines(&yaml, CRATES_JOB);
    assert_eq!(
        job_key(&job, "needs").as_deref(),
        Some("[validate, build, publish]"),
        "crates.io gets a version only once its GitHub release holds the assets"
    );
    for net in [
        "CARGO_NET_RETRY: \"10\"",
        "CARGO_HTTP_MULTIPLEXING: \"false\"",
    ] {
        assert!(
            job.iter().any(|line| line.trim() == net),
            "the publish verify build downloads dependencies the way ci.yml does: {net}"
        );
    }
    assert_eq!(
        job_key(&job, "if").as_deref(),
        Some("startsWith(github.ref, 'refs/tags/')"),
        "a failed validate or build leg publishes nothing"
    );
    assert!(
        job.iter()
            .any(|line| line.trim() == "id-token: write"
                || line.trim().starts_with("id-token: write ")),
        "trusted publishing exchanges the job's OIDC token"
    );
    let auth = job
        .iter()
        .find_map(|line| {
            line.trim()
                .strip_prefix("- uses: rust-lang/crates-io-auth-action@")
        })
        .expect("the job takes its token from rust-lang/crates-io-auth-action");
    let sha = auth.split_whitespace().next().unwrap();
    assert!(
        sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()),
        "the auth action is pinned by commit sha, got {auth:?}"
    );
    assert!(
        !job.iter().any(|line| line.contains("secrets.")),
        "no long-lived registry token: the job reads no repository secret"
    );
    let granted: Vec<&str> = job
        .iter()
        .skip_while(|line| line.trim() != "permissions:")
        .skip(1)
        .take_while(|line| line.starts_with("      "))
        .map(|line| line.split('#').next().unwrap().trim())
        .collect();
    assert_eq!(
        granted,
        ["contents: read", "id-token: write"],
        "the job holds no permission beyond reading the tree and its OIDC token"
    );
    let auth_at = job
        .iter()
        .position(|line| line.contains("rust-lang/crates-io-auth-action@"))
        .unwrap();
    assert_eq!(
        job[auth_at + 1].trim(),
        "id: auth",
        "the publish step reads the token by this step id"
    );
    let step = step_script(&yaml, CRATES_STEP);
    let step_at = job
        .iter()
        .position(|line| line.trim() == format!("- name: {CRATES_STEP}"))
        .unwrap();
    assert!(
        job[step_at..]
            .iter()
            .take_while(|line| !line.trim_start().starts_with("run:"))
            .any(|line| line.trim() == "CARGO_REGISTRY_TOKEN: ${{ steps.auth.outputs.token }}"),
        "the publish step takes its token from the auth step:\n{step}"
    );
    let concurrency_at = job
        .iter()
        .position(|line| line.trim() == "concurrency:")
        .expect("the job sets a concurrency group");
    let concurrency: Vec<&str> = job[concurrency_at + 1..]
        .iter()
        .take_while(|line| line.starts_with("      "))
        .map(|line| line.trim())
        .collect();
    assert_eq!(
        concurrency,
        [
            "group: crates-${{ github.ref }}",
            "cancel-in-progress: false"
        ],
        "two runs for one tag publish one after the other, neither cancelled mid-upload"
    );
}

/// The report job's three outcomes and its operational passes, driven through
/// the guard with a manifest at, above or below what the change needs.
struct Report {
    manifest: &'static str,
    tool: ToolEnv,
}

/// The fake tool's per-type exit statuses, as environment pairs.
type ToolEnv = &'static [(&'static str, &'static str)];

fn tool_env(tool: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
    tool.iter().map(|(key, value)| env(key, value)).collect()
}

fn run_report(fixture: &Fixture, report: &Report) -> Output {
    fixture.set_manifest_version(report.manifest);
    fixture.guard(&["report"], &tool_env(report.tool))
}

const NEEDS_MAJOR: ToolEnv = &[("FAKE_SEMVER_EXIT", "100"), ("FAKE_SEMVER_EXIT_MAJOR", "0")];
const NEEDS_MINOR: ToolEnv = &[
    ("FAKE_SEMVER_EXIT", "100"),
    ("FAKE_SEMVER_EXIT_MINOR", "0"),
    ("FAKE_SEMVER_EXIT_MAJOR", "0"),
];
const NEEDS_NOTHING: ToolEnv = &[("FAKE_SEMVER_EXIT", "0")];

#[test]
fn the_report_passes_a_not_bumped_manifest_with_a_breaking_change_and_names_the_type() {
    let fixture = Fixture::workspace();
    let before = fixture.snapshot();
    let out = fixture.guard(
        &["report"],
        &[
            env("FAKE_SEMVER_EXIT", "100"),
            env("FAKE_SEMVER_EXIT_MAJOR", "0"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        stdout_text(&out),
        "libplasmoid-updater: the change against v0.2.1 needs a major release (a minor bump while below 1.0); the manifest is at 0.2.1, not bumped yet\n",
        "{}",
        text(&out)
    );
    assert_eq!(
        fixture.semver_argv_lines(),
        vec![
            "check-release -p libplasmoid-updater --baseline-rev v0.2.1 --release-type patch"
                .to_string(),
            "check-release -p libplasmoid-updater --baseline-rev v0.2.1 --release-type minor"
                .to_string(),
            "check-release -p libplasmoid-updater --baseline-rev v0.2.1 --release-type major"
                .to_string(),
        ],
        "the report must find the smallest accepted type, trying patch, minor, major in order, against the tag at the manifest version"
    );
    assert_eq!(fixture.snapshot(), before, "the report mutated state");
}

#[test]
fn the_report_names_each_needed_type_for_a_not_bumped_manifest() {
    let cases: [(ToolEnv, &str); 3] = [
        (
            NEEDS_MAJOR,
            "a major release (a minor bump while below 1.0)",
        ),
        (
            NEEDS_MINOR,
            "a minor release (a patch bump while below 1.0)",
        ),
        (NEEDS_NOTHING, "a patch release"),
    ];
    for (tool, needed) in cases {
        let fixture = Fixture::workspace();
        let out = run_report(
            &fixture,
            &Report {
                manifest: "0.2.1",
                tool,
            },
        );
        assert_eq!(out.status.code(), Some(0), "{}", text(&out));
        assert_eq!(
            stdout_text(&out),
            format!(
                "libplasmoid-updater: the change against v0.2.1 needs {needed}; the manifest is at 0.2.1, not bumped yet\n"
            ),
            "{}",
            text(&out)
        );
    }
}

#[test]
fn the_report_passes_a_bump_that_announces_the_change() {
    const PATCH_BUMP: &str = "a minor release (a patch bump while below 1.0)";
    const MINOR_BUMP: &str = "a major release (a minor bump while below 1.0)";
    let cases: [(&str, ToolEnv, &str, &str); 3] = [
        ("0.3.0", NEEDS_MAJOR, MINOR_BUMP, MINOR_BUMP),
        ("0.2.2", NEEDS_MINOR, PATCH_BUMP, PATCH_BUMP),
        ("0.2.2", NEEDS_NOTHING, "a patch release", PATCH_BUMP),
    ];
    for (manifest, tool, needed, implied) in cases {
        let fixture = Fixture::workspace();
        let out = run_report(&fixture, &Report { manifest, tool });
        assert_eq!(out.status.code(), Some(0), "{manifest}:\n{}", text(&out));
        assert_eq!(
            stdout_text(&out),
            format!(
                "libplasmoid-updater: the change against v0.2.1 needs {needed}; manifest version {manifest} is {implied}, which announces it\n"
            ),
            "{}",
            text(&out)
        );
    }
}

#[test]
fn the_report_fails_a_bump_smaller_than_the_change_needs() {
    let fixture = Fixture::workspace();
    fixture.set_manifest_version("0.2.2");
    let before = fixture.snapshot();
    let out = fixture.guard(&["report"], &tool_env(NEEDS_MAJOR));
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert_eq!(
        stderr_text(&out),
        "Error: libplasmoid-updater: the change against v0.2.1 needs a major release (a minor bump while below 1.0), but manifest version 0.2.2 is only a minor release (a patch bump while below 1.0); bump to at least 0.3.0\n",
        "{}",
        text(&out)
    );
    assert_eq!(stdout_text(&out), "");
    assert_eq!(fixture.snapshot(), before, "the report mutated state");
}

#[test]
fn the_report_compares_one_point_x_bumps_by_release_type() {
    let fixture = Fixture::workspace_at("1.4.2");
    let too_small = run_report(
        &fixture,
        &Report {
            manifest: "1.4.3",
            tool: NEEDS_MINOR,
        },
    );
    assert_eq!(too_small.status.code(), Some(1), "{}", text(&too_small));
    assert_eq!(
        stderr_text(&too_small),
        "Error: libplasmoid-updater: the change against v1.4.2 needs a minor release, but manifest version 1.4.3 is only a patch release; bump to at least 1.5.0\n"
    );
    let enough = run_report(
        &fixture,
        &Report {
            manifest: "1.5.0",
            tool: NEEDS_MINOR,
        },
    );
    assert_eq!(enough.status.code(), Some(0), "{}", text(&enough));
    let major_too_small = run_report(
        &fixture,
        &Report {
            manifest: "1.5.0",
            tool: NEEDS_MAJOR,
        },
    );
    assert_eq!(major_too_small.status.code(), Some(1));
    assert_eq!(
        stderr_text(&major_too_small),
        "Error: libplasmoid-updater: the change against v1.4.2 needs a major release, but manifest version 1.5.0 is only a minor release; bump to at least 2.0.0\n"
    );
}

#[test]
fn the_report_only_fails_on_a_too_small_bump() {
    let annotation = |why: &str| {
        format!(
            "::warning::report-only: {why}; no verdict (this job fails only when the manifest version is bumped to a type too small for the change)\n"
        )
    };
    // a check that cannot complete
    let fixture = Fixture::workspace();
    let out = fixture.guard(&["report"], &[env("FAKE_SEMVER_EXIT", "101")]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        stdout_text(&out),
        annotation("the check against v0.2.1 could not finish"),
        "a skipped verdict must surface as an annotation naming why:\n{}",
        text(&out)
    );
    // origin unreachable
    let out = fixture.guard(
        &["report"],
        &[
            env("FAKE_SEMVER_EXIT", "100"),
            env("FAKE_GIT_FAIL_CMD", "ls-remote"),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        stdout_text(&out),
        annotation("no usable baseline tag"),
        "{}",
        text(&out)
    );
    // no release tag yet
    fixture.delete_previous_tag();
    let out = fixture.guard(&["report"], &[env("FAKE_SEMVER_EXIT", "100")]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        stdout_text(&out),
        annotation("no usable baseline tag"),
        "{}",
        text(&out)
    );
}

#[test]
fn the_ci_semver_step_reports_and_fails_only_on_a_too_small_bump() {
    let step = workflow_step("ci.yml", REPORT_STEP);
    let fixture = Fixture::workspace();
    let not_bumped = fixture.run_step(&step, &tool_env(NEEDS_MAJOR));
    assert_eq!(not_bumped.status.code(), Some(0), "{}", text(&not_bumped));
    assert!(
        stdout_text(&not_bumped).contains("needs a major release"),
        "{}",
        text(&not_bumped)
    );
    fixture.set_manifest_version("0.2.2");
    let too_small = fixture.run_step(&step, &tool_env(NEEDS_MAJOR));
    assert_eq!(too_small.status.code(), Some(1), "{}", text(&too_small));
}

#[test]
fn the_manifest_version_mode_reads_the_workspace_version() {
    let fixture = Fixture::workspace_at("3.4.5");
    let out = fixture.guard(&["manifest-version"], &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(stdout_text(&out), "3.4.5\n");
}

/// A shell variable of the guard script, read by sourcing the real file.
fn guard_variable(name: &str) -> String {
    let out = Command::new(system_program("bash"))
        .args([
            "-c",
            ". \"$1\" && printf %s \"${!2}\"",
            "sourcing",
            &guard_script().display().to_string(),
            name,
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    stdout_text(&out)
}

#[test]
fn every_semver_check_runs_on_the_toolchain_both_workflows_pin() {
    let toolchain = guard_variable("SEMVER_TOOLCHAIN");
    assert!(!toolchain.is_empty(), "the guard names no semver toolchain");
    let workflows = repo_root().join(".github/workflows");
    let read = |name: &str| fs::read_to_string(workflows.join(name)).unwrap();
    let release = read("release.yml");
    let ci = read("ci.yml");
    for (workflow, pins) in [
        ("release.yml", semver_pins(&job_lines(&release, "validate"))),
        ("ci.yml", semver_pins(&job_lines(&ci, "semver"))),
    ] {
        assert_eq!(
            pins.0.as_deref(),
            Some(toolchain.as_str()),
            "{workflow}'s semver toolchain must be the guard's SEMVER_TOOLCHAIN"
        );
    }

    // release.sh's local check, the guard's check mode and the CI report all
    // run the tool through that toolchain
    let fixture = Fixture::workspace();
    let out = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(fixture.semver_toolchains(), vec![toolchain.clone()]);
    let checked = fixture.guard(
        &["check", "v0.2.1", "minor"],
        &[env("FAKE_SEMVER_EXIT", "0")],
    );
    assert_eq!(checked.status.code(), Some(0), "{}", text(&checked));
    let reported = fixture.guard(&["report"], &tool_env(NEEDS_MAJOR));
    assert_eq!(reported.status.code(), Some(0), "{}", text(&reported));
    let calls = fixture.semver_toolchains();
    assert_eq!(
        calls.len(),
        1 + 1 + 3,
        "release, check mode, report's three tries"
    );
    assert!(
        calls.iter().all(|call| *call == toolchain),
        "a check ran without the pinned toolchain: {calls:?}"
    );
}

/// A workspace released at `VERSION` (tag on origin and local, no github
/// release visible) with an unrelated commit on top; returns the fixture, the
/// commit the tag names and HEAD.
fn released_then_an_unrelated_commit() -> (Fixture, String, String) {
    let fixture = Fixture::workspace();
    let released = fixture.run(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        0,
    );
    assert_eq!(released.status.code(), Some(0), "{}", text(&released));
    let tagged = fixture.rev_parse("v0.2.2^{commit}");
    fs::write(fixture.repo.join("extra.txt"), "wip\n").unwrap();
    fixture.git(&["add", "extra.txt"]);
    fixture.git(&["commit", "--quiet", "-m", "wip"]);
    let head = fixture.head();
    assert_ne!(tagged, head);
    (fixture, tagged, head)
}

#[test]
fn a_retry_refuses_when_the_released_tag_names_another_commit() {
    for (where_, drop_local, drop_origin) in [
        ("local", false, false),
        ("origin", true, false),
        ("local", false, true),
    ] {
        let (fixture, tagged, head) = released_then_an_unrelated_commit();
        if drop_local {
            fixture.git(&["tag", "--delete", "v0.2.2"]);
        }
        if drop_origin {
            fixture.git(&["push", "--quiet", "origin", "--delete", "v0.2.2"]);
        }
        let before = fixture.snapshot();
        let remote_head = fixture.remote_head();
        let tags = fixture.tags(&fixture.remote);
        let checks = fixture.semver_argv_lines();

        let out = fixture.run(
            &fixture.path_with_tool,
            &[
                "--retry",
                "--workspace",
                "--notes",
                "docs/RELEASE_NOTES.md",
                VERSION,
            ],
            0,
        );
        let log = text(&out);
        assert_eq!(out.status.code(), Some(1), "{where_}: {log}");
        assert_eq!(
            error_lines(&stderr_text(&out)),
            vec![format!(
                "Error: tag v0.2.2 on {where_} names commit {tagged}, but HEAD is {head}; --retry did not start and nothing was pushed"
            )],
            "{where_}: {log}"
        );
        let stderr = stderr_text(&out);
        assert!(
            stderr.lines().all(|line| line.starts_with("Error:")),
            "the refusal states facts and prints no command line:\n{stderr}"
        );
        assert_eq!(printed_commands(&stderr), Vec::<String>::new());
        assert_eq!(
            fixture.remote_head(),
            remote_head,
            "{where_}: the edit was pushed"
        );
        assert_eq!(fixture.tags(&fixture.remote), tags);
        assert_eq!(
            fixture.semver_argv_lines(),
            checks,
            "{where_}: the check ran"
        );
        assert_eq!(fixture.snapshot(), before, "{where_}: {log}");
    }
}

#[test]
fn a_retry_at_the_commit_its_tag_names_still_proceeds() {
    let fixture = Fixture::workspace();
    let args = ["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION];
    let released = fixture.run(&fixture.path_with_tool, &args, 0);
    assert_eq!(released.status.code(), Some(0), "{}", text(&released));
    let out = fixture.run(
        &fixture.path_with_tool,
        &[
            "--retry",
            "--workspace",
            "--notes",
            "docs/RELEASE_NOTES.md",
            VERSION,
        ],
        0,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("Done: released v0.2.2"),
        "{}",
        text(&out)
    );
}

/// Runs release.sh with `args` and asserts it refuses with exactly `expected`
/// as its only stderr line, before the plan and its prompt, with no check run
/// and nothing changed (tree, refs, config, remote).
fn assert_refused_before_any_change(fixture: &Fixture, case: &str, args: &[&str], expected: &str) {
    let before = fixture.snapshot();
    let out = fixture.run(&fixture.path_with_tool, args, 0);
    let log = text(&out);
    assert_eq!(out.status.code(), Some(1), "{case}: {log}");
    assert_eq!(
        stderr_text(&out).lines().collect::<Vec<_>>(),
        vec![expected],
        "{case}: the refusal must be exactly one line naming why:\n{log}"
    );
    assert!(
        !stdout_text(&out).contains("Release plan"),
        "{case}: the refusal must come before the plan and its prompt:\n{log}"
    );
    assert!(
        !fixture.semver_log.exists(),
        "{case}: the check ran:\n{log}"
    );
    assert_eq!(fixture.snapshot(), before, "{case}: {log}");
}

/// The commit `rev` names in the bare remote.
fn remote_commit(fixture: &Fixture, rev: &str) -> String {
    fixture
        .git_stdout(&fixture.remote, &["rev-parse", rev])
        .trim()
        .to_string()
}

fn tag_exists_line(place: &str, commit: &str) -> String {
    format!(
        "Error: tag v0.2.2 already exists {place} (commit {commit}) but has no GitHub release; a tag is never reused, so release the next version; the release did not start"
    )
}

#[test]
fn a_plain_run_refuses_a_tag_origin_already_has() {
    let plain = ["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION];

    // pushed from another clone, on a commit this clone never fetched
    let fixture = Fixture::with_remote_only_tag("v0.2.2");
    let tagged = remote_commit(&fixture, "v0.2.2^{commit}");
    assert_ne!(tagged, fixture.head());
    assert_refused_before_any_change(
        &fixture,
        "origin tag on another commit",
        &plain,
        &tag_exists_line("on origin", &tagged),
    );

    // a hand-pushed tag at HEAD, whose version is still the previous one
    let fixture = Fixture::with_remote_tag_on_known_commit("v0.2.2");
    let head = fixture.head();
    assert_eq!(remote_commit(&fixture, "v0.2.2^{commit}"), head);
    assert_refused_before_any_change(
        &fixture,
        "origin tag at HEAD",
        &plain,
        &tag_exists_line("on origin", &head),
    );

    // the same tag fetched into this clone too: origin is the one named
    fixture.git(&["fetch", "--quiet", "origin", "tag", "v0.2.2"]);
    assert_refused_before_any_change(
        &fixture,
        "tag on origin and in this clone",
        &plain,
        &tag_exists_line("on origin", &head),
    );

    // this clone's own v0.2.2 on another commit than origin's: origin's is named
    let fixture = Fixture::with_remote_only_tag("v0.2.2");
    let tagged = remote_commit(&fixture, "v0.2.2^{commit}");
    fixture.git(&["tag", "v0.2.2", "-m", "local"]);
    assert_ne!(fixture.rev_parse("v0.2.2^{commit}"), tagged);
    assert_refused_before_any_change(
        &fixture,
        "origin and this clone disagree",
        &plain,
        &tag_exists_line("on origin", &tagged),
    );
}

#[test]
fn a_retry_after_a_failed_tag_push_resumes_from_the_local_tag() {
    let fixture = Fixture::workspace();
    fs::write(fixture.repo.join("Cargo.toml"), workspace_manifest(VERSION)).unwrap();
    fs::write(
        fixture.repo.join("docs/RELEASE_NOTES.md"),
        "# Release notes\n\n## 0.2.2\n\n- Add a thing (#12)\n",
    )
    .unwrap();
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "--quiet", "-m", "chore: release v0.2.2"]);
    fixture.git(&["push", "--quiet", "-u", "origin", "main"]);
    fixture.git(&["tag", "v0.2.2", "-m", "release v0.2.2"]);
    let out = fixture.run(
        &fixture.path_with_tool,
        &[
            "--retry",
            "--workspace",
            "--notes",
            "docs/RELEASE_NOTES.md",
            VERSION,
        ],
        0,
    );
    let log = text(&out);
    assert_eq!(out.status.code(), Some(0), "{log}");
    assert!(log.contains("Skipping local tag (v0.2.2 exists)"), "{log}");
    assert!(log.contains("Done: released v0.2.2"), "{log}");
    assert_eq!(fixture.tags(&fixture.remote), "v0.2.1\nv0.2.2", "{log}");
}

#[test]
fn a_plain_run_refuses_a_tag_only_this_clone_has() {
    let plain = ["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION];

    // an annotated tag at HEAD: the line names the commit, not the tag object
    let fixture = Fixture::workspace();
    fixture.git(&["tag", "v0.2.2", "-m", "stale"]);
    let head = fixture.head();
    assert_ne!(
        fixture.rev_parse("v0.2.2"),
        head,
        "the tag must be annotated"
    );
    assert_refused_before_any_change(
        &fixture,
        "local annotated tag at HEAD",
        &plain,
        &tag_exists_line("in this clone", &head),
    );

    // a lightweight tag on a commit no branch holds
    let fixture = Fixture::workspace();
    fixture.git(&["commit", "--quiet", "--allow-empty", "-m", "abandoned"]);
    let abandoned = fixture.head();
    fixture.git(&["tag", "v0.2.2"]);
    fixture.git(&["reset", "--quiet", "--hard", "HEAD~1"]);
    assert_ne!(fixture.head(), abandoned);
    assert_refused_before_any_change(
        &fixture,
        "local lightweight tag on another commit",
        &plain,
        &tag_exists_line("in this clone", &abandoned),
    );

    // an annotated tag on a tree peels to no commit: it still blocks `git
    // tag`, so it refuses too, naming the object it peels to, with nothing of
    // git's own on stderr
    let fixture = Fixture::workspace();
    let tree = fixture.rev_parse("HEAD^{tree}");
    fixture.git(&["tag", "v0.2.2", "-m", "stale", &tree]);
    assert_refused_before_any_change(
        &fixture,
        "local annotated tag on a tree",
        &plain,
        &tag_exists_line("in this clone", &tree),
    );
}

#[test]
fn a_retry_refuses_a_tag_at_head_whose_version_was_never_bumped() {
    let retry = [
        "--retry",
        "--workspace",
        "--notes",
        "docs/RELEASE_NOTES.md",
        VERSION,
    ];
    let line = |place: &str| {
        format!(
            "Error: tag v0.2.2 {place} names HEAD, but HEAD's version is still 0.2.1, so no release run made it; --retry did not start and nothing was pushed: release the next version"
        )
    };

    let fixture = Fixture::with_remote_tag_on_known_commit("v0.2.2");
    assert_refused_before_any_change(&fixture, "origin tag at HEAD", &retry, &line("on origin"));

    fixture.git(&["fetch", "--quiet", "origin", "tag", "v0.2.2"]);
    assert_refused_before_any_change(
        &fixture,
        "tag at HEAD on origin and in this clone",
        &retry,
        &line("on origin"),
    );

    let fixture = Fixture::workspace();
    fixture.git(&["tag", "v0.2.2", "-m", "stale"]);
    assert_refused_before_any_change(
        &fixture,
        "local tag at HEAD",
        &retry,
        &line("in this clone"),
    );
}

#[test]
fn the_report_glosses_a_major_release_at_0_0_x() {
    let fixture = Fixture::workspace_at("0.0.1");
    let out = run_report(
        &fixture,
        &Report {
            manifest: "0.0.1",
            tool: NEEDS_MAJOR,
        },
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        stdout_text(&out),
        "libplasmoid-updater: the change against v0.0.1 needs a major release (any bump while at 0.0.x); the manifest is at 0.0.1, not bumped yet\n"
    );
}

#[test]
fn a_missing_semver_toolchain_refuses_before_any_check() {
    let toolchain = guard_variable("SEMVER_TOOLCHAIN");
    let message = format!(
        "Error: the libplasmoid-updater API check needs the {toolchain} toolchain, which is not installed (install: rustup toolchain install {toolchain})"
    );
    let probe = format!("run {toolchain} rustc --version");
    let none_installed = env("FAKE_RUSTUP_INSTALLED", "");
    let fixture = Fixture::workspace();
    let before = fixture.snapshot();

    let out = fixture.run_with_env(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        std::slice::from_ref(&none_installed),
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert_eq!(error_lines(&stderr_text(&out)), vec![message.clone()]);
    assert_eq!(
        fixture.rustup_calls(),
        vec![probe.clone()],
        "the probe must not install anything"
    );
    assert!(!fixture.semver_log.exists(), "the tool ran");
    assert!(fixture.semver_toolchains().is_empty(), "cargo ran the tool");
    assert_eq!(fixture.snapshot(), before, "{}", text(&out));

    let checked = fixture.guard(
        &["check", "v0.2.1", "minor"],
        std::slice::from_ref(&none_installed),
    );
    assert_eq!(checked.status.code(), Some(1), "{}", text(&checked));
    assert_eq!(error_lines(&stderr_text(&checked)), vec![message.clone()]);
    assert!(!fixture.semver_log.exists(), "the tool ran");

    let reported = fixture.guard(&["report"], &[none_installed]);
    assert_eq!(reported.status.code(), Some(0), "{}", text(&reported));
    assert_eq!(
        stdout_text(&reported),
        format!(
            "::warning::report-only: the {toolchain} toolchain is not installed; no verdict (this job fails only when the manifest version is bumped to a type too small for the change)\n"
        )
    );
    assert!(!fixture.semver_log.exists(), "the tool ran");
    assert!(
        fixture.rustup_calls().iter().all(|call| *call == probe),
        "rustup was asked for more than the probe: {:?}",
        fixture.rustup_calls()
    );

    // the same sandbox with the toolchain installed releases
    let installed = fixture.run_with_env(
        &fixture.path_with_tool,
        &["--workspace", "--notes", "docs/RELEASE_NOTES.md", VERSION],
        &[env("FAKE_RUSTUP_INSTALLED", &toolchain)],
    );
    assert_eq!(installed.status.code(), Some(0), "{}", text(&installed));
    assert_eq!(fixture.semver_toolchains(), vec![toolchain]);
}

impl Fixture {
    /// Imports a `git fast-import` stream into the repo (refs move, the
    /// worktree and index stay).
    fn fast_import(&self, stream: &str) {
        let mut child = Command::new(system_program("git"))
            .arg("-C")
            .arg(&self.repo)
            .args(["fast-import", "--quiet"])
            .env_clear()
            .env("PATH", BASE_PATH)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", self.home.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stream.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "git fast-import failed");
    }
}

#[test]
fn a_retry_finds_the_release_commit_at_the_top_of_a_log_longer_than_a_pipe_buffer() {
    let fixture = Fixture::workspace();
    // 250 commits with ~1 KB subjects between the baseline tag and the release
    // commit: `git log -n 200` is far larger than a pipe buffer, so a reader
    // that stops at the first match (the release subject, on top) leaves the
    // writer to die of SIGPIPE
    let mut stream = String::new();
    let mut parent = fixture.head();
    for index in 0..250 {
        let subject = format!("{} filler {index}", "x".repeat(1000));
        stream += &format!(
            "commit refs/heads/main\nmark :{mark}\ncommitter T <t@example.invalid> {time} +0000\ndata {len}\n{subject}\nfrom {parent}\n\n",
            mark = index + 1,
            time = 1_700_000_000 + index,
            len = subject.len(),
        );
        parent = format!(":{}", index + 1);
    }
    fixture.fast_import(&stream);
    fs::write(fixture.repo.join("Cargo.toml"), workspace_manifest(VERSION)).unwrap();
    fs::write(
        fixture.repo.join("docs/RELEASE_NOTES.md"),
        "# Release notes\n\n## 0.2.2\n\n- Add a thing (#12)\n",
    )
    .unwrap();
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "--quiet", "-m", "chore: release v0.2.2"]);
    fixture.git(&["push", "--quiet", "-u", "origin", "main"]);
    fixture.git(&["tag", "v0.2.2", "-m", "release v0.2.2"]);
    fixture.git(&["push", "--quiet", "origin", "v0.2.2"]);

    let window = fixture
        .git(&["log", "-n", "200", "--format=%s", "--"])
        .stdout;
    assert!(
        window.len() > 150_000,
        "the log window must outgrow a pipe buffer, got {} bytes",
        window.len()
    );
    assert!(window.starts_with(b"chore: release v0.2.2\n"));

    let out = fixture.run(
        &fixture.path_with_tool,
        &[
            "--retry",
            "--workspace",
            "--notes",
            "docs/RELEASE_NOTES.md",
            VERSION,
        ],
        0,
    );
    let log = text(&out);
    assert_eq!(out.status.code(), Some(0), "{log}");
    let lines: Vec<&str> = log.lines().collect();
    for expected in [
        "Skipping commit (chore: release v0.2.2 already in history)",
        "Skipping git push (origin already at HEAD)",
        "Skipping local tag (v0.2.2 exists)",
        "Skipping tag push (v0.2.2 on origin)",
        "Done: released v0.2.2",
    ] {
        assert!(lines.contains(&expected), "missing {expected:?}:\n{log}");
    }
    assert!(
        !log.contains("no staged changes"),
        "the release commit in history was read as absent:\n{log}"
    );
}
