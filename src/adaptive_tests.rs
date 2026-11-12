use crate::adaptive::{
    QualificationReport, QualificationStatus, evaluate_bytes, evaluate_file, run_cli,
};
use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

const FIFO_CHILD_PATH_ENV: &str = "SLOTLINE_ADAPTIVE_FIFO_CHILD_PATH";
static FIFO_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn outcome_record(case_id: &str, target_alias: &str, outcome: &str) -> Value {
    let mut record = json!({
        "scope": "scope-a",
        "workload": "batch",
        "task_family": "extract",
        "case_id": case_id,
        "target_alias": target_alias,
        "model": {
            "model_id": "model-a",
            "model_revision": "revision-1",
            "adapter_version": "adapter-1"
        },
        "outcome": outcome,
        "latency_ms": 10
    });
    if matches!(outcome, "quality_pass" | "quality_fail") {
        let fields = record.as_object_mut().expect("outcome record object");
        // This supplies only operator-provided evidence metadata; no test fetches or proves it.
        fields.insert("evidence_kind".to_owned(), json!("human_review"));
        fields.insert("evidence_ref".to_owned(), json!("operator-review"));
    }
    record
}

fn dataset_bytes(outcomes: Vec<Value>, settings: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema_version": 1,
        "settings": settings,
        "outcomes": outcomes,
    }))
    .expect("serialize test dataset")
}

fn evaluated(outcomes: Vec<Value>, settings: Value) -> QualificationReport {
    evaluate_bytes(&dataset_bytes(outcomes, settings)).expect("valid test dataset")
}

fn assert_invalid(outcomes: Vec<Value>, settings: Value) {
    let error = evaluate_bytes(&dataset_bytes(outcomes, settings)).expect_err("invalid dataset");
    assert_eq!(error.to_string(), "dataset invalid");
}

fn baseline_settings() -> Value {
    json!({"minimum_samples": 1, "minimum_success_bps": 1})
}

fn test_fifo_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "slotline-adaptive-fifo-{}-{}",
        std::process::id(),
        FIFO_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn make_fifo(path: &Path) {
    let encoded_path = CString::new(path.as_os_str().as_bytes()).expect("FIFO path has no NUL");
    assert_eq!(
        // SAFETY: `encoded_path` is a NUL-terminated path and the requested mode is valid.
        unsafe { libc::mkfifo(encoded_path.as_ptr(), 0o600) },
        0,
        "create FIFO: {}",
        std::io::Error::last_os_error()
    );
}

#[test]
fn rejects_fifo_without_blocking_and_sanitizes_error() {
    if let Some(path) = std::env::var_os(FIFO_CHILD_PATH_ENV) {
        let error = evaluate_file(Path::new(&path)).expect_err("FIFO must be refused");
        assert_eq!(error.to_string(), "dataset unavailable");
        return;
    }

    let fifo = test_fifo_path();
    make_fifo(&fifo);
    let mut child = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            "adaptive_tests::rejects_fifo_without_blocking_and_sanitizes_error",
            "--nocapture",
        ])
        .env(FIFO_CHILD_PATH_ENV, &fifo)
        .spawn()
        .expect("start isolated FIFO evaluator");
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                timed_out = true;
                let _ = child.kill();
                break child.wait().expect("reap blocked FIFO evaluator");
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&fifo);
                panic!("observe isolated FIFO evaluator: {error}");
            }
        }
    };
    fs::remove_file(&fifo).expect("remove FIFO");

    assert!(
        !timed_out,
        "FIFO evaluator exceeded its bounded completion window"
    );
    assert!(status.success(), "isolated FIFO evaluator failed: {status}");
}

#[test]
fn groups_operator_supplied_outcomes_and_reports_qualification() {
    let report = evaluate_bytes(
        br#"{
          "schema_version": 1,
          "settings": {"minimum_samples": 2, "minimum_success_bps": 9500},
          "outcomes": [
            {
              "scope": "tenant-a", "workload": "batch", "task_family": "extract",
              "case_id": "case-1", "target_alias": "local-small",
              "model": {"model_id": "qwen", "model_revision": "v1", "adapter_version": "a1"},
              "outcome": "quality_pass", "evidence_kind": "executable_test",
              "evidence_ref": "run-1", "latency_ms": 10, "cost_microusd": 4
            },
            {
              "scope": "tenant-a", "workload": "batch", "task_family": "extract",
              "case_id": "case-2", "target_alias": "local-small",
              "model": {"model_id": "qwen", "model_revision": "v1", "adapter_version": "a1"},
              "outcome": "quality_pass", "evidence_kind": "human_review",
              "evidence_ref": "run-2", "latency_ms": 20
            },
            {
              "scope": "tenant-a", "workload": "batch", "task_family": "extract",
              "case_id": "case-3", "target_alias": "local-small",
              "model": {"model_id": "qwen", "model_revision": "v1", "adapter_version": "a1"},
              "outcome": "infrastructure_failure", "latency_ms": 30
            }
          ]
        }"#,
    )
    .expect("dataset is valid");

    assert!(report.advisory_only);
    assert_eq!(report.groups.len(), 1);
    let group = &report.groups[0];
    assert_eq!(group.record_count, 3);
    assert_eq!(group.reviewed_cases, 2);
    assert_eq!(group.quality_pass, 2);
    assert_eq!(group.quality_fail, 0);
    assert_eq!(group.infrastructure_failure, 1);
    assert_eq!(group.unknown, 0);
    assert!(group.counts_reconciled);
    assert_eq!(group.success_bps, Some(10_000));
    assert_eq!(group.status, QualificationStatus::Qualified);
    assert_eq!(group.average_latency_ms, 20);
    assert_eq!(group.known_cost_microusd, Some(4));
    assert_eq!(group.known_cost_records, 1);
}

