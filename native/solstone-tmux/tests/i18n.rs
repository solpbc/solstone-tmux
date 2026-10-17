// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use solstone_tmux::i18n::{
    ALL_MESSAGES, ArgKind, ArgValue, Locale, MessageId, try_render, with_locale,
};

#[test]
fn complete_discovered_template_corpus_matches_original_rust_bytes() {
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("data/i18n/en-US-before.json")).unwrap();
    assert_eq!(rows.len(), 885, "corpus floor must change explicitly");
    for row in rows {
        let key = row["id"].as_str().unwrap();
        if key.starts_with("real.") {
            continue;
        }
        let id = *ALL_MESSAGES
            .iter()
            .find(|id| id.key() == key.replace('.', "-"))
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(format!(
                "{}/i18n/meta/{}.json",
                env!("CARGO_MANIFEST_DIR"),
                id.key()
            ))
            .unwrap(),
        )
        .unwrap();
        let values = row["args"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| {
                let mut value = value.as_str().unwrap().to_owned();
                let spec = meta["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|arg| arg["name"] == *name)
                    .unwrap()["rust_format_spec"]
                    .as_str()
                    .unwrap();
                if spec == "?" {
                    value = format!("{value:?}");
                }
                assert!(
                    matches!(spec, "" | "?"),
                    "uncovered Rust format spec {spec}"
                );
                (name.as_str(), ArgValue::Text(value))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            try_render(Locale::EnUs, id, &values).unwrap(),
            row["output"].as_str().unwrap(),
            "{key}"
        );
    }
}

#[test]
fn actual_help_error_and_relative_plural_paths_match_pre_application_bytes() {
    with_locale(Locale::EnUs, || {
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("data/i18n/en-US-before.json")).unwrap();
        for row in rows
            .iter()
            .filter(|row| row["id"].as_str().unwrap().starts_with("real."))
        {
            let id = row["id"].as_str().unwrap();
            let actual = if let Some(seconds) = id.strip_prefix("real.about.age.") {
                solstone_tmux::about::journal_line(
                    &solstone_tmux::journal_version::JournalVersionStatus::LastKnown(
                        "1.2.3".into(),
                    ),
                    None,
                    Some(1_000_000),
                    1_000_000 + seconds.parse::<i64>().unwrap(),
                )
            } else if id == "real.cli.help" {
                solstone_tmux::cli::usage()
            } else {
                solstone_tmux::cli::parse_args(
                    ["solstone-tmux", "synthetic-command"]
                        .into_iter()
                        .map(Into::into),
                )
                .unwrap_err()
                .to_string()
            };
            assert_eq!(actual, row["output"].as_str().unwrap(), "{id}");
        }
    });
}

#[test]
fn every_catalog_message_formats_in_both_locales_and_rejects_bad_typed_arguments() {
    assert_eq!(ALL_MESSAGES.len(), 300);
    for &id in ALL_MESSAGES {
        let values = id
            .schema()
            .iter()
            .map(|&(name, kind)| {
                (
                    name,
                    match kind {
                        ArgKind::Text => ArgValue::Text("日本語 {value} yes --mark".into()),
                        ArgKind::Number => ArgValue::Number(2),
                        ArgKind::Datetime => ArgValue::Datetime(0),
                    },
                )
            })
            .collect::<Vec<_>>();
        for locale in [Locale::EnUs, Locale::EnXa] {
            let text = try_render(locale, id, &values).unwrap();
            assert!(!text.is_empty(), "{}", id.key());
            for (_, value) in &values {
                if let ArgValue::Text(value) = value {
                    assert!(text.contains(value), "arguments changed: {}", id.key());
                }
            }
        }
        if !values.is_empty() {
            assert!(try_render(Locale::EnUs, id, &[]).is_err());
        }
    }
    assert!(
        try_render(
            Locale::EnUs,
            MessageId::CliAboutAgeMinute,
            &[("value", ArgValue::Text("1".into()))]
        )
        .is_err()
    );
}

