// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#[allow(dead_code)]
#[path = "support/package_model.rs"]
mod package_model;
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::Compression;
use flate2::write::GzEncoder;
use package_model::{ARTIFACTS, Lane, PRODUCT_VERSION, artifacts_for_lane, checksummed_names};
use serde_json::{Value, json};
use support::TestDirectory;
use tar::{Builder, Header};

const VERSION: &str = PRODUCT_VERSION;
// Derived from the crate version so the publisher suite re-proves itself at every
// release. The publisher was once pinned to the 1.0.0 cutover; deriving these means
// a future version-specific restriction cannot pass these tests unnoticed.
const TAG: &str = concat!("v", env!("CARGO_PKG_VERSION"));
const TITLE: &str = concat!("solstone-tmux ", env!("CARGO_PKG_VERSION"));
const NOTES: &str = concat!(
    "## [",
    env!("CARGO_PKG_VERSION"),
    "] - 2026-08-03\n\n### Added\n- native candidate release."
);
// A version the crate will never carry, so manifest/lockfile disagreement stays a
// real disagreement no matter what the crate version becomes.
const MISMATCHED_VERSION: &str = "9.9.9";
const ORIGIN_PUSH_URL: &str = "git@github.com:solpbc/solstone-tmux.git";
type RefusalSetup = for<'a> fn(&'a PublisherFixture) -> RunRequest<'a>;

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
    assert!(
        fixture.log().contains("git push"),
        "annotated tag object was not transferred through Git:\n{}",
        fixture.log()
    );
    assert!(
        !fixture.log().contains("git/refs"),
        "publisher tried to create a ref for a local-only tag object:\n{}",
        fixture.log()
    );
}

