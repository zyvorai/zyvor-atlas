// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Minimal Prometheus text-exposition writer for `RaftServer::render_metrics` and
//! `NativeEngine::render_metrics`. Callers serve the string on whatever `/metrics` endpoint hosts
//! the node.

use std::fmt::{Display, Write};

#[derive(Debug, Default)]
pub struct PromText {
    out: String,
}

impl PromText {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a metric family. `kind` is `gauge` or `counter`.
    pub fn family(&mut self, name: &str, kind: &str, help: &str) -> &mut Self {
        let _ = writeln!(self.out, "# HELP {name} {help}");
        let _ = writeln!(self.out, "# TYPE {name} {kind}");
        self
    }

    pub fn sample(
        &mut self,
        name: &str,
        labels: &[(&str, &str)],
        value: impl Display,
    ) -> &mut Self {
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (i, (k, v)) in labels.iter().enumerate() {
                if i > 0 {
                    self.out.push(',');
                }
                let _ = write!(self.out, "{k}=\"{}\"", escape(v));
            }
            self.out.push('}');
        }
        let _ = writeln!(self.out, " {value}");
        self
    }

    /// A family with a single unlabelled sample.
    pub fn single(&mut self, name: &str, kind: &str, help: &str, value: impl Display) -> &mut Self {
        self.family(name, kind, help).sample(name, &[], value)
    }

    pub fn finish(self) -> String {
        self.out
    }
}

/// Adds `key="value"` to every sample of an exposition, e.g. to tell apart the metrics of
/// several engines on one node.
pub fn with_label(text: &str, key: &str, value: &str) -> String {
    let label = format!("{key}=\"{}\"", escape(value));
    let mut out = String::with_capacity(text.len() + 32);
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            out.push_str(line);
        } else if let Some(i) = line.find('{') {
            out.push_str(&line[..=i]);
            out.push_str(&label);
            out.push(',');
            out.push_str(&line[i + 1..]);
        } else if let Some(i) = line.find(' ') {
            out.push_str(&line[..i]);
            out.push('{');
            out.push_str(&label);
            out.push('}');
            out.push_str(&line[i..]);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Concatenates expositions, keeping each family's `HELP`/`TYPE` once and its samples together
/// (a family repeated across them is invalid exposition). Families keep their first-seen order.
pub fn merge(texts: &[String]) -> String {
    let mut order: Vec<String> = Vec::new();
    let mut families: std::collections::HashMap<String, (Vec<String>, Vec<String>)> =
        std::collections::HashMap::new();
    let mut current = String::new();
    for line in texts.iter().flat_map(|t| t.lines()) {
        let (name, header) = match line
            .strip_prefix("# HELP ")
            .or(line.strip_prefix("# TYPE "))
        {
            Some(rest) => (rest.split(' ').next().unwrap_or_default(), true),
            None if line.is_empty() => continue,
            None => (line.split(['{', ' ']).next().unwrap_or_default(), false),
        };
        if name != current {
            current = name.to_string();
        }
        let f = families.entry(current.clone()).or_insert_with(|| {
            order.push(current.clone());
            (Vec::new(), Vec::new())
        });
        if header {
            if f.0.len() < 2 && !f.0.iter().any(|h| h == line) {
                f.0.push(line.to_string());
            }
        } else {
            f.1.push(line.to_string());
        }
    }
    let mut out = String::new();
    for name in order {
        let (headers, samples) = &families[&name];
        for l in headers.iter().chain(samples) {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

fn escape(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            c => s.push(c),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_families_and_escapes_labels() {
        let mut p = PromText::new();
        p.single("atlas_x", "gauge", "An x.", 3);
        p.family("atlas_y_total", "counter", "Ys.").sample(
            "atlas_y_total",
            &[("peer", "a\"b\\c\nd")],
            7,
        );
        let text = p.finish();
        assert!(text.contains("# TYPE atlas_x gauge\natlas_x 3\n"));
        assert!(text.contains(r#"atlas_y_total{peer="a\"b\\c\nd"} 7"#));
    }
    #[test]
    fn labels_and_merges_expositions() {
        let mut a = PromText::new();
        a.single("atlas_x", "gauge", "An x.", 1);
        a.family("atlas_y", "gauge", "Ys.")
            .sample("atlas_y", &[("node", "n1")], 2);
        let mut b = PromText::new();
        b.single("atlas_x", "gauge", "An x.", 3);
        b.family("atlas_y", "gauge", "Ys.")
            .sample("atlas_y", &[("node", "n1")], 4);
        let merged = merge(&[
            with_label(&a.finish(), "group", "0"),
            with_label(&b.finish(), "group", "1"),
        ]);
        assert_eq!(
            merged,
            "# HELP atlas_x An x.\n# TYPE atlas_x gauge\natlas_x{group=\"0\"} 1\natlas_x{group=\"1\"} 3\n\
             # HELP atlas_y Ys.\n# TYPE atlas_y gauge\natlas_y{group=\"0\",node=\"n1\"} 2\n\
             atlas_y{group=\"1\",node=\"n1\"} 4\n"
        );
    }
}
