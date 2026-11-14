//! Structural checks on the release and CI workflows' YAML: job wiring,
//! permissions, pins and flags that running a step's script cannot observe.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod workflow_text;

use std::fs;

use workflow_text::{CRATES_STEP, UPLOAD_STEP, entry, job_lines, repo_root, step_script, workflow};

const CRATES_JOB: &str = "crates";

/// The value of a job-level `key:` line.
fn job_key(job: &[&str], key: &str) -> Option<String> {
    let prefix = format!("    {key}: ");
    job.iter()
        .find_map(|line| line.strip_prefix(prefix.as_str()).map(str::to_string))
}

/// The `cargo publish` lines of a script that skip the committed lockfile
/// (no `--locked`) or build the packaged crate (no `--no-verify`).
fn unlocked_or_building_publishes(script: &str) -> Vec<&str> {
    script
        .lines()
        .filter(|line| {
            line.contains("cargo publish")
                && !["--locked", "--no-verify"]
                    .iter()
                    .all(|flag| line.split_whitespace().any(|word| word == *flag))
        })
        .collect()
}

/// The name of every job under a workflow's `jobs:` key, up to the next
/// top-level key. A job key that is not a plain `name:` line (a trailing
/// comment, a flow mapping, an alias) panics.
fn job_names(yaml: &str) -> Vec<&str> {
    yaml.lines()
        .skip_while(|line| line.trim_end() != "jobs:")
        .skip(1)
        .take_while(|line| line.is_empty() || line.starts_with([' ', '#']))
        .filter(|line| {
            line.starts_with("  ")
                && !line.starts_with("   ")
                && !line.trim_start().starts_with('#')
        })
        .map(|line| {
            line.trim_start()
                .strip_suffix(':')
                .filter(|name| {
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                })
                .unwrap_or_else(|| panic!("a job key this reader cannot parse: {line:?}"))
        })
        .collect()
}

/// Whether a `permissions` key grants `id-token: write`: `block` holds the
/// lines nested under it, `inline` the value on the key's own line. An
/// inline value other than the empty `{}`, and a block line that is not a
/// plain `scope: read|write|none`, count as a grant: this reader parses
/// neither.
fn grants_oidc(inline: &str, block: &[&str]) -> bool {
    !matches!(inline, "" | "{}")
        || block.iter().filter(|grant| !grant.is_empty()).any(|grant| {
            match grant.split_once(": ") {
                Some((scope, level))
                    if scope.chars().all(|c| c.is_ascii_lowercase() || c == '-') =>
                {
                    !matches!(level, "read" | "write" | "none")
                        || (scope == "id-token" && level == "write")
                }
                _ => true,
            }
        })
}

/// The jobs of a workflow that can ask GitHub for an OIDC token: a job's own
/// `permissions` replaces the workflow's, and a job without one inherits it.
/// A block is read past its blank and comment lines; a job's `permissions`
/// key that carries a comment on its own line is read as an empty block, so
/// a caller that needs every spelling also counts the lines of the file
/// that name the scope.
fn oidc_jobs(yaml: &str) -> Vec<&str> {
    let top: Vec<&str> = yaml
        .lines()
        .skip_while(|line| !line.starts_with("permissions:"))
        .skip(1)
        .take_while(|line| line.starts_with(' ') || line.trim().is_empty() || line.starts_with('#'))
        .map(|line| line.split('#').next().unwrap().trim())
        .filter(|grant| !grant.is_empty())
        .collect();
    let top_inline = yaml
        .lines()
        .find_map(|line| line.strip_prefix("permissions:"))
        .map_or("", |value| value.split('#').next().unwrap().trim());
    let inherited = grants_oidc(top_inline, &top);
    job_names(yaml)
        .into_iter()
        .filter(|name| {
            let job = job_lines(yaml, name);
            match job
                .iter()
                .find_map(|line| line.strip_prefix("    permissions:"))
            {
                Some(inline) => grants_oidc(
                    inline.split('#').next().unwrap().trim(),
                    &job_permissions(&job),
                ),
                None => inherited,
            }
        })
        .collect()
}

