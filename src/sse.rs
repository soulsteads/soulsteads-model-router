use super::*;

pub(super) const MAX_EVENT_BYTES: usize = 1024 * 1024;

#[cfg(test)]
#[path = "audit_sse_tests.rs"]
mod audit_sse_tests;

#[cfg(test)]
#[path = "stream_progress_tests.rs"]
mod stream_progress_tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamFailure {
    FirstOutput,
    Idle,
    Deadline,
    Invalid(&'static str),
    Transport,
}
impl StreamFailure {
    fn category(self) -> &'static str {
        match self {
            Self::FirstOutput => "first_output_timeout",
            Self::Idle => "stream_idle_timeout",
            Self::Deadline => "deadline_exceeded",
            Self::Invalid(_) => "stream_validation",
            Self::Transport => "stream_transport",
        }
    }
    fn local_timeout(self) -> bool {
        matches!(self, Self::FirstOutput | Self::Idle | Self::Deadline)
    }
}

pub(super) struct PreparedStream {
    pub response: reqwest::Response,
    pub clock: Option<ProgressClock>,
    pub handed_off: tokio::time::Instant,
}

pub(super) struct ProgressClock {
    hard: tokio::time::Instant,
    progress_deadline: Option<tokio::time::Instant>,
    idle: Option<Duration>,
    saw_progress: bool,
}
impl ProgressClock {
    pub(super) fn new(
        start: tokio::time::Instant,
        hard: Duration,
        first: Option<u64>,
        idle: Option<u64>,
    ) -> Self {
        Self {
            hard: start + hard,
            progress_deadline: first.map(|seconds| start + Duration::from_secs(seconds)),
            idle: idle.map(Duration::from_secs),
            saw_progress: false,
        }
    }
    fn deadline(&self) -> tokio::time::Instant {
        self.progress_deadline
            .map_or(self.hard, |progress| progress.min(self.hard))
    }
    fn failure_at(&self, now: tokio::time::Instant) -> Option<StreamFailure> {
        if now >= self.hard {
            Some(StreamFailure::Deadline)
        } else if self
            .progress_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            Some(if self.saw_progress {
                StreamFailure::Idle
            } else {
                StreamFailure::FirstOutput
            })
        } else {
            None
        }
    }
    fn progress(&mut self, now: tokio::time::Instant) {
        self.saw_progress = true;
        self.progress_deadline = self.idle.map(|idle| now + idle);
    }
    fn pause(&mut self, duration: Duration) {
        if let Some(deadline) = &mut self.progress_deadline {
            *deadline += duration;
        }
    }
}

#[derive(Default)]
struct ChatToolFragments(std::collections::BTreeMap<(u64, u64), Value>);

impl ChatToolFragments {
    fn absorb(&mut self, value: &Value) -> Result<(), &'static str> {
        for choice in value["choices"].as_array().ok_or("Invalid choices")? {
            let choice_index = choice.get("index").and_then(Value::as_u64).unwrap_or(0);
            if choice_index >= 128 {
                return Err("Invalid choice index");
            }
            if let Some(calls) = choice["delta"].get("tool_calls") {
                for fragment in calls.as_array().ok_or("Invalid tools")? {
                    let index = fragment
                        .get("index")
                        .and_then(Value::as_u64)
                        .ok_or("Missing tool index")?;
                    if index >= 128 || fragment.as_object().is_none_or(|v| v.len() < 2) {
                        return Err("Invalid tool fragment");
                    }
                    if !self.0.contains_key(&(choice_index, index)) && self.0.len() >= 128 {
                        return Err("Too many assembled tools");
                    }
                    let call = self
                        .0
                        .entry((choice_index, index))
                        .or_insert_with(|| json!({"id":"","function":{"name":""}}));
                    for (key, bound) in [("id", 128), ("name", 64), ("arguments", 65536)] {
                        let part = if key == "id" {
                            fragment.get(key)
                        } else {
                            fragment.get("function").and_then(|f| f.get(key))
                        };
                        if let Some(part) = part {
                            let part = part.as_str().ok_or("Invalid tool fragment field")?;
                            let target = if key == "id" {
                                &mut call[key]
                            } else {
                                &mut call["function"][key]
                            };
                            let old = if target.is_null() {
                                ""
                            } else {
                                target.as_str().ok_or("Invalid tool assembly")?
                            };
                            if old.len().saturating_add(part.len()) > bound {
                                return Err("Tool fragment limit");
                            }
                            *target = Value::String(format!("{old}{part}"));
                        }
                    }
                    if fragment.get("type").is_some() {
                        call["type"] = Value::String("function".into());
                    }
                }
            }
        }
        Ok(())
    }
    fn complete(&self, model: &str) -> bool {
        self.0.values().all(|call| valid_chat(&json!({"model":model,"choices":[{"message":{"role":"assistant","tool_calls":[call]}}]}),model))
    }
}

