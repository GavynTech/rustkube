//! Minimal Server-Sent-Events parser: feed it byte chunks, get `data:`
//! payloads out. Handles events split across chunks, multi-line data, CRLF,
//! and ignores comments/other fields — all the SSE this client needs.

#[derive(Debug, Default)]
pub struct SseParser {
    buffer: String,
}

impl SseParser {
    /// Feed one chunk; returns the data payload of every event completed by
    /// this chunk, in order.
    pub fn feed(&mut self, chunk: &str) -> Vec<String> {
        // Normalize CRLF up front so the blank-line scan below only has one
        // separator to find. A `\r\n` split across chunks resolves once the
        // `\n` arrives, because the dangling `\r` is still in the buffer.
        self.buffer.push_str(chunk);
        self.buffer = self.buffer.replace("\r\n", "\n");
        let mut events = Vec::new();

        // An SSE event ends at a blank line.
        while let Some(boundary) = self.buffer.find("\n\n") {
            let event: String = self.buffer.drain(..boundary + 2).collect();
            let mut data_lines = Vec::new();
            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    data_lines.push(data.strip_prefix(' ').unwrap_or(data));
                }
            }
            if !data_lines.is_empty() {
                events.push(data_lines.join("\n"));
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_chunks_reassemble() {
        let mut parser = SseParser::default();
        assert!(parser.feed("data: {\"a\":").is_empty());
        assert_eq!(parser.feed("1}\n\n"), vec![r#"{"a":1}"#]);
    }

    #[test]
    fn multiple_events_in_one_chunk_come_out_in_order() {
        let mut parser = SseParser::default();
        let events = parser.feed("data: one\n\ndata: two\n\n");
        assert_eq!(events, vec!["one", "two"]);
    }

    #[test]
    fn keep_alive_comments_are_ignored() {
        let mut parser = SseParser::default();
        assert!(parser.feed(": keep-alive\n\n").is_empty());
        assert_eq!(parser.feed("event: message\ndata: x\n\n"), vec!["x"]);
    }

    #[test]
    fn crlf_streams_parse_too() {
        let mut parser = SseParser::default();
        assert_eq!(parser.feed("data: x\r\n\r\n"), vec!["x"]);

        // A CRLF pair split across chunks still resolves.
        let mut split = SseParser::default();
        assert!(split.feed("data: y\r\n\r").is_empty());
        assert_eq!(split.feed("\n"), vec!["y"]);
    }
}
