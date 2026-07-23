pub(crate) const ERROR_MESSAGE_CHARS_LIMIT: usize = 1_024;
const REDACTION: &str = "[REDACTED]";

pub(crate) fn sanitize_message(message: &str, redactions: &[String]) -> String {
    let mut output = String::with_capacity(message.len().min(ERROR_MESSAGE_CHARS_LIMIT));
    let mut index = 0;
    let mut output_chars = 0;
    let mut pending_space = false;

    while index < message.len() && output_chars < ERROR_MESSAGE_CHARS_LIMIT {
        if let Some(secret) = longest_match(&message[index..], redactions) {
            let separator_chars = usize::from(pending_space && !output.is_empty());
            if output_chars + separator_chars + REDACTION.len() > ERROR_MESSAGE_CHARS_LIMIT {
                break;
            }
            if pending_space && !output.is_empty() {
                output.push(' ');
                output_chars += 1;
            }
            output.push_str(REDACTION);
            output_chars += REDACTION.len();
            pending_space = false;
            index += secret.len();
            continue;
        }

        let character = message[index..]
            .chars()
            .next()
            .expect("index must remain on a character boundary");
        index += character.len_utf8();

        if character.is_whitespace() || character.is_control() {
            pending_space = !output.is_empty();
            continue;
        }
        if pending_space {
            if output_chars + 2 > ERROR_MESSAGE_CHARS_LIMIT {
                break;
            }
            output.push(' ');
            output_chars += 1;
            pending_space = false;
        }
        if output_chars == ERROR_MESSAGE_CHARS_LIMIT {
            break;
        }
        output.push(character);
        output_chars += 1;
    }

    output
}

fn longest_match<'a>(message: &str, redactions: &'a [String]) -> Option<&'a str> {
    redactions
        .iter()
        .map(String::as_str)
        .filter(|secret| !secret.is_empty() && message.starts_with(secret))
        .max_by_key(|secret| secret.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_is_bounded_and_cannot_expand_recursively() {
        let message = "a".repeat(ERROR_MESSAGE_CHARS_LIMIT * 4);
        let sanitized = sanitize_message(&message, &["a".to_owned(), "[".to_owned()]);

        assert!(sanitized.chars().count() <= ERROR_MESSAGE_CHARS_LIMIT);
        assert!(!sanitized.contains('a'));
    }

    #[test]
    fn redacts_the_longest_match_and_normalizes_controls() {
        let sanitized = sanitize_message(
            "before\nlong-secret\tafter",
            &["long".to_owned(), "long-secret".to_owned()],
        );

        assert_eq!(sanitized, "before [REDACTED] after");
    }
}
