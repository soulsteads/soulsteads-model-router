use super::{CopilotEndpoint, deadline};
use crate::sse::{ProgressClock, SseDecoder};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::{
    sync::{OwnedSemaphorePermit, mpsc},
    task::JoinHandle,
    time::Instant,
};

struct State {
    permit: Option<OwnedSemaphorePermit>,
    failure: Option<&'static str>,
}

struct Capacity(Arc<Mutex<State>>);

impl Drop for Capacity {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .permit
            .take();
    }
}

struct ForwardedStream {
    receiver: mpsc::Receiver<Bytes>,
    producer: JoinHandle<()>,
    capacity: Capacity,
    hard: Instant,
    finished: bool,
}

impl Stream for ForwardedStream {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        let mut state = this
            .capacity
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let failure = state
            .failure
            .or_else(|| (Instant::now() >= this.hard).then_some("deadline_exceeded"));
        if let Some(failure) = failure {
            this.finished = true;
            this.producer.abort();
            state.permit.take();
            this.receiver.close();
            while this.receiver.try_recv().is_ok() {}
            return Poll::Ready(Some(Err(io::Error::other(failure))));
        }
        match this.receiver.poll_recv(context) {
            Poll::Ready(None) => {
                this.finished = true;
                Poll::Ready(None)
            }
            result => result.map(|bytes| bytes.map(Ok)),
        }
    }
}

impl Drop for ForwardedStream {
    fn drop(&mut self) {
        self.producer.abort();
    }
}

pub(super) fn forward(
    upstream: reqwest::Response,
    permit: OwnedSemaphorePermit,
    clock: ProgressClock,
    endpoint: CopilotEndpoint,
    sse: bool,
) -> impl Stream<Item = io::Result<Bytes>> + Send + 'static {
    let hard = clock.hard_deadline();
    let state = Arc::new(Mutex::new(State {
        permit: Some(permit),
        failure: None,
    }));
    let capacity = Capacity(state.clone());
    let producer_capacity = Capacity(state);
    let (sender, receiver) = mpsc::channel(1);
    let producer = tokio::spawn(async move {
        let result = produce(upstream, &sender, clock, endpoint, sse).await;
        let mut state = producer_capacity
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.permit.take();
        state.failure = result.err();
        drop(state);
        drop(sender);
    });
    ForwardedStream {
        receiver,
        producer,
        capacity,
        hard,
        finished: false,
    }
}

async fn produce(
    upstream: reqwest::Response,
    sender: &mpsc::Sender<Bytes>,
    mut clock: ProgressClock,
    endpoint: CopilotEndpoint,
    sse: bool,
) -> Result<(), &'static str> {
    let mut body = Box::pin(upstream.bytes_stream());
    let mut decoder = SseDecoder::default();
    loop {
        let suspended = Instant::now();
        let slot = match tokio::time::timeout_at(clock.hard_deadline(), sender.reserve()).await {
            Ok(Ok(slot)) => slot,
            Ok(Err(_)) => return Ok(()),
            Err(_) => return Err("deadline_exceeded"),
        };
        clock.pause(suspended.elapsed());
        let next = tokio::select! {
            biased;
            _ = sender.closed() => return Ok(()),
            next = deadline::wait(&clock, body.next()) => next?,
        };
        let bytes = match next {
            Some(Ok(bytes)) => bytes,
            Some(Err(error)) => {
                return Err(if error.is_timeout() {
                    "deadline_exceeded"
                } else {
                    "Copilot response stream failed"
                });
            }
            None => return Ok(()),
        };
        if sse {
            let mut input = bytes.as_ref();
            while let Some(event) = decoder
                .next_event(&mut input)
                .map_err(|_| "Copilot response stream failed")?
            {
                if event
                    .data
                    .as_deref()
                    .and_then(|data| serde_json::from_slice::<Value>(data).ok())
                    .is_some_and(|value| {
                        crate::audit::meaningful(&value, endpoint == CopilotEndpoint::Responses)
                            .is_some()
                    })
                {
                    clock.progress(Instant::now());
                }
            }
        }
        slot.send(bytes);
    }
}
