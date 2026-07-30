use crate::ProviderError;

const BYTES_PER_OUTPUT_TOKEN_LIMIT: usize = 64;
const MIN_OUTPUT_BYTES_LIMIT: usize = 64 * 1_024;
const MAX_OUTPUT_BYTES_LIMIT: usize = 16 * 1_024 * 1_024;
const EVENT_OVERHEAD_BYTES: usize = 256 * 1_024;
const WIRE_BYTES_MULTIPLIER: usize = 64;
const MIN_WIRE_BYTES_LIMIT: usize = 16 * 1_024 * 1_024;
const MAX_WIRE_BYTES_LIMIT: usize = 128 * 1_024 * 1_024;

#[derive(Clone, Copy)]
pub(crate) struct ByteCounter {
    current: usize,
    maximum: usize,
    overflow_message: &'static str,
    limit_message: &'static str,
}

impl ByteCounter {
    pub(crate) fn new(
        maximum: usize,
        overflow_message: &'static str,
        limit_message: &'static str,
    ) -> Self {
        Self {
            current: 0,
            maximum,
            overflow_message,
            limit_message,
        }
    }

    pub(crate) fn add(&mut self, additional: usize) -> Result<(), ProviderError> {
        self.current = self
            .current
            .checked_add(additional)
            .ok_or_else(|| ProviderError::Protocol(self.overflow_message.to_owned()))?;
        if self.current > self.maximum {
            return Err(ProviderError::Protocol(self.limit_message.to_owned()));
        }
        Ok(())
    }
}

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
    fn byte_counter_accepts_the_exact_limit() {
        let mut counter = ByteCounter::new(5, "overflow", "limit");

        counter.add(3).unwrap();
        counter.add(2).unwrap();
    }

    #[test]
    fn byte_counter_rejects_one_byte_over_with_the_supplied_message() {
        let mut counter = ByteCounter::new(4, "overflow", "limit");

        let error = counter.add(5).unwrap_err();

        assert_eq!(error.to_string(), "provider stream was invalid: limit");
    }

    #[test]
    fn byte_counter_rejects_overflow_with_the_supplied_message() {
        let mut counter = ByteCounter::new(usize::MAX, "overflow", "limit");
        counter.add(usize::MAX).unwrap();

        let error = counter.add(1).unwrap_err();

        assert_eq!(error.to_string(), "provider stream was invalid: overflow");
    }

    #[test]
    fn stream_limits_have_absolute_process_safety_caps() {
        let limits = StreamLimits::new(u32::MAX);

        assert_eq!(limits.output, MAX_OUTPUT_BYTES_LIMIT);
        assert_eq!(limits.event, MAX_OUTPUT_BYTES_LIMIT + EVENT_OVERHEAD_BYTES);
        assert_eq!(limits.wire, MAX_WIRE_BYTES_LIMIT);
    }
}