/// The keys of each step of a job, in order: what follows the step's `-`
/// on its own line (nothing when the dash stands alone or before a
/// comment), then its own eight-space keys, comments left out.
fn step_keys<'a>(job: &[&'a str]) -> Vec<Vec<&'a str>> {
    let mut steps: Vec<Vec<&'a str>> = Vec::new();
    for line in job {
        if let Some(first) = line.strip_prefix("      ").and_then(entry) {
            steps.push(if first.is_empty() || first.starts_with('#') {
                Vec::new()
            } else {
                vec![first]
            });
        } else if let (Some(step), Some(key)) = (steps.last_mut(), line.strip_prefix("        "))
            && !key.starts_with([' ', '#'])
        {
            step.push(key);
        }
    }
    steps
}

/// The job's own four-space keys, in order.
fn job_keys<'a>(job: &[&'a str]) -> Vec<&'a str> {
    job.iter()
        .filter_map(|line| line.strip_prefix("    "))
        .filter(|key| !key.starts_with([' ', '#']))
        .map(|key| key.split(':').next().unwrap())
        .collect()
}

/// A job's steps in order: the action a `uses:` step runs (its `@ref` cut),
/// or the name of a step that runs a script.
fn job_steps<'a>(job: &[&'a str]) -> Vec<&'a str> {
    step_keys(job)
        .iter()
        .map(|keys| {
            let value = |prefix: &str| keys.iter().find_map(|key| key.strip_prefix(prefix));
            if let Some(action) = value("uses: ") {
                action.split('@').next().unwrap()
            } else if let Some(name) = value("name: ") {
                name.split("  #").next().unwrap().trim()
            } else {
                "<unnamed step>"
            }
        })
        .collect()
}

/// A line or a whole text in lower case, each run of blanks one space, its
/// ends cut, and no blank left beside a `.`, `[` or `(` or before a `)`: the
/// places an expression may hold one, a line break included, inside the name
/// it reads.
fn expression_text(text: &str) -> String {
    let opens = |c: char| matches!(c, '.' | '[' | '(');
    let joins = |c: char| opens(c) || c == ')';
    let mut joined = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !(joined.is_empty() || joined.ends_with(opens) || word.starts_with(joins)) {
            joined.push(' ');
        }
        joined.push_str(word);
    }
    joined.make_ascii_lowercase();
    joined
}

/// The text inside each `${{ … }}` of a workflow, as expression text: one
/// folded across lines and one on a `#` line count, since an expression is
/// expanded in a script's comment too; prose outside one does not. A `}}`
/// inside a `'…'` string literal ends none, and one that never closes runs
/// to the end of the text.
fn expressions(yaml: &str) -> Vec<String> {
    let text = expression_text(yaml);
    let mut spans = Vec::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("${{") {
        let body = &rest[start + 3..];
        let mut quoted = false;
        let end = body
            .char_indices()
            .find(|&(at, c)| {
                quoted ^= c == '\'';
                !quoted && body[at..].starts_with("}}")
            })
            .map_or(body.len(), |(at, _)| at);
        spans.push(body[..end].to_string());
        rest = &body[end..];
    }
    spans
}

/// The expressions of a workflow that name a repository secret, or the
/// `github` context other than by exactly its `token` or `ref` field, and
/// those this reader cannot place: one holding another `${{` (opened in a
/// comment) or a doubled quote (a single-quoted YAML scalar, or an
/// expression's own `''`).
fn stray_expressions(yaml: &str) -> Vec<String> {
    let field_ends =
        |rest: &str| !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_');
    expressions(yaml)
        .into_iter()
        .filter(|span| {
            span.contains("secrets")
                || span.contains("${{")
                || span.contains("''")
                || !span.split("github").skip(1).all(|after| {
                    [".token", ".ref"]
                        .iter()
                        .any(|field| after.strip_prefix(field).is_some_and(field_ends))
                })
        })
        .collect()
}

/// How often a workflow's text holds `context` (given in lower case, without
/// blanks) once it is one expression text, prose and comments included: the
/// net under `stray_expressions`, which reads no span and so misplaces none.
fn reads(yaml: &str, context: &str) -> usize {
    expression_text(yaml).matches(context).count()
}

