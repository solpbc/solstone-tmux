// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::{Value, json};
use support::TestDirectory;
use tar::{Builder, Header};

const VERSION: &str = "1.0.0";
const TAG: &str = "v1.0.0";
const TITLE: &str = "solstone-tmux 1.0.0";
const NOTES: &str = "## [1.0.0] - 2026-07-29\n\n### Added\n- native candidate release.";
type RefusalSetup = for<'a> fn(&'a PublisherFixture) -> RunRequest<'a>;
const TARGETS: [&str; 3] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
];

#[test]
fn state_none_creates_exact_release() {
    let fixture = PublisherFixture::new();
    let output = fixture.run();
    assert_success(&output);
    fixture.assert_exact_published();
    assert_eq!(
        fixture.mutation_kinds().first().map(String::as_str),
        Some("tag")
    );
}

#[test]
fn state_exact_tag_without_release_reuses_tag() {
    let fixture = PublisherFixture::new();
    fixture.set_remote_tag_exact();
    fixture.clear_log();

    assert_success(&fixture.run());
    fixture.assert_exact_published();
    assert!(!fixture.mutation_kinds().iter().any(|kind| kind == "tag"));
}

#[test]
fn state_exact_draft_uploads_only_missing_assets() {
    let fixture = PublisherFixture::new();
    assert_interrupted(&fixture.run_with_interrupt("asset:3"));
    assert_eq!(fixture.release_assets().len(), 3);
    fixture.clear_log();

    assert_success(&fixture.run());
    fixture.assert_exact_published();
    let mutations = fixture.mutation_kinds();
    assert_eq!(
        mutations
            .iter()
            .filter(|kind| kind.as_str() == "asset")
            .count(),
        10
    );
    assert!(
        !mutations
            .iter()
            .any(|kind| kind == "tag" || kind == "draft")
    );
}

#[test]
fn state_exact_published_release_is_idempotent() {
    let fixture = PublisherFixture::new();
    assert_success(&fixture.run());
    fixture.clear_log();

    let output = fixture.run();
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("already published and exact"));
    assert_no_mutations(&fixture);
}

#[test]
fn state_tag_type_or_target_mismatch_is_immutable_red() {
    for mutation in ["lightweight", "wrong-target"] {
        let fixture = PublisherFixture::new();
        fixture.set_remote_tag(mutation);
        fixture.clear_log();

        assert_failure(&fixture.run());
        assert_no_mutations(&fixture);
    }
}

#[test]
fn state_draft_mismatches_are_immutable_red() {
    for mutation in [
        "wrong-target",
        "wrong-title",
        "wrong-notes",
        "wrong-tag",
        "extra-asset",
        "different-asset",
    ] {
        let fixture = PublisherFixture::new();
        let interruption = if mutation == "different-asset" {
            "asset:1"
        } else {
            "draft"
        };
        assert_interrupted(&fixture.run_with_interrupt(interruption));
        fixture.mutate_release(mutation);
        fixture.clear_log();

        assert_failure(&fixture.run());
        assert_no_mutations(&fixture);
    }
}

#[test]
fn state_published_mismatches_are_immutable_red() {
    for mutation in [
        "wrong-target",
        "wrong-title",
        "wrong-notes",
        "wrong-tag",
        "missing-asset",
        "extra-asset",
        "different-asset",
    ] {
        let fixture = PublisherFixture::new();
        assert_success(&fixture.run());
        fixture.mutate_release(mutation);
        fixture.clear_log();

        assert_failure(&fixture.run());
        assert_no_mutations(&fixture);
    }
}

#[test]
fn state_multiple_releases_is_immutable_red() {
    let fixture = PublisherFixture::new();
    fixture.set_remote_tag_exact();
    fixture.set_multiple_releases();
    fixture.clear_log();

    assert_failure(&fixture.run());
    assert_no_mutations(&fixture);
}

