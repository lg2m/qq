const STORAGE_CONTEXT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextConstraint {
    ModelWindow,
    StorageBackstop,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextTarget {
    pub(crate) max_reducible_input_tokens: Option<u64>,
    pub(crate) max_reducible_input_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextRejectReason {
    Irreducible(ContextConstraint),
    CompactionUnavailable(ContextConstraint),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextEstimate {
    pub(crate) input_bytes: u64,
    pub(crate) estimated_input_tokens: u64,
    pub(crate) output_reserve_tokens: u64,
    pub(crate) context_window: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextPlan {
    Send {
        estimate: ContextEstimate,
    },
    Compact {
        estimate: ContextEstimate,
        reason: ContextConstraint,
        target: ContextTarget,
    },
    Reject {
        estimate: ContextEstimate,
        reason: ContextRejectReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextInput {
    pub(crate) context_window: Option<u32>,
    pub(crate) max_output_tokens: u32,
    pub(crate) system_bytes: u64,
    pub(crate) tool_schema_bytes: u64,
    pub(crate) reducible_message_bytes: u64,
    pub(crate) irreducible_message_bytes: u64,
    /// Provider-measured occupancy of a caller-verified compatible prefix,
    /// including a conservative byte upper bound for newly appended input.
    pub(crate) compatible_input_tokens: Option<u64>,
    pub(crate) compaction_attempted: bool,
}

pub(crate) fn plan(input: ContextInput) -> ContextPlan {
    let fixed_input = input
        .system_bytes
        .saturating_add(input.tool_schema_bytes)
        .saturating_add(input.irreducible_message_bytes);
    let input_bytes = fixed_input.saturating_add(input.reducible_message_bytes);
    // Compatibility is established by the caller from effective model,
    // prompt/tool identity, and an append-only transcript watermark. When it
    // holds, the provider's measured occupancy is more useful than applying
    // the deliberately conservative byte upper bound to the unchanged prefix.
    let estimated_input_tokens = input.compatible_input_tokens.unwrap_or(input_bytes);
    let output_tokens = u64::from(input.max_output_tokens);
    let fixed_tokens = fixed_input.saturating_add(output_tokens);
    let required_tokens = estimated_input_tokens.saturating_add(output_tokens);
    let estimate = ContextEstimate {
        input_bytes,
        estimated_input_tokens,
        output_reserve_tokens: output_tokens,
        context_window: input.context_window,
    };

    let exceeds_storage = input_bytes > STORAGE_CONTEXT_BYTES;
    let exceeds_window = input
        .context_window
        .is_some_and(|window| required_tokens > u64::from(window));
    // A compatible provider measurement covers the complete prior request.
    // Let a measured fit win before classifying raw byte weights as an
    // irreducible model-window overflow; the byte storage backstop remains
    // independently authoritative.
    if !exceeds_storage && !exceeds_window {
        return ContextPlan::Send { estimate };
    }

    let irreducible_window_overflow = input.compatible_input_tokens.is_none()
        && input
            .context_window
            .is_some_and(|window| fixed_tokens > u64::from(window));
    let irreducible_storage_overflow = fixed_input > STORAGE_CONTEXT_BYTES;
    if irreducible_window_overflow || irreducible_storage_overflow {
        let constraint = match (irreducible_window_overflow, irreducible_storage_overflow) {
            (true, true) => ContextConstraint::Both,
            (true, false) => ContextConstraint::ModelWindow,
            (false, true) => ContextConstraint::StorageBackstop,
            (false, false) => unreachable!(),
        };
        return ContextPlan::Reject {
            estimate,
            reason: ContextRejectReason::Irreducible(constraint),
        };
    }
    let constraint = match (exceeds_window, exceeds_storage) {
        (true, true) => ContextConstraint::Both,
        (true, false) => ContextConstraint::ModelWindow,
        (false, true) => ContextConstraint::StorageBackstop,
        (false, false) => unreachable!(),
    };
    let target = ContextTarget {
        max_reducible_input_tokens: match (input.compatible_input_tokens, input.context_window) {
            (None, Some(window)) => Some(
                u64::from(window)
                    .saturating_sub(output_tokens)
                    .saturating_sub(fixed_input),
            ),
            (None, None) | (Some(_), _) => None,
        },
        max_reducible_input_bytes: STORAGE_CONTEXT_BYTES.saturating_sub(fixed_input),
    };
    if input.reducible_message_bytes > 0 && !input.compaction_attempted {
        ContextPlan::Compact {
            estimate,
            reason: constraint,
            target,
        }
    } else {
        ContextPlan::Reject {
            estimate,
            reason: ContextRejectReason::CompactionUnavailable(constraint),
        }
    }
}

pub(crate) fn rejection_message(plan: ContextPlan) -> Option<String> {
    let (estimate, constraint, detail) = match plan {
        ContextPlan::Send { .. } => return None,
        ContextPlan::Compact {
            estimate,
            reason,
            target,
        } => {
            let token_target = target
                .max_reducible_input_tokens
                .map_or_else(|| "unknown".to_owned(), |target| target.to_string());
            let detail = format!(
                "automatic compaction must reduce reducible history to at most {token_target} estimated tokens and {} bytes before this request can start",
                target.max_reducible_input_bytes,
            );
            (estimate, reason, detail)
        }
        ContextPlan::Reject { estimate, reason } => {
            let (constraint, detail) = match reason {
                ContextRejectReason::Irreducible(constraint) => (
                    constraint,
                    "the irreducible request cannot fit even after compaction".to_owned(),
                ),
                ContextRejectReason::CompactionUnavailable(constraint) => (
                    constraint,
                    "compaction was already attempted or no reducible history remains".to_owned(),
                ),
            };
            (estimate, constraint, detail)
        }
    };
    Some(match constraint {
        ContextConstraint::ModelWindow => format!(
            "estimated provider-neutral context requires {} input tokens plus a {}-token output reserve, exceeding the selected model's {}-token window; {detail}",
            estimate.estimated_input_tokens,
            estimate.output_reserve_tokens,
            estimate.context_window.unwrap_or(0),
        ),
        ContextConstraint::StorageBackstop => format!(
            "provider-neutral context measures {} bytes, exceeding the independent {} MiB storage backstop; {detail}",
            estimate.input_bytes,
            STORAGE_CONTEXT_BYTES / (1024 * 1024),
        ),
        ContextConstraint::Both => format!(
            "estimated provider-neutral context requires {} input tokens plus a {}-token output reserve against the selected model's {}-token window and measures {} bytes against the independent {} MiB storage backstop; {detail}",
            estimate.estimated_input_tokens,
            estimate.output_reserve_tokens,
            estimate.context_window.unwrap_or(0),
            estimate.input_bytes,
            STORAGE_CONTEXT_BYTES / (1024 * 1024),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(context_window: Option<u32>) -> ContextInput {
        ContextInput {
            context_window,
            max_output_tokens: 40,
            system_bytes: 10,
            tool_schema_bytes: 10,
            reducible_message_bytes: 20,
            irreducible_message_bytes: 20,
            compatible_input_tokens: None,
            compaction_attempted: false,
        }
    }

    fn assert_send(plan: ContextPlan) {
        assert!(matches!(plan, ContextPlan::Send { .. }), "{plan:?}");
    }

    fn assert_compact(plan: ContextPlan) {
        assert!(matches!(plan, ContextPlan::Compact { .. }), "{plan:?}");
    }

    fn assert_reject(plan: ContextPlan) {
        assert!(matches!(plan, ContextPlan::Reject { .. }), "{plan:?}");
    }

    #[test]
    fn known_window_exact_fit_sends_and_one_token_over_compacts() {
        let ContextPlan::Send { estimate } = plan(input(Some(100))) else {
            panic!("exact fit must send")
        };
        assert_eq!(estimate.estimated_input_tokens, 60);
        assert_eq!(estimate.output_reserve_tokens, 40);
        assert_eq!(estimate.context_window, Some(100));
        assert_eq!(estimate.input_bytes, 60);
        let ContextPlan::Compact { reason, target, .. } = plan(input(Some(99))) else {
            panic!("one token over must compact")
        };
        assert_eq!(reason, ContextConstraint::ModelWindow);
        assert_eq!(target.max_reducible_input_tokens, Some(19));
        assert_eq!(target.max_reducible_input_bytes, STORAGE_CONTEXT_BYTES - 40);
    }

    #[test]
    fn tool_schema_and_output_reserve_are_both_part_of_the_window() {
        let mut request = input(Some(100));
        request.system_bytes = 10;
        request.tool_schema_bytes = 30;
        request.reducible_message_bytes = 0;
        request.irreducible_message_bytes = 20;
        assert_send(plan(request));

        request.tool_schema_bytes = 31;
        assert_reject(plan(request));
    }

    #[test]
    fn one_transcript_rejects_compacts_or_sends_for_three_windows() {
        let request = ContextInput {
            context_window: Some(99),
            max_output_tokens: 40,
            system_bytes: 30,
            tool_schema_bytes: 10,
            reducible_message_bytes: 100,
            irreducible_message_bytes: 20,
            compatible_input_tokens: None,
            compaction_attempted: false,
        };
        assert_reject(plan(request));
        assert_compact(plan(ContextInput {
            context_window: Some(150),
            ..request
        }));
        assert_send(plan(ContextInput {
            context_window: Some(200),
            ..request
        }));
    }

    #[test]
    fn unknown_windows_still_obey_the_independent_storage_backstop() {
        let exact = ContextInput {
            context_window: None,
            max_output_tokens: u32::MAX,
            system_bytes: 0,
            tool_schema_bytes: 0,
            reducible_message_bytes: STORAGE_CONTEXT_BYTES - 1,
            irreducible_message_bytes: 1,
            compatible_input_tokens: None,
            compaction_attempted: false,
        };
        assert_send(plan(exact));
        assert_compact(plan(ContextInput {
            reducible_message_bytes: STORAGE_CONTEXT_BYTES,
            ..exact
        }));
        assert_reject(plan(ContextInput {
            system_bytes: STORAGE_CONTEXT_BYTES + 1,
            reducible_message_bytes: 0,
            irreducible_message_bytes: 0,
            ..exact
        }));
        assert_reject(plan(ContextInput {
            reducible_message_bytes: STORAGE_CONTEXT_BYTES,
            compaction_attempted: true,
            ..exact
        }));
    }

    #[test]
    fn compatible_usage_reuses_measured_occupancy_but_incompatible_input_does_not() {
        let request = input(Some(120));
        assert_send(plan(request));
        assert_compact(plan(ContextInput {
            compatible_input_tokens: Some(91),
            ..request
        }));
        let byte_heavy = ContextInput {
            context_window: Some(150),
            max_output_tokens: 40,
            system_bytes: 10,
            tool_schema_bytes: 10,
            reducible_message_bytes: 120,
            irreducible_message_bytes: 20,
            compatible_input_tokens: None,
            compaction_attempted: false,
        };
        assert_compact(plan(byte_heavy));
        assert_send(plan(ContextInput {
            compatible_input_tokens: Some(70),
            ..byte_heavy
        }));

        let raw_fixed_prefix_exceeds_the_window = ContextInput {
            context_window: Some(100),
            max_output_tokens: 40,
            system_bytes: 80,
            tool_schema_bytes: 0,
            reducible_message_bytes: 100,
            irreducible_message_bytes: 0,
            compatible_input_tokens: Some(10),
            compaction_attempted: true,
        };
        assert_send(plan(raw_fixed_prefix_exceeds_the_window));
        let ContextPlan::Compact { target, .. } = plan(ContextInput {
            compatible_input_tokens: Some(70),
            compaction_attempted: false,
            ..raw_fixed_prefix_exceeds_the_window
        }) else {
            panic!("compatible occupancy cannot prove the reducible prefix is irreducible")
        };
        assert_eq!(target.max_reducible_input_tokens, None);
    }

    #[test]
    fn all_capacity_arithmetic_saturates_closed() {
        assert_reject(plan(ContextInput {
            context_window: Some(u32::MAX),
            max_output_tokens: u32::MAX,
            system_bytes: u64::MAX,
            tool_schema_bytes: u64::MAX,
            reducible_message_bytes: u64::MAX,
            irreducible_message_bytes: u64::MAX,
            compatible_input_tokens: Some(u64::MAX),
            compaction_attempted: false,
        }));
    }
}
