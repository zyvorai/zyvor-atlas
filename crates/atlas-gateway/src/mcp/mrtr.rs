// Copyright (c) 2026 ZyvorAI Labs Private Limited.
// SPDX-License-Identifier: Apache-2.0
//! The write confirm flow on MCP's multi round-trip requests (MRTR, spec 2026-07-28), at the one
//! `call_tool` chokepoint (`guard.rs`), with no change to any tool.
//!
//! Every guarded write tool is two-phase (`write.rs`): a call without a `confirmation_token` returns
//! `{executed: false, plan, confirmation_token, ...}`. For a client that negotiated 2026-07-28 **and**
//! declared the `elicitation` capability, that plan is turned into an `input_required` result instead:
//! one form-mode elicitation ("confirm", a boolean) carrying a readable summary of the plan, and the
//! existing signed, single-use confirmation token as `requestState`. The client asks its user, then
//! retries the same call with `inputResponses` and the echoed `requestState`:
//!
//! * `accept` with `confirm: true`: the token is put into the call's `confirmation_token` argument and
//!   the call is dispatched like any other, so every existing check applies unchanged (signature, expiry,
//!   the caller, the tool, the exact arguments, single use).
//! * `decline`, `cancel` or `confirm: false`: a plain, non-error result; nothing is executed or consumed.
//! * a retry without a usable answer: treated as a fresh call, which plans again and asks again (the spec's
//!   "respond with a new `InputRequiredResult`").
//!
//! Any other client keeps the `confirmation_token` argument flow unchanged: a legacy `initialize`
//! session, a 2026-07-28 client that did not declare elicitation (a server MUST NOT send
//! `inputRequests` the client did not declare), and a client that declared only URL mode. An empty
//! `elicitation: {}` counts as form mode, as the spec defines.
//!
//! `requestState` is attacker-controlled input and decides authorization here, so it is the signed
//! token itself and is never trusted: it is verified when redeemed (HS256 over a key derived from
//! `ATLAS_JWT_SECRET`; subject = the authenticated actor, short expiry, tool name and the exact
//! arguments), and its single use is enforced in the database (`mcp_confirm_used`).

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ElicitRequest, ElicitRequestParams, ElicitResult,
    ElicitationAction, ElicitationSchema, ErrorData as McpError, InputRequest, InputRequests,
    InputRequiredResult, ProtocolVersion, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::{json, Value};

/// Key of the one input request, and of its answer in `inputResponses`.
const CONFIRM_KEY: &str = "confirm";
/// The argument every guarded write tool takes to execute.
const TOKEN_ARG: &str = "confirmation_token";
/// Longest elicitation message; a plan is a handful of short fields.
const MAX_MESSAGE_CHARS: usize = 1500;
const MAX_VALUE_CHARS: usize = 200;

/// Whether this client can be asked to confirm through MRTR: protocol 2026-07-28 or newer, and form-mode
/// elicitation declared (an empty `elicitation` object means form mode).
pub(super) fn client_can_confirm(context: &RequestContext<RoleServer>) -> bool {
    context.protocol_version().is_some_and(|v| v >= ProtocolVersion::V_2026_07_28)
        && context
            .client_capabilities()
            .and_then(|c| c.elicitation)
            .is_some_and(|e| e.form.is_some() || e.url.is_none())
}

/// A guarded write tool is one whose input schema has a `confirmation_token` property.
pub(super) fn is_confirm_tool(tool: &Tool) -> bool {
    tool.input_schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|p| p.contains_key(TOKEN_ARG))
}