#[test]
fn interrupted_exact_states_resume_without_replacement() {
    for interruption in ["tag", "draft", "asset:4"] {
        let fixture = PublisherFixture::new();
        assert_interrupted(&fixture.run_with_interrupt(interruption));
        fixture.clear_log();

        assert_success(&fixture.run());
        fixture.assert_exact_published();
        let log = fixture.log();
        assert!(!log.contains("DELETE"));
        assert!(!log.contains("--clobber"));
    }
}

#[test]
fn every_local_refusal_precedes_any_remote_call() {
    let cases: &[(&str, RefusalSetup)] = &[
        ("dirty tree", |fixture| {
            fs::write(fixture.repo.join("dirty"), b"dirty\n").expect("write dirty file");
            fixture.request()
        }),
        ("malformed commit", |fixture| {
            fixture.request().source_commit("not-a-commit")
        }),
        ("commit differs from HEAD", |fixture| {
            fixture
                .request()
                .source_commit("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        }),
        ("Cargo.toml version", |fixture| {
            fixture.commit_repo_mutation(
                "native/solstone-tmux/Cargo.toml",
                b"[package]\nname = \"solstone-tmux\"\nversion = \"1.0.1\"\n",
            );
            fixture.request()
        }),
        ("Cargo.lock version", |fixture| {
            fixture.commit_repo_mutation(
                "Cargo.lock",
                b"version = 4\n\n[[package]]\nname = \"solstone-tmux\"\nversion = \"1.0.1\"\n",
            );
            fixture.request()
        }),
        ("changelog notes", |fixture| {
            fixture.commit_repo_mutation("CHANGELOG.md", b"# Changelog\n");
            fixture.request()
        }),
        ("candidate file missing", |fixture| {
            fs::remove_file(fixture.candidate.join(&fixture.unsigned_names()[0]))
                .expect("remove candidate file");
            fixture.request()
        }),
        ("candidate extra file", |fixture| {
            fs::write(fixture.candidate.join("extra"), b"extra\n").expect("write extra file");
            fixture.request()
        }),
        ("target record mismatch", |fixture| {
            let record = fixture.candidate.join(format!(
                "solstone-tmux-{VERSION}-{}.target.json",
                TARGETS[0]
            ));
            let mut value: Value = serde_json::from_slice(&fs::read(&record).expect("read record"))
                .expect("parse record");
            value["source_commit"] = Value::String("b".repeat(40));
            fs::write(record, serde_json::to_vec(&value).expect("encode record"))
                .expect("write record");
            fixture.request()
        }),
        ("executable version output", |fixture| {
            fixture.write_executable_tar("printf 'wrong version\\n'");
            fixture.request()
        }),
        ("executable source binding", |fixture| {
            fixture.write_executable_tar(&format!(
                "printf 'solstone-tmux {VERSION} (source {})\\n'",
                "b".repeat(40)
            ));
            fixture.request()
        }),
        ("lightweight local tag", |fixture| {
            fixture.git(&["tag", TAG]);
            fixture.request()
        }),
        ("wrong annotated local tag", |fixture| {
            fixture.git(&[
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "second fixture commit",
            ]);
            fixture.git(&["tag", "-a", TAG, "-m", TITLE, "HEAD^"]);
            fixture.rebuild_candidate();
            fixture.request()
        }),
        ("unsigned validator", |fixture| {
            fixture.request().cargo_fail_at(1)
        }),
        ("complete validator", |fixture| {
            fixture.request().cargo_fail_at(2)
        }),
        ("signature verification", |fixture| {
            fixture.replace_secret_key();
            fixture.request()
        }),
    ];

    for (name, prepare) in cases {
        let fixture = PublisherFixture::new();
        let request = prepare(&fixture);
        fixture.clear_log();
        let output = request.run();
        assert!(
            !output.status.success(),
            "local refusal case {name:?} unexpectedly succeeded"
        );
        assert!(
            fixture.log().is_empty(),
            "local refusal case {name:?} reached gh:\n{}",
            fixture.log()
        );
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "publisher failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "publisher unexpectedly succeeded:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

fn assert_interrupted(output: &Output) {
    assert_failure(output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("native release publisher"),
        "interrupted publisher did not report its bounded failure"
    );
}

fn assert_no_mutations(fixture: &PublisherFixture) {
    let mutations = fixture.mutation_kinds();
    assert!(
        mutations.is_empty(),
        "immutable-red state invoked remote mutations: {mutations:?}\n{}",
        fixture.log()
    );
}

struct PublisherFixture {
    root: TestDirectory,
    repo: PathBuf,
    candidate: PathBuf,
    secret_key: PathBuf,
    fake_bin: PathBuf,
    state: PathBuf,
    remote: PathBuf,
    log_path: PathBuf,
    cargo_count: PathBuf,
    publisher: PathBuf,
}

impl PublisherFixture {
    fn new() -> Self {
        let root = TestDirectory::new("release-publisher");
        let repo = root.path().join("repo");
        let candidate = root.path().join("candidate");
        let fake_bin = root.path().join("bin");
        let remote = root.path().join("remote");
        fs::create_dir_all(repo.join("native/solstone-tmux")).expect("create crate fixture");
        fs::create_dir_all(repo.join("scripts")).expect("create scripts fixture");
        fs::create_dir_all(repo.join("packaging/keys")).expect("create key fixture");
        fs::create_dir_all(&candidate).expect("create candidate fixture");
        fs::create_dir_all(&fake_bin).expect("create fake bin");
        fs::create_dir_all(remote.join("assets")).expect("create fake remote");

        let secret_key = root.path().join("test-only-minisign.key");
        let public_key = repo.join("packaging/keys/solstone-tmux-release.pub");
        let key_output = Command::new("minisign")
            .args(["-G", "-W", "-p"])
            .arg(&public_key)
            .arg("-s")
            .arg(&secret_key)
            .output()
            .expect("generate test minisign key");
        assert!(
            key_output.status.success(),
            "minisign key generation failed: {}",
            String::from_utf8_lossy(&key_output.stderr)
        );

        fs::write(
            repo.join("native/solstone-tmux/Cargo.toml"),
            b"[package]\nname = \"solstone-tmux\"\nversion = \"1.0.0\"\n",
        )
        .expect("write manifest");
        fs::write(
            repo.join("Cargo.lock"),
            b"version = 4\n\n[[package]]\nname = \"solstone-tmux\"\nversion = \"1.0.0\"\n",
        )
        .expect("write lockfile");
        fs::write(
            repo.join("CHANGELOG.md"),
            format!("# Changelog\n\n{NOTES}\n").as_bytes(),
        )
        .expect("write changelog");
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        fs::copy(
            source_root.join("scripts/extract_changelog.sh"),
            repo.join("scripts/extract_changelog.sh"),
        )
        .expect("copy changelog extractor");
        fs::set_permissions(
            repo.join("scripts/extract_changelog.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("chmod changelog extractor");
        fs::write(
            repo.join("scripts/rust-targets.sh"),
            format!(
                "#!/usr/bin/env bash\nprintf '%s\\n' {} \n",
                TARGETS.join(" ")
            ),
        )
        .expect("write target authority");
        fs::set_permissions(
            repo.join("scripts/rust-targets.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("chmod target authority");

        run_checked(
            Command::new("git")
                .arg("init")
                .arg("-q")
                .arg("-b")
                .arg("main")
                .arg(&repo),
        );
        run_checked(Command::new("git").current_dir(&repo).args([
            "config",
            "user.name",
            "Publisher Test",
        ]));
        run_checked(Command::new("git").current_dir(&repo).args([
            "config",
            "user.email",
            "publisher@example.invalid",
        ]));
        run_checked(Command::new("git").current_dir(&repo).args(["add", "."]));
        run_checked(
            Command::new("git")
                .current_dir(&repo)
                .args(["commit", "-q", "-m", "fixture"]),
        );

        let state = remote.join("state.json");
        fs::write(
            &state,
            serde_json::to_vec_pretty(&json!({
                "repo": "solpbc/solstone-tmux",
                "remote_tag": Value::Null,
                "releases": [],
                "next_release_id": 1,
                "next_asset_id": 100,
                "upload_count": 0
            }))
            .expect("encode fake state"),
        )
        .expect("write fake state");
        let log_path = remote.join("gh.log");
        fs::write(&log_path, b"").expect("write gh log");
        let cargo_count = remote.join("cargo.count");

        fs::write(fake_bin.join("gh"), fake_gh()).expect("write fake gh");
        fs::write(fake_bin.join("cargo"), fake_cargo()).expect("write fake cargo");
        for name in ["gh", "cargo"] {
            fs::set_permissions(fake_bin.join(name), fs::Permissions::from_mode(0o755))
                .expect("chmod fake tool");
        }

        let publisher = source_root.join("packaging/publish-release.sh");
        let fixture = Self {
            root,
            repo,
            candidate,
            secret_key,
            fake_bin,
            state,
            remote,
            log_path,
            cargo_count,
            publisher,
        };
        fixture.rebuild_candidate();
        fixture
    }

    fn request(&self) -> RunRequest<'_> {
        RunRequest {
            fixture: self,
            source_commit: self.head(),
            cargo_fail_at: 0,
            interrupt: None,
        }
    }

    fn run(&self) -> Output {
        self.request().run()
    }

    fn run_with_interrupt(&self, interrupt: &str) -> Output {
        self.request().interrupt(interrupt).run()
    }

    fn head(&self) -> String {
        command_stdout(
            Command::new("git")
                .current_dir(&self.repo)
                .args(["rev-parse", "HEAD"]),
        )
    }

    fn git(&self, args: &[&str]) {
        run_checked(Command::new("git").current_dir(&self.repo).args(args));
    }

    fn rebuild_candidate(&self) {
        if self.candidate.exists() {
            fs::remove_dir_all(&self.candidate).expect("clear candidate");
        }
        fs::create_dir(&self.candidate).expect("recreate candidate");
        for name in self.unsigned_names() {
            if name.ends_with("x86_64-linux.tar.gz") {
                self.write_executable_tar(&format!(
                    "printf 'solstone-tmux {VERSION} (source {})\\n'",
                    self.head()
                ));
            } else if name.ends_with(".target.json") {
                let target = TARGETS
                    .iter()
                    .find(|target| name.contains(*target))
                    .expect("record target");
                let record = json!({
                    "schema_version": 1,
                    "product_version": VERSION,
                    "source_commit": self.head(),
                    "rust_target": target,
                    "rustc_vv": "rustc fixture\n",
                    "executable": {
                        "name": "solstone-tmux",
                        "sha256": "a".repeat(64)
                    },
                    "artifacts": []
                });
                fs::write(
                    self.candidate.join(name),
                    serde_json::to_vec(&record).expect("encode record"),
                )
                .expect("write record");
            } else {
                fs::write(
                    self.candidate.join(&name),
                    format!("fixture bytes for {name}\n"),
                )
                .expect("write artifact");
            }
        }
    }

    fn write_executable_tar(&self, body: &str) {
        let name = format!("solstone-tmux-{VERSION}-x86_64-linux.tar.gz");
        let file = fs::File::create(self.candidate.join(name)).expect("create tar fixture");
        let encoder = GzEncoder::new(file, Compression::best());
        let mut archive = Builder::new(encoder);
        let script = format!("#!/usr/bin/env bash\n{body}\n");
        let mut header = Header::new_ustar();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        archive
            .append_data(&mut header, "solstone-tmux", script.as_bytes())
            .expect("append executable fixture");
        archive.finish().expect("finish tar fixture");
    }

    fn unsigned_names(&self) -> Vec<String> {
        let mut names = vec![
            format!("solstone-tmux-{VERSION}-1.aarch64.rpm"),
            format!("solstone-tmux-{VERSION}-1.x86_64.rpm"),
            format!("solstone-tmux-{VERSION}-aarch64-linux.tar.gz"),
            format!("solstone-tmux-{VERSION}-aarch64-macos.pkg"),
            format!("solstone-tmux-{VERSION}-aarch64-macos.tar.gz"),
            format!("solstone-tmux-{VERSION}-x86_64-linux.tar.gz"),
            format!("solstone-tmux_{VERSION}_amd64.deb"),
            format!("solstone-tmux_{VERSION}_arm64.deb"),
        ];
        names.extend(
            TARGETS
                .iter()
                .map(|target| format!("solstone-tmux-{VERSION}-{target}.target.json")),
        );
        names.sort();
        names
    }

    fn commit_repo_mutation(&self, path: &str, bytes: &[u8]) {
        fs::write(self.repo.join(path), bytes).expect("write repository mutation");
        self.git(&["add", path]);
        self.git(&["commit", "-q", "-m", "mutate fixture"]);
        self.rebuild_candidate();
    }

    fn replace_secret_key(&self) {
        let replacement_public = self.root.path().join("replacement.pub");
        let output = Command::new("minisign")
            .args(["-G", "-W", "-p"])
            .arg(replacement_public)
            .arg("-s")
            .arg(&self.secret_key)
            .arg("-f")
            .output()
            .expect("replace secret key");
        assert!(output.status.success(), "replace secret key fixture");
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(&self.state).expect("read fake state"))
            .expect("parse fake state")
    }

    fn write_state(&self, state: &Value) {
        fs::write(
            &self.state,
            serde_json::to_vec_pretty(state).expect("encode fake state"),
        )
        .expect("write fake state");
    }

    fn set_remote_tag_exact(&self) {
        self.set_remote_tag("exact");
    }

    fn set_remote_tag(&self, kind: &str) {
        let mut state = self.state();
        let (object_type, commit) = match kind {
            "exact" => ("tag", self.head()),
            "lightweight" => ("commit", self.head()),
            "wrong-target" => ("tag", "b".repeat(40)),
            _ => panic!("unknown remote tag mutation"),
        };
        state["remote_tag"] = json!({
            "ref": format!("refs/tags/{TAG}"),
            "type": object_type,
            "object_sha": "c".repeat(40),
            "commit": commit
        });
        self.write_state(&state);
    }

    fn set_multiple_releases(&self) {
        let mut state = self.state();
        state["releases"] = json!([
            release_fixture(1, &self.head(), true),
            release_fixture(2, &self.head(), true)
        ]);
        state["next_release_id"] = json!(3);
        self.write_state(&state);
    }

    fn mutate_release(&self, mutation: &str) {
        let mut state = self.state();
        let release = state["releases"][0]
            .as_object_mut()
            .expect("release object");
        match mutation {
            "wrong-target" => {
                release.insert("target_commitish".to_owned(), Value::String("b".repeat(40)));
            }
            "wrong-title" => {
                release.insert("name".to_owned(), Value::String("wrong title".to_owned()));
            }
            "wrong-notes" => {
                release.insert("body".to_owned(), Value::String("wrong notes".to_owned()));
            }
            "wrong-tag" => {
                release.insert("tag_name".to_owned(), Value::String("v9.9.9".to_owned()));
            }
            "missing-asset" => {
                let removed = release
                    .get_mut("assets")
                    .and_then(Value::as_array_mut)
                    .expect("asset array")
                    .pop()
                    .expect("published asset");
                let id = removed["id"].as_u64().expect("asset id");
                let _ = fs::remove_file(self.remote.join("assets").join(id.to_string()));
            }
            "extra-asset" => {
                let id = 90_001;
                release
                    .get_mut("assets")
                    .and_then(Value::as_array_mut)
                    .expect("asset array")
                    .push(json!({"id": id, "name": "extra.asset"}));
                fs::write(self.remote.join("assets").join(id.to_string()), b"extra\n")
                    .expect("write extra remote asset");
            }
            "different-asset" => {
                let id = release["assets"]
                    .as_array()
                    .and_then(|assets| assets.first())
                    .and_then(|asset| asset["id"].as_u64())
                    .expect("remote asset id");
                fs::write(
                    self.remote.join("assets").join(id.to_string()),
                    b"different\n",
                )
                .expect("mutate remote asset");
            }
            _ => panic!("unknown release mutation"),
        }
        self.write_state(&state);
    }

    fn release_assets(&self) -> Vec<Value> {
        self.state()["releases"][0]["assets"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn assert_exact_published(&self) {
        let state = self.state();
        assert_eq!(state["remote_tag"]["type"], "tag");
        assert_eq!(state["remote_tag"]["commit"], self.head());
        let releases = state["releases"].as_array().expect("release array");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0]["tag_name"], TAG);
        assert_eq!(releases[0]["target_commitish"], self.head());
        assert_eq!(releases[0]["name"], TITLE);
        assert_eq!(releases[0]["body"], NOTES);
        assert_eq!(releases[0]["draft"], false);
        assert_eq!(
            releases[0]["assets"].as_array().expect("asset array").len(),
            13
        );
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    fn clear_log(&self) {
        fs::write(&self.log_path, b"").expect("clear fake gh log");
    }

    fn mutation_kinds(&self) -> Vec<String> {
        self.log()
            .lines()
            .filter_map(|line| line.strip_prefix("MUTATE\t"))
            .filter_map(|line| line.split('\t').next())
            .map(str::to_owned)
            .collect()
    }
}

struct RunRequest<'a> {
    fixture: &'a PublisherFixture,
    source_commit: String,
    cargo_fail_at: usize,
    interrupt: Option<String>,
}

impl RunRequest<'_> {
    fn source_commit(mut self, commit: &str) -> Self {
        self.source_commit = commit.to_owned();
        self
    }

    fn cargo_fail_at(mut self, position: usize) -> Self {
        self.cargo_fail_at = position;
        self
    }

    fn interrupt(mut self, point: &str) -> Self {
        self.interrupt = Some(point.to_owned());
        self
    }

    fn run(self) -> Output {
        let fixture = self.fixture;
        let _ = fs::remove_file(&fixture.cargo_count);
        let current_path = std::env::var_os("PATH").expect("PATH");
        let path = std::env::join_paths(
            std::iter::once(fixture.fake_bin.clone()).chain(std::env::split_paths(&current_path)),
        )
        .expect("fake PATH");
        let mut command = Command::new(&fixture.publisher);
        command
            .current_dir(&fixture.repo)
            .arg(&self.source_commit)
            .arg(&fixture.candidate)
            .arg(&fixture.secret_key)
            .env("PATH", path)
            .env("HOME", fixture.root.path().join("home"))
            .env("TMPDIR", fixture.root.path().join("tmp"))
            .env("FAKE_GH_STATE", &fixture.state)
            .env("FAKE_GH_REMOTE", &fixture.remote)
            .env("FAKE_GH_LOG", &fixture.log_path)
            .env("FAKE_GH_EXPECTED_COMMIT", fixture.head())
            .env("FAKE_GH_INTERRUPT", self.interrupt.unwrap_or_default())
            .env("FAKE_CARGO_COUNT", &fixture.cargo_count)
            .env("FAKE_CARGO_FAIL_AT", self.cargo_fail_at.to_string());
        fs::create_dir_all(fixture.root.path().join("home")).expect("create fake home");
        fs::create_dir_all(fixture.root.path().join("tmp")).expect("create fake tmp");
        command.output().expect("run real publisher script")
    }
}

fn release_fixture(id: u64, commit: &str, draft: bool) -> Value {
    json!({
        "id": id,
        "tag_name": TAG,
        "target_commitish": commit,
        "name": TITLE,
        "body": NOTES,
        "draft": draft,
        "assets": []
    })
}

fn command_stdout(command: &mut Command) -> String {
    let output = command.output().expect("run fixture command");
    assert!(
        output.status.success(),
        "fixture command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("UTF-8 fixture stdout")
        .trim()
        .to_owned()
}

fn run_checked(command: &mut Command) {
    let output = command.output().expect("run fixture command");
    assert!(
        output.status.success(),
        "fixture command failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fake_cargo() -> &'static str {
    r#"#!/usr/bin/env bash
set -euo pipefail
count=0
if [[ -f "$FAKE_CARGO_COUNT" ]]; then
    count="$(<"$FAKE_CARGO_COUNT")"
fi
count=$((count + 1))
printf '%s\n' "$count" >"$FAKE_CARGO_COUNT"
if [[ "${FAKE_CARGO_FAIL_AT:-0}" != "0" && "$count" == "$FAKE_CARGO_FAIL_AT" ]]; then
    exit 97
fi
"#
}

fn fake_gh() -> &'static str {
    r#"#!/usr/bin/env bash
set -euo pipefail

log_call() {
    local kind="$1"
    shift
    {
        printf '%s\t' "$kind"
        printf '%q\t' "$@"
        printf '\n'
    } >>"$FAKE_GH_LOG"
}

write_state() {
    local temporary="$FAKE_GH_STATE.tmp"
    jq "$@" "$FAKE_GH_STATE" >"$temporary"
    mv "$temporary" "$FAKE_GH_STATE"
}

interrupt_after() {
    local point="$1"
    if [[ "${FAKE_GH_INTERRUPT:-}" == "$point" ]]; then
        exit 97
    fi
}

if [[ "$1" == "repo" && "$2" == "view" ]]; then
    log_call READ "$@"
    jq -r '.repo' "$FAKE_GH_STATE"
    exit 0
fi

if [[ "$1" == "release" && "$2" == "upload" ]]; then
    log_call MUTATE asset "$@"
    tag="$3"
    path="$4"
    name="${path##*/}"
    release_index="$(
        jq --arg tag "$tag" \
            '[.releases | to_entries[] | select(.value.tag_name == $tag)] | if length == 1 then .[0].key else -1 end' \
            "$FAKE_GH_STATE"
    )"
    [[ "$release_index" != "-1" ]] || exit 9
    asset_id="$(jq -r '.next_asset_id' "$FAKE_GH_STATE")"
    cp "$path" "$FAKE_GH_REMOTE/assets/$asset_id"
    write_state \
        --argjson index "$release_index" \
        --argjson id "$asset_id" \
        --arg name "$name" \
        '.releases[$index].assets += [{id: $id, name: $name}] |
         .next_asset_id += 1 |
         .upload_count += 1'
    upload_count="$(jq -r '.upload_count' "$FAKE_GH_STATE")"
    interrupt_after "asset:$upload_count"
    exit 0
fi

[[ "$1" == "api" ]] || exit 9
shift
method=GET
paginate=false
slurp=false
endpoint=""
fields=()
while (($#)); do
    case "$1" in
        -X)
            method="$2"
            shift 2
            ;;
        -f | -F)
            fields+=("$2")
            shift 2
            ;;
        -H)
            shift 2
            ;;
        --paginate)
            paginate=true
            shift
            ;;
        --slurp)
            slurp=true
            shift
            ;;
        *)
            endpoint="$1"
            shift
            ;;
    esac
done

kind=READ
mutation=""
if [[ "$method" == "POST" && "$endpoint" == */git/refs ]]; then
    kind=MUTATE
    mutation=tag
elif [[ "$method" == "POST" && "$endpoint" == */releases ]]; then
    kind=MUTATE
    mutation=draft
elif [[ "$method" == "PATCH" && "$endpoint" == */releases/* ]]; then
    kind=MUTATE
    mutation=publish
fi
log_call "$kind" "$mutation" gh api "$method" "$endpoint" "${fields[@]}"

case "$method:$endpoint" in
    GET:*/git/matching-refs/tags/*)
        jq -c '
            if .remote_tag == null then []
            else [{
                ref: .remote_tag.ref,
                object: {
                    type: .remote_tag.type,
                    sha: .remote_tag.object_sha
                }
            }]
            end
        ' "$FAKE_GH_STATE"
        ;;
    GET:*/git/tags/*)
        jq -c '{
            object: {
                type: "commit",
                sha: .remote_tag.commit
            }
        }' "$FAKE_GH_STATE"
        ;;
    GET:*/releases\?per_page=100)
        if $paginate && $slurp; then
            jq -c '[.releases]' "$FAKE_GH_STATE"
        else
            jq -c '.releases' "$FAKE_GH_STATE"
        fi
        ;;
    GET:*/releases/assets/*)
        asset_id="${endpoint##*/}"
        cat "$FAKE_GH_REMOTE/assets/$asset_id"
        ;;
    GET:*/releases/*)
        release_id="${endpoint##*/}"
        jq -c --argjson id "$release_id" \
            '.releases[] | select(.id == $id)' "$FAKE_GH_STATE"
        ;;
    POST:*/git/refs)
        tag_ref=""
        tag_object=""
        for field in "${fields[@]}"; do
            case "$field" in
                ref=*) tag_ref="${field#ref=}" ;;
                sha=*) tag_object="${field#sha=}" ;;
            esac
        done
        write_state \
            --arg ref "$tag_ref" \
            --arg object "$tag_object" \
            --arg commit "$FAKE_GH_EXPECTED_COMMIT" \
            '.remote_tag = {
                ref: $ref,
                type: "tag",
                object_sha: $object,
                commit: $commit
            }'
        interrupt_after tag
        jq -c '.remote_tag' "$FAKE_GH_STATE"
        ;;
    POST:*/releases)
        tag_name=""
        target_commitish=""
        name=""
        body=""
        draft=false
        for field in "${fields[@]}"; do
            case "$field" in
                tag_name=*) tag_name="${field#tag_name=}" ;;
                target_commitish=*) target_commitish="${field#target_commitish=}" ;;
                name=*) name="${field#name=}" ;;
                body=*) body="${field#body=}" ;;
                draft=*) draft="${field#draft=}" ;;
            esac
        done
        release_id="$(jq -r '.next_release_id' "$FAKE_GH_STATE")"
        write_state \
            --argjson id "$release_id" \
            --arg tag "$tag_name" \
            --arg target "$target_commitish" \
            --arg name "$name" \
            --arg body "$body" \
            --argjson draft "$draft" \
            '.releases += [{
                id: $id,
                tag_name: $tag,
                target_commitish: $target,
                name: $name,
                body: $body,
                draft: $draft,
                assets: []
            }] |
            .next_release_id += 1'
        interrupt_after draft
        jq -c --argjson id "$release_id" \
            '.releases[] | select(.id == $id)' "$FAKE_GH_STATE"
        ;;
    PATCH:*/releases/*)
        release_id="${endpoint##*/}"
        write_state --argjson id "$release_id" \
            '(.releases[] | select(.id == $id) | .draft) = false'
        jq -c --argjson id "$release_id" \
            '.releases[] | select(.id == $id)' "$FAKE_GH_STATE"
        ;;
    *)
        exit 9
        ;;
esac
"#
}
