// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use solstone_tmux::config::{CONFIG_FILENAME, RuntimeConfig};
use solstone_tmux::health::{HEALTH_FILENAME, HealthWriter, SyncFacts};
use solstone_tmux::instance_lock::InstanceLock;
use solstone_tmux::migration::{IMPORTED_LEGACY_FIELDS, MigrationOutcome, migrate_legacy_config};
use solstone_tmux::paths::{PlatformKind, ensure_private_directory};
use support::{IsolatedRoots, TestDirectory};

const FIXTURE_SHA256: &str = "8ffd795ed331fddef033a920fb6bf5934887b58bda3d6f6cf7b4214ad18c9ceb";
const KEY_CANARY: &str = "LEGACYKEYCANARY-do-not-copy";
const URL_CANARY: &str = "http://legacy-canary.invalid:5015";
const EXPECTED_NATIVE: &[u8] = br#"{"stream":"extro.tmux","capture_interval":7,"segment_interval":600,"cache_retention_days":14,"status_indicator":false}"#;

#[test]
fn real_legacy_fixture_migrates_exact_fields_without_mutating_source_or_captures() {
    let fixture = MigrationFixture::new("migration-real-fixture");
    let legacy_bytes = legacy_fixture_bytes();
    fixture.write_legacy(&legacy_bytes);
    fixture.populate_capture_tree();
    let legacy_before = FileSnapshot::at(&fixture.legacy_path());
    let captures_before = tree_snapshot(&fixture.captures_root());

    let outcome = fixture.migrate("ignored.example");

    assert_eq!(outcome, MigrationOutcome::Migrated);
    assert_eq!(IMPORTED_LEGACY_FIELDS.len(), 5);
    assert_eq!(
        fs::read(fixture.native_path()).expect("read migrated config"),
        EXPECTED_NATIVE
    );
    assert_eq!(
        fs::metadata(fixture.native_path())
            .expect("native metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(FileSnapshot::at(&fixture.legacy_path()), legacy_before);
    assert_eq!(tree_snapshot(&fixture.captures_root()), captures_before);
    assert_no_canaries(&fs::read(fixture.native_path()).expect("native bytes"));
}

#[test]
fn fixture_digest_and_exact_writer_bytes_are_frozen() {
    let bytes = legacy_fixture_bytes();
    assert_eq!(bytes.len(), 321);
    assert_eq!(format!("{:x}", Sha256::digest(&bytes)), FIXTURE_SHA256);
    assert!(bytes.ends_with(b"\n"));
    assert!(
        bytes
            .windows(KEY_CANARY.len())
            .any(|part| part == KEY_CANARY.as_bytes())
    );
    assert!(
        bytes
            .windows(URL_CANARY.len())
            .any(|part| part == URL_CANARY.as_bytes())
    );
}

#[test]
fn existing_native_and_repeat_migration_are_noops() {
    let fixture = MigrationFixture::new("migration-native-present");
    let native = br#"{"stream":"kept","capture_interval":9}"#;
    fs::write(fixture.native_path(), native).expect("write native config");
    fs::create_dir_all(fixture.legacy_path().parent().expect("legacy parent"))
        .expect("legacy parent");
    symlink(
        fixture.data_root.join("missing-legacy-referent"),
        fixture.legacy_path(),
    )
    .expect("unreadable legacy alias");

    assert_eq!(
        fixture.migrate("ignored.example"),
        MigrationOutcome::NativePresent
    );
    assert_eq!(
        fs::read(fixture.native_path()).expect("native remains"),
        native
    );
    assert!(
        fs::symlink_metadata(fixture.legacy_path())
            .expect("legacy alias")
            .file_type()
            .is_symlink()
    );

    fs::remove_file(fixture.native_path()).expect("remove initial native config");
    fs::remove_file(fixture.legacy_path()).expect("remove legacy alias");
    fixture.write_legacy(&legacy_fixture_bytes());
    assert_eq!(
        fixture.migrate("ignored.example"),
        MigrationOutcome::Migrated
    );
    let first = FileSnapshot::at(&fixture.native_path());
    assert_eq!(
        fixture.migrate("ignored.example"),
        MigrationOutcome::NativePresent
    );
    assert_eq!(FileSnapshot::at(&fixture.native_path()), first);
}

#[test]
fn ignored_legacy_fields_and_unknown_fields_never_reach_native_settings() {
    let fixture = MigrationFixture::new("migration-ignored-fields");
    let mut legacy =
        serde_json::from_slice::<serde_json::Value>(&legacy_fixture_bytes()).expect("fixture JSON");
    legacy["unknown"] = serde_json::json!({"nested": "value"});
    fixture.write_legacy(&serde_json::to_vec(&legacy).expect("mutated fixture"));

    assert_eq!(
        fixture.migrate("ignored.example"),
        MigrationOutcome::Migrated
    );
    let bytes = fs::read(fixture.native_path()).expect("native config");
    assert_eq!(bytes, EXPECTED_NATIVE);
    assert_no_canaries(&bytes);
    assert!(!String::from_utf8_lossy(&bytes).contains("sync_retry"));
    assert!(!String::from_utf8_lossy(&bytes).contains("unknown"));
}

#[test]
fn empty_legacy_stream_uses_the_native_hostname_default() {
    let fixture = MigrationFixture::new("migration-empty-stream");
    let empty_fixture = fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/legacy/config-empty-stream.json"),
    )
    .expect("read Python-authored empty-stream fixture");
    assert_eq!(
        format!("{:x}", Sha256::digest(&empty_fixture)),
        "58864ab5a3ab3c35aa58437d5853f2c497ce0f39e9dd0f8b0d75da1b34a1688f"
    );
    fixture.write_legacy(&empty_fixture);

    assert_eq!(
        fixture.migrate("Owner Host.example.com"),
        MigrationOutcome::Migrated
    );
    let config = RuntimeConfig::load(&fixture.config_root, "Owner Host.example.com")
        .expect("load migrated defaults");
    assert_eq!(config.stream.as_str(), "owner-host.tmux");
    assert_eq!(
        fs::read(fixture.native_path()).expect("native config"),
        br#"{"stream":null,"capture_interval":7,"segment_interval":600,"cache_retention_days":14,"status_indicator":false}"#
    );
}

#[test]
fn missing_legacy_and_macos_are_non_mutating() {
    let fixture = MigrationFixture::new("migration-absent");
    assert_eq!(
        fixture.migrate("ignored.example"),
        MigrationOutcome::LegacyAbsent
    );
    assert!(!fixture.native_path().exists());

    let temporary = TestDirectory::new("migration-macos");
    let missing_data = temporary.path().join("data-does-not-exist");
    let missing_config = temporary.path().join("config-does-not-exist");
    assert_eq!(
        migrate_legacy_config(
            PlatformKind::Macos,
            &missing_data,
            &missing_config,
            "ignored.example"
        )
        .expect("macOS no-op"),
        MigrationOutcome::NotApplicable
    );
    assert!(!missing_data.exists());
    assert!(!missing_config.exists());
}

#[test]
fn invalid_legacy_settings_leave_both_locations_unchanged() {
    for (label, bytes, expected) in [
        (
            "syntax",
            b"{LEGACYKEYCANARY-do-not-copy".as_slice(),
            "legacy settings are invalid for",
        ),
        (
            "type",
            br#"{"capture_interval":"LEGACYKEYCANARY-do-not-copy"}"#.as_slice(),
            "legacy settings are invalid for",
        ),
        (
            "interval",
            br#"{"capture_interval":0}"#.as_slice(),
            "legacy settings could not be validated for",
        ),
        (
            "stream",
            br#"{"stream":"........................................................................................................................................................................................................."}"#.as_slice(),
            "legacy settings could not be validated for",
        ),
    ] {
        let fixture = MigrationFixture::new(&format!("migration-invalid-{label}"));
        fixture.write_legacy(bytes);
        let before = FileSnapshot::at(&fixture.legacy_path());

        let error = fixture.migrate_error("ignored.example");

        assert!(error.starts_with(expected), "{error}");
        assert_eq!(FileSnapshot::at(&fixture.legacy_path()), before);
        assert!(!fixture.native_path().exists());
        assert_no_canaries(error.as_bytes());
    }
}

#[test]
fn symlink_special_and_unreadable_legacy_settings_are_refused_unchanged() {
    let symlink_fixture = MigrationFixture::new("migration-legacy-symlink");
    let referent = symlink_fixture.data_root.join("legacy-referent");
    fs::write(&referent, legacy_fixture_bytes()).expect("write referent");
    fs::create_dir_all(
        symlink_fixture
            .legacy_path()
            .parent()
            .expect("legacy parent"),
    )
    .expect("legacy directory");
    symlink(&referent, symlink_fixture.legacy_path()).expect("legacy symlink");
    let referent_before = FileSnapshot::at(&referent);
    let error = symlink_fixture.migrate_error("ignored.example");
    assert!(error.starts_with("legacy settings are invalid for"));
    assert_no_canaries(error.as_bytes());
    assert_eq!(FileSnapshot::at(&referent), referent_before);
    assert!(
        fs::symlink_metadata(symlink_fixture.legacy_path())
            .expect("symlink metadata")
            .file_type()
            .is_symlink()
    );
    assert!(!symlink_fixture.native_path().exists());

    let special_fixture = MigrationFixture::new("migration-legacy-special");
    fs::create_dir_all(special_fixture.legacy_path()).expect("special legacy directory");
    let error = special_fixture.migrate_error("ignored.example");
    assert!(error.starts_with("legacy settings are invalid for"));
    assert_no_canaries(error.as_bytes());
    assert!(special_fixture.legacy_path().is_dir());
    assert!(!special_fixture.native_path().exists());

    let unreadable_fixture = MigrationFixture::new("migration-legacy-unreadable");
    unreadable_fixture.write_legacy(&legacy_fixture_bytes());
    let before = fs::read(unreadable_fixture.legacy_path()).expect("snapshot unreadable source");
    fs::set_permissions(
        unreadable_fixture.legacy_path(),
        fs::Permissions::from_mode(0o000),
    )
    .expect("remove legacy permissions");
    let error = unreadable_fixture.migrate_error("ignored.example");
    fs::set_permissions(
        unreadable_fixture.legacy_path(),
        fs::Permissions::from_mode(0o600),
    )
    .expect("restore legacy permissions");
    assert!(error.starts_with("legacy settings could not be read while preparing"));
    assert_no_canaries(error.as_bytes());
    assert_eq!(
        fs::read(unreadable_fixture.legacy_path()).expect("legacy remains"),
        before
    );
    assert!(!unreadable_fixture.native_path().exists());

    let inspect_fixture = MigrationFixture::new("migration-legacy-inspect");
    let legacy_path = inspect_fixture.legacy_path();
    let legacy_parent = legacy_path.parent().expect("legacy parent");
    fs::create_dir_all(legacy_parent).expect("legacy parent");
    fs::set_permissions(legacy_parent, fs::Permissions::from_mode(0o000))
        .expect("remove legacy parent permissions");
    let error = inspect_fixture.migrate_error("ignored.example");
    fs::set_permissions(legacy_parent, fs::Permissions::from_mode(0o700))
        .expect("restore legacy parent permissions");
    assert!(error.starts_with("legacy settings could not be inspected while preparing"));
    assert_no_canaries(error.as_bytes());
    assert!(!inspect_fixture.native_path().exists());
}

#[test]
fn symlink_and_special_native_targets_are_refused_before_legacy_read() {
    for special in [false, true] {
        let fixture = MigrationFixture::new(if special {
            "migration-native-special"
        } else {
            "migration-native-symlink"
        });
        fixture.write_legacy(&legacy_fixture_bytes());
        let legacy_before = FileSnapshot::at(&fixture.legacy_path());
        if special {
            fs::create_dir(fixture.native_path()).expect("native special directory");
        } else {
            let referent = fixture.config_root.join("native-referent");
            fs::write(&referent, b"native referent").expect("native referent");
            symlink(&referent, fixture.native_path()).expect("native symlink");
        }

        let error = fixture.migrate_error("ignored.example");

        assert_eq!(
            error,
            format!(
                "native settings target is not a regular file: {}",
                fixture.native_path().display()
            )
        );
        assert_no_canaries(error.as_bytes());
        assert_eq!(FileSnapshot::at(&fixture.legacy_path()), legacy_before);
    }
}

#[test]
fn write_failure_before_rename_leaves_native_absent_and_legacy_unchanged() {
    let fixture = MigrationFixture::new("migration-before-rename");
    fixture.write_legacy(&legacy_fixture_bytes());
    let legacy_before = FileSnapshot::at(&fixture.legacy_path());
    fs::set_permissions(&fixture.config_root, fs::Permissions::from_mode(0o500))
        .expect("make destination read-only");

    let error = fixture.migrate_error("ignored.example");

    fs::set_permissions(&fixture.config_root, fs::Permissions::from_mode(0o700))
        .expect("restore destination permissions");
    assert!(error.starts_with("migrated settings could not be written to"));
    assert_no_canaries(error.as_bytes());
    assert!(!fixture.native_path().exists());
    assert_eq!(FileSnapshot::at(&fixture.legacy_path()), legacy_before);
}

#[cfg_attr(target_os = "macos", ignore = "legacy Python migration is Linux-only")]
#[tokio::test]
async fn canaries_are_absent_from_native_errors_health_and_captured_stderr() {
    let fixture = MigrationFixture::new("migration-canary-surfaces");
    fixture.write_legacy(&legacy_fixture_bytes());
    assert_eq!(
        fixture.migrate("ignored.example"),
        MigrationOutcome::Migrated
    );
    assert_no_canaries(&fs::read(fixture.native_path()).expect("native config"));

    let health_root = fixture
        ._temporary
        .path()
        .join("health-root-without-sensitive-input");
    ensure_private_directory(&health_root).expect("health root");
    let lock = InstanceLock::acquire(&health_root).expect("health lock");
    HealthWriter::new(health_root.clone(), &lock)
        .write(&SyncFacts::default(), 1)
        .await
        .expect("health snapshot");
    assert_no_canaries(&fs::read(health_root.join(HEALTH_FILENAME)).expect("health bytes"));

    let stderr_fixture = BinaryMigrationFixture::new("migration-canary-stderr");
    stderr_fixture.write_invalid_legacy_with_canaries();
    let output = stderr_fixture.run();
    assert_eq!(output.status.code(), Some(1));
    assert_no_canaries(&output.stderr);
    assert!(String::from_utf8_lossy(&output.stderr).contains("legacy settings are invalid for"));
}

fn legacy_fixture_bytes() -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/legacy")
            .join(CONFIG_FILENAME),
    )
    .expect("read real legacy fixture")
}

