// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Minimal Prometheus text-exposition writer for `RaftServer::render_metrics` and
//! `NativeEngine::render_metrics`. Callers serve the string on whatever `/metrics` endpoint hosts
//! the node.

use std::{
    collections::HashSet,
    fmt::{Display, Write},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Upper bounds, in seconds, of the finite buckets of every latency [`Histogram`]: from a fast
/// flash sync to a badly stalled disk.
pub const LATENCY_BUCKETS: [f64; 17] = [
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
    5.0, 10.0, 30.0,
];

/// A latency distribution over [`LATENCY_BUCKETS`], updated with two relaxed atomic adds.
#[derive(Debug, Default)]
pub struct Histogram {
    /// Observations per bucket, not cumulative; the last one is `+Inf`.
    buckets: [AtomicU64; LATENCY_BUCKETS.len() + 1],
    sum_ns: AtomicU64,
}

/// One look at a [`Histogram`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistogramSnapshot {
    /// As in [`Histogram`]: per bucket, not cumulative.
    pub buckets: [u64; LATENCY_BUCKETS.len() + 1],
    pub sum_ns: u64,
}

impl HistogramSnapshot {
    /// Observations in all.
    pub fn count(&self) -> u64 {
        self.buckets.iter().sum()
    }
}

impl Histogram {
    pub fn observe(&self, took: Duration) {
        let secs = took.as_secs_f64();
        let bucket = LATENCY_BUCKETS.partition_point(|le| *le < secs);
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        let ns = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
    }

    /// Runs `f` and observes how long it took, whatever it returned.
    pub fn time<T>(&self, f: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let out = f();
        self.observe(started.elapsed());
        out
    }

    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            buckets: std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed)),
            sum_ns: self.sum_ns.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Default)]
pub struct PromText {
    out: String,
}

impl PromText {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a metric family. `kind` is `gauge`, `counter` or `histogram`.
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

    /// One series of a `histogram` family: its cumulative buckets, sum (seconds) and count.
    pub fn histogram(
        &mut self,
        name: &str,
        labels: &[(&str, &str)],
        h: &HistogramSnapshot,
    ) -> &mut Self {
        let bucket = format!("{name}_bucket");
        let bounds = LATENCY_BUCKETS.iter().map(|le| le.to_string());
        let mut below = 0;
        for (le, n) in bounds.chain(["+Inf".to_string()]).zip(h.buckets) {
            below += n;
            let mut with_le = labels.to_vec();
            with_le.push(("le", &le));
            self.sample(&bucket, &with_le, below);
        }
        let sum = Duration::from_nanos(h.sum_ns).as_secs_f64();
        self.sample(&format!("{name}_sum"), labels, sum);
        self.sample(&format!("{name}_count"), labels, below)
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
    // A histogram's samples carry suffixed names but belong to the family its headers name.
    let mut histograms = HashSet::new();
    for line in texts.iter().flat_map(|t| t.lines()) {
        let (name, header) = match line
            .strip_prefix("# HELP ")
            .or(line.strip_prefix("# TYPE "))
        {
            Some(rest) => {
                let (name, kind) = rest.split_once(' ').unwrap_or((rest, ""));
                if line.starts_with("# TYPE ") && kind == "histogram" {
                    histograms.insert(name);
                }
                (name, true)
            }
            None if line.is_empty() => continue,
            None => {
                let name = line.split(['{', ' ']).next().unwrap_or_default();
                let family = ["_bucket", "_sum", "_count"]
                    .iter()
                    .filter_map(|suffix| name.strip_suffix(suffix))
                    .find(|base| histograms.contains(base));
                (family.unwrap_or(name), false)
            }
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
    fn histograms_bucket_cumulatively_and_merge_as_one_family() {
        let h = Histogram::default();
        // On a bound counts into it; past the last bound counts only into +Inf.
        h.observe(Duration::from_millis(5));
        h.observe(Duration::from_millis(3));
        h.observe(Duration::from_secs(60));
        assert_eq!(h.time(|| 7), 7);
        let s = h.snapshot();
        assert_eq!(s.count(), 4);
        assert!(s.sum_ns >= 60_008_000_000);

        let text = |node: &str| {
            let mut p = PromText::new();
            p.family("atlas_h_seconds", "histogram", "H.").histogram(
                "atlas_h_seconds",
                &[("node", node)],
                &s,
            );
            p.single("atlas_other_count", "gauge", "Not part of a histogram.", 1);
            p.finish()
        };
        let one = text("n1");
        for line in [
            "atlas_h_seconds_bucket{node=\"n1\",le=\"0.005\"} 3",
            "atlas_h_seconds_bucket{node=\"n1\",le=\"30\"} 3",
            "atlas_h_seconds_bucket{node=\"n1\",le=\"+Inf\"} 4",
            "atlas_h_seconds_count{node=\"n1\"} 4",
        ] {
            assert!(one.lines().any(|l| l == line), "{line} not in:\n{one}");
        }
        assert!(one.contains("atlas_h_seconds_sum{node=\"n1\"} 60.0"));

        // Both texts' samples stay under the one header, ahead of the next family.
        let merged = merge(&[one.clone(), text("n2")]);
        assert_eq!(
            merged.matches("# TYPE atlas_h_seconds histogram").count(),
            1
        );
        let names: Vec<&str> = merged
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.split(['{', ' ']).next().unwrap())
            .collect();
        let other = names
            .iter()
            .position(|n| *n == "atlas_other_count")
            .unwrap();
        assert_eq!(other, names.len() - 2);
        assert!(names[..other].iter().all(|n| n.starts_with("atlas_h_")));
        assert!(with_label("atlas_h_seconds_sum 1.5\n", "group", "1")
            .contains("atlas_h_seconds_sum{group=\"1\"} 1.5"));
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
