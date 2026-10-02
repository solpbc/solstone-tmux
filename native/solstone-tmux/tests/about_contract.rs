// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use solstone_tmux::about::{
    AboutBlock, HostFacts, decode_about, journal_line, native_macos_arch, normalize_arch,
    parse_os_release, render_line,
};
use solstone_tmux::journal_version::JournalVersionStatus;

#[test]
fn authority_import_is_byte_exact_and_behavior_uses_the_imported_literals() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let import: Value = serde_json::from_slice(
        &std::fs::read(root.join("contracts/about-contract-import.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        import["authority_repository"],
        "https://github.com/solpbc/solstone-journal"
    );
    assert_eq!(
        import["authority_commit"],
        "ec1983799b66d3616708851d01803e4f3d6f0a20"
    );
    assert_eq!(
        import["source_path"],
        "core/crates/solstone-core-about/bundle"
    );
    let vendor = root.join("vendor/about-contract");
    let bytes = std::fs::read(vendor.join("manifest.json")).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        import["manifest_sha256"]
    );
    let manifest: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(manifest["bundle_version"], import["bundle_version"]);
    let artifacts = manifest["artifacts"].as_object().unwrap();
    for (name, digest) in artifacts {
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(std::fs::read(vendor.join(name)).unwrap())
            ),
            digest.as_str().unwrap()
        );
    }
    let actual = std::fs::read_dir(&vendor)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<BTreeSet<_>>();
    let expected = artifacts
        .keys()
        .cloned()
        .chain(std::iter::once("manifest.json".into()))
        .collect();
    assert_eq!(actual, expected);
    let contract: Value =
        serde_json::from_slice(&std::fs::read(vendor.join("contract.json")).unwrap()).unwrap();
    for fixture in contract["fixtures"].as_array().unwrap() {
        assert_eq!(
            render_line(
                "journal",
                fixture["version"].as_str().unwrap(),
                fixture["build"].as_str(),
                fixture["os"].as_str().unwrap(),
                fixture["os_version"].as_str().unwrap(),
                fixture["arch"].as_str().unwrap()
            ),
            fixture["about"].as_str().unwrap()
        );
    }
    for (canonical, aliases) in contract["arch_aliases"].as_object().unwrap() {
        for alias in aliases.as_array().unwrap() {
            assert_eq!(normalize_arch(alias.as_str().unwrap()), canonical);
        }
    }
    let resources: Value =
        serde_json::from_slice(&std::fs::read(vendor.join("resources.json")).unwrap()).unwrap();
    for resource in resources["valid"].as_array().unwrap() {
        assert!(decode_about(&serde_json::to_vec(resource).unwrap()).is_some());
    }
    for resource in resources["invalid"].as_array().unwrap() {
        assert!(decode_about(&serde_json::to_vec(resource).unwrap()).is_none());
    }
}

#[test]
fn current_last_known_legacy_unknown_and_crossed_versions_use_one_renderer() {
    let facts = decode_about(r#"{"protocol_version":1,"version":"1.2.3","os":"ubuntu","os_version":"24.04","arch":"x86_64","about":"journal 1.2.3 · ubuntu 24.04 · x86_64"}"#.as_bytes()).unwrap();
    assert_eq!(
        journal_line(
            &JournalVersionStatus::Current("v1.2.3".into()),
            Some(&facts),
            Some(0),
            172800
        ),
        facts.about
    );
    assert_eq!(
        journal_line(
            &JournalVersionStatus::LastKnown("1.2.3".into()),
            Some(&facts),
            Some(0),
            172800
        ),
        "journal 1.2.3 · ubuntu 24.04 · x86_64 · last seen 2 days ago"
    );
    assert_eq!(
        journal_line(
            &JournalVersionStatus::LastKnown("1.2.3".into()),
            None,
            None,
            172800
        ),
        "journal 1.2.3"
    );
    assert_eq!(
        journal_line(
            &JournalVersionStatus::Current("2.0.29".into()),
            Some(&facts),
            Some(0),
            172800
        ),
        "journal 2.0.29"
    );
    assert_eq!(
        journal_line(
            &JournalVersionStatus::Unknown,
            Some(&facts),
            Some(0),
            172800
        ),
        "journal unknown"
    );
}

#[test]
fn native_machine_and_marketing_os_never_use_process_slice_or_private_names() {
    assert_eq!(native_macos_arch(Some(true), Some("x86_64")), "arm64");
    assert_eq!(native_macos_arch(Some(false), Some("x86_64")), "x86_64");
    assert_eq!(native_macos_arch(None, Some("arm64")), "");
    assert_eq!(native_macos_arch(Some(false), None), "");
    assert_eq!(
        parse_os_release("NAME=PRIVATE_OWNER_DISTRO\nID=ubuntu\nVERSION_ID=\"24.04\"\n"),
        ("ubuntu".into(), "24.04".into())
    );
    assert_eq!(
        parse_os_release("ID=fedora\n"),
        ("fedora".into(), "".into())
    );
    assert_eq!(
        parse_os_release("NAME=PRIVATE_OWNER_DISTRO\n"),
        ("linux".into(), "".into())
    );
}

#[test]
fn report_preserves_snapshot_bytes_and_projects_out_populated_private_fields() {
    let mut resource = json!({"protocol_version":1,"version":"2.0.29","os":"ubuntu","os_version":"24.04","arch":"x86_64","about":"journal 2.0.29 · ubuntu 24.04 · x86_64"});
    for field in [
        "name",
        "hostname",
        "owner_label",
        "account",
        "path",
        "instance_id",
        "ca_fingerprint",
        "address",
        "provider",
        "model",
    ] {
        resource[field] = json!(format!("PRIVATE_{field}"));
    }
    assert!(
        serde_json::to_string(&resource)
            .unwrap()
            .contains("PRIVATE_hostname")
    );
    let facts = decode_about(&serde_json::to_vec(&resource).unwrap()).unwrap();
    assert!(!serde_json::to_string(&facts).unwrap().contains("PRIVATE_"));
    let block = AboutBlock {
        text: format!("tmux app 2.0.11 · macos 26.5 · arm64\n{}", facts.about),
        host: HostFacts {
            os: "macos".into(),
            os_version: "26.5".into(),
            arch: "arm64".into(),
        },
    };
    let url = solstone_tmux::support::report_url("offline", &block);
    let parsed = reqwest::Url::parse(&url).unwrap();
    let fields = reqwest::Url::parse(&format!(
        "https://example.invalid/?{}",
        parsed.fragment().unwrap()
    ))
    .unwrap();
    let about = fields
        .query_pairs()
        .find(|(name, _)| name == "about")
        .unwrap()
        .1
        .into_owned();
    assert_eq!(about, block.text);
    assert!(parsed.query().is_none());
    for forbidden in [
        "PRIVATE_",
        "hostname",
        "instance_id",
        "provider",
        "build=",
        "%2Fhome%2F",
        "%2FUsers%2F",
    ] {
        assert!(!url.contains(forbidden));
    }
    let producer = include_str!("../src/support.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert!(!producer.contains("SOLSTONE_TMUX_SOURCE_COMMIT"));
    for forbidden in [
        ".journal_name",
        ".home_label",
        ".instance_id",
        ".hostname",
        ".client_cert",
        ".model",
        ".provider",
    ] {
        assert!(!producer.contains(forbidden));
    }
    resource["about"] = json!("journal 2.0.29 · wrong facts");
    assert!(decode_about(&serde_json::to_vec(&resource).unwrap()).is_none());
    resource["build"] = Value::Null;
    assert!(decode_about(&serde_json::to_vec(&resource).unwrap()).is_none());
}
