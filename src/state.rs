use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RouteStatus {
    Unknown,
    Unreachable,
    Reachable,
    Advertised,
    Ready,
    Serving,
    Stale,
    CoolingDown,
}
#[derive(Clone, Copy, Default)]
pub(crate) struct RequestCapabilities {
    pub(crate) image: bool,
    pub(crate) tools: bool,
    pub(crate) json_schema: bool,
    pub(crate) json_object: bool,
    pub(crate) responses: bool,
    pub(crate) responses_json_schema: bool,
    pub(crate) responses_json_object: bool,
}
impl RouteStatus {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Unreachable => "unreachable",
            Self::Reachable => "reachable",
            Self::Advertised => "advertised",
            Self::Ready => "ready",
            Self::Serving => "serving",
            Self::Stale => "stale",
            Self::CoolingDown => "cooling_down",
        }
    }
}
pub(crate) struct RouteObservation {
    pub(crate) status: RouteStatus,
    pub(crate) checked: Option<Instant>,
    pub(crate) served: Option<Instant>,
    pub(crate) qualified: Option<Instant>,
    pub(crate) last_successful_qualification: Option<Instant>,
    pub(crate) cooldown_until: Option<Instant>,
    pub(crate) failures: u32,
    pub(crate) model_present: bool,
    pub(crate) last_probe: Option<ProbeDiagnostic>,
    pub(crate) last_failed_probe: Option<ProbeDiagnostic>,
    pub(crate) invalidation_epoch: u64,
}
#[derive(Clone, Copy)]
pub(crate) struct ProbeDiagnostic {
    pub(crate) stage: &'static str,
    pub(crate) outcome: &'static str,
    pub(crate) http_status: Option<u16>,
    pub(crate) latency_ms: u64,
    pub(crate) at: Instant,
}
pub(crate) struct SelectionState {
    pub(crate) sequence: u64,
    pub(crate) last_selected: Vec<u64>,
    pub(crate) leases: HashMap<[u8; 32], AffinityLease>,
    pub(crate) last_prune: Instant,
}
#[derive(Clone, Copy)]
pub(crate) struct AffinityLease {
    pub(crate) route_idx: usize,
    pub(crate) created_at: Instant,
    pub(crate) last_seen: Instant,
}
pub(crate) struct RateState {
    pub(crate) tokens: f64,
    pub(crate) last: Instant,
}
pub(crate) struct CredentialState {
    pub(crate) config: config::Credential,
    pub(crate) class_semaphore: Arc<Semaphore>,
    pub(crate) rate: Mutex<RateState>,
}
impl Deref for CredentialState {
    type Target = config::Credential;
    fn deref(&self) -> &Self::Target {
        &self.config
    }
}
pub(crate) struct AuthState {
    pub(crate) snapshot: Option<Arc<key_manager::AuthSnapshot>>,
    pub(crate) credentials: HashMap<String, Arc<CredentialState>>,
}
pub(crate) struct Runtime {
    pub(crate) policy_digest: String,
    pub(crate) policy_generation: u64,
    pub(crate) audit_metrics: Arc<crate::audit::Metrics>,
    pub(crate) policy: Policy,
    pub(crate) auth: Arc<RwLock<Arc<AuthState>>>,
    pub(crate) auth_limiter: Arc<Semaphore>,
    pub(crate) client: reqwest::Client,
    pub(crate) probes: reqwest::Client,
    pub(crate) codex_semaphore: Arc<Semaphore>,
    pub(crate) codex_client: reqwest::Client,
    pub(crate) semaphores: Vec<Arc<Semaphore>>,
    pub(crate) qualification_locks: Vec<Arc<tokio::sync::Mutex<()>>>,
    pub(crate) observations: Arc<Mutex<Vec<RouteObservation>>>,
    pub(crate) backend_telemetry: Arc<Mutex<Vec<crate::backend_telemetry::Track>>>,
    pub(crate) selection: Arc<Mutex<SelectionState>>,
    pub(crate) requests: Arc<AtomicU64>,
    pub(crate) rejects: Arc<AtomicU64>,
    pub(crate) upstream_errors: Arc<AtomicU64>,
    pub(crate) route_attempts: Vec<Arc<AtomicU64>>,
    pub(crate) route_completions: Vec<Arc<AtomicU64>>,
    pub(crate) route_failure_events: Vec<Arc<AtomicU64>>,
    pub(crate) route_skips: Arc<Mutex<HashMap<(usize, &'static str), u64>>>,
    pub(crate) recent_routes: Arc<Mutex<VecDeque<(Instant, usize)>>>,
    pub(crate) affinity_hits: Arc<AtomicU64>,
    pub(crate) affinity_rebinds: Arc<AtomicU64>,
    pub(crate) affinity_expired: Arc<AtomicU64>,
}

pub(crate) struct AppState {
    pub(crate) current: RwLock<Arc<Runtime>>,
    pub(crate) reload_lock: tokio::sync::Mutex<()>,
    pub(crate) path: PathBuf,
}

impl AppState {
    pub(crate) fn new(path: PathBuf, policy: Policy) -> Result<Self, String> {
        Ok(Self {
            current: RwLock::new(make_runtime(policy, None)?),
            reload_lock: tokio::sync::Mutex::new(()),
            path,
        })
    }

    pub(crate) fn current(&self) -> Arc<Runtime> {
        self.current.read().unwrap().clone()
    }
}