#[test]
fn rejects_unreviewed_quality_and_duplicate_fields_without_echoing_input() {
    let missing_evidence = br#"{
      "schema_version": 1,
      "outcomes": [{
        "scope": "tenant-a", "workload": "batch", "task_family": "extract",
        "case_id": "case-1", "target_alias": "local-small",
        "model": {"model_id": "qwen", "model_revision": "v1", "adapter_version": "a1"},
        "outcome": "quality_pass", "latency_ms": 10
      }]
    }"#;
    let error = evaluate_bytes(missing_evidence).expect_err("quality requires reviewed evidence");
    assert_eq!(error.to_string(), "dataset invalid");

    let duplicate_field = br#"{"schema_version":1,"schema_version":1,"outcomes":[]}"#;
    assert!(evaluate_bytes(duplicate_field).is_err());
}

#[test]
fn keeps_scopes_and_model_revisions_separate_and_marks_unpaired_coverage() {
    let report = evaluate_bytes(
        br#"{
          "schema_version": 1,
          "settings": {"minimum_samples": 1, "minimum_success_bps": 9000},
          "outcomes": [
            {
              "scope":"tenant-a", "workload":"batch", "task_family":"extract", "case_id":"case-1",
              "target_alias":"local", "model":{"model_id":"qwen","model_revision":"v1","adapter_version":"a1"},
              "outcome":"quality_pass", "evidence_kind":"human_review", "evidence_ref":"review-1", "latency_ms":5
            },
            {
              "scope":"tenant-a", "workload":"batch", "task_family":"extract", "case_id":"case-2",
              "target_alias":"cloud", "model":{"model_id":"gpt","model_revision":"v2","adapter_version":"native-2"},
              "outcome":"quality_pass", "evidence_kind":"human_review", "evidence_ref":"review-2", "latency_ms":8
            },
            {
              "scope":"tenant-b", "workload":"batch", "task_family":"extract", "case_id":"case-1",
              "target_alias":"local", "model":{"model_id":"qwen","model_revision":"v2","adapter_version":"a1"},
              "outcome":"unknown", "latency_ms":6
            }
          ]
        }"#,
    )
    .expect("dataset is valid");

    assert_eq!(report.groups.len(), 3);
    assert!(
        report
            .groups
            .iter()
            .all(|group| group.identity.scope != "tenant-a" || !group.comparable)
    );
    assert!(
        report
            .groups
            .iter()
            .any(|group| group.identity.scope == "tenant-b" && group.comparable)
    );
    let tenant_a = report
        .comparisons
        .iter()
        .find(|comparison| comparison.scope == "tenant-a")
        .expect("tenant-a coverage comparison");
    assert!(!tenant_a.comparable);
    assert_eq!(tenant_a.preferred_target, None);
    assert!(run_cli(&["not-evaluate".to_owned(), "dataset.json".to_owned()]).is_err());
}

#[test]
fn distinguishes_insufficient_below_threshold_and_qualified_groups() {
    let report = evaluated(
        vec![
            outcome_record("only-case", "insufficient", "quality_pass"),
            outcome_record("pass-case", "below-threshold", "quality_pass"),
            outcome_record("fail-case", "below-threshold", "quality_fail"),
            outcome_record("pass-one", "qualified", "quality_pass"),
            outcome_record("pass-two", "qualified", "quality_pass"),
        ],
        json!({"minimum_samples": 2, "minimum_success_bps": 9_000}),
    );

    let status_for = |target: &str| {
        &report
            .groups
            .iter()
            .find(|group| group.identity.target_alias == target)
            .expect("expected target group")
            .status
    };
    assert_eq!(
        status_for("insufficient"),
        &QualificationStatus::InsufficientSamples
    );
    assert_eq!(
        status_for("below-threshold"),
        &QualificationStatus::QualityBelowThreshold
    );
    assert_eq!(status_for("qualified"), &QualificationStatus::Qualified);
}

