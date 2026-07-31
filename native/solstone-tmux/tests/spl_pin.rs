// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use support::TestDirectory;

const WORKSPACE_MANIFEST: &str = "Cargo.toml";
const NATIVE_MANIFEST: &str = "native/solstone-tmux/Cargo.toml";
const LOCKFILE: &str = "Cargo.lock";
const DENY_CONFIG: &str = "deny.toml";

#[test]
fn spl_pin_valid_tree_passes() {
    let fixture = PinFixture::new(|_| {});
    assert_success(&fixture.run());
}

#[test]
fn spl_pin_ignores_table_after_package_block() {
    let fixture = PinFixture::new(|repo| {
        rewrite_lock_package(repo, "spl-transport", |block| {
            format!("{block}[metadata]\nsource = \"metadata-only\"\n\n")
        });
    });
    assert_success(&fixture.run());
}

#[test]
fn spl_pin_missing_inputs_fail_cleanly() {
    let fixture = PinFixture::empty();
    let output = fixture.run();
    let expected = format!(
        "{}: spl-core and spl-transport require exactly one approved Git source in [sources].allow-git; set allow-git to an array containing one quoted Git URL\n\
         {}: spl-core and spl-transport pin check requires this file; restore Cargo.toml\n\
         {}: spl-core and spl-transport pin check requires this file; restore native/solstone-tmux/Cargo.toml\n\
         {}: spl-core and spl-transport pin check requires this file; restore Cargo.lock",
        fixture.path(DENY_CONFIG).display(),
        fixture.path(WORKSPACE_MANIFEST).display(),
        fixture.path(NATIVE_MANIFEST).display(),
        fixture.path(LOCKFILE).display(),
    );
    assert_exact_failure(&output, &expected);
}