#[test]
fn locale_precedence_and_default_are_explicit() {
    let env = |key: &str| match key {
        "LC_ALL" => Some("en_XA.UTF-8".into()),
        "LC_MESSAGES" => Some("en-US".into()),
        "LANG" => Some("en-XA".into()),
        _ => None,
    };
    assert_eq!(Locale::resolve(None, env), Locale::EnXa);
    assert_eq!(Locale::resolve(Some("en-US"), env), Locale::EnUs);
    assert_eq!(
        Locale::resolve(None, |key| if key == "LANG" {
            Some("tr_TR.UTF-8".into())
        } else {
            None
        }),
        Locale::EnUs
    );
    assert_eq!(
        Locale::resolve(None, |key| if key == "LC_MESSAGES" {
            Some("en-XA".into())
        } else {
            None
        }),
        Locale::EnXa
    );
    assert_eq!(Locale::resolve(None, |_| None), Locale::EnUs);
}

#[test]
fn pseudo_locale_preserves_tokens_marks_and_machine_contracts() {
    with_locale(Locale::EnXa, || {
        let question = solstone_tmux::pairing_answer::ASK_IDENTIFIED.text();
        assert!(question.starts_with('['));
        assert!(question.contains("yes"));
        assert!(question.contains("no"));
        let alternate = solstone_tmux::pairing_answer::ASK_UNAVAILABLE.text();
        assert!(alternate.contains("continue"));
        assert!(alternate.contains("cancel"));
        let help = solstone_tmux::cli::usage();
        assert!(help.contains("solstone-tmux setup [--mark"));
        assert_eq!(
            solstone_tmux::health::DiagnosticCode::SetupInputInvalid.as_str(),
            "setup_input_invalid"
        );
        assert_eq!(
            solstone_tmux::journal_version::JournalVersionStatus::LastKnown("1.2.3".into())
                .render(),
            "1.2.3 (last known)"
        );
        for token in ["yes", "y", "no", "n", "continue", "cancel"] {
            assert!(solstone_tmux::pairing_answer::parse_answer_token(token).is_some());
        }
        assert!(solstone_tmux::pairing_answer::parse_answer_token("sí").is_none());
    });
}

#[test]
fn terminal_padding_uses_display_cells() {
    assert_eq!(solstone_tmux::i18n::pad_terminal("中文", 6), "中文  ");
    assert_eq!(
        solstone_tmux::i18n::pad_terminal("e\u{301}", 3),
        "e\u{301}  "
    );
    assert_eq!(solstone_tmux::i18n::pad_terminal("中文", 2), "中文");
}

