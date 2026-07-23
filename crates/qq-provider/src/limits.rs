const BYTES_PER_OUTPUT_TOKEN_LIMIT: usize = 64;
const MIN_OUTPUT_BYTES_LIMIT: usize = 64 * 1_024;
const MAX_OUTPUT_BYTES_LIMIT: usize = 16 * 1_024 * 1_024;
const EVENT_OVERHEAD_BYTES: usize = 256 * 1_024;
const WIRE_BYTES_MULTIPLIER: usize = 64;
const MIN_WIRE_BYTES_LIMIT: usize = 16 * 1_024 * 1_024;
const MAX_WIRE_BYTES_LIMIT: usize = 128 * 1_024 * 1_024;

#[derive(Clone, Copy)]
pub(crate) struct StreamLimits {
    pub(crate) output: usize,
    pub(crate) event: usize,
    pub(crate) wire: usize,
}

impl StreamLimits {
    pub(crate) fn new(max_output_tokens: u32) -> Self {
        let output = (max_output_tokens as usize)
            .saturating_mul(BYTES_PER_OUTPUT_TOKEN_LIMIT)
            .clamp(MIN_OUTPUT_BYTES_LIMIT, MAX_OUTPUT_BYTES_LIMIT);
        Self {
            output,
            event: output.saturating_add(EVENT_OVERHEAD_BYTES),
            wire: output
                .saturating_mul(WIRE_BYTES_MULTIPLIER)
                .clamp(MIN_WIRE_BYTES_LIMIT, MAX_WIRE_BYTES_LIMIT),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_limits_have_absolute_process_safety_caps() {
        let limits = StreamLimits::new(u32::MAX);

        assert_eq!(limits.output, MAX_OUTPUT_BYTES_LIMIT);
        assert_eq!(limits.event, MAX_OUTPUT_BYTES_LIMIT + EVENT_OVERHEAD_BYTES);
        assert_eq!(limits.wire, MAX_WIRE_BYTES_LIMIT);
    }
}
