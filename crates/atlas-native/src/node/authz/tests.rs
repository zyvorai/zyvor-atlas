// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use super::*;

const METHODS: [&str; 7] = ["GET", "PUT", "POST", "DELETE", "HEAD", "PATCH", "OPTIONS"];

/// Router patterns that dispatch further instead of naming one route.
const DISPATCH: [&str; 5] = [
    r#""v1" , "members""#,
    r#""v1" , "fs" | "fs-snapshots" , . ."#,
    r#""v1" , "fs-snapshots" , id , . ."#,
    r#""v1" , "fs" , fs , . ."#,
    r#""v1" , "fs" , fs , "inodes" , ino , rest @ . ."#,
];

/// Routes answered before any credential is asked for.
const PUBLIC: [(&str, &str); 3] = [("GET", "/healthz"), ("GET", "/readyz"), ("GET", "/metrics")];

#[derive(Debug, PartialEq)]
enum Tok {
    Str(String),
    Word(String),
    Punct(char),
}

/// Just enough of a Rust lexer to find string literals outside comments.
fn lex(src: &str) -> Vec<Tok> {
    let c: Vec<char> = src.chars().collect();
    let at = |i: usize| c.get(i).copied().unwrap_or('\0');
    let (mut i, mut out) = (0, Vec::new());
    while i < c.len() {
        match c[i] {
            '/' if at(i + 1) == '/' => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '/' if at(i + 1) == '*' => {
                i += 2;
                while i < c.len() && !(c[i] == '*' && at(i + 1) == '/') {
                    i += 1;
                }
                i += 2;
            }
            '"' => {
                let mut s = String::new();
                i += 1;
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' {
                        s.push(c[i]);
                        i += 1;
                    }
                    s.push(at(i));
                    i += 1;
                }
                i += 1;
                out.push(Tok::Str(s));
            }
            // A character literal (which may be a quote), not a lifetime.
            '\'' if at(i + 1) == '\\' => {
                i += 3;
                while i < c.len() && c[i] != '\'' {
                    i += 1;
                }
                i += 1;
            }
            '\'' if at(i + 2) == '\'' => i += 3,
            x if x.is_alphanumeric() || x == '_' => {
                let mut s = String::new();
                while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_') {
                    s.push(c[i]);
                    i += 1;
                }
                out.push(Tok::Word(s));
            }
            x => {
                if !x.is_whitespace() {
                    out.push(Tok::Punct(x));
                }
                i += 1;
            }
        }
    }
    out
}

fn text(t: &Tok) -> String {
    match t {
        Tok::Str(s) => format!("\"{s}\""),
        Tok::Word(w) => w.clone(),
        Tok::Punct(c) => c.to_string(),
    }
}

#[derive(Default)]
struct Scan {
    /// `(method, pattern with every binding as "{}")`.
    routes: BTreeSet<(String, String)>,
    /// Route-like source the scanner cannot account for.
    problems: Vec<String>,
}