fn assert_no_canaries(bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    assert!(!text.contains(KEY_CANARY), "key canary leaked: {text}");
    assert!(!text.contains(URL_CANARY), "URL canary leaked: {text}");
}

struct MigrationFixture {
    _temporary: TestDirectory,
    data_root: PathBuf,
    config_root: PathBuf,
}

impl MigrationFixture {
    fn new(label: &str) -> Self {
        let temporary = TestDirectory::new(label);
        let data_root = temporary.path().join("data");
        let config_root = temporary.path().join("native-config");
        ensure_private_directory(&data_root).expect("data root");
        ensure_private_directory(&config_root).expect("config root");
        Self {
            _temporary: temporary,
            data_root,
            config_root,
        }
    }

    fn legacy_path(&self) -> PathBuf {
        self.data_root.join("config").join(CONFIG_FILENAME)
    }

    fn native_path(&self) -> PathBuf {
        self.config_root.join(CONFIG_FILENAME)
    }

    fn captures_root(&self) -> PathBuf {
        self.data_root.join("captures")
    }

    fn write_legacy(&self, bytes: &[u8]) {
        fs::create_dir_all(self.legacy_path().parent().expect("legacy parent"))
            .expect("legacy parent");
        fs::write(self.legacy_path(), bytes).expect("write legacy settings");
        fs::set_permissions(self.legacy_path(), fs::Permissions::from_mode(0o640))
            .expect("legacy mode");
    }

