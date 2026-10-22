#![forbid(unsafe_code)]
#![allow(clippy::print_stdout)]
//! Fails closed on missing/malformed Criterion results and significant regressions.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{collections::BTreeMap, fmt::Write, fs, path::Path};

#[derive(Deserialize)]
struct Samples {
    iters: Vec<f64>,
    times: Vec<f64>,
}
#[derive(Deserialize)]
struct Estimate {
    point_estimate: f64,
}
#[derive(Deserialize)]
struct Estimates {
    median: Estimate,
}
struct Measurement {
    median: f64,
    samples: Vec<f64>,
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    ensure!(fs::metadata(path)?.len() <= 16 << 20, "oversized result");
    serde_json::from_slice(&fs::read(path)?).context("invalid Criterion JSON")
}
fn load(root: &Path, baseline: &str) -> Result<BTreeMap<String, Measurement>> {
    fn visit(dir: &Path, baseline: &str, out: &mut BTreeMap<String, Measurement>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            if entry.file_name() == baseline {
                let estimates: Estimates = read_json(&path.join("estimates.json"))?;
                let samples: Samples = read_json(&path.join("sample.json"))?;
                ensure!(
                    samples.iters.len() == samples.times.len()
                        && (10..=10_000).contains(&samples.iters.len())
                        && samples.iters.iter().all(|n| n.is_finite() && *n > 0.0),
                    "missing samples"
                );
                let samples: Vec<_> = samples
                    .times
                    .iter()
                    .zip(&samples.iters)
                    .map(|(time, iters)| time / iters)
                    .collect();
                ensure!(
                    samples.iter().all(|s| s.is_finite() && *s > 0.0)
                        && estimates.median.point_estimate.is_finite()
                        && estimates.median.point_estimate > 0.0,
                    "invalid timing"
                );
                // benchmark.json carries the full ID (Criterion sanitizes directory names).
                let meta: serde_json::Value = read_json(&path.join("benchmark.json"))?;
                let name = meta
                    .get("full_id")
                    .and_then(serde_json::Value::as_str)
                    .context("missing benchmark ID")?
                    .to_owned();
                ensure!(
                    out.insert(
                        name,
                        Measurement {
                            median: estimates.median.point_estimate,
                            samples
                        }
                    )
                    .is_none(),
                    "duplicate benchmark ID"
                );
            } else if !["report", "change", "new", "base"]
                .iter()
                .any(|name| entry.file_name() == *name)
            {
                visit(&path, baseline, out)?;
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(root, baseline, &mut out)?;
    ensure!(!out.is_empty(), "no Criterion measurements for {baseline}");
    Ok(out)
}
#[allow(
    clippy::indexing_slicing,
    reason = "callers pass validated nonempty samples"
)]
fn median(samples: &mut [f64]) -> f64 {
    samples.sort_unstable_by(f64::total_cmp);
    let middle = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        samples[middle - 1].midpoint(samples[middle])
    } else {
        samples[middle]
    }
}
// Deterministic percentile bootstrap of the ratio of independent sample medians.
// Resample per-iteration times, never total batch durations or confidence endpoints.
#[allow(
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    reason = "nonempty validated samples; truncating PRNG to pointer width preserves random selection; fixed 10000 resamples"
)]
fn interval(base: &[f64], head: &[f64]) -> (f64, f64) {
    let mut random = 0x9e37_79b9_7f4a_7c15_u64;
    let mut pick = |samples: &[f64]| {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        samples[(random as usize) % samples.len()]
    };
    let mut a = vec![0.0; base.len()];
    let mut b = vec![0.0; head.len()];
    let mut changes = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        for value in &mut a {
            *value = pick(base);
        }
        for value in &mut b {
            *value = pick(head);
        }
        changes.push((median(&mut b) / median(&mut a) - 1.0) * 100.0);
    }
    changes.sort_unstable_by(f64::total_cmp);
    (changes[250], changes[9749])
}
fn escape(name: &str) -> String {
    name.replace('|', "&#124;").replace(['\r', '\n'], " ")
}
fn compare(
    base: &BTreeMap<String, Measurement>,
    head: &BTreeMap<String, Measurement>,
) -> (String, bool) {
    let mut table = String::from(
        "<!-- yhotos-benchmark-gate -->\n| Benchmark | Base median (ns) | Head median (ns) | Change | 95% CI | Gate |\n|---|---:|---:|---:|---:|---|\n",
    );
    let mut failed = false;
    let mut matched = 0;
    for (name, measurement) in base {
        let Some(current) = head.get(name) else {
            let _ = writeln!(
                table,
                "| {} | {:.2} | missing | | | FAIL |",
                escape(name),
                measurement.median
            );
            failed = true;
            continue;
        };
        matched += 1;
        let delta = (current.median / measurement.median - 1.0) * 100.0;
        let (low, high) = interval(&measurement.samples, &current.samples);
        let regression = current.median > measurement.median * 1.05 && low > 0.0;
        failed |= regression;
        let _ = writeln!(
            table,
            "| {} | {:.2} | {:.2} | {delta:+.2}% | [{low:+.2}%, {high:+.2}%] | {} |",
            escape(name),
            measurement.median,
            current.median,
            if regression { "FAIL" } else { "pass" }
        );
    }
    for (name, current) in head {
        if !base.contains_key(name) {
            let _ = writeln!(
                table,
                "| {} | new | {:.2} | | | new baseline |",
                escape(name),
                current.median
            );
        }
    }
    (table, failed || matched == 0)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [base_root, head_root] = args.as_slice() else {
        bail!("usage: compare BASE_CRITERION_DIR HEAD_CRITERION_DIR");
    };
    let base = load(Path::new(base_root), "main")?;
    let head = load(Path::new(head_root), "pr")?;
    let (table, failed) = compare(&base, &head);
    print!("{table}");
    ensure!(!failed, "benchmark regression or missing measurements");
    Ok(())
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    fn result(value: f64) -> BTreeMap<String, Measurement> {
        BTreeMap::from([(
            "test/path".into(),
            Measurement {
                median: value,
                samples: vec![value; 100],
            },
        )])
    }
    #[test]
    fn deliberate_twenty_percent_slowdown_fails_and_revert_passes() {
        let (table, failed) = compare(&result(100.0), &result(120.0));
        assert!(failed);
        assert!(table.contains("+20.00%") && table.contains("FAIL"));
        assert!(!compare(&result(100.0), &result(100.0)).1);
        assert!(!compare(&result(100.0), &result(104.0)).1);
        assert!(!compare(&result(100.0), &result(105.0)).1);
    }
    #[test]
    fn noise_crossing_zero_does_not_fail() {
        let mut head = result(120.0);
        head.get_mut("test/path").unwrap().samples = (0..100)
            .map(|i| if i < 50 { 50.0 } else { 200.0 })
            .collect();
        assert!(!compare(&result(100.0), &head).1);
    }
    #[test]
    fn missing_results_fail_closed() {
        assert!(compare(&result(100.0), &BTreeMap::new()).1);
        let temp = tempfile::tempdir().unwrap();
        assert!(load(temp.path(), "pr").is_err());
        let dir = temp.path().join("test/pr");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("estimates.json"),
            r#"{"median":{"point_estimate":100}}"#,
        )
        .unwrap();
        fs::write(dir.join("sample.json"), r#"{"iters":[0],"times":[100]}"#).unwrap();
        assert!(load(temp.path(), "pr").is_err());
    }
}