impl Scan {
    fn source(&mut self, file: &str, src: &str) {
        let mut t = lex(src);
        // Unit tests, which by convention end a file, are not the router.
        let test_mod = ["#", "[", "cfg", "(", "test", ")", "]", "mod"];
        if let Some(end) = (0..t.len()).find(|i| {
            t[*i..]
                .iter()
                .take(test_mod.len())
                .map(text)
                .eq(test_mod.iter().map(|s| s.to_string()))
        }) {
            t.truncate(end);
        }
        let punct = |i: usize, p: char| t.get(i) == Some(&Tok::Punct(p));
        let method = |i: usize| match t.get(i) {
            Some(Tok::Str(s)) if METHODS.contains(&s.as_str()) => Some(s.clone()),
            _ => None,
        };
        let mut func = String::new();
        for (i, tok) in t.iter().enumerate() {
            match tok {
                Tok::Word(w) if w == "fn" => {
                    if let Some(Tok::Word(name)) = t.get(i + 1) {
                        func.clone_from(name);
                    }
                }
                Tok::Str(s) if s.starts_with("/v1") => {
                    self.problems
                        .push(format!("{file}: path matched as a string: {s:?}"));
                }
                // `"POST" => ...` inside a `["v1", "members"]` block.
                Tok::Str(_) if punct(i + 1, '=') && punct(i + 2, '>') => {
                    if let Some(m) = method(i) {
                        self.routes.insert((m, "v1/members".into()));
                    }
                }
                // `("GET", "/healthz")`.
                Tok::Punct('(') if method(i + 1).is_some() && punct(i + 2, ',') => {
                    if let (Some(m), Some(Tok::Str(path))) = (method(i + 1), t.get(i + 3)) {
                        if !PUBLIC.contains(&(m.as_str(), path.as_str())) {
                            self.problems
                                .push(format!("{file}: unauthenticated route {m} {path}"));
                        }
                    }
                }
                Tok::Punct('[') => {
                    let mut depth = 0;
                    let Some(close) = (i..t.len()).find(|j| {
                        if punct(*j, '[') {
                            depth += 1;
                        } else if punct(*j, ']') {
                            depth -= 1;
                        }
                        depth == 0
                    }) else {
                        continue;
                    };
                    let inner = &t[i + 1..close];
                    let versioned = inner.first() == Some(&Tok::Str("v1".into()));
                    let after_method =
                        (i >= 3 && punct(i - 3, '(') && punct(i - 1, ',')).then(|| method(i - 2));
                    // In `inode_route`, a bracket in pattern position names what follows an inode.
                    let pattern_position = i > 0
                        && (matches!(&t[i - 1], Tok::Punct('(' | ',' | '|' | '{' | '}'))
                            || t[i - 1] == Tok::Word("let".into()));
                    let relative = func == "inode_route"
                        && pattern_position
                        && matches!(inner.first(), None | Some(Tok::Str(_)));
                    let words = inner.iter().map(text).collect::<Vec<_>>().join(" ");
                    let Some(Some(m)) = after_method else {
                        if versioned && !DISPATCH.contains(&words.as_str()) {
                            self.problems
                                .push(format!("{file}: unrecognised route pattern [{words}]"));
                        } else if relative && !inner.is_empty() {
                            self.problems
                                .push(format!("{file}: unrecognised inode pattern [{words}]"));
                        }
                        continue;
                    };
                    if !versioned && func != "inode_route" {
                        self.problems.push(format!(
                            "{file}: {m} [{words}] in {func}: teach the scanner this sub-router"
                        ));
                        continue;
                    }
                    let mut segs: Vec<String> = if versioned {
                        Vec::new()
                    } else {
                        ["v1", "fs", "{}", "inodes", "{}"].map(String::from).into()
                    };
                    let mut understood = true;
                    for part in inner.split(|t| *t == Tok::Punct(',')) {
                        match part {
                            [Tok::Str(s)] => segs.push(s.clone()),
                            [Tok::Word(_)] => segs.push("{}".into()),
                            [] => {}
                            _ => understood = false,
                        }
                    }
                    if understood {
                        self.routes.insert((m, segs.join("/")));
                    } else {
                        self.problems
                            .push(format!("{file}: unrecognised route pattern {m} [{words}]"));
                    }
                }
                _ => {}
            }
        }
    }

    /// Problems, plus routes the table lacks and table entries the router lacks.
    fn against_table(mut self) -> Vec<String> {
        let table: BTreeSet<(String, String)> = ROUTES
            .iter()
            .map(|r| {
                let p: Vec<&str> = r
                    .pattern
                    .split('/')
                    .map(|s| if s.starts_with('{') { "{}" } else { s })
                    .collect();
                (r.method.to_string(), p.join("/"))
            })
            .collect();
        for (m, p) in self.routes.difference(&table) {
            self.problems
                .push(format!("unclassified route {m} {p}: add it to authz::ROUTES"));
        }
        for (m, p) in table.difference(&self.routes) {
            self.problems
                .push(format!("authz::ROUTES lists {m} {p}, which the router lacks"));
        }
        self.problems
    }
}

fn router_sources() -> BTreeMap<PathBuf, String> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            // This module and its tests.
            if path.file_stem().is_some_and(|n| n == "authz") {
                continue;
            }
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.insert(path.clone(), std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = BTreeMap::new();
    walk(&src.join("node"), &mut out);
    let node = src.join("node.rs");
    out.insert(node.clone(), std::fs::read_to_string(node).unwrap());
    out
}