/// What to do with a call that may be the retry of an earlier `input_required` round.
pub(super) enum Retry {
    /// Dispatch the call (with the token injected on an accepted retry, or as it was otherwise).
    Proceed,
    /// The user declined or cancelled: answer without dispatching.
    Refused(&'static str),
}

/// Take `requestState` and `inputResponses` off the call (they never travel further) and decide what
/// the retry means. Only called for a client that [`client_can_confirm`] and a tool that
/// [`is_confirm_tool`].
pub(super) fn take_retry(request: &mut CallToolRequestParams) -> Result<Retry, McpError> {
    let state = request.request_state.take();
    let responses = request.input_responses.take();
    let (Some(state), Some(responses)) = (state, responses) else {
        // Not a retry, or one without an answer: a fresh call plans again and asks again.
        return Ok(Retry::Proceed);
    };
    if request.arguments.as_ref().is_some_and(|a| a.contains_key(TOKEN_ARG)) {
        return Err(McpError::invalid_params(
            "send either requestState with inputResponses or a confirmation_token argument, not both",
            None,
        ));
    }
    let answer = responses
        .get(CONFIRM_KEY)
        .and_then(|v| serde_json::from_value::<ElicitResult>(v.clone()).ok());
    match answer {
        Some(a) if a.action == ElicitationAction::Accept => {
            let confirmed = a.content.as_ref().and_then(|c| c.get(CONFIRM_KEY)).and_then(Value::as_bool);
            if confirmed == Some(true) {
                request
                    .arguments
                    .get_or_insert_with(Default::default)
                    .insert(TOKEN_ARG.into(), Value::String(state));
                Ok(Retry::Proceed)
            } else {
                Ok(Retry::Refused("declined"))
            }
        }
        Some(a) if a.action == ElicitationAction::Decline => Ok(Retry::Refused("declined")),
        Some(_) => Ok(Retry::Refused("cancelled")),
        None => Ok(Retry::Proceed),
    }
}

/// The result for a declined or cancelled confirmation: not an error, nothing was executed.
pub(super) fn refused_result(tool: &str, outcome: &str) -> CallToolResult {
    CallToolResult::structured(json!({
        "executed": false,
        "outcome": outcome,
        "message": format!("{tool} was {outcome} and not executed; nothing changed."),
    }))
}

/// Turn a plan result (`executed: false` plus a `confirmation_token`) into an `input_required` result, or
/// `None` when the result is anything else.
pub(super) fn plan_to_input_required(result: &CallToolResult) -> Option<InputRequiredResult> {
    if result.is_error == Some(true) {
        return None;
    }
    let plan: Value = match &result.structured_content {
        Some(v) => v.clone(),
        None => serde_json::from_str(&result.content.first()?.as_text()?.text).ok()?,
    };
    if plan.get("executed") != Some(&Value::Bool(false)) {
        return None;
    }
    let token = plan.get(TOKEN_ARG)?.as_str()?.to_string();
    let schema = ElicitationSchema::builder()
        .required_bool_with(CONFIRM_KEY, |b| {
            b.title("Confirm").description("true to run this action, false to leave everything unchanged")
        })
        .build()
        .ok()?;
    let request = ElicitRequest::new(ElicitRequestParams::FormElicitationParams {
        meta: None,
        message: summary(&plan),
        requested_schema: schema,
    });
    let mut requests = InputRequests::new();
    requests.insert(CONFIRM_KEY.to_string(), InputRequest::Elicitation(request));
    Some(InputRequiredResult::new(Some(requests), Some(token)))
}

/// A readable summary of a plan for the user who is asked to confirm. The token is not in it.
fn summary(plan: &Value) -> String {
    let body = plan.get("plan").and_then(Value::as_object);
    let action = body.and_then(|p| p.get("action")).and_then(Value::as_str).unwrap_or("this action");
    let mut out = format!("Atlas asks you to confirm: {action}\n");
    for (k, v) in body.into_iter().flatten().filter(|(k, _)| k.as_str() != "action") {
        let text = match v {
            Value::Null => continue,
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let text: String = text.chars().take(MAX_VALUE_CHARS).collect();
        out.push_str(&format!("{k}: {text}\n"));
    }
    out.push_str("Accept to run it; decline or cancel changes nothing.");
    out.chars().take(MAX_MESSAGE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn plan_result() -> CallToolResult {
        CallToolResult::structured(json!({
            "executed": false,
            "plan": { "action": "create_snapshot", "volume_id": "vol_a", "volume_name": "a", "snapshot_name": null },
            "confirmation_token": "tok.en.sig",
            "expires_in_secs": 300,
            "next": "call again",
        }))
    }

    fn retry(responses: Value, args: Value) -> CallToolRequestParams {
        let map: BTreeMap<String, Value> = serde_json::from_value(responses).unwrap();
        CallToolRequestParams::new("create_snapshot")
            .with_arguments(args.as_object().unwrap().clone())
            .with_input_responses(map)
            .with_request_state("tok.en.sig")
    }

    #[test]
    fn a_plan_becomes_one_confirm_elicitation_carrying_the_token_as_request_state() {
        let r = plan_to_input_required(&plan_result()).expect("plan converts");
        assert_eq!(r.request_state.as_deref(), Some("tok.en.sig"));
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["resultType"], "input_required");
        let ask = &v["inputRequests"]["confirm"];
        assert_eq!(ask["method"], "elicitation/create");
        assert_eq!(ask["params"]["mode"], "form");
        assert_eq!(ask["params"]["requestedSchema"]["properties"]["confirm"]["type"], "boolean");
        let message = ask["params"]["message"].as_str().unwrap();
        assert!(message.contains("create_snapshot") && message.contains("volume_id: vol_a"), "{message}");
        assert!(!message.contains("tok.en.sig") && !message.contains("snapshot_name"), "{message}");
    }

    #[test]
    fn only_a_plan_converts() {
        for v in [
            json!({ "executed": true, "result": {} }),
            json!({ "executed": false }),
            json!({ "something": "else", "confirmation_token": "t" }),
            json!([1, 2]),
        ] {
            assert!(plan_to_input_required(&CallToolResult::structured(v)).is_none());
        }
        assert!(plan_to_input_required(&CallToolResult::error(vec![])).is_none());
    }

    #[test]
    fn accept_injects_the_token_and_the_state_never_travels_on() {
        let mut req = retry(json!({ "confirm": { "action": "accept", "content": { "confirm": true } } }), json!({ "volume_id": "vol_a" }));
        assert!(matches!(take_retry(&mut req), Ok(Retry::Proceed)));
        assert_eq!(req.arguments.as_ref().unwrap()["confirmation_token"], "tok.en.sig");
        assert!(req.request_state.is_none() && req.input_responses.is_none());
    }

    #[test]
    fn decline_cancel_and_confirm_false_execute_nothing() {
        for (responses, outcome) in [
            (json!({ "confirm": { "action": "decline" } }), "declined"),
            (json!({ "confirm": { "action": "cancel" } }), "cancelled"),
            (json!({ "confirm": { "action": "accept", "content": { "confirm": false } } }), "declined"),
            (json!({ "confirm": { "action": "accept" } }), "declined"),
        ] {
            let mut req = retry(responses, json!({ "volume_id": "vol_a" }));
            match take_retry(&mut req) {
                Ok(Retry::Refused(o)) => assert_eq!(o, outcome),
                _ => panic!("must refuse"),
            }
            assert!(req.arguments.as_ref().unwrap().get("confirmation_token").is_none());
        }
    }

    #[test]
    fn a_missing_or_garbled_answer_plans_again_and_both_credentials_are_refused() {
        for responses in [json!({}), json!({ "other": { "action": "accept" } }), json!({ "confirm": "yes" })] {
            let mut req = retry(responses, json!({ "volume_id": "vol_a" }));
            assert!(matches!(take_retry(&mut req), Ok(Retry::Proceed)));
            assert!(req.arguments.as_ref().unwrap().get("confirmation_token").is_none(), "no token without an accept");
        }
        // Only a state, or only responses: also a fresh call.
        let mut req = CallToolRequestParams::new("create_snapshot").with_request_state("x");
        assert!(matches!(take_retry(&mut req), Ok(Retry::Proceed)));
        assert!(req.request_state.is_none());
        // requestState with an explicit confirmation_token argument is ambiguous.
        let mut req = retry(
            json!({ "confirm": { "action": "accept", "content": { "confirm": true } } }),
            json!({ "volume_id": "vol_a", "confirmation_token": "mine" }),
        );
        assert!(take_retry(&mut req).is_err());
    }

    #[test]
    fn refused_results_are_not_errors() {
        let r = refused_result("create_snapshot", "declined");
        assert_eq!(r.is_error, Some(false));
        assert_eq!(r.structured_content.unwrap()["executed"], false);
    }
}
