use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

const MAX_DATASET_BYTES: usize = 4 * 1024 * 1024;
const MAX_RECORDS: usize = 10_000;
const MAX_SCOPE_BYTES: usize = 256;
const MAX_LABEL_BYTES: usize = 128;
const MAX_IDENTITY_BYTES: usize = 256;
const MAX_EVIDENCE_REF_BYTES: usize = 512;
const MAX_LATENCY_MS: u64 = 86_400_000;
const MAX_COST_MICROUSD: u64 = 1_000_000_000_000;
const DEFAULT_MINIMUM_SAMPLES: u64 = 20;
const DEFAULT_MINIMUM_SUCCESS_BPS: u64 = 9_500;

#[derive(Clone, PartialEq, Eq)]
pub struct AdaptiveError(&'static str);

impl fmt::Debug for AdaptiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for AdaptiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for AdaptiveError {}

fn invalid() -> AdaptiveError {
    AdaptiveError("dataset invalid")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dataset {
    schema_version: u8,
    #[serde(default)]
    settings: Settings,
    outcomes: Vec<OutcomeRecord>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(default = "default_minimum_samples")]
    minimum_samples: u64,
    #[serde(default = "default_minimum_success_bps")]
    minimum_success_bps: u64,
}

fn default_minimum_samples() -> u64 {
    DEFAULT_MINIMUM_SAMPLES
}

fn default_minimum_success_bps() -> u64 {
    DEFAULT_MINIMUM_SUCCESS_BPS
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            minimum_samples: default_minimum_samples(),
            minimum_success_bps: default_minimum_success_bps(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutcomeRecord {
    scope: String,
    workload: String,
    task_family: String,
    case_id: String,
    target_alias: String,
    model: ModelIdentity,
    outcome: Outcome,
    #[serde(default)]
    evidence_kind: Option<EvidenceKind>,
    #[serde(default)]
    evidence_ref: Option<String>,
    latency_ms: u64,
    #[serde(default)]
    cost_microusd: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelIdentity {
    pub model_id: String,
    pub model_revision: String,
    pub adapter_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    QualityPass,
    QualityFail,
    InfrastructureFailure,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceKind {
    ExecutableTest,
    SchemaValidation,
    HumanReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QualificationStatus {
    InsufficientSamples,
    QualityBelowThreshold,
    Qualified,
}

#[derive(Debug, Serialize)]
pub struct QualificationReport {
    pub schema_version: u8,
    pub advisory_only: bool,
    pub criteria: Criteria,
    pub groups: Vec<QualificationGroup>,
    pub comparisons: Vec<CoverageComparison>,
    pub advisory_qualified_groups: Vec<GroupIdentity>,
}

#[derive(Debug, Serialize)]
pub struct Criteria {
    pub minimum_samples: u64,
    pub minimum_success_bps: u64,
    pub meaning: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct GroupIdentity {
    pub scope: String,
    pub workload: String,
    pub task_family: String,
    pub target_alias: String,
    pub model_id: String,
    pub model_revision: String,
    pub adapter_version: String,
}

#[derive(Debug, Serialize)]
pub struct QualificationGroup {
    #[serde(flatten)]
    pub identity: GroupIdentity,
    pub record_count: u64,
    pub reviewed_cases: u64,
    pub quality_pass: u64,
    pub quality_fail: u64,
    pub infrastructure_failure: u64,
    pub unknown: u64,
    pub success_bps: Option<u64>,
    pub average_latency_ms: u64,
    pub known_cost_microusd: Option<u64>,
    pub known_cost_records: u64,
    pub counts_reconciled: bool,
    pub status: QualificationStatus,
    pub comparable: bool,
}

#[derive(Debug, Serialize)]
pub struct CoverageComparison {
    pub scope: String,
    pub workload: String,
    pub task_family: String,
    pub model_groups: u64,
    pub comparable: bool,
    pub preferred_target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GroupKey {
    scope: String,
    workload: String,
    task_family: String,
    target_alias: String,
    model: ModelIdentity,
}

impl From<&GroupKey> for GroupIdentity {
    fn from(key: &GroupKey) -> Self {
        Self {
            scope: key.scope.clone(),
            workload: key.workload.clone(),
            task_family: key.task_family.clone(),
            target_alias: key.target_alias.clone(),
            model_id: key.model.model_id.clone(),
            model_revision: key.model.model_revision.clone(),
            adapter_version: key.model.adapter_version.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CoverageKey {
    scope: String,
    workload: String,
    task_family: String,
}

#[derive(Default)]
struct GroupTotals {
    record_count: u64,
    quality_pass: u64,
    quality_fail: u64,
    infrastructure_failure: u64,
    unknown: u64,
    latency_total_ms: u64,
    known_cost_microusd: u64,
    known_cost_records: u64,
    reviewed_case_ids: BTreeSet<String>,
}

pub fn evaluate_file(path: &Path) -> Result<QualificationReport, AdaptiveError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| AdaptiveError("dataset unavailable"))?;
    if !file
        .metadata()
        .map_err(|_| AdaptiveError("dataset unavailable"))?
        .is_file()
    {
        return Err(AdaptiveError("dataset unavailable"));
    }
    let mut bytes = Vec::with_capacity(MAX_DATASET_BYTES.min(8192));
    file.take((MAX_DATASET_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| AdaptiveError("dataset unreadable"))?;
    evaluate_bytes(&bytes)
}

pub fn evaluate_bytes(bytes: &[u8]) -> Result<QualificationReport, AdaptiveError> {
    if bytes.len() > MAX_DATASET_BYTES {
        return Err(AdaptiveError("dataset exceeds size limit"));
    }
    let dataset: Dataset = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if dataset.schema_version != 1
        || dataset.settings.minimum_samples == 0
        || dataset.settings.minimum_samples > MAX_RECORDS as u64
        || dataset.settings.minimum_success_bps == 0
        || dataset.settings.minimum_success_bps > 10_000
        || dataset.outcomes.len() > MAX_RECORDS
    {
        return Err(invalid());
    }

    let mut groups: BTreeMap<GroupKey, GroupTotals> = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    for record in dataset.outcomes {
        validate_record(&record)?;
        let key = GroupKey {
            scope: record.scope,
            workload: record.workload,
            task_family: record.task_family,
            target_alias: record.target_alias,
            model: record.model,
        };
        let duplicate_key = (key.clone(), record.case_id.clone());
        if !duplicates.insert(duplicate_key) {
            return Err(invalid());
        }
        let totals = groups.entry(key).or_default();
        totals.record_count = checked_add(totals.record_count, 1)?;
        totals.latency_total_ms = checked_add(totals.latency_total_ms, record.latency_ms)?;
        if let Some(cost) = record.cost_microusd {
            totals.known_cost_microusd = checked_add(totals.known_cost_microusd, cost)?;
            totals.known_cost_records = checked_add(totals.known_cost_records, 1)?;
        }
        match record.outcome {
            Outcome::QualityPass => {
                totals.quality_pass = checked_add(totals.quality_pass, 1)?;
                totals.reviewed_case_ids.insert(record.case_id);
            }
            Outcome::QualityFail => {
                totals.quality_fail = checked_add(totals.quality_fail, 1)?;
                totals.reviewed_case_ids.insert(record.case_id);
            }
            Outcome::InfrastructureFailure => {
                totals.infrastructure_failure = checked_add(totals.infrastructure_failure, 1)?;
            }
            Outcome::Unknown => {
                totals.unknown = checked_add(totals.unknown, 1)?;
            }
        }
    }

    let coverage = coverage_by_group(&groups);
    let mut report_groups = Vec::with_capacity(groups.len());
    let mut qualified_groups = Vec::new();
    for (key, totals) in &groups {
        let reviewed_cases = checked_add(totals.quality_pass, totals.quality_fail)?;
        let success_bps = if reviewed_cases == 0 {
            None
        } else {
            Some(
                totals
                    .quality_pass
                    .checked_mul(10_000)
                    .and_then(|scaled| scaled.checked_div(reviewed_cases))
                    .ok_or_else(invalid)?,
            )
        };
        let status = qualification_status(
            reviewed_cases,
            success_bps,
            dataset.settings.minimum_samples,
            dataset.settings.minimum_success_bps,
        );
        let identity = GroupIdentity::from(key);
        if status == QualificationStatus::Qualified {
            qualified_groups.push(identity.clone());
        }
        report_groups.push(QualificationGroup {
            identity,
            record_count: totals.record_count,
            reviewed_cases,
            quality_pass: totals.quality_pass,
            quality_fail: totals.quality_fail,
            infrastructure_failure: totals.infrastructure_failure,
            unknown: totals.unknown,
            success_bps,
            average_latency_ms: totals.latency_total_ms / totals.record_count,
            known_cost_microusd: (totals.known_cost_records > 0)
                .then_some(totals.known_cost_microusd),
            known_cost_records: totals.known_cost_records,
            counts_reconciled: reviewed_cases
                .checked_add(totals.infrastructure_failure)
                .and_then(|count| count.checked_add(totals.unknown))
                == Some(totals.record_count),
            status,
            comparable: coverage[&CoverageKey::from(key)],
        });
    }

    let comparisons = comparison_report(&groups, &coverage)?;
    Ok(QualificationReport {
        schema_version: 1,
        advisory_only: true,
        criteria: Criteria {
            minimum_samples: dataset.settings.minimum_samples,
            minimum_success_bps: dataset.settings.minimum_success_bps,
            meaning: "caller-selected empirical criteria; not a statistical guarantee or certified truth",
        },
        groups: report_groups,
        comparisons,
        advisory_qualified_groups: qualified_groups,
    })
}

impl From<&GroupKey> for CoverageKey {
    fn from(key: &GroupKey) -> Self {
        Self {
            scope: key.scope.clone(),
            workload: key.workload.clone(),
            task_family: key.task_family.clone(),
        }
    }
}

fn coverage_by_group(groups: &BTreeMap<GroupKey, GroupTotals>) -> BTreeMap<CoverageKey, bool> {
    let mut cases_by_coverage: BTreeMap<CoverageKey, Vec<&BTreeSet<String>>> = BTreeMap::new();
    for (key, totals) in groups {
        cases_by_coverage
            .entry(CoverageKey::from(key))
            .or_default()
            .push(&totals.reviewed_case_ids);
    }
    cases_by_coverage
        .into_iter()
        .map(|(key, case_sets)| {
            let comparable = case_sets
                .first()
                .is_none_or(|first| case_sets.iter().all(|cases| *cases == *first));
            (key, comparable)
        })
        .collect()
}

fn comparison_report(
    groups: &BTreeMap<GroupKey, GroupTotals>,
    coverage: &BTreeMap<CoverageKey, bool>,
) -> Result<Vec<CoverageComparison>, AdaptiveError> {
    let mut group_counts: BTreeMap<CoverageKey, u64> = BTreeMap::new();
    for key in groups.keys() {
        let count = group_counts.entry(CoverageKey::from(key)).or_default();
        *count = checked_add(*count, 1)?;
    }
    Ok(group_counts
        .into_iter()
        .map(|(key, model_groups)| CoverageComparison {
            comparable: coverage[&key],
            scope: key.scope,
            workload: key.workload,
            task_family: key.task_family,
            model_groups,
            preferred_target: None,
        })
        .collect())
}

fn qualification_status(
    reviewed_cases: u64,
    success_bps: Option<u64>,
    minimum_samples: u64,
    minimum_success_bps: u64,
) -> QualificationStatus {
    if reviewed_cases < minimum_samples {
        QualificationStatus::InsufficientSamples
    } else if success_bps.is_none_or(|value| value < minimum_success_bps) {
        QualificationStatus::QualityBelowThreshold
    } else {
        QualificationStatus::Qualified
    }
}

fn validate_record(record: &OutcomeRecord) -> Result<(), AdaptiveError> {
    validate_string(&record.scope, MAX_SCOPE_BYTES)?;
    validate_string(&record.workload, MAX_LABEL_BYTES)?;
    validate_string(&record.task_family, MAX_LABEL_BYTES)?;
    validate_string(&record.case_id, MAX_LABEL_BYTES)?;
    validate_string(&record.target_alias, MAX_LABEL_BYTES)?;
    validate_string(&record.model.model_id, MAX_IDENTITY_BYTES)?;
    validate_string(&record.model.model_revision, MAX_IDENTITY_BYTES)?;
    validate_string(&record.model.adapter_version, MAX_IDENTITY_BYTES)?;
    if record.latency_ms == 0 || record.latency_ms > MAX_LATENCY_MS {
        return Err(invalid());
    }
    if record
        .cost_microusd
        .is_some_and(|cost| cost > MAX_COST_MICROUSD)
    {
        return Err(invalid());
    }
    match record.outcome {
        Outcome::QualityPass | Outcome::QualityFail => {
            if record.evidence_kind.is_none()
                || record.evidence_ref.as_deref().is_none_or(|reference| {
                    validate_string(reference, MAX_EVIDENCE_REF_BYTES).is_err()
                })
            {
                return Err(invalid());
            }
        }
        Outcome::InfrastructureFailure | Outcome::Unknown => {
            if record.evidence_kind.is_some() || record.evidence_ref.is_some() {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

fn validate_string(value: &str, maximum_bytes: usize) -> Result<(), AdaptiveError> {
    if value.is_empty()
        || value.len() > maximum_bytes
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(invalid());
    }
    Ok(())
}

fn checked_add(left: u64, right: u64) -> Result<u64, AdaptiveError> {
    left.checked_add(right).ok_or_else(invalid)
}

pub fn run_cli(args: &[String]) -> Result<(), AdaptiveError> {
    let [command, dataset] = args else {
        return Err(AdaptiveError(
            "usage: slotline routing evaluate DATASET.json",
        ));
    };
    if command != "evaluate" {
        return Err(AdaptiveError(
            "usage: slotline routing evaluate DATASET.json",
        ));
    }
    let report = evaluate_file(Path::new(dataset))?;
    serde_json::to_writer(std::io::stdout(), &report)
        .map_err(|_| AdaptiveError("report unavailable"))?;
    println!();
    Ok(())
}
