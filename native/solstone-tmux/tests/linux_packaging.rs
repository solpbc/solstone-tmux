// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

#![cfg(unix)]

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use support::TestDirectory;

const MEMBERS: &str = "./usr/bin/solstone-tmux\n./usr/share/doc/solstone-tmux\n./usr/share/doc/solstone-tmux/LICENSE\n./usr/share/doc/solstone-tmux/RUST_DEPENDENCY_NOTICES.txt";

fn executable(path: &Path, body: &str) {
    fs::write(path, body).expect("fixture script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("fixture mode");
}

#[test]
fn rpm_payload_gate_checks_bytes_members_and_subprocess_failures() {
    let temp = TestDirectory::new("rpm-payload-gate");
    let tools = temp.path().join("tools");
    fs::create_dir(&tools).expect("fixture tools");
    executable(
        &tools.join("rpm2cpio"),
        "#!/bin/sh\n[ \"$FAIL_DECOMPRESS\" = 1 ] && exit 7\ncat \"$1\"\n",
    );
    executable(
        &tools.join("cpio"),
        "#!/bin/sh\ncase \"$2\" in --list) [ \"$FAIL_LIST\" = 1 ] && exit 8; printf '%s\\n' \"$MEMBERS\" ;; --extract) [ \"$FAIL_EXTRACT\" = 1 ] && exit 9; case \"$4\" in ./usr/bin/solstone-tmux) cat ;; *) cat \"$REPO_ROOT/${4##*/}\" ;; esac ;; *) exit 10 ;; esac\n",
    );
    let source = temp.path().join("source");
    let package = temp.path().join("package");
    fs::write(&source, b"exact source executable").expect("source fixture");
    fs::write(&package, b"exact source executable").expect("package fixture");
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let helper = repo_root.join("packaging/linux/verify-rpm-payload.sh");
    let run = |members: &str, decompress: &str, list: &str, extract: &str| {
        Command::new("bash")
            .arg(&helper)
            .arg(&package)
            .arg(&source)
            .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
            .env("MEMBERS", members)
            .env("REPO_ROOT", &repo_root)
            .env("FAIL_DECOMPRESS", decompress)
            .env("FAIL_LIST", list)
            .env("FAIL_EXTRACT", extract)
            .output()
            .expect("payload gate")
            .status
    };
    assert!(run(MEMBERS, "0", "0", "0").success());
    fs::write(&package, b"stripped source executable").expect("mutation fixture");
    assert!(!run(MEMBERS, "0", "0", "0").success());
    fs::write(&package, b"exact source executable").expect("restore fixture");
    assert!(
        !run(
            &format!("{MEMBERS}\n./usr/bin/solstone-tmux"),
            "0",
            "0",
            "0"
        )
        .success()
    );
    // A package without the dependency notices is refused.
    assert!(
        !run(
            "./usr/bin/solstone-tmux\n./usr/share/doc/solstone-tmux\n./usr/share/doc/solstone-tmux/LICENSE",
            "0",
            "0",
            "0"
        )
        .success()
    );
    assert!(!run("./usr/bin/other", "0", "0", "0").success());
    assert_eq!(run(MEMBERS, "1", "0", "0").code(), Some(7));
    assert_eq!(run(MEMBERS, "0", "1", "0").code(), Some(8));
    assert_eq!(run(MEMBERS, "0", "0", "1").code(), Some(9));
}
