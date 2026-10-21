use super::*;

const MAX_AUDIT_CANDIDATES: usize = 16;
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Eligibility {
    Eligible,
    AliasDisabled,
    CredentialRoute,
    AlreadyTried,
    Disabled,
    Context,
    Streaming,
    Capability,
    PhysicalUnavailable,
    CoolingDown,
    Stale,
    Unreachable,
    NotServing,
    StaleHealth,
    StaleQualification,
    Full,
    FallbackGate,
    RaceFull,
}
impl Eligibility {
    fn from_reason(reason: &str) -> Self {
        match reason {
            "alias_disabled" => Self::AliasDisabled,
            "credential_route" => Self::CredentialRoute,
            "already_tried" => Self::AlreadyTried,
            "disabled" => Self::Disabled,
            "context" => Self::Context,
            "streaming" => Self::Streaming,
            "capability" => Self::Capability,
            "physical_unavailable" => Self::PhysicalUnavailable,
            "cooling_down" => Self::CoolingDown,
            "stale" => Self::Stale,
            "unreachable" => Self::Unreachable,
            "stale_health" => Self::StaleHealth,
            "stale_qualification" => Self::StaleQualification,
            "full" => Self::Full,
            _ => Self::NotServing,
        }
    }
}
#[derive(Clone, Copy)]
enum SelectionReason {
    OnlyEligible,
    OccupancyRank,
    AffinityTie,
    PriorityTie,
    RotationTie,
    NoEligible,
}
impl SelectionReason {
    fn name(self) -> &'static str {
        match self {
            Self::OnlyEligible => "only_eligible",
            Self::OccupancyRank => "occupancy_rank",
            Self::AffinityTie => "affinity_tie",
            Self::PriorityTie => "priority_tie",
            Self::RotationTie => "rotation_tie",
            Self::NoEligible => "no_eligible",
        }
    }
}
#[derive(serde::Serialize)]
struct CandidateSnapshot<'route> {
    #[serde(skip)]
    idx: usize,
    route: &'route str,
    status: &'static str,
    free: usize,
    capacity: usize,
    priority: u32,
    fallback_only: bool,
    preferred: bool,
    eligibility: Eligibility,
}
struct SelectionAudit<'route> {
    candidates: Vec<CandidateSnapshot<'route>>,
    enabled: bool,
    omitted: usize,
}
impl<'route> SelectionAudit<'route> {
    fn new() -> Self {
        Self {
            candidates: Vec::new(),
            enabled: tracing::enabled!(tracing::Level::INFO),
            omitted: 0,
        }
    }
    fn capture(
        &mut self,
        runtime: &'route Runtime,
        idx: usize,
        entry: &config::AliasRoute,
        preferred: Option<usize>,
        eligibility: Eligibility,
    ) {
        if !self.enabled {
            return;
        }
        if self.candidates.len() == MAX_AUDIT_CANDIDATES {
            self.omitted += 1;
            return;
        }
        self.candidates.push(CandidateSnapshot {
            idx,
            route: crate::audit::label(&runtime.policy.routes[idx].config.name),
            status: runtime.observations.lock().unwrap()[idx].status.name(),
            free: runtime.semaphores[idx].available_permits(),
            capacity: runtime.policy.routes[idx].config.safe_concurrency,
            priority: entry.priority,
            fallback_only: entry.fallback_only,
            preferred: preferred == Some(idx),
            eligibility,
        });
    }
    fn update(&mut self, _runtime: &Runtime, idx: usize, eligibility: Eligibility) {
        if let Some(candidate) = self
            .candidates
            .iter_mut()
            .find(|candidate| candidate.idx == idx)
        {
            candidate.eligibility = eligibility;
        }
    }
    fn emit(
        &self,
        runtime: &Runtime,
        alias: &str,
        attempt: usize,
        selected: Option<usize>,
        reason: SelectionReason,
    ) {
        if self.enabled {
            info!(alias=crate::audit::label(alias),attempt,route=selected.map(|idx|crate::audit::label(&runtime.policy.routes[idx].config.name)),reason=reason.name(),candidates=%serde_json::to_string(&self.candidates).expect("bounded diagnostics"),candidates_omitted=self.omitted,policy_generation=runtime.policy_generation,policy_sha256=%runtime.policy_digest,"selection decision");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MissingCapability {
    Text,
    Image,
    Tools,
    JsonSchema,
    JsonObject,
    Responses,
    ResponsesJsonSchema,
    ResponsesJsonObject,
    Streaming,
}

impl MissingCapability {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Tools => "tools",
            Self::JsonSchema => "json_schema",
            Self::JsonObject => "json_object",
            Self::Responses => "responses",
            Self::ResponsesJsonSchema => "responses_json_schema",
            Self::ResponsesJsonObject => "responses_json_object",
            Self::Streaming => "streaming",
        }
    }
}

pub(super) fn missing_capabilities(
    route: &config::Route,
    stream: bool,
    needs: RequestCapabilities,
) -> Vec<MissingCapability> {
    [
        (MissingCapability::Text, true),
        (MissingCapability::Image, needs.image),
        (MissingCapability::Tools, needs.tools),
        (MissingCapability::JsonSchema, needs.json_schema),
        (MissingCapability::JsonObject, needs.json_object),
        (MissingCapability::Responses, needs.responses),
        (
            MissingCapability::ResponsesJsonSchema,
            needs.responses_json_schema,
        ),
        (
            MissingCapability::ResponsesJsonObject,
            needs.responses_json_object,
        ),
        (MissingCapability::Streaming, stream),
    ]
    .into_iter()
    .filter_map(|(capability, required)| {
        let supported = if capability == MissingCapability::Streaming {
            route.streaming
        } else {
            route
                .capabilities
                .iter()
                .any(|name| name == capability.name())
        };
        (required && !supported).then_some(capability)
    })
    .collect()
}

pub(super) fn route_ineligibility(
    runtime: &Runtime,
    route_idx: usize,
    stream: bool,
    required_context: u32,
    needs: RequestCapabilities,
) -> Option<&'static str> {
    let route = &runtime.policy.routes[route_idx].config;
    if !route.enabled {
        return Some("disabled");
    }
    if required_context > route.context_limit {
        return Some("context");
    }
    if stream && !route.streaming {
        return Some("streaming");
    }
    if !missing_capabilities(route, false, needs).is_empty() {
        return Some("capability");
    }
    let observations = runtime.observations.lock().unwrap();
    let obs = &observations[route_idx];
    if !physical_route_ready(runtime, route_idx, &observations) {
        return Some("physical_unavailable");
    }
    if obs.status != RouteStatus::Serving {
        return Some(match obs.status {
            RouteStatus::CoolingDown => "cooling_down",
            RouteStatus::Stale => "stale",
            RouteStatus::Unreachable => "unreachable",
            _ => "not_serving",
        });
    }
    if !obs.checked.is_some_and(|t| {
        t.elapsed() <= Duration::from_secs(runtime.policy.config.refresh_seconds * 3)
    }) {
        return Some("stale_health");
    }
    if !obs
        .qualified
        .is_some_and(|t| t.elapsed() <= Duration::from_secs(900))
    {
        return Some("stale_qualification");
    }
    if runtime.semaphores[route_idx].available_permits() == 0 {
        return Some("full");
    }
    None
}

