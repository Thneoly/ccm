//! Circuit breaker: the CLOSED/OPEN/HALF_OPEN admission state machine, keyed
//! by model alias.

use crate::proxy::ProxyState;
use crate::route::CircuitBreakerPolicy;
use crate::routing::decision::now_ms;

#[derive(Clone, Default)]
pub(crate) struct CircuitState {
    pub(crate) consecutive_failures: usize,
    pub(crate) open_until_ms: Option<u64>,
    pub(crate) half_open_probe_in_flight: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum CircuitDecision {
    Closed,
    HalfOpen,
    SkipOpen(u64),
    SkipHalfOpen,
}

pub(crate) fn circuit_decision_name(decision: CircuitDecision) -> String {
    match decision {
        CircuitDecision::Closed => "CLOSED".to_string(),
        CircuitDecision::HalfOpen => "HALF_OPEN".to_string(),
        CircuitDecision::SkipOpen(until) => format!("OPEN until {until}"),
        CircuitDecision::SkipHalfOpen => "HALF_OPEN probe in flight".to_string(),
    }
}

pub(crate) async fn circuit_admit(
    state: &ProxyState,
    model: &str,
    policy: &CircuitBreakerPolicy,
) -> CircuitDecision {
    if !policy.enabled {
        return CircuitDecision::Closed;
    }

    let now = now_ms();
    let mut circuits = state.circuits.write().await;
    let circuit = circuits.entry(model.to_string()).or_default();

    match circuit.open_until_ms {
        Some(until) if until > now => CircuitDecision::SkipOpen(until),
        Some(_) if circuit.half_open_probe_in_flight => CircuitDecision::SkipHalfOpen,
        Some(_) => {
            circuit.half_open_probe_in_flight = true;
            CircuitDecision::HalfOpen
        }
        None => CircuitDecision::Closed,
    }
}

pub(crate) async fn circuit_failure(
    state: &ProxyState,
    model: &str,
    policy: &CircuitBreakerPolicy,
) {
    if !policy.enabled {
        return;
    }

    let mut circuits = state.circuits.write().await;
    let circuit = circuits.entry(model.to_string()).or_default();
    circuit.consecutive_failures += 1;

    if circuit.half_open_probe_in_flight || circuit.consecutive_failures >= policy.failure_threshold
    {
        circuit.open_until_ms = Some(now_ms().saturating_add(policy.open_ms));
        circuit.half_open_probe_in_flight = false;
    }
}

pub(crate) async fn circuit_success(
    state: &ProxyState,
    model: &str,
    policy: &CircuitBreakerPolicy,
) {
    if !policy.enabled {
        return;
    }

    let mut circuits = state.circuits.write().await;
    let circuit = circuits.entry(model.to_string()).or_default();
    circuit.consecutive_failures = 0;
    circuit.open_until_ms = None;
    circuit.half_open_probe_in_flight = false;
}

pub(crate) async fn release_half_open_probe(state: &ProxyState, model: &str) {
    let mut circuits = state.circuits.write().await;
    if let Some(circuit) = circuits.get_mut(model) {
        circuit.half_open_probe_in_flight = false;
    }
}
