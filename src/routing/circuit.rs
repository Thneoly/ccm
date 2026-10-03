//! Circuit breaker: the CLOSED/OPEN/HALF_OPEN admission state machine, keyed
//! by model alias.

use crate::history::CircuitTransition;
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

/// Circuit state label for history transitions. At transition instants the
/// (open, probe) pair is unambiguous; an elapsed-but-unprobed OPEN cooldown
/// (`HALF_OPEN_READY` in the control API) is still `OPEN` here because no
/// transition is recorded until the probe is admitted.
fn circuit_state_name(circuit: &CircuitState) -> &'static str {
    match (
        circuit.open_until_ms.is_some(),
        circuit.half_open_probe_in_flight,
    ) {
        (true, true) => "HALF_OPEN",
        (true, false) => "OPEN",
        (false, _) => "CLOSED",
    }
}

/// Record a state transition to the history store (v0.4 M5). Best-effort:
/// a disabled store no-ops.
fn record_transition(state: &ProxyState, model: &str, from: &str, to: &str, reason: &str) {
    state.history.record_circuit_transition(&CircuitTransition {
        timestamp_ms: now_ms(),
        model: model.to_string(),
        from: from.to_string(),
        to: to.to_string(),
        reason: reason.to_string(),
    });
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
            record_transition(
                state,
                model,
                "OPEN",
                "HALF_OPEN",
                "cooldown elapsed; probe admitted",
            );
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
    let from = circuit_state_name(circuit);
    circuit.consecutive_failures += 1;

    if circuit.half_open_probe_in_flight || circuit.consecutive_failures >= policy.failure_threshold
    {
        circuit.open_until_ms = Some(now_ms().saturating_add(policy.open_ms));
        circuit.half_open_probe_in_flight = false;
        // Record only state changes; a failure while already OPEN merely
        // re-arms the cooldown (no transition).
        if from != "OPEN" {
            let reason = if from == "HALF_OPEN" {
                "half-open probe failed"
            } else {
                "failure threshold reached"
            };
            record_transition(state, model, from, "OPEN", reason);
        }
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
    let from = circuit_state_name(circuit);
    circuit.consecutive_failures = 0;
    circuit.open_until_ms = None;
    circuit.half_open_probe_in_flight = false;
    if from != "CLOSED" {
        let reason = if from == "HALF_OPEN" {
            "half-open probe succeeded"
        } else {
            "success reset"
        };
        record_transition(state, model, from, "CLOSED", reason);
    }
}

pub(crate) async fn release_half_open_probe(state: &ProxyState, model: &str) {
    let mut circuits = state.circuits.write().await;
    if let Some(circuit) = circuits.get_mut(model) {
        if circuit.half_open_probe_in_flight {
            circuit.half_open_probe_in_flight = false;
            record_transition(
                state,
                model,
                "HALF_OPEN",
                "OPEN",
                "probe released without a verdict",
            );
        }
    }
}
