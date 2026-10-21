use super::*;

#[test]
fn validated_progress_classification_excludes_decoys_and_snapshots() {
    for (responses, value, expected) in [
        (
            false,
            json!({"choices":[{"delta":{"role":"assistant","content":""}}]}),
            false,
        ),
        (
            false,
            json!({"choices":[],"usage":{"output_tokens":1}}),
            false,
        ),
        (false, json!({"choices":[{"delta":{"content":"x"}}]}), true),
        (
            false,
            json!({"choices":[{"delta":{"reasoning_content":"x"}}]}),
            true,
        ),
        (
            false,
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"echo","arguments":""}}]}}]}),
            false,
        ),
        (
            false,
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"echo","arguments":"{}"}}]}}]}),
            true,
        ),
        (
            true,
            json!({"type":"response.output_text.delta","delta":"x"}),
            true,
        ),
        (
            true,
            json!({"type":"response.reasoning_text.delta","delta":"x"}),
            true,
        ),
        (
            true,
            json!({"type":"response.function_call_arguments.delta","delta":"{}"}),
            true,
        ),
        (
            true,
            json!({"type":"response.output_text.delta","delta":""}),
            false,
        ),
        (
            true,
            json!({"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"snapshot"}]}}),
            false,
        ),
        (
            true,
            json!({"type":"response.unknown.delta","delta":"decoy"}),
            false,
        ),
    ] {
        let event = SseEvent {
            data: Some(value.to_string().into_bytes()),
            fields: Vec::new(),
            event: None,
        };
        let result = normalize_event(
            event,
            "alias",
            "backend",
            responses,
            false,
            &mut false,
            &mut ChatToolFragments::default(),
        )
        .unwrap();
        assert_eq!(result.3, expected, "{value}");
    }
    let result = normalize_event(
        SseEvent {
            data: None,
            fields: b":heartbeat\n".to_vec(),
            event: None,
        },
        "alias",
        "backend",
        false,
        false,
        &mut false,
        &mut ChatToolFragments::default(),
    )
    .unwrap();
    assert!(!result.3);
    let malformed = SseEvent {
        data: Some(br#"{"choices":[{"delta":{"content":42}}]}"#.to_vec()),
        fields: Vec::new(),
        event: None,
    };
    assert!(
        normalize_event(
            malformed,
            "alias",
            "backend",
            false,
            false,
            &mut false,
            &mut ChatToolFragments::default()
        )
        .is_err()
    );
}

#[test]
fn partial_event_bytes_are_not_validated_progress() {
    let mut decoder = SseDecoder::default();
    let mut first = &b"data:{\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n"[..];
    assert!(decoder.next_event(&mut first).unwrap().is_none());
    let mut delimiter = &b"\n"[..];
    let event = decoder.next_event(&mut delimiter).unwrap().unwrap();
    assert!(
        normalize_event(
            event,
            "alias",
            "backend",
            false,
            false,
            &mut false,
            &mut ChatToolFragments::default()
        )
        .unwrap()
        .3
    );
}

#[test]
fn idle_only_waits_for_first_progress_before_arming() {
    let start = tokio::time::Instant::now();
    let mut clock = ProgressClock::new(start, Duration::from_secs(4), None, Some(1));
    assert_eq!(clock.failure_at(start + Duration::from_secs(2)), None);
    clock.progress(start + Duration::from_secs(2));
    assert_eq!(clock.failure_at(start + Duration::from_millis(2999)), None);
    assert_eq!(
        clock.failure_at(start + Duration::from_secs(3)),
        Some(StreamFailure::Idle)
    );
}

// These tests use explicit instants, avoiding scheduler-dependent sleeps.
#[test]
fn idle_clock_resets_only_on_progress_and_cannot_extend_hard_cap() {
    let start = tokio::time::Instant::now();
    let mut clock = ProgressClock::new(start, Duration::from_secs(3), Some(1), Some(1));
    clock.progress(start + Duration::from_millis(500));
    assert_eq!(clock.failure_at(start + Duration::from_millis(1499)), None);
    assert!(
        clock
            .failure_at(start + Duration::from_millis(1500))
            .is_some()
    );
    clock.progress(start + Duration::from_millis(1400));
    assert_eq!(clock.failure_at(start + Duration::from_secs(2)), None);
    assert_eq!(
        clock.failure_at(start + Duration::from_secs(3)),
        Some(StreamFailure::Deadline)
    );
}

#[test]
fn first_output_clock_includes_headers_and_ignores_raw_activity() {
    let start = tokio::time::Instant::now();
    let clock = ProgressClock::new(start, Duration::from_secs(3), Some(1), None);
    assert_eq!(clock.failure_at(start + Duration::from_millis(999)), None);
    assert_eq!(
        clock.failure_at(start + Duration::from_secs(1)),
        Some(StreamFailure::FirstOutput)
    );
}

#[test]
fn meaningful_output_disarms_first_output_clock() {
    let start = tokio::time::Instant::now();
    let mut clock = ProgressClock::new(start, Duration::from_secs(3), Some(1), None);
    clock.progress(start + Duration::from_millis(500));
    assert_eq!(clock.failure_at(start + Duration::from_secs(2)), None);
    assert_eq!(
        clock.failure_at(start + Duration::from_secs(3)),
        Some(StreamFailure::Deadline)
    );
}

#[test]
fn downstream_pause_extends_progress_but_never_hard_deadline() {
    let start = tokio::time::Instant::now();
    let mut clock = ProgressClock::new(start, Duration::from_secs(3), Some(1), None);
    clock.pause(Duration::from_secs(1));
    assert_eq!(clock.failure_at(start + Duration::from_millis(1500)), None);
    assert_eq!(
        clock.failure_at(start + Duration::from_secs(2)),
        Some(StreamFailure::FirstOutput)
    );
    clock.pause(Duration::from_secs(10));
    assert_eq!(
        clock.failure_at(start + Duration::from_secs(3)),
        Some(StreamFailure::Deadline)
    );
}