#[test]
fn spl_pin_rejects_untracked_repository() {
    let fixture = PinFixture::untracked();
    let expected = format!(
        "{}: spl-core and spl-transport copied-tree check requires a Git repository; run this check against a repository working tree",
        fixture.repo().display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_missing_allow_git_authority() {
    let fixture = PinFixture::new(|repo| {
        remove_matching_line(&repo.join(DENY_CONFIG), "allow-git =");
    });
    assert_exact_failure(&fixture.run(), &authority_diagnostic(&fixture));
}

#[test]
fn spl_pin_rejects_multiple_allow_git_sources() {
    let fixture = PinFixture::new(|repo| {
        let source = approved_source(repo);
        replace_matching_line(
            &repo.join(DENY_CONFIG),
            "allow-git =",
            &format!("allow-git = [\"{source}\", \"https://example.invalid/alternate\"]"),
        );
    });
    assert_exact_failure(&fixture.run(), &authority_diagnostic(&fixture));
}

#[test]
fn spl_pin_rejects_malformed_allow_git_authority() {
    let fixture = PinFixture::new(|repo| {
        replace_matching_line(&repo.join(DENY_CONFIG), "allow-git =", "allow-git = [");
    });
    assert_exact_failure(&fixture.run(), &authority_diagnostic(&fixture));
}

#[test]
fn spl_pin_rejects_missing_workspace_dependency() {
    let fixture = PinFixture::new(|repo| {
        remove_manifest_entry(repo, WORKSPACE_MANIFEST, "spl-core");
    });
    let source = approved_source(fixture.repo());
    let expected = format!(
        "{}: spl-core is missing from [workspace.dependencies]; add spl-core = {{ git = \"{source}\", rev = \"<40-character lowercase hex>\" }}",
        fixture.path(WORKSPACE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_duplicate_workspace_dependency() {
    let fixture = PinFixture::new(|repo| {
        duplicate_manifest_entry(repo, WORKSPACE_MANIFEST, "spl-core");
    });
    let expected = format!(
        "{}: spl-core appears more than once in [workspace.dependencies]; keep exactly one Git revision declaration",
        fixture.path(WORKSPACE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_malformed_workspace_declaration() {
    let fixture = PinFixture::new(|repo| {
        let line = manifest_entry(repo, WORKSPACE_MANIFEST, "spl-core");
        replace_manifest_entry(
            repo,
            WORKSPACE_MANIFEST,
            "spl-core",
            &line.replace(" }", ", features = \"x\" }"),
        );
    });
    let source = approved_source(fixture.repo());
    let expected = format!(
        "{}: spl-core must declare only git and rev in [workspace.dependencies]; replace it with spl-core = {{ git = \"{source}\", rev = \"<40-character lowercase hex>\" }}",
        fixture.path(WORKSPACE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_unapproved_workspace_source() {
    let fixture = PinFixture::new(|repo| {
        let line = manifest_entry(repo, WORKSPACE_MANIFEST, "spl-core");
        let source = inline_value(&line, "git");
        replace_manifest_entry(
            repo,
            WORKSPACE_MANIFEST,
            "spl-core",
            &line.replace(&source, "https://example.invalid/alternate"),
        );
    });
    let source = approved_source(fixture.repo());
    let expected = format!(
        "{}: spl-core must use the Git source approved by deny.toml; set git = \"{source}\" in [workspace.dependencies]",
        fixture.path(WORKSPACE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_non_rev_workspace_selector() {
    let fixture = PinFixture::new(|repo| {
        let line = manifest_entry(repo, WORKSPACE_MANIFEST, "spl-core");
        replace_manifest_entry(
            repo,
            WORKSPACE_MANIFEST,
            "spl-core",
            &line.replace("rev =", "branch ="),
        );
    });
    let expected = format!(
        "{}: spl-core must select a revision, not a branch, tag, version, or path; pin it with rev = \"<40-character lowercase hex>\" in [workspace.dependencies]",
        fixture.path(WORKSPACE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_invalid_workspace_revision() {
    let fixture = PinFixture::new(|repo| {
        replace_workspace_revision(repo, "spl-core", "abcdef0");
    });
    let expected = format!(
        "{}: spl-core revision must be exactly 40 lowercase hexadecimal characters; set rev = \"<40-character lowercase hex>\" in [workspace.dependencies]",
        fixture.path(WORKSPACE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_differing_workspace_revisions() {
    let fixture = PinFixture::new(|repo| {
        let revision = workspace_revision(repo);
        replace_workspace_revision(repo, "spl-transport", &different_revision(&revision));
    });
    let expected = format!(
        "{}: spl-core and spl-transport must use the same revision in [workspace.dependencies]; set both rev values to one 40-character lowercase hexadecimal revision",
        fixture.path(WORKSPACE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_missing_native_dependency() {
    let fixture = PinFixture::new(|repo| {
        remove_manifest_entry(repo, NATIVE_MANIFEST, "spl-core");
    });
    let expected = format!(
        "{}: spl-core is missing from [dependencies]; add spl-core = {{ workspace = true }}",
        fixture.path(NATIVE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_duplicate_native_dependency() {
    let fixture = PinFixture::new(|repo| {
        duplicate_manifest_entry(repo, NATIVE_MANIFEST, "spl-core");
    });
    let expected = format!(
        "{}: spl-core appears more than once in [dependencies]; keep exactly one spl-core = {{ workspace = true }} entry",
        fixture.path(NATIVE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_native_direct_override() {
    let fixture = PinFixture::new(|repo| {
        let direct = manifest_entry(repo, WORKSPACE_MANIFEST, "spl-core");
        replace_manifest_entry(repo, NATIVE_MANIFEST, "spl-core", &direct);
    });
    let expected = format!(
        "{}: spl-core must inherit the workspace declaration; replace the entry with spl-core = {{ workspace = true }}",
        fixture.path(NATIVE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_missing_lock_package() {
    let fixture = PinFixture::new(|repo| {
        rewrite_lockfile(repo, |contents| {
            let (start, end) = lock_block_range(&contents, "spl-core");
            format!("{}{}", &contents[..start], &contents[end..])
        });
    });
    let expected = format!(
        "{}: spl-core is missing from Cargo.lock; regenerate the lockfile from the workspace declaration",
        fixture.path(LOCKFILE).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_lock_revision_mismatch() {
    let fixture = PinFixture::new(|repo| {
        let revision = workspace_revision(repo);
        let alternate = different_revision(&revision);
        rewrite_lock_package(repo, "spl-core", |block| {
            block.replace(&revision, &alternate)
        });
    });
    let expected = format!(
        "{}: spl-core resolves at a revision other than the workspace declaration; regenerate the lockfile at the declared revision",
        fixture.path(LOCKFILE).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_lock_source_absent() {
    let fixture = PinFixture::new(|repo| {
        rewrite_lock_package(repo, "spl-core", |block| {
            block
                .lines()
                .filter(|line| !line.starts_with("source = "))
                .map(|line| format!("{line}\n"))
                .collect()
        });
    });
    let expected = format!(
        "{}: spl-core resolves without the workspace Git source; remove local routing and regenerate the lockfile from the workspace declaration",
        fixture.path(LOCKFILE).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_multiple_lock_sources() {
    let fixture = PinFixture::new(|repo| {
        rewrite_lock_package(repo, "spl-core", |block| {
            let source = block
                .lines()
                .find(|line| line.starts_with("source = "))
                .expect("source line");
            block.replacen(source, &format!("{source}\n{source}"), 1)
        });
    });
    let expected = format!(
        "{}: spl-core has multiple source declarations in its package block; regenerate the lockfile from the workspace declaration",
        fixture.path(LOCKFILE).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_lock_source_from_another_source() {
    let fixture = PinFixture::new(|repo| {
        rewrite_lock_package(repo, "spl-core", |block| {
            let source = block
                .lines()
                .find(|line| line.starts_with("source = "))
                .expect("source line");
            block.replacen(
                source,
                "source = \"registry+https://example.invalid/index\"",
                1,
            )
        });
    });
    let expected = format!(
        "{}: spl-core resolves from a source other than the workspace declaration; regenerate the lockfile from the declared Git source",
        fixture.path(LOCKFILE).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_lock_source_with_approved_prefix() {
    let fixture = PinFixture::new(|repo| {
        let approved = approved_source(repo);
        let revision = workspace_revision(repo);
        rewrite_lock_package(repo, "spl-core", |block| {
            let source = block
                .lines()
                .find(|line| line.starts_with("source = "))
                .expect("source line");
            block.replacen(
                source,
                &format!("source = \"git+{approved}-fork?rev={revision}#{revision}\""),
                1,
            )
        });
    });
    let expected = format!(
        "{}: spl-core resolves from a source other than the workspace declaration; regenerate the lockfile from the declared Git source",
        fixture.path(LOCKFILE).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_duplicate_lock_package_resolution() {
    let fixture = PinFixture::new(|repo| {
        rewrite_lockfile(repo, |mut contents| {
            let (start, end) = lock_block_range(&contents, "spl-core");
            let duplicate = contents[start..end].to_owned();
            contents.push_str(&duplicate);
            contents
        });
    });
    let expected = format!(
        "{}: spl-core has multiple package resolutions; remove alternate resolutions by regenerating the lockfile from the workspace declaration",
        fixture.path(LOCKFILE).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_workspace_patch_routing() {
    let fixture = PinFixture::new(|repo| {
        append_text(
            &repo.join(WORKSPACE_MANIFEST),
            "\n[patch.crates-io]\nspl-core = { path = \"vendor/local-dependency\" }\n",
        );
    });
    let expected = format!(
        "{}: spl-core must not be routed through [patch]; remove the spl-core patch entry and use its [workspace.dependencies] declaration",
        fixture.path(WORKSPACE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_single_quoted_patch_key() {
    let fixture = PinFixture::new(|repo| {
        append_text(
            &repo.join(WORKSPACE_MANIFEST),
            "\n[patch.crates-io]\n'spl-core' = { path = \"vendor/local-dependency\" }\n",
        );
    });
    let expected = format!(
        "{}: spl-core must not be routed through [patch]; remove the spl-core patch entry and use its [workspace.dependencies] declaration",
        fixture.path(WORKSPACE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_native_patch_routing() {
    let fixture = PinFixture::new(|repo| {
        append_text(
            &repo.join(NATIVE_MANIFEST),
            "\n[patch.crates-io]\nspl-core = { path = \"vendor/local-dependency\" }\n",
        );
    });
    let expected = format!(
        "{}: spl-core must not be routed through [patch]; remove the spl-core patch entry and use its [workspace.dependencies] declaration",
        fixture.path(NATIVE_MANIFEST).display()
    );
    assert_exact_failure(&fixture.run(), &expected);
}

#[test]
fn spl_pin_rejects_tracked_in_tree_copy() {
    let fixture = PinFixture::new(|repo| {
        let copied = repo.join("vendor/spl-core/README.txt");
        fs::create_dir_all(copied.parent().expect("copied parent")).expect("create copied tree");
        fs::write(copied, b"fixture\n").expect("write copied tree fixture");
    });
    assert_exact_failure(
        &fixture.run(),
        "vendor/spl-core/README.txt: copied in-tree SPL implementation is forbidden",
    );
}

struct PinFixture {
    _root: TestDirectory,
    repo: PathBuf,
    helper: PathBuf,
}

impl PinFixture {
    fn new(mutate: impl FnOnce(&Path)) -> Self {
        Self::from_shipped(mutate, true)
    }

    fn untracked() -> Self {
        Self::from_shipped(|_| {}, false)
    }

    fn from_shipped(mutate: impl FnOnce(&Path), tracked: bool) -> Self {
        let root = TestDirectory::new("pin-guard");
        let repo = root.path().join("repo");
        fs::create_dir_all(repo.join("native/solstone-tmux"))
            .expect("create native fixture directory");
        let source_root = source_root();
        for relative in [WORKSPACE_MANIFEST, NATIVE_MANIFEST, LOCKFILE, DENY_CONFIG] {
            fs::copy(source_root.join(relative), repo.join(relative)).expect("copy pin input");
        }
        mutate(&repo);
        if tracked {
            initialize_git(&repo);
        }
        Self {
            _root: root,
            repo,
            helper: source_root.join("scripts/spl-pin.sh"),
        }
    }

    fn empty() -> Self {
        let root = TestDirectory::new("pin-guard");
        let repo = root.path().join("repo");
        fs::create_dir(&repo).expect("create empty fixture repository");
        initialize_git(&repo);
        Self {
            _root: root,
            repo,
            helper: source_root().join("scripts/spl-pin.sh"),
        }
    }

    fn repo(&self) -> &Path {
        &self.repo
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.repo.join(relative)
    }

    fn run(&self) -> Output {
        Command::new(&self.helper)
            .arg(&self.repo)
            .output()
            .expect("run shipped SPL pin guard")
    }
}

fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn initialize_git(repo: &Path) {
    run_checked(
        Command::new("git")
            .arg("init")
            .arg("-q")
            .arg("-b")
            .arg("main")
            .arg(repo),
    );
    run_checked(Command::new("git").current_dir(repo).args([
        "config",
        "user.name",
        "Pin Guard Test",
    ]));
    run_checked(Command::new("git").current_dir(repo).args([
        "config",
        "user.email",
        "pin-guard@example.invalid",
    ]));
    run_checked(Command::new("git").current_dir(repo).args(["add", "."]));
    run_checked(Command::new("git").current_dir(repo).args([
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "fixture",
    ]));
}

fn run_checked(command: &mut Command) {
    let output = command.output().expect("run fixture command");
    assert!(
        output.status.success(),
        "fixture command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "SPL pin guard failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "SPL pin guard wrote to stdout");
    assert!(output.stderr.is_empty(), "SPL pin guard wrote to stderr");
}

fn assert_exact_failure(output: &Output, expected: &str) {
    assert!(
        !output.status.success(),
        "SPL pin guard unexpectedly passed"
    );
    assert!(output.stdout.is_empty(), "SPL pin guard wrote to stdout");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!("{expected}\n")
    );
}

fn authority_diagnostic(fixture: &PinFixture) -> String {
    format!(
        "{}: spl-core and spl-transport require exactly one approved Git source in [sources].allow-git; set allow-git to an array containing one quoted Git URL",
        fixture.path(DENY_CONFIG).display()
    )
}

fn approved_source(repo: &Path) -> String {
    let contents = fs::read_to_string(repo.join(DENY_CONFIG)).expect("read deny fixture");
    contents
        .lines()
        .find(|line| line.trim_start().starts_with("allow-git ="))
        .and_then(|line| line.split('"').nth(1))
        .expect("approved source")
        .to_owned()
}

fn manifest_entry(repo: &Path, relative: &str, package: &str) -> String {
    let contents = fs::read_to_string(repo.join(relative)).expect("read manifest fixture");
    let prefix = format!("{package} =");
    contents
        .lines()
        .find(|line| line.starts_with(&prefix))
        .expect("package entry")
        .to_owned()
}

fn replace_manifest_entry(repo: &Path, relative: &str, package: &str, replacement: &str) {
    let path = repo.join(relative);
    let original = manifest_entry(repo, relative, package);
    replace_once(&path, &original, replacement);
}

fn remove_manifest_entry(repo: &Path, relative: &str, package: &str) {
    let path = repo.join(relative);
    let original = manifest_entry(repo, relative, package);
    replace_once(&path, &format!("{original}\n"), "");
}

fn duplicate_manifest_entry(repo: &Path, relative: &str, package: &str) {
    let path = repo.join(relative);
    let original = manifest_entry(repo, relative, package);
    replace_once(&path, &original, &format!("{original}\n{original}"));
}

fn inline_value(line: &str, key: &str) -> String {
    let prefix = format!("{key} = \"");
    let value = line
        .split_once(&prefix)
        .map(|(_, rest)| rest)
        .expect("inline key");
    value
        .split_once('"')
        .map(|(value, _)| value)
        .expect("quoted inline value")
        .to_owned()
}

fn workspace_revision(repo: &Path) -> String {
    inline_value(&manifest_entry(repo, WORKSPACE_MANIFEST, "spl-core"), "rev")
}

fn replace_workspace_revision(repo: &Path, package: &str, replacement: &str) {
    let line = manifest_entry(repo, WORKSPACE_MANIFEST, package);
    let revision = inline_value(&line, "rev");
    replace_manifest_entry(
        repo,
        WORKSPACE_MANIFEST,
        package,
        &line.replace(&revision, replacement),
    );
}

fn different_revision(revision: &str) -> String {
    let mut bytes = revision.as_bytes().to_vec();
    bytes[0] = if bytes[0] == b'0' { b'1' } else { b'0' };
    String::from_utf8(bytes).expect("ASCII revision")
}

fn remove_matching_line(path: &Path, prefix: &str) {
    let contents = fs::read_to_string(path).expect("read fixture file");
    let mut matches = 0;
    let rewritten = contents
        .lines()
        .filter(|line| {
            let keep = !line.trim_start().starts_with(prefix);
            if !keep {
                matches += 1;
            }
            keep
        })
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    assert_eq!(matches, 1, "one matching line");
    fs::write(path, rewritten).expect("rewrite fixture file");
}

fn replace_matching_line(path: &Path, prefix: &str, replacement: &str) {
    let contents = fs::read_to_string(path).expect("read fixture file");
    let original = contents
        .lines()
        .find(|line| line.trim_start().starts_with(prefix))
        .expect("matching line");
    replace_once(path, original, replacement);
}

fn replace_once(path: &Path, original: &str, replacement: &str) {
    let contents = fs::read_to_string(path).expect("read fixture file");
    assert_eq!(contents.matches(original).count(), 1, "one replacement");
    fs::write(path, contents.replacen(original, replacement, 1)).expect("rewrite fixture file");
}

fn append_text(path: &Path, suffix: &str) {
    let mut contents = fs::read_to_string(path).expect("read fixture file");
    contents.push_str(suffix);
    fs::write(path, contents).expect("append fixture text");
}

fn rewrite_lockfile(repo: &Path, rewrite: impl FnOnce(String) -> String) {
    let path = repo.join(LOCKFILE);
    let contents = fs::read_to_string(&path).expect("read lock fixture");
    fs::write(path, rewrite(contents)).expect("rewrite lock fixture");
}

fn rewrite_lock_package(repo: &Path, package: &str, rewrite: impl FnOnce(&str) -> String) {
    rewrite_lockfile(repo, |contents| {
        let (start, end) = lock_block_range(&contents, package);
        format!(
            "{}{}{}",
            &contents[..start],
            rewrite(&contents[start..end]),
            &contents[end..]
        )
    });
}

fn lock_block_range(contents: &str, package: &str) -> (usize, usize) {
    let marker = format!("[[package]]\nname = \"{package}\"\n");
    let start = contents.find(&marker).expect("package block");
    let after_marker = start + marker.len();
    let end = contents[after_marker..]
        .find("\n[[package]]")
        .map_or(contents.len(), |offset| after_marker + offset + 1);
    (start, end)
}