/// The lines of a workflow that grant a scope `write` on the key's own line,
/// however the grant is spaced or quoted, comments left out.
fn write_grant_lines(yaml: &str) -> usize {
    yaml.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter(|line| {
            let comment_at = [" #", "\t#"]
                .iter()
                .filter_map(|mark| line.find(mark))
                .min();
            line[..comment_at.unwrap_or(line.len())]
                .chars()
                .filter(|c| !c.is_whitespace() && !matches!(c, '"' | '\''))
                .collect::<String>()
                .contains(":write")
        })
        .count()
}

/// The lines of a job that run `cargo`, comments left out.
fn cargo_lines<'a>(job: &[&'a str]) -> Vec<&'a str> {
    job.iter()
        .map(|line| line.trim())
        .filter(|line| !line.starts_with('#'))
        .filter(|line| {
            line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                .any(|word| word == "cargo")
        })
        .collect()
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
    for next in ["- name: b", "-\n    name: b"] {
        let no_run = format!("steps:\n  - name: a\n    uses: x\n  {next}\n    run: echo b\n");
        assert!(
            std::panic::catch_unwind(|| step_script(&no_run, "a")).is_err(),
            "a step without a run must not borrow the next step's: {next:?}"
        );
    }
}

#[test]
fn the_job_readers_find_the_keys_they_are_given() {
    let yaml = "jobs:\n  a:\n  # note\n    if: x\n  b:\n    if: y\n";
    let a = job_lines(yaml, "a");
    assert_eq!(
        job_key(&a, "if").as_deref(),
        Some("x"),
        "a comment line inside a job does not end it"
    );
    let b = job_lines(yaml, "b");
    assert_eq!(job_key(&b, "if").as_deref(), Some("y"));
}

#[test]
fn the_publish_flag_scan_flags_what_it_must() {
    let script = "cargo publish --locked -p a\ncargo publish --no-verify -p b\ncargo publish --locked --no-verify -p c\necho cargo\n";
    assert_eq!(
        unlocked_or_building_publishes(script),
        [
            "cargo publish --locked -p a",
            "cargo publish --no-verify -p b"
        ]
    );
}

#[test]
fn the_oidc_readers_find_every_job_that_can_ask_for_a_token() {
    let own = "name: x\npermissions:\n  contents: read\n\njobs:\n  # a note\n  plain:\n    steps: []\n  token:\n    permissions:\n      contents: read\n      id-token: write  # oidc\n    steps:\n      - uses: a/b@v1\n        with:\n          permissions: none\n  narrowed:\n    permissions:\n      contents: write\n    steps: []\n  inline:\n    permissions: write-all\n    steps: []\n";
    assert_eq!(job_names(own), ["plain", "token", "narrowed", "inline"]);
    assert_eq!(
        oidc_jobs(own),
        ["token", "inline"],
        "a job's own block decides, and an inline value this reader cannot parse counts as a grant"
    );
    let inherited = "permissions:\n  id-token: write\n\njobs:\n  plain:\n    steps: []\n  narrowed:\n    permissions:\n      contents: read\n    steps: []\n";
    assert_eq!(
        oidc_jobs(inherited),
        ["plain"],
        "a job without its own block inherits the workflow's grant"
    );
    assert_eq!(
        oidc_jobs("permissions: write-all\n\njobs:\n  plain:\n    steps: []\n"),
        ["plain"]
    );
    for grant in [
        "id-token: \"write\"",
        "id-token:  write",
        "\"id-token\": write",
    ] {
        let spelled = format!("jobs:\n  odd:\n    permissions:\n      {grant}\n    steps: []\n");
        assert_eq!(
            oidc_jobs(&spelled),
            ["odd"],
            "a grant this reader cannot parse counts as one: {grant}"
        );
    }
    for unreadable in [
        "jobs:\n  lint: # why\n    steps: []\n",
        "jobs:\n  lint: {steps: []}\n",
    ] {
        assert!(
            std::panic::catch_unwind(|| oidc_jobs(unreadable)).is_err(),
            "a job key the readers would skip must fail, not pass: {unreadable:?}"
        );
    }
    assert_eq!(
        oidc_jobs("jobs:\n  fmt:\n    permissions: {}\n    steps: []\n"),
        Vec::<&str>::new(),
        "an empty inline block grants nothing"
    );
    assert_eq!(
        job_names("jobs:\n  a:\n    steps: []\n# a column-0 note\n  b:\n    steps: []\nname: x\n"),
        ["a", "b"],
        "a comment at column 0 does not end the jobs"
    );
    let steps = "jobs:\n  a:\n    strategy:\n      matrix:\n        include:\n          - target: x\n    steps:\n      - uses: actions/checkout@abc # v7\n        with:\n          name: not a step name\n      - name: Fetch   # why\n        uses: actions/download-artifact@def\n      - name: Build it\n        # cargo build in a comment\n        run: |\n          cargo build --release\n          echo cargo-deny not-cargo\n      - run: make\n";
    let a = job_lines(steps, "a");
    assert_eq!(
        job_steps(&a),
        [
            "actions/checkout",
            "actions/download-artifact",
            "Build it",
            "<unnamed step>"
        ]
    );
    assert_eq!(cargo_lines(&a), ["cargo build --release"]);
    assert_eq!(job_keys(&a), ["strategy", "steps"]);
    assert_eq!(
        step_keys(&a)[2],
        ["name: Build it", "run: |"],
        "a step's keys leave out its comments and its script"
    );
    let bare = "jobs:\n  a:\n    steps:\n      - name: Fetch\n        uses: a/b@v1\n      -\n        uses: c/d@v1\n      -\n        name: Warm\n        run: make warm\n";
    let a = job_lines(bare, "a");
    assert_eq!(
        job_steps(&a),
        ["a/b", "c/d", "Warm"],
        "a dash alone on its line opens a step too"
    );
    assert_eq!(step_keys(&a)[2], ["name: Warm", "run: make warm"]);
}