#[test]
fn restricted_fluent_contract_and_metadata_match_the_generated_api() {
    use fluent_syntax::ast::{Entry, Expression, InlineExpression, Pattern, PatternElement};
    use std::collections::BTreeSet;
    fn inline(
        value: &InlineExpression<&str>,
        terms: &BTreeSet<&str>,
        variables: &mut BTreeSet<String>,
        selects: &mut usize,
    ) {
        match value {
            InlineExpression::StringLiteral { .. } | InlineExpression::NumberLiteral { .. } => {}
            InlineExpression::VariableReference { id } => {
                variables.insert(id.name.to_owned());
            }
            InlineExpression::TermReference {
                id,
                attribute,
                arguments,
            } => {
                assert!(terms.contains(id.name));
                assert!(attribute.is_none());
                assert!(arguments.is_none());
            }
            InlineExpression::FunctionReference { id, arguments } => {
                assert!(matches!(id.name, "NUMBER" | "DATETIME"));
                for arg in &arguments.positional {
                    inline(arg, terms, variables, selects);
                }
                for arg in &arguments.named {
                    inline(&arg.value, terms, variables, selects);
                }
            }
            InlineExpression::Placeable { expression } => {
                expression_check(expression, terms, variables, selects)
            }
            InlineExpression::MessageReference { .. } => {
                panic!("message references are outside the restricted contract")
            }
        }
    }
    fn expression_check(
        expression: &Expression<&str>,
        terms: &BTreeSet<&str>,
        variables: &mut BTreeSet<String>,
        selects: &mut usize,
    ) {
        match expression {
            Expression::Inline(value) => inline(value, terms, variables, selects),
            Expression::Select { selector, variants } => {
                *selects += 1;
                inline(selector, terms, variables, selects);
                for variant in variants {
                    pattern(&variant.value, terms, variables, selects);
                }
            }
        }
    }
    fn pattern(
        value: &Pattern<&str>,
        terms: &BTreeSet<&str>,
        variables: &mut BTreeSet<String>,
        selects: &mut usize,
    ) {
        for element in &value.elements {
            if let PatternElement::Placeable { expression } = element {
                expression_check(expression, terms, variables, selects);
            }
        }
    }
    for ftl in [solstone_tmux::i18n::EN_US, solstone_tmux::i18n::EN_XA] {
        let resource = fluent_syntax::parser::parse(ftl).unwrap();
        let terms = resource
            .body
            .iter()
            .filter_map(|entry| {
                if let Entry::Term(term) = entry {
                    Some(term.id.name)
                } else {
                    None
                }
            })
            .collect::<BTreeSet<_>>();
        let mut found = BTreeSet::new();
        for entry in &resource.body {
            if let Entry::Message(message) = entry {
                assert!(message.attributes.is_empty());
                assert!(found.insert(message.id.name));
                let id = *ALL_MESSAGES
                    .iter()
                    .find(|id| id.key() == message.id.name)
                    .unwrap();
                let mut selects = 0;
                let mut variables = BTreeSet::new();
                pattern(
                    message.value.as_ref().unwrap(),
                    &terms,
                    &mut variables,
                    &mut selects,
                );
                assert!(selects <= 1, "{}", id.key());
                assert_eq!(
                    variables,
                    id.schema()
                        .iter()
                        .map(|(name, _)| name.to_string())
                        .collect()
                );
                let meta: serde_json::Value = serde_json::from_str(
                    &std::fs::read_to_string(format!(
                        "{}/i18n/meta/{}.json",
                        env!("CARGO_MANIFEST_DIR"),
                        id.key()
                    ))
                    .unwrap(),
                )
                .unwrap();
                for field in [
                    "description",
                    "surfaces",
                    "args",
                    "max_length",
                    "risk",
                    "screenshots",
                    "source_revision",
                ] {
                    assert!(meta.get(field).is_some(), "{} {field}", id.key());
                }
                assert!(matches!(
                    meta["risk"].as_str().unwrap(),
                    "routine" | "account" | "payment" | "legal" | "statutory" | "protocol"
                ));
                assert_eq!(meta["source_revision"].as_str().unwrap().len(), 40);
            }
        }
        assert_eq!(found.len(), ALL_MESSAGES.len());
    }
}

#[test]
fn cli_errors_and_diagnostics_expose_semantics_before_rendering() {
    for locale in [Locale::EnUs, Locale::EnXa] {
        with_locale(locale, || {
            let error = solstone_tmux::cli::parse_args(
                ["solstone-tmux", "synthetic-command"]
                    .into_iter()
                    .map(Into::into),
            )
            .unwrap_err();
            assert_eq!(error.message().id, MessageId::CliCliParseArgsUnknownCommand);
            assert_eq!(
                error.message().arguments[0],
                ("arg0", ArgValue::Text("synthetic-command".into()))
            );
            assert_eq!(
                error.message().arguments[1],
                ("arg1", ArgValue::Text(solstone_tmux::cli::usage()))
            );
            assert_eq!(
                solstone_tmux::health::DiagnosticCode::SetupInputInvalid.message_id(),
                MessageId::CliHealthMessageSetupInputIsInvalid
            );
            assert!(
                solstone_tmux::health::DiagnosticCode::SetupInputInvalid
                    .message_id()
                    .schema()
                    .is_empty()
            );
        });
    }
}

#[test]
#[should_panic(expected = "text requires a message without arguments")]
fn parameterized_messages_cannot_silently_render_as_empty_text() {
    MessageId::CliAboutAgeMinute.text();
}