    fn populate_capture_tree(&self) {
        for relative in [
            "20260729/extro.tmux/120000_300",
            "20260729/extro.tmux/121000.incomplete",
            "20260729/extro.tmux/122000.failed",
        ] {
            let directory = self.captures_root().join(relative);
            fs::create_dir_all(&directory).expect("capture fixture directory");
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o750))
                .expect("capture directory mode");
            let payload = directory.join("tmux_main_screen.jsonl");
            fs::write(&payload, format!("{relative}\n")).expect("capture fixture file");
            fs::set_permissions(payload, fs::Permissions::from_mode(0o640))
                .expect("capture file mode");
        }
    }

    fn migrate(&self, hostname: &str) -> MigrationOutcome {
        migrate_legacy_config(
            PlatformKind::Linux,
            &self.data_root,
            &self.config_root,
            hostname,
        )
        .expect("migration succeeds")
    }

    fn migrate_error(&self, hostname: &str) -> String {
        migrate_legacy_config(
            PlatformKind::Linux,
            &self.data_root,
            &self.config_root,
            hostname,
        )
        .expect_err("migration must fail")
        .to_string()
    }
}

struct BinaryMigrationFixture {
    _temporary: TestDirectory,
    roots: IsolatedRoots,
}

impl BinaryMigrationFixture {
    fn new(label: &str) -> Self {
        let temporary = TestDirectory::new(label);
        let roots = IsolatedRoots::new(temporary.path());
        Self {
            _temporary: temporary,
            roots,
        }
    }