#[test]
fn the_expression_reader_cuts_the_blanks_an_expression_may_hold() {
    for (line, text) in [
        ("GH: ${{ GITHUB . Token }}", "gh: ${{ github.token }}"),
        ("X: ${{ toJSON ( github ) }}", "x: ${{ tojson(github) }}"),
        ("Y: ${{ github [ 'token' ] }}", "y: ${{ github['token' ] }}"),
        (
            "  # gated on the secret so a fork cannot sign",
            "# gated on the secret so a fork cannot sign",
        ),
    ] {
        assert_eq!(expression_text(line), text);
    }
}

#[test]
fn the_expression_reader_takes_every_span_on_any_line_and_across_lines() {
    let yaml = "jobs:\n  a:  # the token is minted by GitHub (on GitHub)\n    steps:\n      - run: |\n          # ${{ Secrets.X }}\n        env:\n          GH: ${{ github\n            . token }}\n          GRP: ${{ ( github || null ) . token }}\n";
    assert_eq!(
        expressions(yaml),
        [" secrets.x ", " github.token ", "(github || null).token "],
        "a script's `#` line and a fold are read, in lower case; prose outside a span is not"
    );
    assert_eq!(
        expressions("a: ${{ format('}}{0}', matrix.os) }} b: ${{ 'it''s' }}\n"),
        [" format('}}{0}', matrix.os) ", " 'it''s' "],
        "a `}}` inside a string literal ends no span"
    );
}

#[test]
fn an_expression_naming_a_repository_secret_is_stray() {
    assert_eq!(
        stray_expressions("a: ${{ matrix.os }}\n# ${{ format('{0}', Secrets.X) }}\n"),
        [" format('{0}', secrets.x) "]
    );
}

#[test]
fn an_expression_the_reader_cannot_place_is_stray() {
    assert_eq!(
        stray_expressions(
            "a: '${{ format(''}}{0}'', matrix.os) }}'\n# a ${{ won't\nb: |\n  # ${{ format('}}{0}', matrix.os) }}\n"
        ),
        [" format(''", " won't b: | # ${{ format('"],
        "a span that ends where GitHub's does not is stray, whatever it reads"
    );
}