#[test]
fn every_route_of_the_router_is_classified() {
    let mut scan = Scan::default();
    for (path, src) in router_sources() {
        scan.source(&path.display().to_string(), &src);
    }
    assert!(scan.routes.len() > 60, "{:?}", scan.routes);
    let problems = scan.against_table();
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn the_scan_notices_routes_added_without_a_classification() {
    let added = [
        r#"fn f() { match x { ("PATCH", ["v1", "volumes", id]) => 1 } }"#,
        r#"fn f() { match x { ("GET", ["v1", "backup"]) => 1 } }"#,
        r#"fn f() { match x { ("GET" | "HEAD", ["v1", "fs"]) => 1 } }"#,
        r#"fn f() { match x { (_, ["v1", "backup", ..]) => 1 } }"#,
        r#"fn f() { if let ["v1", "backup"] = segs { } }"#,
        r#"fn f() { if req.path == "/v1/backup" { } }"#,
        r#"fn f() { match x { ("POST", "/restore") => 1 } }"#,
        r#"fn f() { match x { ("POST", ["restore"]) => 1 } }"#,
        r#"fn f() { match m { "GET" => 1, "PATCH" => 2 } }"#,
        r#"fn inode_route() { match x { ("POST", ["truncate"]) => 1 } }"#,
        r#"fn inode_route() { match x { ("GET" | "HEAD", ["data"]) => 1 } }"#,
        r#"fn inode_route() { match x { (_, ["acl", ..]) => 1 } }"#,
    ];
    let mut real = Scan::default();
    for (path, src) in router_sources() {
        real.source(&path.display().to_string(), &src);
    }
    for src in added {
        let mut scan = Scan {
            routes: real.routes.clone(),
            problems: Vec::new(),
        };
        scan.source("new.rs", src);
        assert!(!scan.against_table().is_empty(), "went unnoticed: {src}");
    }
    // Comments, test modules and indexing are not routes.
    let mut scan = Scan {
        routes: real.routes,
        problems: Vec::new(),
    };
    scan.source(
        "quiet.rs",
        r#"//! ("GET", ["v1", "x"])
        fn inode_route() { let a = body["inos"]; let q = '"'; f(&[ino]); } /* "/v1/x" */
        #[cfg(test)]
        mod tests { const P: &str = "/v1/x"; }"#,
    );
    let problems = scan.against_table();
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn no_two_table_entries_match_the_same_request() {
    for (i, a) in ROUTES.iter().enumerate() {
        for b in &ROUTES[i + 1..] {
            let (x, y): (Vec<_>, Vec<_>) = (
                a.pattern.split('/').collect(),
                b.pattern.split('/').collect(),
            );
            let overlap = a.method == b.method
                && x.len() == y.len()
                && x.iter()
                    .zip(&y)
                    .all(|(x, y)| x == y || x.starts_with('{') || y.starts_with('{'));
            assert!(!overlap, "{} {} and {}", a.method, a.pattern, b.pattern);
        }
    }
}

#[test]
fn the_table_keeps_its_rules() {
    for r in ROUTES {
        let what = format!("{} {}", r.method, r.pattern);
        assert!(r.pattern.starts_with("v1/"), "{what}");
        let unlock = r.pattern.ends_with("/unlock");
        assert_eq!(r.roles == UNLOCK, unlock, "{what}");
        assert_eq!(r.roles & UNLOCK != 0, unlock, "{what}");
        assert_eq!(r.permits(ADMIN), !unlock, "{what}");
        assert_eq!(r.permits(UNLOCK), unlock, "{what}");
        assert!(!r.permits(NONE), "{what}");
        if r.method != "GET" {
            assert!(!r.permits(READ), "{what} changes something");
        }
        let names_object = r.pattern.contains("{fs}") || r.pattern.contains("{vol}");
        if r.on == On::Object {
            assert!(names_object, "{what}");
        } else {
            assert!(!names_object, "{what}");
        }
        // A new role for data or read-only tokens must say which object it is limited by.
        if r.permits(DATA) && r.method != "GET" {
            assert_eq!(r.on, On::Object, "{what}");
        }
    }
}

#[test]
fn paths_are_classified_as_the_router_reads_them() {
    let pattern = |m: &str, p: &str| classify(m, &segments(p)).map(|r| r.pattern);
    assert_eq!(pattern("GET", "/v1/volumes"), Some("v1/volumes"));
    assert_eq!(pattern("GET", "//v1/volumes//"), Some("v1/volumes"));
    for (m, p) in [
        ("get", "/v1/volumes"),
        ("HEAD", "/v1/volumes"),
        ("PATCH", "/v1/volumes"),
        ("GET", "/V1/volumes"),
        ("GET", "/v1//volumes"),
        ("GET", "/v1/%76olumes"),
        ("GET", "/v1%2Fvolumes"),
        ("GET", "http://node/v1/volumes"),
        ("GET", "/v1/fs/a/../b/statfs"),
        ("POST", "/v1/volumes/a/unlock"),
        ("GET", "/v1"),
        ("GET", "/"),
    ] {
        assert_eq!(pattern(m, p), None, "{m} {p}");
    }
}

fn write_tokens(dir: &Path, list: &str, files: &[(&str, &str)]) -> PathBuf {
    for (name, value) in files {
        std::fs::write(dir.join(name), value).unwrap();
    }
    let path = dir.join("tokens.json");
    std::fs::write(&path, list).unwrap();
    path
}

#[test]
fn bad_token_lists_are_refused_without_repeating_secrets() {
    const A: &str = "secret-aaaaaaaaaaaaaaaa";
    const B: &str = "secret-bbbbbbbbbbbbbbbb";
    let td = tempfile::tempdir().unwrap();
    let dir = td.path();
    std::fs::write(dir.join("admin"), "secret-admin-aaaaaaaaaa\n").unwrap();
    let files = [
        ("a", A),
        ("a2", A),
        ("b", B),
        ("short", "secret-short"),
        ("spaced", "secret-with a space-in-it"),
        ("admin-copy", "secret-admin-aaaaaaaaaa"),
    ];
    let one = |extra: &str| format!(r#"{{"tokens":[{{"name":"x",{extra}}}]}}"#);
    let bad = [
        ("{".to_string(), "line 1"),
        (r#"{"tokens":[]}"#.into(), "no tokens"),
        (one(r#""token":"secret-inline-aaaaaaaa","roles":["admin"]"#), "valid token list"),
        (one(r#""token_file":"a","roles":"secret-misplaced-aaaa""#), "valid token list"),
        (one(r#""token_file":"a","roles":["root"]"#), "valid token list"),
        (one(r#""token_file":"a","roles":[]"#), "no roles"),
        (one(r#""token_file":"missing","roles":["admin"]"#), "missing"),
        (one(r#""token_file":"short","roles":["admin"]"#), "at least 16"),
        (one(r#""token_file":"spaced","roles":["admin"]"#), "without spaces"),
        (one(r#""token_file":"admin-copy","roles":["read-only"]"#), "same value as api_token_file"),
        (one(r#""token_file":"a","roles":["data"]"#), "needs filesystems or volumes"),
        (one(r#""token_file":"a","roles":["data"],"filesystems":[]"#), "both empty"),
        (one(r#""token_file":"a","roles":["data"],"filesystems":["a@s"]"#), "must be ids"),
        (one(r#""token_file":"a","roles":["data"],"filesystems":["*","a"]"#), "must be ids"),
        (one(r#""token_file":"a","roles":["data"],"filesystems":["a*"]"#), "must be ids"),
        (one(r#""token_file":"a","roles":["lifecycle"],"filesystems":["a"]"#), "only limit"),
        (one(r#""token_file":"a","roles":["data","lifecycle"],"volumes":["*"]"#), "only limit"),
        (one(r#""token_file":"a","roles":["admin"],"filesystem":["a"]"#), "valid token list"),
        (
            r#"{"tokens":[{"name":"x","token_file":"a","roles":["admin"]},
                {"name":"x","token_file":"b","roles":["admin"]}]}"#
                .into(),
            "listed twice",
        ),
        (
            r#"{"tokens":[{"name":"x","token_file":"a","roles":["admin"]},
                {"name":"y","token_file":"a2","roles":["read-only"]}]}"#
                .into(),
            "x and y have the same value",
        ),
        (
            r#"{"tokens":[{"name":"x y","token_file":"a","roles":["admin"]}]}"#.into(),
            "name must be",
        ),
    ];
    for (list, expect) in bad {
        let path = write_tokens(dir, &list, &files);
        let err = ApiAuth::load(Some(&dir.join("admin")), Some(&path))
            .err()
            .unwrap_or_else(|| panic!("accepted {list}"))
            .to_string();
        assert!(err.contains(expect), "{list}: {err}");
        assert!(!err.contains("secret-"), "{err}");
    }
    assert!(ApiAuth::load(None, Some(&dir.join("nowhere.json"))).is_err());
}

fn request(method: &str, path: &str, token: Option<&str>) -> Request {
    Request {
        method: method.into(),
        path: path.into(),
        version: "HTTP/1.1".into(),
        query: Default::default(),
        headers: token
            .map(|t| ("authorization".to_string(), format!("Bearer {t}")))
            .into_iter()
            .collect(),
        body: Vec::new(),
        client_verified: false,
    }
}

#[test]
fn without_credentials_the_api_stays_open() {
    let auth = ApiAuth::load(None, None).unwrap();
    assert!(auth.admit(&request("POST", "/v1/anything", None)).is_ok());
    assert!(auth.render_metrics("n").is_none());
}

#[test]
fn audit_records_carry_no_tokens_or_client_text() {
    const ADMIN_TOKEN: &str = "admintoken0123456789";
    const SCOPED_TOKEN: &str = "scopedtoken0123456789";
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("admin"), ADMIN_TOKEN).unwrap();
    let list = write_tokens(
        td.path(),
        r#"{"tokens":[{"name":"ro","token_file":"ro","roles":["read-only"]}]}"#,
        &[("ro", SCOPED_TOKEN)],
    );
    let auth = ApiAuth::load(Some(&td.path().join("admin")), Some(&list)).unwrap();
    for (method, path) in [
        ("DELETE", format!("/v1/fs/{SCOPED_TOKEN}")),
        ("DELETE", format!("/v1/fs/{ADMIN_TOKEN}")),
        ("DELETE", format!("/v1/{SCOPED_TOKEN}")),
        ("DELETE", "/v1/fs/a\u{1b}[31m\"}".to_string()),
        (SCOPED_TOKEN, "/v1/volumes".to_string()),
    ] {
        let req = request(method, &path, Some(SCOPED_TOKEN));
        let route = classify(&req.method, &segments(&req.path));
        let line = auth.line("denied", 403, Some("ro"), &req, route, "role", 3);
        assert!(!line.contains(SCOPED_TOKEN), "{line}");
        assert!(!line.contains(ADMIN_TOKEN), "{line}");
        assert!(!line.contains('\u{1b}'), "{line}");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["token"], "ro");
        assert_eq!(v["dropped_before"], 3);
        let resp = auth.admit(&req).err().unwrap();
        let body = String::from_utf8(resp.body).unwrap();
        assert!(
            !body.contains(SCOPED_TOKEN) && !body.contains(ADMIN_TOKEN),
            "{body}"
        );
    }
}

#[test]
fn audit_lines_are_rate_limited_and_the_dropped_ones_counted() {
    let t = Throttle::default();
    let start = Instant::now();
    let written = (0..100).filter(|_| t.admit(start).is_some()).count();
    assert_eq!(written, AUDIT_PER_SEC as usize);
    assert_eq!(
        t.admit(start + Duration::from_secs(1)),
        Some(100 - u64::from(AUDIT_PER_SEC))
    );
    assert_eq!(t.total.load(Ordering::Relaxed), 101);
}

#[test]
fn a_listing_that_cannot_be_filtered_is_not_passed_on() {
    let permit = || {
        Permit(Some(Filter {
            list: "volumes",
            field: "id",
            allowed: BTreeSet::from(["a".to_string()]),
        }))
    };
    let out = permit().apply(Response::json(
        200,
        &json!({ "volumes": [{ "id": "a" }, { "id": "b" }, {}] }),
    ));
    assert_eq!(out.body, br#"{"volumes":[{"id":"a"}]}"#);
    for body in [json!({ "other": [{ "id": "b" }] }), json!([{ "id": "b" }])] {
        assert_eq!(permit().apply(Response::json(200, &body)).status, 500);
    }
    assert_eq!(permit().apply(Response::text(503, "no leader")).status, 503);
}