    fn write_invalid_legacy_with_canaries(&self) {
        let path = self.roots.data_root().join("config").join(CONFIG_FILENAME);
        fs::create_dir_all(path.parent().expect("legacy parent")).expect("legacy parent");
        fs::write(
            path,
            format!(r#"{{"capture_interval":"{KEY_CANARY}","server_url":"{URL_CANARY}"}}"#),
        )
        .expect("invalid legacy settings");
    }

    fn run(&self) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_solstone-tmux"))
            .arg("run")
            .env_clear()
            .envs(self.roots.entries().iter().cloned())
            .output()
            .expect("run migration binary")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileSnapshot {
    bytes: Vec<u8>,
    mode: u32,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

impl FileSnapshot {
    fn at(path: &Path) -> Self {
        let metadata = fs::metadata(path).expect("snapshot metadata");
        Self {
            bytes: fs::read(path).expect("snapshot bytes"),
            mode: metadata.mode(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TreeEntry {
    directory: bool,
    size: u64,
    mode: u32,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

fn tree_snapshot(root: &Path) -> BTreeMap<PathBuf, TreeEntry> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, TreeEntry>) {
        let metadata = fs::metadata(path).expect("tree metadata");
        snapshot.insert(
            path.strip_prefix(root)
                .expect("tree entry under root")
                .to_owned(),
            TreeEntry {
                directory: metadata.is_dir(),
                size: metadata.size(),
                mode: metadata.mode(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
            },
        );
        if metadata.is_dir() {
            let mut children = fs::read_dir(path)
                .expect("tree directory")
                .map(|entry| entry.expect("tree entry").path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                visit(root, &child, snapshot);
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}
