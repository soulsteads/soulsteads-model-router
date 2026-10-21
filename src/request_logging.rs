//! Server-owned correlation; caller metadata is never accepted as log identity.
use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use futures_util::StreamExt;
use tracing::{Instrument, info, warn};
#[cfg(test)]
#[path = "audit_timing_tests.rs"]
mod audit_timing_tests;

use std::time::Instant;

struct BodyLifecycle {
    status: u16,
    span: tracing::Span,
    terminal: bool,
    context: crate::audit::Context,
    handler: bool,
}
impl Drop for BodyLifecycle {
    fn drop(&mut self) {
        if !self.terminal {
            let _entered = self.span.enter();
            crate::audit::terminal(
                &self.context,
                self.status,
                if self.handler {
                    "request handler cancelled"
                } else {
                    "request body cancelled"
                },
            );
        }
    }
}

pub(crate) async fn correlate(mut request: Request, next: Next) -> Response {
    let started = Instant::now();
    let endpoint = match request.uri().path() {
        "/v1/chat/completions" => "chat",
        "/v1/responses" => "responses",
        "/v1/embeddings" => "embeddings",
        _ => "other",
    };
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // Never process an uncorrelated request if entropy is unavailable.
        return crate::json_error(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "request_id_unavailable",
            "Request correlation unavailable",
        );
    }
    let request_id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let id = axum::http::HeaderValue::from_str(&request_id).expect("hex ID");
    request.headers_mut().insert("x-request-id", id.clone());
    let span = tracing::info_span!("request", request_id = %request_id, endpoint);
    let context = crate::audit::context(started, endpoint);
    let mut lifecycle = BodyLifecycle {
        status: 0,
        span: span.clone(),
        terminal: false,
        context: context.clone(),
        handler: true,
    };
    let mut response = crate::audit::CURRENT
        .scope(context.clone(), next.run(request))
        .instrument(span.clone())
        .await;
    let status = response.status().as_u16();
    lifecycle.status = status;
    lifecycle.handler = false;
    crate::audit::headers(&context, started.elapsed().as_millis() as u64);
    async {
        if status >= 400 {
            warn!(
                status,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "request rejected or failed"
            );
        } else {
            info!(
                status,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "request response headers"
            );
        }
    }
    .instrument(span.clone())
    .await;
    response.headers_mut().insert("x-request-id", id);
    let (parts, body) = response.into_parts();
    // SSE is consumed after the handler future returns. Instrument each poll,
    // not just response construction, so stream and route-failure logs retain ID.
    let stream = async_stream::stream! {
        let mut lifecycle = lifecycle;
        let mut upstream = body.into_data_stream();
        while let Some(chunk) = crate::audit::CURRENT.scope(context.clone(),upstream.next()).instrument(span.clone()).await {
            match chunk {
                Ok(bytes) => yield Ok::<_, axum::Error>(bytes),
                Err(error) => {
                    lifecycle.terminal = true;
                    async { crate::audit::terminal(&context,status,"request stream failed"); }
                        .instrument(span.clone()).await;
                    yield Err(error);
                    return;
                }
            }
        }
        lifecycle.terminal = true;
        async { crate::audit::terminal(&context,status,"request body completed"); }.instrument(span).await;
    };
    Response::from_parts(parts, Body::from_stream(stream))
}