#[test]
fn excludes_unknown_and_infrastructure_outcomes_from_quality_counts() {
    let report = evaluated(
        vec![
            outcome_record("pass", "target", "quality_pass"),
            outcome_record("fail", "target", "quality_fail"),
            outcome_record("infra", "target", "infrastructure_failure"),
            outcome_record("unknown", "target", "unknown"),
        ],
        json!({"minimum_samples": 1, "minimum_success_bps": 10_000}),
    );
    let group = &report.groups[0];

    assert_eq!(group.record_count, 4);
    assert_eq!(group.reviewed_cases, 2);
    assert_eq!(group.quality_pass, 1);
    assert_eq!(group.quality_fail, 1);
    assert_eq!(group.infrastructure_failure, 1);
    assert_eq!(group.unknown, 1);
    assert_eq!(group.success_bps, Some(5_000));
    assert_eq!(group.status, QualificationStatus::QualityBelowThreshold);
    assert!(group.counts_reconciled);
}

#[test]
fn isolates_scope_workload_task_and_exact_model_identity() {
    let base = outcome_record("case-1", "target", "quality_pass");
    let mut other_scope = base.clone();
    other_scope["scope"] = json!("scope-b");
    let mut other_workload = base.clone();
    other_workload["workload"] = json!("interactive");
    let mut other_task = base.clone();
    other_task["task_family"] = json!("summarize");
    let mut other_model = base.clone();
    other_model["model"]["model_id"] = json!("model-b");
    let mut other_revision = base.clone();
    other_revision["model"]["model_revision"] = json!("revision-2");
    let mut other_adapter = base.clone();
    other_adapter["model"]["adapter_version"] = json!("adapter-2");

    let report = evaluated(
        vec![
            base,
            other_scope,
            other_workload,
            other_task,
            other_model,
            other_revision,
            other_adapter,
        ],
        baseline_settings(),
    );

    assert_eq!(report.groups.len(), 7);
    assert!(
        report
            .groups
            .iter()
            .any(|group| group.identity.scope == "scope-b")
    );
    assert!(
        report
            .groups
            .iter()
            .any(|group| group.identity.workload == "interactive")
    );
    assert!(
        report
            .groups
            .iter()
            .any(|group| group.identity.task_family == "summarize")
    );
    assert!(
        report
            .groups
            .iter()
            .any(|group| group.identity.model_id == "model-b")
    );
    assert!(
        report
            .groups
            .iter()
            .any(|group| group.identity.model_revision == "revision-2")
    );
    assert!(
        report
            .groups
            .iter()
            .any(|group| group.identity.adapter_version == "adapter-2")
    );
}

#[test]
fn reports_matching_and_unpaired_reviewed_coverage_without_preference() {
    let mut paired_local_one = outcome_record("case-1", "local", "quality_pass");
    paired_local_one["scope"] = json!("paired");
    let mut paired_local_two = outcome_record("case-2", "local", "quality_pass");
    paired_local_two["scope"] = json!("paired");
    let mut paired_cloud_one = outcome_record("case-1", "cloud", "quality_pass");
    paired_cloud_one["scope"] = json!("paired");
    let mut paired_cloud_two = outcome_record("case-2", "cloud", "quality_pass");
    paired_cloud_two["scope"] = json!("paired");
    let mut unpaired_local = outcome_record("case-1", "local", "quality_pass");
    unpaired_local["scope"] = json!("unpaired");
    let mut unpaired_cloud = outcome_record("case-2", "cloud", "quality_pass");
    unpaired_cloud["scope"] = json!("unpaired");

    let report = evaluated(
        vec![
            paired_local_one,
            paired_local_two,
            paired_cloud_one,
            paired_cloud_two,
            unpaired_local,
            unpaired_cloud,
        ],
        baseline_settings(),
    );
    let paired = report
        .comparisons
        .iter()
        .find(|comparison| comparison.scope == "paired")
        .expect("paired comparison");
    let unpaired = report
        .comparisons
        .iter()
        .find(|comparison| comparison.scope == "unpaired")
        .expect("unpaired comparison");

    assert_eq!(paired.model_groups, 2);
    assert!(paired.comparable);
    assert_eq!(paired.preferred_target, None);
    assert_eq!(unpaired.model_groups, 2);
    assert!(!unpaired.comparable);
    assert_eq!(unpaired.preferred_target, None);
    assert!(
        report
            .groups
            .iter()
            .filter(|group| group.identity.scope == "paired")
            .all(|group| group.comparable)
    );
    assert!(
        report
            .groups
            .iter()
            .filter(|group| group.identity.scope == "unpaired")
            .all(|group| !group.comparable)
    );
}