#[test]
fn sign_and_validate_only_produces_a_complete_candidate_without_release_surface_access() {
    let fixture = PublisherFixture::new();

    assert_success(&fixture.request().sign_and_validate_only().run());
    fixture.assert_signed_candidate();
    assert!(
        fixture.log().is_empty(),
        "sign-only mode invoked gh:\n{}",
        fixture.log()
    );
    assert_eq!(fixture.state()["remote_tag"], Value::Null);
    assert_eq!(fixture.state()["releases"], json!([]));
    assert!(
        !fixture
            .candidate
            .join(package_model::SHA256SUMS_NAME)
            .exists(),
        "sign-only mode mutated its unsigned input"
    );
    assert!(
        !fixture
            .candidate
            .join(package_model::SIGNATURE_NAME)
            .exists(),
        "sign-only mode mutated its unsigned input"
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
fn origin_push_url_must_match_the_api_repository_before_tag_mutation() {
    let fixture = PublisherFixture::new();
    fixture.set_origin_push_url("git@example.invalid:someone-else/repository.git");
    fixture.clear_log();

    assert_failure(&fixture.run());
    assert_no_mutations(&fixture);
    assert_eq!(fixture.state()["remote_tag"], Value::Null);
}

#[test]
fn origin_repoint_after_validation_cannot_redirect_the_tag_push() {
    let fixture = PublisherFixture::new();
    let output = fixture.request().remote_mutation("origin-repoint").run();

    assert_success(&output);
    fixture.assert_exact_published();
    assert_eq!(
        command_stdout(
            Command::new("git")
                .current_dir(&fixture.repo)
                .args(["remote", "get-url", "--push", "origin"]),
        ),
        "git@example.invalid:redirected/repository.git"
    );
    assert!(
        fixture.log().contains(ORIGIN_PUSH_URL),
        "push did not use the captured verified destination:\n{}",
        fixture.log()
    );
}

#[test]
fn local_tag_repoint_before_push_cannot_change_the_captured_source_object() {
    let fixture = PublisherFixture::new();
    let output = fixture.request().remote_mutation("local-tag-repoint").run();

    assert_success(&output);
    let state = fixture.state();
    assert_eq!(state["remote_tag"]["type"], "tag");
    assert_eq!(state["remote_tag"]["commit"], fixture.head());
    assert_eq!(state["releases"][0]["draft"], false);
    let current_local_tag = command_stdout(Command::new("git").current_dir(&fixture.repo).args([
        "rev-parse",
        "--verify",
        &format!("refs/tags/{TAG}"),
    ]));
    assert_ne!(
        state["remote_tag"]["object_sha"], current_local_tag,
        "fixture did not repoint the mutable local tag"
    );
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
fn concurrent_remote_drift_is_rejected_before_publication() {
    for mutation in [
        "wrong-title",
        "wrong-notes",
        "wrong-target",
        "tag-target",
        "tag-object",
    ] {
        let fixture = PublisherFixture::new();
        let output = fixture.request().remote_mutation(mutation).run();
        assert_failure(&output);
        assert!(
            !fixture
                .mutation_kinds()
                .iter()
                .any(|kind| kind == "publish"),
            "concurrent drift {mutation:?} reached publication:\n{}",
            fixture.log()
        );
        if mutation == "tag-target" || mutation == "tag-object" {
            assert_eq!(
                fixture.state()["releases"].as_array().map(Vec::len),
                Some(0),
                "tag drift should stop before a draft exists"
            );
        } else {
            assert_eq!(fixture.state()["releases"][0]["draft"], true);
        }
    }
}

#[test]
fn concurrent_tag_creation_rejects_the_non_force_push_without_overwrite() {
    let fixture = PublisherFixture::new();
    let output = fixture.request().remote_mutation("tag-collision").run();

    assert_failure(&output);
    assert_eq!(
        fixture.state()["releases"].as_array().map(Vec::len),
        Some(0)
    );
    assert_eq!(fixture.state()["remote_tag"]["object_sha"], "d".repeat(40));
    assert!(
        !fixture.mutation_kinds().iter().any(|kind| kind == "tag"),
        "rejected non-force push was recorded as our tag mutation:\n{}",
        fixture.log()
    );
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
                format!(
                    "[package]\nname = \"solstone-tmux\"\nversion = \"{MISMATCHED_VERSION}\"\n"
                )
                .as_bytes(),
            );
            fixture.request()
        }),
        ("Cargo.lock version", |fixture| {
            fixture.commit_repo_mutation(
                "Cargo.lock",
                format!(
                    "version = 4\n\n[[package]]\nname = \"solstone-tmux\"\nversion = \"{MISMATCHED_VERSION}\"\n"
                )
                .as_bytes(),
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
        ("candidate directory symlink", |fixture| {
            let real_candidate = fixture.root.path().join("candidate-real");
            fs::rename(&fixture.candidate, &real_candidate).expect("move candidate directory");
            symlink(&real_candidate, &fixture.candidate).expect("symlink candidate directory");
            fixture.request()
        }),
        ("secret key symlink", |fixture| {
            let real_secret = fixture.root.path().join("test-only-minisign-real.key");
            fs::rename(&fixture.secret_key, &real_secret).expect("move secret key");
            symlink(&real_secret, &fixture.secret_key).expect("symlink secret key");
            fixture.request()
        }),
        ("target record mismatch", |fixture| {
            let record = fixture.candidate.join(format!(
                "solstone-tmux-{VERSION}-{}.target.json",
                Lane::LinuxX86_64.rust_target()
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
    signed_candidate: PathBuf,
    secret_key: PathBuf,
    fake_bin: PathBuf,
    state: PathBuf,
    remote: PathBuf,
    log_path: PathBuf,
    cargo_count: PathBuf,
    real_git: PathBuf,
    real_minisign: PathBuf,
    publisher: PathBuf,
}

impl PublisherFixture {
    fn new() -> Self {
        let root = TestDirectory::new("release-publisher");
        let repo = root.path().join("repo");
        let candidate = root.path().join("candidate");
        let signed_candidate = root.path().join("signed-candidate");
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
            format!("[package]\nname = \"solstone-tmux\"\nversion = \"{VERSION}\"\n").as_bytes(),
        )
        .expect("write manifest");
        fs::write(
            repo.join("Cargo.lock"),
            format!(
                "version = 4\n\n[[package]]\nname = \"solstone-tmux\"\nversion = \"{VERSION}\"\n"
            )
            .as_bytes(),
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
                Lane::ALL
                    .into_iter()
                    .map(Lane::rust_target)
                    .collect::<Vec<_>>()
                    .join(" ")
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
        run_checked(Command::new("git").current_dir(&repo).args([
            "remote",
            "add",
            "origin",
            "git@github.com:solpbc/solstone-tmux.git",
        ]));

        let state = remote.join("state.json");
        fs::write(
            &state,
            serde_json::to_vec_pretty(&json!({
                "repo": "solpbc/solstone-tmux",
                "remote_tag": Value::Null,
                "releases": [],
                "next_release_id": 1,
                "next_asset_id": 100,
                "upload_count": 0,
                "release_reads": 0,
                "tag_reads": 0
            }))
            .expect("encode fake state"),
        )
        .expect("write fake state");
        let log_path = remote.join("gh.log");
        fs::write(&log_path, b"").expect("write gh log");
        let cargo_count = remote.join("cargo.count");
        let real_git = PathBuf::from(command_stdout(
            Command::new("sh").args(["-c", "command -v git"]),
        ));
        let real_minisign = PathBuf::from(command_stdout(
            Command::new("sh").args(["-c", "command -v minisign"]),
        ));

        fs::write(fake_bin.join("gh"), fake_gh()).expect("write fake gh");
        fs::write(fake_bin.join("cargo"), fake_cargo()).expect("write fake cargo");
        fs::write(fake_bin.join("git"), fake_git()).expect("write fake git");
        fs::write(fake_bin.join("minisign"), fake_minisign()).expect("write fake minisign");
        for name in ["gh", "cargo", "git", "minisign"] {
            fs::set_permissions(fake_bin.join(name), fs::Permissions::from_mode(0o755))
                .expect("chmod fake tool");
        }

        let publisher = source_root.join("packaging/publish-release.sh");
        let fixture = Self {
            root,
            repo,
            candidate,
            signed_candidate,
            secret_key,
            fake_bin,
            state,
            remote,
            log_path,
            cargo_count,
            real_git,
            real_minisign,
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
            remote_mutation: None,
            sign_and_validate_only: false,
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

    fn set_origin_push_url(&self, url: &str) {
        self.git(&["remote", "set-url", "--push", "origin", url]);
    }

    fn rebuild_candidate(&self) {
        if self.candidate.exists() {
            fs::remove_dir_all(&self.candidate).expect("clear candidate");
        }
        fs::create_dir(&self.candidate).expect("recreate candidate");
        for artifact in ARTIFACTS.iter() {
            if artifact.lane == Lane::LinuxX86_64
                && artifact.kind == package_model::ArtifactKind::TarGz
            {
                self.write_executable_tar(&format!(
                    "printf 'solstone-tmux {VERSION} (source {})\\n'",
                    self.head()
                ));
            } else {
                fs::write(
                    self.candidate.join(&artifact.name),
                    format!("fixture bytes for {}\n", artifact.name),
                )
                .expect("write artifact");
            }
        }
        for lane in Lane::ALL {
            let mut artifacts = artifacts_for_lane(lane)
                .into_iter()
                .map(|artifact| {
                    json!({
                        "name": artifact.name,
                        "sha256": "a".repeat(64)
                    })
                })
                .collect::<Vec<_>>();
            artifacts.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
            let record = json!({
                "schema_version": 1,
                "product_version": VERSION,
                "source_commit": self.head(),
                "rust_target": lane.rust_target(),
                "rustc_vv": "rustc fixture\n",
                "executable": {
                    "name": "solstone-tmux",
                    "sha256": "a".repeat(64)
                },
                "artifacts": artifacts
            });
            fs::write(
                self.candidate.join(lane.record_name()),
                serde_json::to_vec(&record).expect("encode record"),
            )
            .expect("write record");
        }
    }

    fn assert_signed_candidate(&self) {
        let mut actual = fs::read_dir(&self.signed_candidate)
            .expect("read signed candidate")
            .map(|entry| {
                entry
                    .expect("read signed candidate entry")
                    .file_name()
                    .into_string()
                    .expect("signed candidate name")
            })
            .collect::<Vec<_>>();
        actual.sort();
        let mut expected = self.unsigned_names();
        expected.push(package_model::SHA256SUMS_NAME.to_owned());
        expected.push(package_model::SIGNATURE_NAME.to_owned());
        expected.sort();
        assert_eq!(actual, expected);
    }

    fn write_executable_tar(&self, body: &str) {
        let name = artifacts_for_lane(Lane::LinuxX86_64)
            .into_iter()
            .find(|artifact| artifact.kind == package_model::ArtifactKind::TarGz)
            .expect("x86 Linux tar artifact")
            .name;
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
        checksummed_names()
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
        let local_tag = Command::new("git")
            .current_dir(&self.repo)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/tags/{TAG}"),
            ])
            .output()
            .expect("inspect optional local tag");
        if local_tag.status.success() {
            let object = String::from_utf8(local_tag.stdout)
                .expect("UTF-8 local tag object")
                .trim()
                .to_owned();
            assert_eq!(state["remote_tag"]["object_sha"], object);
        } else {
            assert_eq!(local_tag.status.code(), Some(1));
        }
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
    remote_mutation: Option<String>,
    sign_and_validate_only: bool,
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

    fn remote_mutation(mut self, mutation: &str) -> Self {
        self.remote_mutation = Some(mutation.to_owned());
        self
    }

    fn sign_and_validate_only(mut self) -> Self {
        self.sign_and_validate_only = true;
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
        if self.sign_and_validate_only {
            let _ = fs::remove_dir_all(&fixture.signed_candidate);
            command
                .arg("--sign-and-validate-only")
                .arg(&self.source_commit)
                .arg(&fixture.candidate)
                .arg(&fixture.secret_key)
                .arg(&fixture.signed_candidate);
        } else {
            command
                .arg(&self.source_commit)
                .arg(&fixture.candidate)
                .arg(&fixture.secret_key);
        }
        command
            .current_dir(&fixture.repo)
            .env("PATH", path)
            .env("MINISIGN_BIN", fixture.fake_bin.join("minisign"))
            .env("HOME", fixture.root.path().join("home"))
            .env("TMPDIR", fixture.root.path().join("tmp"))
            .env("FAKE_GH_STATE", &fixture.state)
            .env("FAKE_GH_REMOTE", &fixture.remote)
            .env("FAKE_GH_LOG", &fixture.log_path)
            .env("FAKE_GH_EXPECTED_COMMIT", fixture.head())
            .env("FAKE_GH_EXPECTED_VERSION", VERSION)
            .env("FAKE_GH_INTERRUPT", self.interrupt.unwrap_or_default())
            .env(
                "FAKE_GH_REMOTE_MUTATION",
                self.remote_mutation.unwrap_or_default(),
            )
            .env("FAKE_CARGO_COUNT", &fixture.cargo_count)
            .env("FAKE_CARGO_FAIL_AT", self.cargo_fail_at.to_string())
            .env("FAKE_REAL_GIT", &fixture.real_git)
            .env("FAKE_REAL_MINISIGN", &fixture.real_minisign)
            .env("FAKE_REPO", &fixture.repo)
            .env("FAKE_EXPECTED_PUSH_URL", ORIGIN_PUSH_URL);
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
if [[ "$count" == "1" ]]; then
    source_binding_found=false
    while IFS= read -r candidate_tar; do
        if tar -xOf "$candidate_tar" 2>/dev/null |
            grep -aFq "solstone-tmux $FAKE_GH_EXPECTED_VERSION (source $FAKE_GH_EXPECTED_COMMIT)"; then
            source_binding_found=true
            break
        fi
    done < <(find "$SOLSTONE_TMUX_TEST_UNSIGNED_CANDIDATE" -type f -name '*.tar.gz' | sort)
    $source_binding_found || exit 98
fi
"#
}

fn fake_minisign() -> &'static str {
    r#"#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "-v" ]]; then
    printf '%s\n' 'minisign 0.11'
    exit 0
fi
exec "$FAKE_REAL_MINISIGN" "$@"
"#
}

fn fake_git() -> &'static str {
    r#"#!/usr/bin/env bash
set -euo pipefail

original=("$@")
repo=""
if [[ "${1:-}" == "-C" ]]; then
    repo="$2"
    shift 2
fi

if [[ "${1:-}" != "push" ]]; then
    exec "$FAKE_REAL_GIT" "${original[@]}"
fi
shift
remote="${1:-}"
refspec="${2:-}"
[[ -n "$repo" && "$remote" == "$FAKE_EXPECTED_PUSH_URL" ]] || exit 9
[[ "$refspec" == *:refs/tags/* ]] || exit 9
source_ref="${refspec%%:*}"
target_ref="${refspec#*:}"
[[ "$source_ref" =~ ^[0-9a-f]{40}$ ]] || exit 9

if [[ "${FAKE_GH_REMOTE_MUTATION:-}" == "local-tag-repoint" ]]; then
    "$FAKE_REAL_GIT" -C "$repo" tag -f -a "${target_ref#refs/tags/}" \
        "$FAKE_GH_EXPECTED_COMMIT" -m "concurrent competing annotation"
fi

tag_object="$source_ref"
[[ "$("$FAKE_REAL_GIT" -C "$repo" cat-file -t "$tag_object")" == "tag" ]] || exit 9
commit="$("$FAKE_REAL_GIT" -C "$repo" rev-parse "$source_ref^{commit}")"
if [[ "${FAKE_GH_REMOTE_MUTATION:-}" == "tag-collision" ]]; then
    temporary="$FAKE_GH_STATE.tmp"
    jq \
        --arg ref "$target_ref" \
        --arg object "$(printf 'd%.0s' {1..40})" \
        --arg commit "$commit" \
        '.remote_tag = {
            ref: $ref,
            type: "tag",
            object_sha: $object,
            commit: $commit
        }' "$FAKE_GH_STATE" >"$temporary"
    mv "$temporary" "$FAKE_GH_STATE"
    exit 1
fi
{
    printf 'MUTATE\ttag\tgit push\t'
    printf '%q\t' "${original[@]}"
    printf '\n'
} >>"$FAKE_GH_LOG"
temporary="$FAKE_GH_STATE.tmp"
jq \
    --arg ref "$target_ref" \
    --arg object "$tag_object" \
    --arg commit "$commit" \
    '.remote_tag = {
        ref: $ref,
        type: "tag",
        object_sha: $object,
        commit: $commit
    }' "$FAKE_GH_STATE" >"$temporary"
mv "$temporary" "$FAKE_GH_STATE"
if [[ "${FAKE_GH_INTERRUPT:-}" == "tag" ]]; then
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
    jq -c '{
        nameWithOwner: .repo,
        url: ("https://github.com/" + .repo),
        sshUrl: ("git@github.com:" + .repo + ".git")
    }' "$FAKE_GH_STATE"
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
        write_state '.tag_reads += 1'
        tag_reads="$(jq -r '.tag_reads' "$FAKE_GH_STATE")"
        if [[ "$tag_reads" == "1" && "${FAKE_GH_REMOTE_MUTATION:-}" == "origin-repoint" ]]; then
            "$FAKE_REAL_GIT" -C "$FAKE_REPO" remote set-url --push origin \
                git@example.invalid:redirected/repository.git
        fi
        if [[ "$tag_reads" == "2" && "${FAKE_GH_REMOTE_MUTATION:-}" == "tag-target" ]]; then
            write_state '.remote_tag.commit = ("b" * 40)'
        fi
        if [[ "$tag_reads" == "2" && "${FAKE_GH_REMOTE_MUTATION:-}" == "tag-object" ]]; then
            write_state '.remote_tag.object_sha = ("d" * 40)'
        fi
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
        write_state '.release_reads += 1'
        release_reads="$(jq -r '.release_reads' "$FAKE_GH_STATE")"
        if [[ "$release_reads" == "2" ]]; then
            case "${FAKE_GH_REMOTE_MUTATION:-}" in
                wrong-title)
                    write_state '.releases[0].name = "concurrent title"'
                    ;;
                wrong-notes)
                    write_state '.releases[0].body = "concurrent notes"'
                    ;;
                wrong-target)
                    write_state '.releases[0].target_commitish = ("b" * 40)'
                    ;;
            esac
        fi
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