// Inputs remain explicit to preserve the established route-selection call contract.
#[allow(clippy::too_many_arguments)]
pub(super) fn select_route(
    runtime: &Runtime,
    alias_name: &str,
    alias: &config::Alias,
    stream: bool,
    required_context: u32,
    needs: RequestCapabilities,
    excluded: &[usize],
    affinity_key: Option<&[u8; 32]>,
    allowed_routes: &[String],
) -> Option<(usize, OwnedSemaphorePermit)> {
    // Serialize the utilization snapshot, tie rotation, and physical admission.
    let mut selection = runtime.selection.lock().unwrap();
    let now = Instant::now();
    let lease_duration = Duration::from_secs(runtime.policy.config.affinity_lease_seconds);
    if now.duration_since(selection.last_prune) >= Duration::from_secs(60) {
        let before = selection.leases.len();
        selection
            .leases
            .retain(|_, lease| now.duration_since(lease.last_seen) < lease_duration);
        runtime
            .affinity_expired
            .fetch_add((before - selection.leases.len()) as u64, Ordering::Relaxed);
        selection.last_prune = now;
    }
    let preferred = affinity_key.and_then(|key| {
        let lease = selection.leases.get(key)?;
        if now.duration_since(lease.last_seen) < lease_duration {
            Some(lease.route_idx)
        } else {
            selection.leases.remove(key);
            runtime.affinity_expired.fetch_add(1, Ordering::Relaxed);
            None
        }
    });
    let mut audit = SelectionAudit::new();
    let mut candidates: Vec<_> = alias
        .routes
        .iter()
        .filter_map(|entry| {
            let idx = runtime
                .policy
                .routes
                .iter()
                .position(|r| r.config.name == entry.route)?;
            if !entry.enabled {
                audit.capture(runtime, idx, entry, preferred, Eligibility::AliasDisabled);
                record_skip(runtime, alias_name, idx, "alias_disabled");
                return None;
            }
            if !allowed_routes.is_empty() && !allowed_routes.contains(&entry.route) {
                audit.capture(runtime, idx, entry, preferred, Eligibility::CredentialRoute);
                record_skip(runtime, alias_name, idx, "credential_route");
                return None;
            }
            if excluded.contains(&idx) {
                audit.capture(runtime, idx, entry, preferred, Eligibility::AlreadyTried);
                record_skip(runtime, alias_name, idx, "already_tried");
                return None;
            }
            if let Some(reason) = route_ineligibility(runtime, idx, stream, required_context, needs)
            {
                audit.capture(
                    runtime,
                    idx,
                    entry,
                    preferred,
                    Eligibility::from_reason(reason),
                );
                record_skip(runtime, alias_name, idx, reason);
                if reason == "capability" || reason == "streaming" {
                    for capability in
                        missing_capabilities(&runtime.policy.routes[idx].config, stream, needs)
                    {
                        info!(
                            alias = crate::audit::label(alias_name),
                            route = crate::audit::label(&runtime.policy.routes[idx].config.name),
                            reason,
                            missing_capability = capability.name(),
                            "route missing capability"
                        );
                    }
                }
                return None;
            }
            audit.capture(runtime, idx, entry, preferred, Eligibility::Eligible);
            Some((idx, entry.priority, entry.fallback_only))
        })
        .collect();

    // An existing lease may remain on a fallback route after primary recovery.
    // New assignments still obey the fallback gate.
    let pinned_eligible = preferred.is_some_and(|idx| candidates.iter().any(|c| c.0 == idx));
    if !pinned_eligible && candidates.iter().any(|c| !c.2) {
        for candidate in candidates.iter().filter(|c| c.2) {
            audit.update(runtime, candidate.0, Eligibility::FallbackGate);
            record_skip(runtime, alias_name, candidate.0, "fallback_gate")
        }
        candidates.retain(|c| !c.2);
    }
    // A permit may still be consumed by a probe or released by a completed request.
    // Re-rank after a failed acquisition to use the current admission state.
    while !candidates.is_empty() {
        candidates.sort_by(|a, b| {
            let free_a = runtime.semaphores[a.0].available_permits();
            let free_b = runtime.semaphores[b.0].available_permits();
            let used_a = runtime.policy.routes[a.0].config.safe_concurrency - free_a;
            let used_b = runtime.policy.routes[b.0].config.safe_concurrency - free_b;
            used_a
                .cmp(&used_b)
                .then_with(|| free_b.cmp(&free_a))
                .then_with(|| (preferred != Some(a.0)).cmp(&(preferred != Some(b.0))))
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| selection.last_selected[a.0].cmp(&selection.last_selected[b.0]))
        });
        let candidate = candidates.remove(0);
        let (idx, priority, _) = candidate;
        let reason = candidates
            .first()
            .map_or(SelectionReason::OnlyEligible, |other| {
                let free = runtime.semaphores[idx].available_permits();
                let other_free = runtime.semaphores[other.0].available_permits();
                if free != other_free
                    || runtime.policy.routes[idx].config.safe_concurrency - free
                        != runtime.policy.routes[other.0].config.safe_concurrency - other_free
                {
                    SelectionReason::OccupancyRank
                } else if (preferred == Some(idx)) != (preferred == Some(other.0)) {
                    SelectionReason::AffinityTie
                } else if priority != other.1 {
                    SelectionReason::PriorityTie
                } else {
                    SelectionReason::RotationTie
                }
            });
        if let Ok(permit) = runtime.semaphores[idx].clone().try_acquire_owned() {
            selection.sequence += 1;
            selection.last_selected[idx] = selection.sequence;
            if let Some(key) = affinity_key {
                if !selection.leases.contains_key(key)
                    && selection.leases.len() >= MAX_AFFINITY_LEASES
                    && let Some(oldest) = selection
                        .leases
                        .iter()
                        .min_by_key(|(_, lease)| lease.last_seen)
                        .map(|(key, _)| *key)
                {
                    selection.leases.remove(&oldest);
                }
                let created_at = selection
                    .leases
                    .get(key)
                    .map_or(now, |lease| lease.created_at);
                selection.leases.insert(
                    *key,
                    AffinityLease {
                        route_idx: idx,
                        created_at,
                        last_seen: now,
                    },
                );
                if preferred == Some(idx) {
                    runtime.affinity_hits.fetch_add(1, Ordering::Relaxed);
                } else if preferred.is_some() {
                    runtime.affinity_rebinds.fetch_add(1, Ordering::Relaxed);
                }
            }
            record_attempt(runtime, idx);
            audit.emit(runtime, alias_name, excluded.len() + 1, Some(idx), reason);
            info!(
                alias = crate::audit::label(alias_name),
                route = crate::audit::label(&runtime.policy.routes[idx].config.name),
                priority = priority,
                affinity_hit = preferred == Some(idx),
                "route selected"
            );
            return Some((idx, permit));
        }
        record_skip(runtime, alias_name, idx, "race_full");
        audit.update(runtime, idx, Eligibility::RaceFull);
        if preferred == Some(idx) && candidates.iter().any(|c| !c.2) {
            for candidate in candidates.iter().filter(|c| c.2) {
                audit.update(runtime, candidate.0, Eligibility::FallbackGate);
                record_skip(runtime, alias_name, candidate.0, "fallback_gate");
            }
            candidates.retain(|c| !c.2);
        }
    }
    audit.emit(
        runtime,
        alias_name,
        excluded.len() + 1,
        None,
        SelectionReason::NoEligible,
    );
    None
}