#[derive(Default)]
pub(super) struct SseDecoder {
    line: Vec<u8>,
    data: Option<Vec<u8>>,
    fields: Vec<u8>,
    event: Option<String>,
    size: usize,
    after_cr: bool,
}

pub(super) struct SseEvent {
    pub(super) data: Option<Vec<u8>>,
    fields: Vec<u8>,
    pub(super) event: Option<String>,
}

impl SseDecoder {
    pub(super) fn next_event(
        &mut self,
        input: &mut &[u8],
    ) -> Result<Option<SseEvent>, &'static str> {
        while let Some((&byte, rest)) = input.split_first() {
            *input = rest;
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.size += 1;
            if self.size > MAX_EVENT_BYTES {
                return Err("SSE event too large");
            }
            if byte != b'\r' && byte != b'\n' {
                self.line.push(byte);
                continue;
            }
            self.after_cr = byte == b'\r';
            if self.line.is_empty() {
                self.size = 0;
                if self.data.is_some() || !self.fields.is_empty() {
                    return Ok(Some(SseEvent {
                        data: self.data.take(),
                        fields: std::mem::take(&mut self.fields),
                        event: self.event.take(),
                    }));
                }
                self.event = None;
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let text = std::str::from_utf8(&line).map_err(|_| "Invalid SSE encoding")?;
            let (field, value) = text.split_once(':').unwrap_or((text, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            if field == "data" {
                if let Some(data) = &mut self.data {
                    data.push(b'\n');
                }
                let data = self.data.get_or_insert_with(Vec::new);
                data.extend_from_slice(value.as_bytes());
            } else {
                if field == "event" {
                    self.event = Some(value.to_owned());
                }
                self.fields.extend_from_slice(&line);
                self.fields.push(b'\n');
            }
        }
        Ok(None)
    }
}

impl SseEvent {
    fn encode(self, data: Option<&[u8]>) -> Vec<u8> {
        let mut bytes = self.fields;
        if let Some(data) = data {
            bytes.extend_from_slice(b"data: ");
            bytes.extend_from_slice(data);
            bytes.push(b'\n');
        }
        bytes.push(b'\n');
        bytes
    }
}

fn valid_chat_chunk(payload: &Value, model: &str) -> bool {
    valid_envelope(payload, model, false)
        && payload
            .get("object")
            .is_none_or(|object| object.as_str() == Some("chat.completion.chunk"))
        && payload
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| {
                (!choices.is_empty() || payload.get("usage").is_some_and(Value::is_object))
                    && choices.iter().all(|choice| {
                        choice.is_object()
                            && choice.get("error").is_none_or(Value::is_null)
                            && choice.get("message").is_none_or(|message| {
                                message.get("error").is_none_or(Value::is_null)
                            })
                            && choice
                                .get("index")
                                .is_none_or(|index| index.as_u64().is_some())
                            && choice
                                .get("finish_reason")
                                .is_none_or(|reason| reason.is_null() || reason.is_string())
                            && choice.get("delta").is_some_and(|delta| {
                                valid_envelope(delta, model, false)
                                    && delta
                                        .get("role")
                                        .is_none_or(|role| role.as_str() == Some("assistant"))
                                    && ["content", "reasoning", "reasoning_content"].iter().all(
                                        |key| {
                                            delta.get(key).is_none_or(|value| {
                                                value.is_null() || value.is_string()
                                            })
                                        },
                                    )
                                    && delta.get("tool_calls").is_none_or(|calls| {
                                        calls.as_array().is_some_and(|calls| {
                                            calls.iter().all(|call| {
                                                call.is_object()
                                                    && call.get("index").is_none_or(|index| {
                                                        index.as_u64().is_some()
                                                    })
                                                    && call.get("type").is_none_or(|kind| {
                                                        kind.as_str() == Some("function")
                                                    })
                                                    && call.get("id").is_none_or(Value::is_string)
                                                    && call.get("function").is_none_or(|function| {
                                                        function.is_object()
                                                            && ["name", "arguments"].iter().all(
                                                                |key| {
                                                                    function.get(key).is_none_or(
                                                                        Value::is_string,
                                                                    )
                                                                },
                                                            )
                                                    })
                                            })
                                        })
                                    })
                            })
                    })
            })
}

fn normalize_event(
    event: SseEvent,
    alias: &str,
    model: &str,
    responses: bool,
    needs_tail: bool,
    saw_completed: &mut bool,
    tools: &mut ChatToolFragments,
) -> Result<(Vec<u8>, bool, bool, bool), &'static str> {
    if matches!(
        event.event.as_deref(),
        Some("error" | "response.error" | "response.failed")
    ) {
        return Err("Upstream SSE error");
    }
    let Some(data) = event.data.as_ref() else {
        return Ok((event.encode(None), false, false, false));
    };
    if data == b"[DONE]" {
        if responses {
            return Err("Responses terminal missing");
        }
        if !tools.complete(model) {
            return Err("Incomplete tool calls");
        }
        return Ok((event.encode(Some(b"[DONE]")), true, false, false));
    }
    let mut value: Value = serde_json::from_slice(data).map_err(|_| "Invalid SSE data")?;
    if !valid_envelope(&value, model, false) {
        return Err("Invalid SSE envelope");
    }
    let mut terminal = false;
    if responses {
        if super::upstream::responses_semantic_error(&value) {
            return Err("Responses semantic error");
        }
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or("Invalid Responses event")?
            .to_owned();
        if !kind.starts_with("response.")
            || matches!(kind.as_str(), "response.failed" | "response.error")
            || event.event.as_ref().is_some_and(|name| name != &kind)
        {
            return Err("Upstream Responses event failed");
        }
        for key in ["item", "part", "message"] {
            if value
                .get(key)
                .and_then(Value::as_object)
                .is_some_and(|object| object.get("error").is_some_and(|e| !e.is_null()))
            {
                return Err("Responses semantic error");
            }
        }
        let completed = matches!(kind.as_str(), "response.completed" | "response.incomplete");
        if completed || kind == "response.done" {
            let response = value
                .get("response")
                .ok_or("Missing terminal Responses object")?;
            let valid_terminal = if kind == "response.done" {
                *saw_completed
                    && valid_envelope(response, model, true)
                    && response.get("object").and_then(Value::as_str) == Some("response")
            } else {
                valid_responses(response, model, true)
                    && response.get("status").and_then(Value::as_str)
                        == Some(if kind == "response.completed" {
                            "completed"
                        } else {
                            "incomplete"
                        })
            };
            if !valid_terminal {
                return Err("Invalid terminal Responses object");
            }
            if kind == "response.done" {
                if !needs_tail || !*saw_completed {
                    return Err("Responses tail out of order");
                }
                terminal = true;
            } else {
                *saw_completed = true;
                terminal = !needs_tail;
            }
        }
        if let Some(response) = value.get_mut("response") {
            if !valid_envelope(response, model, true)
                || response.get("object").and_then(Value::as_str) != Some("response")
            {
                return Err("Invalid Responses object");
            }
            response["model"] = Value::String(alias.to_owned());
        }
        if value.get("delta").is_some_and(|delta| !delta.is_string()) {
            return Err("Invalid Responses delta");
        }
    } else {
        if !valid_chat_chunk(&value, model) {
            return Err("Invalid chat chunk");
        }
        tools.absorb(&value)?;
    }
    crate::audit::observe(&value, crate::audit::meaningful(&value, responses));
    if responses && value.get("type").and_then(Value::as_str) == Some("response.incomplete") {
        crate::audit::backend("incomplete");
    } else if responses && value.get("type").and_then(Value::as_str) == Some("response.completed") {
        crate::audit::backend("complete");
    }
    if value.get("model").is_some() {
        value["model"] = Value::String(alias.to_owned());
    }
    let mut bytes = event.encode(Some(value.to_string().as_bytes()));
    if terminal && needs_tail {
        bytes.extend_from_slice(b"data: [DONE]\n\n");
    }
    let choice_bearing = !responses
        && value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| !choices.is_empty());
    let progress = crate::audit::meaningful(&value, responses).is_some()
        && (!responses
            || value
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.ends_with(".delta")));
    Ok((bytes, terminal, choice_bearing, progress))
}

