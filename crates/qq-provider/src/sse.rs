use std::string::FromUtf8Error;

use crate::ProviderError;

#[derive(Clone, Copy)]
pub(crate) enum Utf8ErrorMessage {
    Static(&'static str),
    WithSource(&'static str),
}

impl Utf8ErrorMessage {
    fn into_provider_error(self, source: FromUtf8Error) -> ProviderError {
        match self {
            Self::Static(message) => ProviderError::Protocol(message.to_owned()),
            Self::WithSource(message) => ProviderError::Protocol(format!("{message}: {source}")),
        }
    }
}

#[derive(Clone, Copy)]
enum SseMode {
    DataOnly,
    Named(Utf8ErrorMessage),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub(crate) name: Option<String>,
    pub(crate) data: String,
}

pub(crate) struct SseDecoder {
    bom_prefix: Vec<u8>,
    bom_checked: bool,
    line: Vec<u8>,
    event_name: Option<String>,
    data: Vec<u8>,
    event_bytes: usize,
    max_event_bytes: usize,
    skip_line_feed: bool,
    mode: SseMode,
    size_overflow: &'static str,
    size_limit: &'static str,
    data_utf8: Utf8ErrorMessage,
}

impl SseDecoder {
    pub(crate) const fn data_only(
        max_event_bytes: usize,
        size_overflow: &'static str,
        size_limit: &'static str,
        data_utf8: Utf8ErrorMessage,
    ) -> Self {
        Self::new(
            max_event_bytes,
            SseMode::DataOnly,
            size_overflow,
            size_limit,
            data_utf8,
        )
    }

    pub(crate) const fn named(
        max_event_bytes: usize,
        size_overflow: &'static str,
        size_limit: &'static str,
        data_utf8: Utf8ErrorMessage,
        name_utf8: Utf8ErrorMessage,
    ) -> Self {
        Self::new(
            max_event_bytes,
            SseMode::Named(name_utf8),
            size_overflow,
            size_limit,
            data_utf8,
        )
    }

    const fn new(
        max_event_bytes: usize,
        mode: SseMode,
        size_overflow: &'static str,
        size_limit: &'static str,
        data_utf8: Utf8ErrorMessage,
    ) -> Self {
        Self {
            bom_prefix: Vec::new(),
            bom_checked: false,
            line: Vec::new(),
            event_name: None,
            data: Vec::new(),
            event_bytes: 0,
            max_event_bytes,
            skip_line_feed: false,
            mode,
            size_overflow,
            size_limit,
            data_utf8,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, ProviderError> {
        let mut events = Vec::new();

        for &byte in bytes {
            if !self.bom_checked {
                self.bom_prefix.push(byte);
                if b"\xef\xbb\xbf".starts_with(&self.bom_prefix) {
                    if self.bom_prefix.len() == 3 {
                        self.bom_prefix.clear();
                        self.bom_checked = true;
                    }
                    continue;
                }

                self.bom_checked = true;
                for prefix_byte in std::mem::take(&mut self.bom_prefix) {
                    self.push_byte(prefix_byte, &mut events)?;
                }
                continue;
            }

            self.push_byte(byte, &mut events)?;
        }

        Ok(events)
    }

    fn push_byte(&mut self, byte: u8, events: &mut Vec<SseEvent>) -> Result<(), ProviderError> {
        if self.skip_line_feed {
            self.skip_line_feed = false;
            if byte == b'\n' {
                return Ok(());
            }
        }

        match byte {
            b'\r' => {
                if let Some(event) = self.finish_line()? {
                    events.push(event);
                }
                self.skip_line_feed = true;
            }
            b'\n' => {
                if let Some(event) = self.finish_line()? {
                    events.push(event);
                }
            }
            _ => {
                self.event_bytes = self
                    .event_bytes
                    .checked_add(1)
                    .ok_or_else(|| ProviderError::Protocol(self.size_overflow.to_owned()))?;
                if self.event_bytes > self.max_event_bytes {
                    return Err(ProviderError::Protocol(self.size_limit.to_owned()));
                }
                self.line.push(byte);
            }
        }

        Ok(())
    }

    fn finish_line(&mut self) -> Result<Option<SseEvent>, ProviderError> {
        if self.line.is_empty() {
            self.event_bytes = 0;
            let name = self.event_name.take();
            if self.data.is_empty() {
                return Ok(None);
            }

            self.data.pop();
            let data = String::from_utf8(std::mem::take(&mut self.data))
                .map_err(|error| self.data_utf8.into_provider_error(error))?;
            return Ok(Some(SseEvent { name, data }));
        }

        let line = std::mem::take(&mut self.line);
        if line.starts_with(b":") {
            return Ok(None);
        }

        let (field, value) = line.iter().position(|byte| *byte == b':').map_or_else(
            || (line.as_slice(), &[][..]),
            |colon| {
                let value = line[colon + 1..]
                    .strip_prefix(b" ")
                    .unwrap_or(&line[colon + 1..]);
                (&line[..colon], value)
            },
        );
        match field {
            b"event" => {
                if let SseMode::Named(error_message) = self.mode {
                    self.event_name = Some(
                        String::from_utf8(value.to_vec())
                            .map_err(|error| error_message.into_provider_error(error))?,
                    );
                }
            }
            b"data" => {
                self.data.extend_from_slice(value);
                self.data.push(b'\n');
            }
            _ => {}
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVERFLOW: &str = "event size overflowed";
    const LIMIT: &str = "event exceeded size limit";
    const DATA_UTF8: Utf8ErrorMessage = Utf8ErrorMessage::Static("data was not UTF-8");
    const NAME_UTF8: Utf8ErrorMessage = Utf8ErrorMessage::Static("name was not UTF-8");

    #[test]
    fn data_only_handles_fragmentation_bom_line_endings_comments_and_multiline_data() {
        let source = concat!(
            "\u{feff}: comment\r\n",
            "event: ignored\r",
            "data: first h",
            "é\n",
            "data: second\r\r",
        );
        let mut decoder = SseDecoder::data_only(1_024, OVERFLOW, LIMIT, DATA_UTF8);
        let mut events = Vec::new();

        for byte in source.as_bytes() {
            events.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
        }

        assert_eq!(
            events,
            [SseEvent {
                name: None,
                data: "first hé\nsecond".to_owned(),
            }]
        );
    }

    #[test]
    fn data_only_ignores_invalid_utf8_event_names() {
        let mut decoder = SseDecoder::data_only(1_024, OVERFLOW, LIMIT, DATA_UTF8);

        let events = decoder.push(b"event: \xff\ndata: valid\n\n").unwrap();

        assert_eq!(events[0].data, "valid");
        assert_eq!(events[0].name, None);
    }

    #[test]
    fn named_mode_captures_names_and_validates_their_utf8() {
        let mut decoder = SseDecoder::named(1_024, OVERFLOW, LIMIT, DATA_UTF8, NAME_UTF8);
        let events = decoder.push(b"event: message_stop\ndata: {}\n\n").unwrap();
        assert_eq!(events[0].name.as_deref(), Some("message_stop"));

        let error = decoder.push(b"event: \xff\ndata: {}\n\n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider stream was invalid: name was not UTF-8"
        );
    }

    #[test]
    fn incomplete_eof_does_not_dispatch_and_size_limit_is_enforced_before_termination() {
        let mut decoder = SseDecoder::data_only(7, OVERFLOW, LIMIT, DATA_UTF8);

        assert!(decoder.push(b"data: x").unwrap().is_empty());
        let error = decoder.push(b"y").unwrap_err();

        assert_eq!(
            error.to_string(),
            "provider stream was invalid: event exceeded size limit"
        );
    }
}