#[test]
fn the_read_count_sees_a_context_named_anywhere_in_the_text() {
    let yaml = "jobs:\n  a:\n    steps:\n      - run: |\n          # ${{ Secrets.X }}\n        env:\n          GH: ${{ github\n            . token }}\n          ALL: ${{ join(github . *, ' ') }}\n          REF: ${{ github.ref }}\n          GRP: ${{ ( github ) . token }}\n";
    for context in ["secrets", "github.token", "github.*", "github)"] {
        assert_eq!(
            reads(yaml, context),
            1,
            "a `#` line, a fold, the all-fields filter and a grouped context are each one read: {context}"
        );
    }
}

#[test]
fn an_expression_naming_the_github_context_past_its_token_or_ref_field_is_stray() {
    let yaml = "a: ${{ github.ref }}\nb: ${{ (github || null).token }}\nc: ${{ github.ref_name }}\nd: ${{ github.token }}\n";
    assert_eq!(
        stray_expressions(yaml),
        ["(github || null).token ", " github.ref_name "],
        "the whole context, and a field that only starts like an allowed one, are stray"
    );
}

#[test]
fn the_write_grant_count_sees_a_grant_however_it_is_quoted() {
    let yaml = "jobs:\n  a:\n    permissions:\n      contents: read\n\n      packages: \"write\"\n      # pages: write\n# actions: write\n    steps:\n      - uses: a/b@v1  # never contents: write\n      - uses: c/d@v1\t# nor pages: write\n";
    assert_eq!(
        write_grant_lines(yaml),
        1,
        "a quoted grant past a blank line counts; a comment, whole-line or trailing, does not"
    );
}

#[test]
fn the_ci_workflow_runs_the_gate_jobs() {
    let ci = workflow("ci.yml");
    assert_eq!(
        job_names(&ci),
        ["fmt", "check", "msrv", "deny", "audit", "coverage"],
        "the CI job list is pinned: a job added or removed in ci.yml is added or removed here"
    );
}

/// Every file in the workflows directory is read, so the tool cannot return
/// through another workflow or another job of one.
#[test]
fn no_workflow_names_semver_checks() {
    let dir = repo_root().join(".github/workflows");
    let mut names = Vec::new();
    for path in fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
    {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("semver-checks"),
            "{name} still names semver-checks"
        );
        names.push(name);
    }
    for pinned in ["ci.yml", "release.yml"] {
        assert!(
            names.iter().any(|name| name.as_str() == pinned),
            "the workflows directory holds {pinned}: {names:?}"
        );
    }
}

/// The `with:` options of the job's `actions/checkout` step.
fn checkout_options<'a>(job: &[&'a str]) -> Vec<&'a str> {
    let Some(checkout) = job
        .iter()
        .position(|line| line.trim().starts_with("- uses: actions/checkout@"))
    else {
        return Vec::new();
    };
    job[checkout + 1..]
        .iter()
        .take_while(|line| entry(line.trim_start()).is_none())
        .map(|line| line.trim())
        .collect()
}

#[test]
fn the_checkout_reader_keeps_to_its_own_step() {
    let own = "jobs:\n  a:\n    steps:\n      - uses: actions/checkout@v1\n        with:\n          fetch-depth: 0\n      - uses: other\n";
    assert_eq!(
        checkout_options(&job_lines(own, "a")),
        ["with:", "fetch-depth: 0"],
        "a checkout's own options are read"
    );
    for next in ["- uses: other", "-\n        uses: other"] {
        let borrowed = format!(
            "jobs:\n  a:\n    steps:\n      - uses: actions/checkout@v1\n      {next}\n        with:\n          fetch-depth: 0\n"
        );
        assert!(
            !checkout_options(&job_lines(&borrowed, "a")).contains(&"fetch-depth: 0"),
            "a checkout must not borrow the next step's options: {next:?}"
        );
    }
}

/// The lines nested under the job's own `permissions:` key, blank and
/// comment lines left out, trailing comments cut.
fn job_permissions<'a>(job: &[&'a str]) -> Vec<&'a str> {
    job.iter()
        .skip_while(|line| line.trim_end() != "    permissions:")
        .skip(1)
        .take_while(|line| {
            line.starts_with("     ")
                || line.trim().is_empty()
                || line.trim_start().starts_with('#')
        })
        .map(|line| line.split('#').next().unwrap().trim())
        .filter(|grant| !grant.is_empty())
        .collect()
}

