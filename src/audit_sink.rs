use std::{
    fmt,
    io::{self, Write},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
};
use tracing_subscriber::{
    fmt::{
        FmtContext, FormatEvent, FormatFields,
        format::{Format, Json, Writer},
    },
    registry::LookupSpan,
};

pub(crate) const RECORD_BYTES: usize = 8192;
pub(crate) const QUEUE_RECORDS: usize = 256;
pub(crate) static COUNTERS: Counters = Counters::new();

pub(crate) struct Counters {
    pub(crate) full: AtomicU64,
    pub(crate) oversize: AtomicU64,
    pub(crate) errors: AtomicU64,
    pub(crate) disconnected: AtomicU64,
    pub(crate) written: AtomicU64,
    pub(crate) abandoned: AtomicU64,
}
impl Counters {
    const fn new() -> Self {
        Self {
            full: AtomicU64::new(0),
            oversize: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            disconnected: AtomicU64::new(0),
            written: AtomicU64::new(0),
            abandoned: AtomicU64::new(0),
        }
    }
    pub(crate) fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({"queue_full": self.full.load(Ordering::Relaxed), "format_dropped": self.oversize.load(Ordering::Relaxed), "sink_errors": self.errors.load(Ordering::Relaxed), "disconnected": self.disconnected.load(Ordering::Relaxed), "written": self.written.load(Ordering::Relaxed), "abandoned":self.abandoned.load(Ordering::Relaxed), "queue_records":QUEUE_RECORDS, "record_bytes":RECORD_BYTES})
    }
}

struct Record {
    bytes: [u8; RECORD_BYTES],
    length: usize,
    pending: Option<&'static Counters>,
}
impl Default for Record {
    fn default() -> Self {
        Self {
            bytes: [0; RECORD_BYTES],
            length: 0,
            pending: None,
        }
    }
}
impl Drop for Record {
    fn drop(&mut self) {
        if let Some(counters) = self.pending {
            counters.abandoned.fetch_add(1, Ordering::Relaxed);
        }
    }
}
impl fmt::Write for Record {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if text.len() > RECORD_BYTES - self.length {
            return Err(fmt::Error);
        }
        self.bytes[self.length..self.length + text.len()].copy_from_slice(text.as_bytes());
        self.length += text.len();
        Ok(())
    }
}

pub(crate) struct BoundedFormat(Format<Json>);
pub(crate) fn formatter() -> BoundedFormat {
    BoundedFormat(tracing_subscriber::fmt::format().json())
}
impl<S, N> FormatEvent<S, N> for BoundedFormat
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        context: &FmtContext<'_, S, N>,
        mut output: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        let mut record = Record::default();
        if self
            .0
            .format_event(context, Writer::new(&mut record), event)
            .is_err()
        {
            COUNTERS.oversize.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        output
            .write_str(std::str::from_utf8(&record.bytes[..record.length]).map_err(|_| fmt::Error)?)
    }
}