pub(super) fn validated_sse(
    runtime: Arc<Runtime>,
    route_idx: usize,
    route_permit: OwnedSemaphorePermit,
    class_permit: OwnedSemaphorePermit,
    prepared: PreparedStream,
    alias: String,
    responses: bool,
) -> Body {
    let model = runtime.policy.routes[route_idx].config.model.clone();
    let needs_tail = responses && runtime.policy.routes[route_idx].config.responses_done_tail;
    let stream = async_stream::stream! {
        let mut permits = Some((route_permit, class_permit));
        let mut clock = prepared.clock;
        if let Some(clock) = &mut clock { clock.pause(prepared.handed_off.elapsed()); }
        let mut upstream = Some(Box::pin(prepared.response.bytes_stream()));
        let mut decoder = SseDecoder::default();
        let mut saw_completed = false;
        let mut tools = ChatToolFragments::default();
        let mut saw_chat_choices = false;
        let failure = 'upstream: loop {
            if let Some(failure) = clock.as_ref().and_then(|clock| clock.failure_at(tokio::time::Instant::now())) { break failure; }
            let next = if let Some(clock) = &clock {
                match tokio::time::timeout_at(clock.deadline(), upstream.as_mut().unwrap().next()).await {
                    Ok(next) => next,
                    Err(_) => break clock.failure_at(tokio::time::Instant::now()).unwrap(),
                }
            } else { upstream.as_mut().unwrap().next().await };
            let chunk = match next {
                Some(Ok(chunk)) => chunk,
                Some(Err(error)) => break if error.is_timeout() { StreamFailure::Deadline } else { StreamFailure::Transport },
                None => break StreamFailure::Invalid("Upstream SSE ended without terminal"),
            };
            let mut input = chunk.as_ref();
            loop {
                if let Some(failure) = clock.as_ref().and_then(|clock| clock.failure_at(tokio::time::Instant::now())) { break 'upstream failure; }
                let event = match decoder.next_event(&mut input) {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(error) => break 'upstream StreamFailure::Invalid(error),
                };
                match normalize_event(event, &alias, &model, responses, needs_tail, &mut saw_completed, &mut tools) {
                    Ok((bytes, terminal, choice_bearing, progress)) => {
                        if progress && let Some(clock) = &mut clock { clock.progress(tokio::time::Instant::now()); }
                        if terminal && !responses && !saw_chat_choices {
                            break 'upstream StreamFailure::Invalid("Chat terminal missing valid choices");
                        }
                        saw_chat_choices |= choice_bearing;
                        if terminal {
                            if !responses {crate::audit::backend("complete");}
                            drop(upstream.take());
                            drop(permits.take());
                            mark_success(&runtime, route_idx);
                            runtime.route_completions[route_idx].fetch_add(1, Ordering::Relaxed);
                            yield Ok::<Bytes, io::Error>(Bytes::from(bytes));
                            return;
                        }
                        let suspended = tokio::time::Instant::now();
                        yield Ok(Bytes::from(bytes));
                        if let Some(clock) = &mut clock { clock.pause(suspended.elapsed()); }
                    }
                    Err(error) => break 'upstream StreamFailure::Invalid(error),
                }
            }
        };
        drop(upstream.take());
        drop(permits.take());
        runtime.upstream_errors.fetch_add(1, Ordering::Relaxed);
        crate::audit::backend("failed");
        if !failure.local_timeout() { mark_failure(&runtime, route_idx); }
        warn!(route=%runtime.policy.routes[route_idx].config.name,error_category=failure.category(), "upstream stream failed");
        yield Err(io::Error::other(failure.category()));
    };
    Body::from_stream(stream)
}