#[test]
fn the_job_that_builds_the_assets_holds_no_oidc_token() {
    let nested = "jobs:\n  a:\n    steps:\n      - uses: x\n        with:\n          permissions: none\n  b:\n    permissions:\n      contents: read  # tree\n      id-token: write\n    steps: []\n";
    assert!(
        job_permissions(&job_lines(nested, "a")).is_empty(),
        "a step's own `permissions` option is not the job's block"
    );
    assert_eq!(
        job_permissions(&job_lines(nested, "b")),
        ["contents: read", "id-token: write"]
    );
    let spread = "jobs:\n  b:\n    permissions:\n      contents: read\n\n      # why\n      packages:\n        write\n    steps: []\n";
    assert_eq!(
        job_permissions(&job_lines(spread, "b")),
        ["contents: read", "packages:", "write"],
        "a grant past a blank or comment line is read, its level on the next line too"
    );
    let yaml = workflow("release.yml");
    let build = job_lines(&yaml, "build");
    assert_eq!(
        job_permissions(&build),
        ["contents: read"],
        "crates.io's trusted publisher cannot tell one job of this workflow from another, so a job that runs every dependency's build script must not be able to ask for an OIDC token"
    );
    assert!(
        !build
            .iter()
            .any(|line| line.contains("actions/attest-build-provenance@")),
        "the attestation needs `id-token: write`, so it cannot run in the build job"
    );
    let publish = job_lines(&yaml, "publish");
    assert_eq!(
        job_permissions(&publish),
        ["contents: write", "id-token: write", "attestations: write"]
    );
    let at = |needle: &str| {
        publish
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("the publish job has no {needle:?}"))
    };
    let download = at("actions/download-artifact@");
    let attest = at("actions/attest-build-provenance@");
    let upload = at("- name: Upload assets to the release");
    assert!(
        download < attest && attest < upload,
        "the assets are attested once downloaded and before they reach the release"
    );
    let subjects: Vec<&str> = publish[attest..upload]
        .iter()
        .filter_map(|line| line.trim().strip_prefix("subject-path: "))
        .collect();
    assert_eq!(subjects, ["artifacts/plasmoid-updater-*"]);
}

#[test]
fn publish_runs_only_after_a_successful_validate() {
    let yaml = workflow("release.yml");
    let publish = job_lines(&yaml, "publish");
    assert_eq!(
        job_key(&publish, "needs").as_deref(),
        Some("[validate, build]")
    );
    assert_eq!(
        job_key(&publish, "if").as_deref(),
        Some(
            "${{ !cancelled() && needs.validate.result == 'success' && startsWith(github.ref, 'refs/tags/') }}"
        ),
        "a failed validate skips every build leg, so publish must not run to fail on the missing artifacts; a cancelled run uploads nothing, which `always()` would not honour"
    );
}

#[test]
fn every_crates_publish_uses_the_lockfile_and_builds_nothing() {
    let step = step_script(&workflow("release.yml"), CRATES_STEP);
    assert!(
        step.lines().any(|line| line.contains("cargo publish")),
        "the crates step runs no cargo publish:\n{step}"
    );
    assert_eq!(
        unlocked_or_building_publishes(&step),
        Vec::<&str>::new(),
        "every publish resolves against the committed Cargo.lock and skips the packaged crate's build, which would run every dependency's build script beside the registry token"
    );
}

