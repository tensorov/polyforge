//! Pure coverage-floor evaluation.
//!
//! Decides whether a coverage report clears a minimum line-coverage floor —
//! per crate aggregate and per file. The module is **PURE**: no I/O, no
//! `std::env`, no hashing, no serialization, no randomness. The same report
//! always yields the same verdict (deterministic), which makes it safe to
//! call from the gate path and from CI.
//!
//! Ratios are expressed as fractions in `[0, 1]` (e.g. `0.80` = 80%). The CLI
//! deserialization layer (see `polyforge-cli`) is responsible for converting
//! tool-specific percentages (0-100) into these fractions before calling
//! [`CoverageFloor::evaluate`]; this module never sees a coverage tool's
//! on-disk format.

/// Line-coverage ratio of a single crate (aggregate over its files).
#[derive(Debug, Clone, PartialEq)]
pub struct CrateCoverage {
    /// Crate name, e.g. `polyforge-core`.
    pub name: String,
    /// Aggregate line ratio as a fraction in `[0, 1]`.
    pub ratio: f64,
}

/// Line-coverage ratio of a single file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileCoverage {
    /// File path as reported by the coverage tool.
    pub path: String,
    /// Line ratio as a fraction in `[0, 1]`.
    pub ratio: f64,
}

/// A coverage report: per-crate aggregate ratios plus per-file ratios.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CoverageReport {
    /// Per-crate aggregate line ratios.
    pub crates: Vec<CrateCoverage>,
    /// Per-file line ratios.
    pub files: Vec<FileCoverage>,
}

/// The coverage floors enforced by [`CoverageFloor::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoverageFloor {
    /// Minimum aggregate line ratio per crate.
    pub aggregate: f64,
    /// Minimum line ratio per file.
    pub per_file: f64,
}

impl Default for CoverageFloor {
    fn default() -> Self {
        Self {
            aggregate: 0.80,
            per_file: 0.80,
        }
    }
}

/// The scope a coverage failure names: a crate aggregate or a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageScope {
    /// A crate's aggregate line ratio fell below the floor.
    Crate(String),
    /// A single file's line ratio fell below the floor.
    File(String),
}

/// One scope that failed to clear the coverage floor.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageFailure {
    /// The offending scope (crate aggregate or file).
    pub scope: CoverageScope,
    /// The measured ratio (fraction in `[0, 1]`).
    pub ratio: f64,
    /// The floor threshold that was not met (fraction in `[0, 1]`).
    pub threshold: f64,
}

/// The verdict of a coverage-floor evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageVerdict {
    /// `true` iff every crate aggregate and every file cleared its floor.
    pub passed: bool,
    /// The scopes below their floor, in report order. Empty when `passed`.
    pub failures: Vec<CoverageFailure>,
}

