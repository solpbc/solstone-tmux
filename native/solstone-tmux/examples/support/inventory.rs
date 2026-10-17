// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

//! Deterministic Rust string discovery, including token trees inside macros.
use proc_macro2::{TokenStream, TokenTree};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

struct Inventory {
    path: String,
    source: String,
    context: Vec<String>,
    rows: Vec<Value>,
    macro_data: Option<Value>,
}
impl Inventory {
    fn byte(&self, p: proc_macro2::LineColumn) -> usize {
        self.source
            .split_inclusive('\n')
            .take(p.line - 1)
            .map(str::len)
            .sum::<usize>()
            + self
                .source
                .lines()
                .nth(p.line - 1)
                .unwrap_or_default()
                .chars()
                .take(p.column)
                .map(char::len_utf8)
                .sum::<usize>()
    }
    fn literal(&mut self, value: String, span: proc_macro2::Span, kind: &str) {
        let start = span.start();
        let end = span.end();
        self.rows.push(json!({"path": self.path, "line": start.line, "column":start.column,
            "start":self.byte(start), "end":self.byte(end), "value":value, "kind":kind,
            "macro":self.macro_data, "context":self.context.join("/"), "scope":if self.path.contains("/tests/") || self.context.iter().any(|x|x=="mod:tests" || x=="scope:test") {"test"} else {"source"}}));
    }
    fn tokens(&mut self, tokens: TokenStream) {
        for token in tokens {
            match token {
                TokenTree::Group(group) => self.tokens(group.stream()),
                TokenTree::Literal(literal) => {
                    if let Ok(value) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                        self.literal(value.value(), literal.span(), "macro-string");
                    } else if let Ok(value) =
                        syn::parse_str::<syn::LitByteStr>(&literal.to_string())
                    {
                        self.literal(
                            String::from_utf8_lossy(&value.value()).into_owned(),
                            literal.span(),
                            "macro-bytes",
                        );
                    }
                }
                _ => {}
            }
        }
    }
}
impl<'ast> Visit<'ast> for Inventory {
    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        self.context.push(format!(
            "attribute:{}",
            node.path().segments.last().unwrap().ident
        ));
        visit::visit_attribute(self, node);
        self.context.pop();
    }
    fn visit_field_value(&mut self, node: &'ast syn::FieldValue) {
        self.context.push(format!(
            "field:{}",
            match &node.member {
                syn::Member::Named(x) => x.to_string(),
                syn::Member::Unnamed(x) => x.index.to_string(),
            }
        ));
        visit::visit_field_value(self, node);
        self.context.pop();
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.context.push(format!("method:{}", node.method));
        self.visit_expr(&node.receiver);
        for argument in &node.args {
            let output = matches!(node.method.to_string().as_str(), "write_str" | "push_str")
                && matches!(argument, syn::Expr::Lit(literal) if matches!(literal.lit, syn::Lit::Str(_)));
            if output {
                self.context.push("literal-output:method".into());
            }
            self.visit_expr(argument);
            if output {
                self.context.pop();
            }
        }
        self.context.pop();
    }
    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        let name = node.path.segments.last().unwrap().ident.to_string();
        if name.chars().all(|x| x.is_ascii_uppercase() || x == '_') {
            self.literal(
                self.source[self.byte(node.span().start())..self.byte(node.span().end())]
                    .to_owned(),
                node.span(),
                "path",
            );
        }
        visit::visit_expr_path(self, node);
    }
    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        self.literal(node.value(), node.span(), "string");
    }
    fn visit_lit_byte_str(&mut self, node: &'ast syn::LitByteStr) {
        self.literal(
            String::from_utf8_lossy(&node.value()).into_owned(),
            node.span(),
            "bytes",
        );
    }
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.context.push(format!(
            "macro:{}",
            node.path.segments.last().unwrap().ident
        ));
        let previous = self.macro_data.take();
        let arguments = syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
            .parse2(node.tokens.clone())
            .ok()
            .map(|args| {
                args.iter()
                    .map(|arg| {
                        let span = arg.span();
                        self.source[self.byte(span.start())..self.byte(span.end())].to_owned()
                    })
                    .collect::<Vec<_>>()
            });
        self.macro_data = Some(
            json!({"name":node.path.segments.last().unwrap().ident.to_string(),"start":self.byte(node.span().start()),"end":self.byte(node.span().end()),"arguments":arguments}),
        );
        if std::env::args().any(|value| value == "--macros") {
            self.literal(
                node.path.segments.last().unwrap().ident.to_string(),
                node.span(),
                "macro-node",
            );
        }
        if let Ok(args) = syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
            .parse2(node.tokens.clone())
        {
            for arg in &args {
                self.visit_expr(arg);
            }
        } else {
            self.tokens(node.tokens.clone());
        }
        self.macro_data = previous;
        self.context.pop();
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let test = node.attrs.iter().any(|a| a.path().segments.last().is_some_and(|x| x.ident=="test") || matches!(&a.meta,syn::Meta::List(list) if list.path.is_ident("cfg") && list.tokens.to_string().contains("test")));
        if test {
            self.context.push("scope:test".into());
        }
        self.context.push(format!("fn:{}", node.sig.ident));
        visit::visit_item_fn(self, node);
        self.context.pop();
        if test {
            self.context.pop();
        }
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.context.push(format!("fn:{}", node.sig.ident));
        visit::visit_impl_item_fn(self, node);
        self.context.pop();
    }
    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        self.context.push(format!("const:{}", node.ident));
        visit::visit_item_const(self, node);
        self.context.pop();
    }
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        self.context.push(format!("mod:{}", node.ident));
        visit::visit_item_mod(self, node);
        self.context.pop();
    }
}
fn files(root: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files(&path, found);
        } else if path.extension().is_some_and(|x| x == "rs") {
            found.push(path);
        }
    }
}
pub fn discover(root: &Path) -> Vec<Value> {
    let mut paths = Vec::new();
    files(&root.join("src"), &mut paths);
    files(&root.join("tests"), &mut paths);
    paths.sort();
    let mut rows = Vec::new();
    for path in paths {
        let source = std::fs::read_to_string(&path).unwrap();
        let tree = syn::parse_file(&source).unwrap();
        let mut inventory = Inventory {
            path: path.to_string_lossy().into_owned(),
            source,
            context: vec![],
            rows: vec![],
            macro_data: None,
        };
        inventory.visit_file(&tree);
        rows.extend(inventory.rows);
    }
    rows
}