/// Every job of the release workflow is listed, so a new one fails here
/// until it is added with what its token may do, and a listed job that can
/// ask for an OIDC token fails once it gains a step. The readers take the
/// plain spellings only, so the whole file is read too: the OIDC scope's
/// name and a `write` level on its key's own line are counted on every line
/// that is not a comment, and every `${{ … }}` expression, a script's `#`
/// line included, may name the `github` context by exactly its `token` or
/// `ref` field and no repository secret; under that, the text as a whole,
/// prose included, spells no secret and no read of the whole context.
/// What this cannot see: a listed script step edited to compile through
/// something other than `cargo` (a `make`, a script file), an expression
/// whose braces, quotes or context name are written with a YAML escape (a
/// secret so spelled still reds the text-wide count), and a credential an
/// action reads through its own input default.
#[test]
fn no_job_that_can_ask_for_an_oidc_token_runs_dependency_code() {
    let yaml = workflow("release.yml");
    assert_eq!(
        job_names(&yaml),
        ["validate", "build", "package", "publish", CRATES_JOB],
        "a new job is read for what its token may do beside the code it runs before it is listed here"
    );
    assert_eq!(
        oidc_jobs(&yaml),
        ["publish", CRATES_JOB],
        "crates.io's trusted publisher cannot tell one job of this workflow from another, so every job holding `id-token: write` can buy a registry token"
    );
    let publish = job_lines(&yaml, "publish");
    assert_eq!(
        job_steps(&publish),
        [
            "actions/download-artifact",
            "actions/attest-build-provenance",
            "Generate sha256sums.txt",
            UPLOAD_STEP
        ],
        "a step added to a job holding an OIDC token is checked for dependency code before it is listed here"
    );
    assert_eq!(
        cargo_lines(&publish),
        Vec::<&str>::new(),
        "the publish job handles built files only"
    );
    let crates = job_lines(&yaml, CRATES_JOB);
    assert_eq!(
        job_steps(&crates),
        [
            "actions/checkout",
            "dtolnay/rust-toolchain",
            "rust-lang/crates-io-auth-action",
            CRATES_STEP
        ],
        "a step added to a job holding an OIDC token is checked for dependency code before it is listed here"
    );
    assert_eq!(
        cargo_lines(&crates),
        [r#"if cargo publish --locked --no-verify -p "$crate" 2>&1 | tee "$log"; then"#],
        "the only cargo call beside the registry token is a publish that builds nothing"
    );
    let lines_holding = |needle: &str| {
        yaml.lines()
            .filter(|line| !line.trim_start().starts_with('#') && line.contains(needle))
            .count()
    };
    assert_eq!(
        lines_holding("id-token"),
        2,
        "only the publish and crates jobs name the OIDC scope, so a grant spelled past the readers above still fails here"
    );
    assert_eq!(
        lines_holding("write-all"),
        0,
        "`write-all` grants `id-token: write` too"
    );
    assert_eq!(
        write_grant_lines(&yaml),
        4,
        "the publish job's three write scopes and the crates job's one are the file's only write grants on a key's own line"
    );
    for expression in ["secrets", "github[", "toJSON("] {
        assert_eq!(
            lines_holding(expression),
            0,
            "the workflow reads no repository secret, and the repo token by one spelling only: {expression}"
        );
    }
    assert_eq!(
        stray_expressions(&yaml),
        Vec::<String>::new(),
        "an expression names the github context by exactly its `token` or `ref` field, and no repository secret"
    );
    for context in ["secrets", "github[", "github.*", "github)", "tojson("] {
        assert_eq!(
            reads(&yaml, context),
            0,
            "the file's text, prose and comments included, spells no repository secret and the whole github context nowhere: {context}"
        );
    }
    assert_eq!(
        lines_holding("github.token"),
        1,
        "the repo token is read on one line that is not a comment"
    );
    assert_eq!(
        expressions(&yaml)
            .iter()
            .map(|span| span.matches("github.token").count())
            .sum::<usize>(),
        1,
        "the repo token is read by one expression in the whole file"
    );
    let upload_at = publish
        .iter()
        .position(|line| line.trim() == format!("- name: {UPLOAD_STEP}"))
        .expect("the publish job has its upload step");
    assert!(
        publish[upload_at..]
            .iter()
            .take_while(|line| !line.trim_start().starts_with("run:"))
            .any(|line| line.trim() == "GH_TOKEN: ${{ github.token }}"),
        "gh in the upload step reads the repo token from the step's own env"
    );
    assert_eq!(
        lines_holding("CARGO_REGISTRY_TOKEN"),
        1,
        "the registry token is named once in the whole file"
    );
    assert!(
        !yaml.lines().any(|line| line.starts_with("env:")),
        "a workflow-level env reaches the jobs that run dependency code"
    );
    for name in job_names(&yaml) {
        let token_lines = job_lines(&yaml, name)
            .iter()
            .filter(|line| line.contains("CARGO_REGISTRY_TOKEN"))
            .count();
        assert_eq!(
            token_lines,
            usize::from(name == CRATES_JOB),
            "the registry token reaches one step of the crates job and no other job: {name}"
        );
    }
}

/// The build the publish skips runs here, in a job that holds neither the
/// registry token nor an OIDC token to buy one with.
#[test]
fn a_job_without_a_token_builds_both_packages_before_the_crates_publish() {
    let yaml = workflow("release.yml");
    let package = job_lines(&yaml, "package");
    assert_eq!(job_permissions(&package), ["contents: read"]);
    assert_eq!(job_key(&package, "needs").as_deref(), Some("validate"));
    assert_eq!(
        job_key(&package, "if").as_deref(),
        Some("startsWith(github.ref, 'refs/tags/')")
    );
    for secret in ["secrets.", "github.token", "GITHUB_TOKEN"] {
        assert!(
            !package.iter().any(|line| line.contains(secret)),
            "the job that runs every dependency's build script reads no credential: {secret}"
        );
    }
    let options = checkout_options(&package);
    assert!(
        options.contains(&"persist-credentials: false"),
        "the checkout keeps no token for a build script to read: {options:?}"
    );
    for net in [
        "CARGO_NET_RETRY: \"10\"",
        "CARGO_HTTP_MULTIPLEXING: \"false\"",
    ] {
        assert!(
            package.iter().any(|line| line.trim() == net),
            "the package build downloads dependencies the way ci.yml does: {net}"
        );
    }
    assert_eq!(
        job_keys(&package),
        [
            "name",
            "needs",
            "permissions",
            "if",
            "runs-on",
            "env",
            "steps"
        ],
        "a key that lets the job pass without building (`continue-on-error`) is not among them"
    );
    assert_eq!(
        job_steps(&package),
        [
            "actions/checkout",
            "dtolnay/rust-toolchain",
            "Build both crates from their packages"
        ]
    );
    assert_eq!(
        step_keys(&package)[2],
        [
            "name: Build both crates from their packages",
            "run: cargo package --locked --workspace"
        ],
        "one call packages every member against the committed Cargo.lock and builds each package, so the binary's package builds against the library's package before crates.io has it; the step carries no key that skips it or forgives its failure"
    );
    assert_eq!(
        cargo_lines(&package),
        ["run: cargo package --locked --workspace"],
        "the job runs cargo nowhere else"
    );
    let build_at = package
        .iter()
        .position(|line| line.trim() == "- name: Build both crates from their packages")
        .expect("the package job has its build step");
    assert_eq!(
        package[build_at..]
            .iter()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect::<Vec<_>>(),
        [
            "- name: Build both crates from their packages",
            "run: cargo package --locked --workspace"
        ],
        "the step ends the job, and no line continues its command"
    );
    assert_eq!(
        job_key(&job_lines(&yaml, CRATES_JOB), "needs").as_deref(),
        Some("[validate, build, publish, package]"),
        "nothing is published from a package that did not build"
    );
}

#[test]
fn the_crates_job_publishes_through_trusted_publishing_after_the_build() {
    let yaml = workflow("release.yml");
    let job = job_lines(&yaml, CRATES_JOB);
    assert_eq!(
        job_key(&job, "needs").as_deref(),
        Some("[validate, build, publish, package]"),
        "crates.io gets a version only once its GitHub release holds the assets and both packages built"
    );
    for net in [
        "CARGO_NET_RETRY: \"10\"",
        "CARGO_HTTP_MULTIPLEXING: \"false\"",
    ] {
        assert!(
            job.iter().any(|line| line.trim() == net),
            "cargo reaches crates.io the way ci.yml does: {net}"
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
    assert!(
        !job.iter()
            .any(|line| line.trim_start().starts_with("environment:")),
        "crates.io's trusted publisher for this workflow names no environment, so a job that sets one gets no token"
    );
    assert_eq!(
        job_permissions(&job),
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
