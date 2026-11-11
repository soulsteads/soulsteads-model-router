use super::*;
use std::io::Write;
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn audit_sse_first_meaningful_not_role_heartbeat_and_usage_reused() {
    use tracing::instrument::WithSubscriber;
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let context = crate::audit::context(Instant::now(), "chat");
    crate::audit::CURRENT.scope(context.clone(),async {
        let mut tools=ChatToolFragments::default();let mut completed=false;
        let mut normalize=|value:Value| normalize_event(SseEvent{fields:Vec::new(),event:None,data:Some(value.to_string().into_bytes())},"alias","backend",false,false,&mut completed,&mut tools).unwrap();
        normalize(json!({"choices":[{"delta":{"role":"assistant","content":""}}]}));
        normalize(json!({"choices":[],"usage":{"prompt_tokens":8}}));
        assert!(!std::str::from_utf8(&capture.0.lock().unwrap()).unwrap().contains("request first output"));
        normalize(json!({"choices":[{"delta":{"reasoning_content":"PRIVATE_REASONING"}}]}));
        normalize(json!({"choices":[{"delta":{"content":"PRIVATE_TEXT"}}]}));
        normalize(json!({"choices":[{"delta":{"refusal":"PRIVATE_REFUSAL"}}]}));
        normalize(json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":3,"total_tokens":11}}));
        crate::audit::terminal(&context,200,"request body completed");
    }).with_subscriber(subscriber).await;
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let events: Vec<Value> = logs
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let first: Vec<_> = events
        .iter()
        .filter(|event| event["fields"]["message"] == "request first output")
        .collect();
    assert_eq!(
        first.len(),
        1,
        "first meaningful output missing or duplicated"
    );
    assert_eq!(first[0]["fields"]["output_kind"], "reasoning");
    assert_eq!(events.last().unwrap()["fields"]["total_tokens"], 11);
    assert!(!logs.contains("PRIVATE"));
}