#[test]
fn rejects_duplicate_cases_unknown_fields_and_nested_duplicate_fields() {
    assert_invalid(
        vec![
            outcome_record("case-1", "target", "quality_pass"),
            outcome_record("case-1", "target", "quality_fail"),
        ],
        baseline_settings(),
    );

    let unknown_root = br#"{"schema_version":1,"outcomes":[],"unexpected":true}"#;
    let error = evaluate_bytes(unknown_root).expect_err("unknown root field must be rejected");
    assert_eq!(error.to_string(), "dataset invalid");

    let mut unknown_nested = outcome_record("case-1", "target", "quality_pass");
    unknown_nested["model"]["unexpected"] = json!(true);
    assert_invalid(vec![unknown_nested], baseline_settings());

    let nested_duplicate = br#"{
        "schema_version": 1,
        "outcomes": [{
          "scope":"scope-a", "workload":"batch", "task_family":"extract", "case_id":"case-1",
          "target_alias":"target", "model":{"model_id":"model-a","model_id":"model-b","model_revision":"revision-1","adapter_version":"adapter-1"},
          "outcome":"quality_pass", "evidence_kind":"human_review", "evidence_ref":"operator-review", "latency_ms":10
        }]
    }"#;
    let error = evaluate_bytes(nested_duplicate).expect_err("duplicate nested field must fail");
    assert_eq!(error.to_string(), "dataset invalid");
}

#[test]
fn enforces_dataset_record_numeric_and_string_bounds() {
    let too_large = vec![b' '; 4 * 1024 * 1024 + 1];
    let error = evaluate_bytes(&too_large).expect_err("over-size dataset must fail");
    assert_eq!(error.to_string(), "dataset exceeds size limit");

    let over_count = (0..10_001)
        .map(|index| outcome_record(&format!("case-{index}"), "target", "unknown"))
        .collect();
    assert_invalid(over_count, baseline_settings());

    let mut at_limits = outcome_record(&"c".repeat(128), &"a".repeat(128), "quality_pass");
    at_limits["scope"] = json!("s".repeat(256));
    at_limits["workload"] = json!("w".repeat(128));
    at_limits["task_family"] = json!("t".repeat(128));
    at_limits["model"]["model_id"] = json!("m".repeat(256));
    at_limits["model"]["model_revision"] = json!("r".repeat(256));
    at_limits["model"]["adapter_version"] = json!("a".repeat(256));
    at_limits["evidence_ref"] = json!("e".repeat(512));
    at_limits["latency_ms"] = json!(86_400_000_u64);
    at_limits["cost_microusd"] = json!(1_000_000_000_000_u64);
    assert!(
        evaluate_bytes(&dataset_bytes(
            vec![at_limits.clone()],
            json!({"minimum_samples": 10_000, "minimum_success_bps": 10_000}),
        ))
        .is_ok()
    );

    for invalid_settings in [
        json!({"minimum_samples": 0, "minimum_success_bps": 1}),
        json!({"minimum_samples": 10_001, "minimum_success_bps": 1}),
        json!({"minimum_samples": 1, "minimum_success_bps": 0}),
        json!({"minimum_samples": 1, "minimum_success_bps": 10_001}),
    ] {
        assert_invalid(vec![at_limits.clone()], invalid_settings);
    }

    let mut zero_latency = at_limits.clone();
    zero_latency["latency_ms"] = json!(0);
    assert_invalid(vec![zero_latency], baseline_settings());
    let mut excessive_latency = at_limits.clone();
    excessive_latency["latency_ms"] = json!(86_400_001_u64);
    assert_invalid(vec![excessive_latency], baseline_settings());
    let mut excessive_cost = at_limits.clone();
    excessive_cost["cost_microusd"] = json!(1_000_000_000_001_u64);
    assert_invalid(vec![excessive_cost], baseline_settings());
    let mut excessive_scope = at_limits.clone();
    excessive_scope["scope"] = json!("s".repeat(257));
    assert_invalid(vec![excessive_scope], baseline_settings());
    let mut excessive_label = at_limits.clone();
    excessive_label["workload"] = json!("w".repeat(129));
    assert_invalid(vec![excessive_label], baseline_settings());
    let mut excessive_identity = at_limits.clone();
    excessive_identity["model"]["model_revision"] = json!("r".repeat(257));
    assert_invalid(vec![excessive_identity], baseline_settings());
    let mut excessive_evidence_ref = at_limits.clone();
    excessive_evidence_ref["evidence_ref"] = json!("e".repeat(513));
    assert_invalid(vec![excessive_evidence_ref], baseline_settings());
    let mut control_character = at_limits;
    control_character["target_alias"] = json!("target\nname");
    assert_invalid(vec![control_character], baseline_settings());
}