impl CoverageFloor {
    /// Evaluate `report` against this floor.
    ///
    /// Deterministic: identical reports always yield identical verdicts, and
    /// failures are reported in report order (crates first, then files). The
    /// floor is **inclusive**: a ratio exactly equal to the threshold passes.
    /// A ratio that is `NaN` never clears the floor. An empty report (no
    /// crates, no files) passes vacuously.
    pub fn evaluate(&self, report: &CoverageReport) -> CoverageVerdict {
        let mut failures = Vec::new();
        for crate_cov in &report.crates {
            // Explicit NaN guard: `NaN < x` is false, which would silently
            // pass a malformed ratio; fail closed instead.
            if crate_cov.ratio.is_nan() || crate_cov.ratio < self.aggregate {
                failures.push(CoverageFailure {
                    scope: CoverageScope::Crate(crate_cov.name.clone()),
                    ratio: crate_cov.ratio,
                    threshold: self.aggregate,
                });
            }
        }
        for file_cov in &report.files {
            if file_cov.ratio.is_nan() || file_cov.ratio < self.per_file {
                failures.push(CoverageFailure {
                    scope: CoverageScope::File(file_cov.path.clone()),
                    ratio: file_cov.ratio,
                    threshold: self.per_file,
                });
            }
        }
        CoverageVerdict {
            passed: failures.is_empty(),
            failures,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floor() -> CoverageFloor {
        CoverageFloor::default()
    }

    fn crate_cov(name: &str, ratio: f64) -> CrateCoverage {
        CrateCoverage {
            name: name.to_string(),
            ratio,
        }
    }

    fn file_cov(path: &str, ratio: f64) -> FileCoverage {
        FileCoverage {
            path: path.to_string(),
            ratio,
        }
    }

    fn report(crates: Vec<CrateCoverage>, files: Vec<FileCoverage>) -> CoverageReport {
        CoverageReport { crates, files }
    }

    #[test]
    fn aggregate_below_floor_fails_naming_crate() {
        let verdict = floor().evaluate(&report(vec![crate_cov("polyforge-core", 0.79)], vec![]));
        assert!(!verdict.passed);
        assert_eq!(verdict.failures.len(), 1);
        assert_eq!(
            verdict.failures[0].scope,
            CoverageScope::Crate("polyforge-core".to_string())
        );
        assert_eq!(verdict.failures[0].ratio, 0.79);
        assert_eq!(verdict.failures[0].threshold, 0.80);
    }

    #[test]
    fn per_file_below_floor_fails_naming_file() {
        // Aggregate clears the floor; a single file does not -> fail names the file.
        let verdict = floor().evaluate(&report(
            vec![crate_cov("polyforge-core", 0.95)],
            vec![file_cov("crates/polyforge-core/src/ledger.rs", 0.79)],
        ));
        assert!(!verdict.passed);
        assert_eq!(verdict.failures.len(), 1);
        assert_eq!(
            verdict.failures[0].scope,
            CoverageScope::File("crates/polyforge-core/src/ledger.rs".to_string())
        );
        assert_eq!(verdict.failures[0].ratio, 0.79);
        assert_eq!(verdict.failures[0].threshold, 0.80);
    }

    #[test]
    fn exact_floor_passes_inclusive() {
        // Aggregate AND file both exactly at 0.80: inclusive -> pass.
        let verdict = floor().evaluate(&report(
            vec![crate_cov("polyforge-core", 0.80)],
            vec![file_cov("crates/polyforge-core/src/gate.rs", 0.80)],
        ));
        assert!(verdict.passed, "0.80 must satisfy a 0.80 floor (inclusive)");
        assert!(verdict.failures.is_empty());
    }

    #[test]
    fn empty_report_passes() {
        let verdict = floor().evaluate(&report(vec![], vec![]));
        assert!(verdict.passed);
        assert!(verdict.failures.is_empty());
    }

    #[test]
    fn same_report_twice_yields_identical_verdict() {
        let report = report(
            vec![
                crate_cov("polyforge-core", 0.85),
                crate_cov("polyforge-toolrunner", 0.91),
            ],
            vec![
                file_cov("crates/polyforge-core/src/evidence.rs", 0.97),
                file_cov("crates/polyforge-toolrunner/src/runner.rs", 0.42),
            ],
        );
        let a = floor().evaluate(&report);
        let b = floor().evaluate(&report);
        assert_eq!(a, b, "deterministic: identical report -> identical verdict");
        assert!(!a.passed);
        // Failures are reported in report order: crates first, then files.
        assert_eq!(
            a.failures
                .iter()
                .map(|f| f.scope.clone())
                .collect::<Vec<_>>(),
            vec![CoverageScope::File(
                "crates/polyforge-toolrunner/src/runner.rs".to_string()
            )]
        );
    }

    #[test]
    fn nan_ratio_is_a_failure_not_a_pass() {
        // A NaN ratio must never clear the floor (fail-closed semantics).
        let verdict =
            floor().evaluate(&report(vec![crate_cov("polyforge-core", f64::NAN)], vec![]));
        assert!(!verdict.passed);
        assert_eq!(verdict.failures.len(), 1);
        assert_eq!(
            verdict.failures[0].scope,
            CoverageScope::Crate("polyforge-core".to_string())
        );
    }
}