#[derive(Clone)]
pub(crate) struct Sink {
    sender: SyncSender<Box<Record>>,
    counters: &'static Counters,
}
pub(crate) struct RecordWriter {
    sink: Sink,
    record: Option<Box<Record>>,
    invalid: bool,
}
impl Sink {
    pub(crate) fn start<W: Write + Send + 'static>(output: W) -> io::Result<Self> {
        Self::start_with(output, &COUNTERS)
    }
    fn start_with<W: Write + Send + 'static>(
        mut output: W,
        counters: &'static Counters,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Box<Record>>(QUEUE_RECORDS);
        std::thread::Builder::new()
            .name("slotline-audit".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    while let Ok(mut record) = receiver.recv() {
                        output.write_all(&record.bytes[..record.length])?;
                        output.flush()?;
                        record.pending = None;
                        counters.written.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok::<_, io::Error>(())
                }));
                drop(receiver);
                if !matches!(result, Ok(Ok(()))) {
                    counters.errors.fetch_add(1, Ordering::Release);
                }
            })?;
        Ok(Self { sender, counters })
    }
    fn writer(&self) -> RecordWriter {
        RecordWriter {
            sink: self.clone(),
            record: Some(Box::new(Record::default())),
            invalid: false,
        }
    }
}
impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for Sink {
    type Writer = RecordWriter;
    fn make_writer(&'writer self) -> Self::Writer {
        self.writer()
    }
}
impl Write for RecordWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let record = self.record.as_mut().unwrap();
        if bytes.len() > RECORD_BYTES - record.length {
            self.invalid = true;
        }
        if !self.invalid {
            record.bytes[record.length..record.length + bytes.len()].copy_from_slice(bytes);
            record.length += bytes.len();
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for RecordWriter {
    fn drop(&mut self) {
        let mut record = self.record.take().unwrap();
        if self.invalid {
            self.sink.counters.oversize.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if record.length == 0 {
            return;
        }
        if record.bytes[record.length - 1] != b'\n' {
            self.sink.counters.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        record.pending = Some(self.sink.counters);
        match self.sink.sender.try_send(record) {
            Ok(()) => {}
            Err(TrySendError::Full(mut record)) => {
                record.pending = None;
                self.sink.counters.full.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(mut record)) => {
                record.pending = None;
                self.sink
                    .counters
                    .disconnected
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };

    #[test]
    fn audit_sink_accounts_for_failed_and_queued_abandonment() {
        struct BlockedFailure {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            panic: bool,
        }
        impl Write for BlockedFailure {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
                if self.panic {
                    panic!("synthetic queued failure");
                }
                Err(io::Error::other("synthetic queued failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        for panic in [false, true] {
            let counters = Box::leak(Box::new(Counters::new()));
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let sink = Sink::start_with(
                BlockedFailure {
                    entered: entered_tx,
                    release: release_rx,
                    panic,
                },
                counters,
            )
            .unwrap();
            sink.writer().write_all(b"{}\n").unwrap();
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            for _ in 0..7 {
                sink.writer().write_all(b"{}\n").unwrap();
            }
            release_tx.send(()).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while counters.errors.load(Ordering::Acquire) == 0 {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert_eq!(counters.snapshot()["abandoned"], 8);
            assert_eq!(counters.written.load(Ordering::Relaxed), 0);
            sink.writer().write_all(b"{}\n").unwrap();
            assert_eq!(counters.disconnected.load(Ordering::Relaxed), 1);
            assert_eq!(counters.snapshot()["abandoned"], 8);
        }
    }

    #[derive(Clone)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn audit_sink_format_bounds_drop_whole_record() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = Buffer(bytes.clone());
        let subscriber = tracing_subscriber::fmt()
            .json()
            .event_format(formatter())
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(large = %"x".repeat(RECORD_BYTES * 2), "oversized");
            tracing::info!("intact");
        });
        let records = bytes.lock().unwrap();
        let records = std::str::from_utf8(&records).unwrap();
        assert_eq!(
            records.lines().count(),
            1,
            "oversized record must be dropped, not truncated"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(records).unwrap()["fields"]["message"],
            "intact"
        );
    }

    #[test]
    fn audit_sink_slow_full_never_waits_and_records_are_atomic() {
        let counters = Box::leak(Box::new(Counters::new()));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        struct Slow {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            buffer: Buffer,
            first: bool,
        }
        impl Write for Slow {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.first {
                    self.first = false;
                    self.entered.send(()).unwrap();
                    self.release.recv().unwrap();
                }
                self.buffer.write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let buffer = Buffer(Arc::new(Mutex::new(Vec::new())));
        let sink = Sink::start_with(
            Slow {
                entered: entered_tx,
                release: release_rx,
                buffer: buffer.clone(),
                first: true,
            },
            counters,
        )
        .unwrap();
        let send = || {
            let mut writer = sink.writer();
            writer.write_all(b"{\"message\":").unwrap();
            writer.write_all(b"\"intact\"}\n").unwrap();
        };
        send();
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        for _ in 0..QUEUE_RECORDS {
            send();
        }
        let started = std::time::Instant::now();
        for _ in 0..10000 {
            send();
        }
        let saturated_elapsed = started.elapsed();
        assert_eq!(counters.full.load(Ordering::Relaxed), 10000);
        assert!(saturated_elapsed < std::time::Duration::from_secs(2));
        println!(
            "AUDIT_QUEUE_SATURATION={}",
            serde_json::json!({"records":10000,"elapsed_ns":saturated_elapsed.as_nanos(),"ns_per_record":saturated_elapsed.as_nanos() as f64 / 10000.0,"queue_full":10000,"sink_blocked":true})
        );
        release_tx.send(()).unwrap();
        drop(sink);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while counters.written.load(Ordering::Relaxed) < (QUEUE_RECORDS + 1) as u64 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let records = buffer.0.lock().unwrap();
        for line in std::str::from_utf8(&records).unwrap().lines() {
            assert!(serde_json::from_str::<serde_json::Value>(line).is_ok());
        }
    }

    #[test]
    fn audit_sink_error_and_panic_disconnect_without_inference_panic() {
        struct Failing(bool);
        impl Write for Failing {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                if self.0 {
                    panic!("synthetic sink panic");
                }
                Err(io::Error::other("synthetic failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        for panic in [false, true] {
            let counters = Box::leak(Box::new(Counters::new()));
            let sink = Sink::start_with(Failing(panic), counters).unwrap();
            sink.writer().write_all(b"{}\n").unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while counters.errors.load(Ordering::Relaxed) == 0 {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            sink.writer().write_all(b"{}\n").unwrap();
            assert_eq!(counters.disconnected.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    #[ignore = "synthetic overhead exercise; run explicitly in release mode"]
    fn audit_sink_release_overhead_measurement() {
        use tracing_subscriber::util::SubscriberInitExt;
        const SAMPLES: usize = 7;
        const EVENTS: usize = 20000;
        #[derive(Clone)]
        struct CountBytes(Arc<AtomicU64>);
        impl Write for CountBytes {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        struct Blocked {
            bytes: CountBytes,
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            first: bool,
        }
        impl Write for Blocked {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.first {
                    self.first = false;
                    self.entered.send(()).unwrap();
                    self.release.recv().unwrap();
                }
                self.bytes.write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut modes = Vec::new();
        for mode in [
            "disabled",
            "synchronous_json_null",
            "bounded_json_null",
            "bounded_async_null",
            "bounded_async_saturated",
        ] {
            let mut samples = Vec::new();
            for _ in 0..SAMPLES {
                let bytes = Arc::new(AtomicU64::new(0));
                let counters = Box::leak(Box::new(Counters::new()));
                let output = CountBytes(bytes.clone());
                let (entered_tx, entered_rx) = mpsc::channel();
                let (release_tx, release_rx) = mpsc::channel();
                let subscriber: Box<dyn tracing::Subscriber + Send + Sync> = match mode {
                    "disabled" => Box::new(tracing::subscriber::NoSubscriber::default()),
                    "synchronous_json_null" => Box::new(
                        tracing_subscriber::fmt()
                            .json()
                            .with_writer(move || output.clone())
                            .finish(),
                    ),
                    "bounded_json_null" => Box::new(
                        tracing_subscriber::fmt()
                            .json()
                            .event_format(formatter())
                            .with_writer(move || output.clone())
                            .finish(),
                    ),
                    "bounded_async_saturated" => Box::new(
                        tracing_subscriber::fmt()
                            .json()
                            .event_format(formatter())
                            .with_writer(
                                Sink::start_with(
                                    Blocked {
                                        bytes: output,
                                        entered: entered_tx,
                                        release: release_rx,
                                        first: true,
                                    },
                                    counters,
                                )
                                .unwrap(),
                            )
                            .finish(),
                    ),
                    _ => Box::new(
                        tracing_subscriber::fmt()
                            .json()
                            .event_format(formatter())
                            .with_writer(Sink::start_with(output, counters).unwrap())
                            .finish(),
                    ),
                };
                let elapsed;
                {
                    let _guard = subscriber.set_default();
                    let span = tracing::info_span!(
                        "request",
                        request_id = "synthetic-benchmark",
                        endpoint = "chat"
                    );
                    let _entered = span.enter();
                    if mode == "bounded_async_saturated" {
                        tracing::info!("block synthetic sink");
                        entered_rx
                            .recv_timeout(std::time::Duration::from_secs(2))
                            .unwrap();
                    }
                    for _ in 0..1000 {
                        tracing::info!(
                            route = "local",
                            class = "interactive",
                            elapsed_ms = 12,
                            outcome = "success_first_attempt",
                            "request body completed"
                        );
                    }
                    let started = std::time::Instant::now();
                    for _ in 0..EVENTS {
                        tracing::info!(
                            route = "local",
                            class = "interactive",
                            elapsed_ms = 12,
                            outcome = "success_first_attempt",
                            "request body completed"
                        );
                    }
                    elapsed = started.elapsed();
                }
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                if mode == "bounded_async_saturated" {
                    release_tx.send(()).unwrap();
                }
                if mode.starts_with("bounded_async") {
                    while counters.written.load(Ordering::Relaxed)
                        + counters.full.load(Ordering::Relaxed)
                        < (EVENTS + 1000 + usize::from(mode == "bounded_async_saturated")) as u64
                    {
                        assert!(std::time::Instant::now() < deadline);
                        std::thread::yield_now();
                    }
                }
                if mode == "bounded_async_saturated" {
                    assert_eq!(
                        counters.full.load(Ordering::Relaxed),
                        (EVENTS + 1000 - QUEUE_RECORDS) as u64
                    );
                }
                samples.push(serde_json::json!({"ns_per_event":elapsed.as_nanos() as f64 / EVENTS as f64,"bytes_including_warmup":bytes.load(Ordering::Relaxed),"sink":counters.snapshot()}));
            }
            modes.push(serde_json::json!({"mode":mode,"samples":samples}));
        }
        println!(
            "AUDIT_SINK_BENCHMARK={}",
            serde_json::json!({"events_per_sample":EVENTS,"warmup_events":1000,"samples_per_mode":SAMPLES,"modes":modes,"build_source_sha256":crate::audit_identity::BUILD,"boundary":"producer formatting/enqueue to synthetic null writer; not journal durability or inference"})
        );
    }
}
