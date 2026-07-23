//! Minimal OpenRouter chat-completions client.
//!
//! Requests are always streamed (Server-Sent Events). [`Client::open_stream`]
//! exposes the token deltas as they arrive; [`Client::complete`] drains that
//! stream to a single [`Completion`] for callers that only want the final text.

use anyhow::{Context, Result, anyhow};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

const API_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Endpoint the client posts to. Overridable so tests can point at a local
/// server and assert on the exact request we put on the wire.
#[derive(Debug, Clone)]
struct Endpoint(String);

impl Default for Endpoint {
    fn default() -> Self {
        Self(API_URL.to_string())
    }
}

/// A single turn in the conversation, in the shape the API expects.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    api_key: String,
    model: String,
    endpoint: Endpoint,
}

/// What a completed request gives back to the UI.
#[derive(Debug, Clone)]
pub struct Completion {
    pub content: String,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

/// One event from a streaming response.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A chunk of assistant content to append.
    Delta(String),
    /// The stream finished. Usage arrives here, in the final chunk.
    Done { usage: Option<Usage> },
}

impl Client {
    pub fn new(api_key: String, model: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("ai-harness/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            api_key,
            model,
            endpoint: Endpoint::default(),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    #[cfg(test)]
    fn with_endpoint(mut self, url: impl Into<String>) -> Self {
        self.endpoint = Endpoint(url.into());
        self
    }

    /// Open a streaming request, yielding token deltas as they arrive.
    ///
    /// The POST and status check happen up front, so a failed request surfaces
    /// here rather than partway through the stream.
    pub async fn open_stream(
        &self,
        messages: &[Message],
    ) -> Result<impl Stream<Item = Result<StreamEvent>>> {
        let body = ChatRequest {
            model: &self.model,
            messages,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let response = self
            .http
            .post(&self.endpoint.0)
            .bearer_auth(&self.api_key)
            // OpenRouter uses these for attribution on its leaderboards; both are optional.
            .header("HTTP-Referer", "https://github.com/local/ai-harness")
            .header("X-Title", "ai-harness")
            .json(&body)
            .send()
            .await
            .context("sending request to OpenRouter")?;

        let status = response.status();
        if !status.is_success() {
            // Errors are a normal JSON body, not a stream. Read it whole.
            let text = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<ErrorEnvelope>(&text)
                .ok()
                .and_then(|r| r.error)
                .map(|e| e.message)
                .unwrap_or_else(|| truncate(&text, 800));
            return Err(anyhow!("OpenRouter returned HTTP {status}: {message}"));
        }

        Ok(sse_events(response.bytes_stream()))
    }

    /// Drain a streaming request to a single completion, for callers that only
    /// want the final text. The UI streams instead (see [`Client::open_stream`]);
    /// this is used by tests and keeps a simple non-streaming entry point.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn complete(&self, messages: &[Message]) -> Result<Completion> {
        let stream = self.open_stream(messages).await?;
        futures_util::pin_mut!(stream);

        let mut content = String::new();
        let mut usage = None;
        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::Delta(delta) => content.push_str(&delta),
                // Usage arrives in its own chunk, followed by a `[DONE]` whose
                // usage is None; keep the populated one.
                StreamEvent::Done { usage: Some(u) } => usage = Some(u),
                StreamEvent::Done { usage: None } => {}
            }
        }
        Ok(Completion { content, usage })
    }
}

/// Frame a byte stream of Server-Sent Events into [`StreamEvent`]s.
///
/// SSE events are separated by a blank line; each carries one or more `data:`
/// lines. We buffer bytes until a complete event boundary (`\n\n`) is seen, so a
/// multibyte character or a `data:` line split across network chunks is never
/// decoded halfway.
fn sse_events(
    bytes: impl Stream<Item = reqwest::Result<bytes::Bytes>>,
) -> impl Stream<Item = Result<StreamEvent>> {
    // State carried across chunks: the undecoded byte buffer, and whether the
    // terminating `[DONE]` has been seen (so trailing bytes are ignored).
    struct State<S> {
        bytes: S,
        buffer: Vec<u8>,
        done: bool,
    }

    let init = State {
        bytes: Box::pin(bytes),
        buffer: Vec::new(),
        done: false,
    };

    futures_util::stream::unfold(init, |mut state| async move {
        loop {
            // Emit any complete events already buffered.
            if let Some(boundary) = find_boundary(&state.buffer) {
                let raw = state.buffer.drain(..boundary.end).collect::<Vec<_>>();
                let block = String::from_utf8_lossy(&raw[..boundary.content]);
                match parse_sse_event(&block) {
                    Some(Ok(StreamEvent::Done { usage })) => {
                        state.done = true;
                        return Some((Ok(StreamEvent::Done { usage }), state));
                    }
                    Some(result) => return Some((result, state)),
                    None => continue, // comment or contentless keepalive
                }
            }

            if state.done {
                return None;
            }

            match state.bytes.next().await {
                Some(Ok(chunk)) => state.buffer.extend_from_slice(&chunk),
                Some(Err(e)) => {
                    return Some((Err(anyhow!("stream error: {e}")), state));
                }
                None => {
                    // Stream ended without an explicit [DONE]; treat as finished.
                    return None;
                }
            }
        }
    })
}

/// Where an event block ends in the buffer: `content` bytes of text, then
/// `end` bytes consumed including the blank-line separator.
struct Boundary {
    content: usize,
    end: usize,
}

fn find_boundary(buffer: &[u8]) -> Option<Boundary> {
    // Events are separated by "\n\n" (tolerate "\r\n\r\n").
    buffer
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| Boundary {
            content: i,
            end: i + 2,
        })
        .or_else(|| {
            buffer
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| Boundary {
                    content: i,
                    end: i + 4,
                })
        })
}

/// Parse one SSE event block into a [`StreamEvent`].
///
/// Returns `None` for comments and keepalives (nothing to emit), `Some(Ok(..))`
/// for a delta or the terminator, and `Some(Err(..))` for a data line whose JSON
/// we cannot make sense of.
fn parse_sse_event(block: &str) -> Option<Result<StreamEvent>> {
    // Concatenate the payload of every `data:` line in the event.
    let mut data = String::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim());
        }
        // Lines starting with ':' are comments; other fields (event:, id:) are
        // irrelevant here and ignored.
    }

    if data.is_empty() {
        return None;
    }
    if data == "[DONE]" {
        return Some(Ok(StreamEvent::Done { usage: None }));
    }

    let chunk: StreamChunk = match serde_json::from_str(&data) {
        Ok(chunk) => chunk,
        Err(e) => {
            return Some(Err(anyhow!(
                "could not parse stream chunk ({e}): {}",
                truncate(&data, 300)
            )));
        }
    };

    if let Some(err) = chunk.error {
        return Some(Err(anyhow!("OpenRouter stream error: {}", err.message)));
    }

    let delta = chunk
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.delta.content)
        .unwrap_or_default();

    // A usage-only final chunk carries no content; report it as Done so the
    // caller records token counts.
    if delta.is_empty() {
        if let Some(usage) = chunk.usage {
            return Some(Ok(StreamEvent::Done { usage: Some(usage) }));
        }
        return None;
    }
    Some(Ok(StreamEvent::Delta(delta)))
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// One streamed chunk: `{"choices":[{"delta":{"content":"…"}}],"usage":…}`.
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    // `reasoning` deltas are intentionally not accumulated into content.
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
}

/// A non-streaming error body: `{"error": {"message": …}}`.
#[derive(Deserialize)]
struct ErrorEnvelope {
    #[serde(default)]
    error: Option<ApiError>,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// What the test server saw.
    struct Captured {
        headers: Vec<String>,
        body: String,
    }

    /// Serve exactly one request, replying with `status` and `response_body`.
    /// Returns the client and a handle yielding what the server received.
    fn serve_once(
        status: &'static str,
        response_body: &'static str,
    ) -> (Client, std::thread::JoinHandle<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().unwrap();

        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);

            let mut headers = Vec::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read header");
                let trimmed = line.trim_end().to_string();
                if trimmed.is_empty() {
                    break;
                }
                if let Some(v) = trimmed.strip_prefix("content-length: ") {
                    content_length = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = trimmed.strip_prefix("Content-Length: ") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                headers.push(trimmed);
            }

            let mut buf = vec![0u8; content_length];
            reader.read_exact(&mut buf).expect("read body");
            let body = String::from_utf8_lossy(&buf).to_string();

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let mut stream = reader.into_inner();
            stream
                .write_all(response.as_bytes())
                .expect("write response");
            stream.flush().ok();

            Captured { headers, body }
        });

        let client = Client::new("test-key".into(), "test/model".into())
            .unwrap()
            .with_endpoint(format!("http://{addr}/chat/completions"));
        (client, handle)
    }

    /// Serve one request with a `text/event-stream` body built from `events`.
    /// Each event becomes a `data: …` block; a terminating `[DONE]` is appended.
    fn serve_sse(events: Vec<String>) -> (Client, std::thread::JoinHandle<Captured>) {
        let mut body = String::new();
        for event in events {
            body.push_str(&format!("data: {event}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");
        serve_body("200 OK", "text/event-stream", body)
    }

    /// Serve one request with an arbitrary body, capturing what was received.
    fn serve_body(
        status: &'static str,
        content_type: &'static str,
        body: String,
    ) -> (Client, std::thread::JoinHandle<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().unwrap();

        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);

            let mut headers = Vec::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read header");
                let trimmed = line.trim_end().to_string();
                if trimmed.is_empty() {
                    break;
                }
                if let Some(v) = trimmed.to_lowercase().strip_prefix("content-length: ") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                headers.push(trimmed);
            }
            let mut buf = vec![0u8; content_length];
            reader.read_exact(&mut buf).expect("read body");
            let request_body = String::from_utf8_lossy(&buf).to_string();

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut stream = reader.into_inner();
            stream
                .write_all(response.as_bytes())
                .expect("write response");
            stream.flush().ok();

            Captured {
                headers,
                body: request_body,
            }
        });

        let client = Client::new("test-key".into(), "test/model".into())
            .unwrap()
            .with_endpoint(format!("http://{addr}/chat/completions"));
        (client, handle)
    }

    /// A content-delta chunk in OpenRouter's streaming shape.
    fn delta(text: &str) -> String {
        format!(
            r#"{{"choices":[{{"delta":{{"content":{}}}}}]}}"#,
            json_str(text)
        )
    }

    fn json_str(s: &str) -> String {
        serde_json::to_string(s).unwrap()
    }

    #[tokio::test]
    async fn sends_model_and_messages_with_auth() {
        let (client, server) = serve_sse(vec![delta("hi")]);

        let messages = vec![Message::system("be terse"), Message::user("hello")];
        client
            .complete(&messages)
            .await
            .expect("request should succeed");

        let captured = server.join().unwrap();
        let headers = captured.headers.join("\n").to_lowercase();
        assert!(
            headers.contains("authorization: bearer test-key"),
            "missing bearer auth in:\n{headers}"
        );

        let sent: serde_json::Value =
            serde_json::from_str(&captured.body).expect("valid JSON body");
        assert_eq!(sent["model"], "test/model");
        assert_eq!(sent["stream"], true, "requests must ask for streaming");
        assert_eq!(sent["stream_options"]["include_usage"], true);
        assert_eq!(sent["messages"][0]["role"], "system");
        assert_eq!(sent["messages"][0]["content"], "be terse");
        assert_eq!(sent["messages"][1]["role"], "user");
        assert_eq!(sent["messages"][1]["content"], "hello");
    }

    #[tokio::test]
    async fn assembles_multi_chunk_content_in_order() {
        let (client, server) = serve_sse(vec![delta("2 + 2 "), delta("= "), delta("4")]);
        let completion = client.complete(&[Message::user("2+2")]).await.unwrap();
        server.join().unwrap();
        assert_eq!(completion.content, "2 + 2 = 4");
    }

    #[tokio::test]
    async fn parses_usage_from_the_final_chunk() {
        let usage = r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":2}}"#;
        let (client, server) = serve_sse(vec![delta("4"), usage.to_string()]);

        let completion = client.complete(&[Message::user("2+2")]).await.unwrap();
        server.join().unwrap();

        assert_eq!(completion.content, "4");
        let usage = completion.usage.expect("usage should parse");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 2);
    }

    #[tokio::test]
    async fn response_without_usage_still_succeeds() {
        let (client, server) = serve_sse(vec![delta("ok")]);
        let completion = client.complete(&[Message::user("hi")]).await.unwrap();
        server.join().unwrap();
        assert_eq!(completion.content, "ok");
        assert!(completion.usage.is_none());
    }

    #[tokio::test]
    async fn open_stream_yields_deltas_in_order() {
        let (client, server) = serve_sse(vec![delta("a"), delta("b"), delta("c")]);
        let messages = [Message::user("hi")];
        let stream = client.open_stream(&messages).await.unwrap();
        futures_util::pin_mut!(stream);

        let mut deltas = Vec::new();
        while let Some(event) = stream.next().await {
            if let StreamEvent::Delta(d) = event.unwrap() {
                deltas.push(d);
            }
        }
        server.join().unwrap();
        assert_eq!(deltas, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn keepalive_comments_are_ignored() {
        // OpenRouter interleaves ": OPENROUTER PROCESSING" comment lines.
        let (first, second) = (delta("hel"), delta("lo"));
        let body = format!(
            ": OPENROUTER PROCESSING\n\n\
             data: {first}\n\n\
             : still working\n\n\
             data: {second}\n\n\
             data: [DONE]\n\n"
        );
        let (client, server) = serve_body("200 OK", "text/event-stream", body);
        let completion = client.complete(&[Message::user("hi")]).await.unwrap();
        server.join().unwrap();
        assert_eq!(completion.content, "hello");
    }

    #[tokio::test]
    async fn a_content_delta_split_across_events_assembles() {
        // Each token is its own SSE event; the assembled string is contiguous.
        let (client, server) = serve_sse(vec![delta("日本"), delta("語")]);
        let completion = client.complete(&[Message::user("hi")]).await.unwrap();
        server.join().unwrap();
        assert_eq!(completion.content, "日本語");
    }

    #[tokio::test]
    async fn an_empty_stream_yields_empty_content() {
        // Just [DONE], no deltas. The app layer turns empty content into a
        // protocol error; the client itself simply reports nothing.
        let (client, server) = serve_sse(vec![]);
        let completion = client.complete(&[Message::user("hi")]).await.unwrap();
        server.join().unwrap();
        assert_eq!(completion.content, "");
        assert!(completion.usage.is_none());
    }

    #[tokio::test]
    async fn surfaces_the_api_error_message() {
        let (client, server) = serve_once(
            "401 Unauthorized",
            r#"{"error":{"message":"No auth credentials found","code":401}}"#,
        );

        let err = client.complete(&[Message::user("hi")]).await.unwrap_err();
        server.join().unwrap();
        assert!(
            err.to_string().contains("No auth credentials found"),
            "error should quote the API message, got: {err}"
        );
    }

    #[tokio::test]
    async fn an_error_chunk_mid_stream_is_surfaced() {
        // A streamed error arrives as a data chunk carrying {"error":…}.
        let error = r#"{"error":{"message":"rate limited","code":429}}"#;
        let (client, server) = serve_sse(vec![delta("partial"), error.to_string()]);

        let err = client.complete(&[Message::user("hi")]).await.unwrap_err();
        server.join().unwrap();
        assert!(err.to_string().contains("rate limited"), "got: {err}");
    }

    #[tokio::test]
    async fn a_malformed_chunk_is_reported_not_dropped() {
        let (client, server) = serve_sse(vec!["{not valid json".to_string()]);
        let err = client.complete(&[Message::user("hi")]).await.unwrap_err();
        server.join().unwrap();
        assert!(err.to_string().contains("parse stream chunk"), "got: {err}");
    }

    #[tokio::test]
    async fn non_json_body_reports_the_status_and_snippet() {
        let (client, server) = serve_once("502 Bad Gateway", "upstream exploded");
        let err = client.complete(&[Message::user("hi")]).await.unwrap_err();
        server.join().unwrap();
        let text = err.to_string();
        assert!(
            text.contains("502"),
            "should mention the status, got: {text}"
        );
        assert!(
            text.contains("upstream exploded"),
            "should include the body, got: {text}"
        );
    }

    #[test]
    fn parse_sse_event_reads_a_content_delta() {
        let event = parse_sse_event(r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#);
        assert_eq!(event.unwrap().unwrap(), StreamEvent::Delta("hi".into()));
    }

    #[test]
    fn parse_sse_event_treats_done_as_terminal() {
        assert_eq!(
            parse_sse_event("data: [DONE]").unwrap().unwrap(),
            StreamEvent::Done { usage: None }
        );
    }

    #[test]
    fn parse_sse_event_ignores_comments_and_blanks() {
        assert!(parse_sse_event(": OPENROUTER PROCESSING").is_none());
        assert!(parse_sse_event("").is_none());
        assert!(parse_sse_event("event: ping").is_none());
    }

    #[test]
    fn parse_sse_event_reports_a_usage_only_chunk_as_done() {
        let event = parse_sse_event(
            r#"data: {"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
        );
        match event.unwrap().unwrap() {
            StreamEvent::Done { usage: Some(u) } => {
                assert_eq!(u.prompt_tokens, 3);
                assert_eq!(u.completion_tokens, 1);
            }
            other => panic!("expected Done with usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_event_flags_bad_json() {
        assert!(parse_sse_event("data: {oops").unwrap().is_err());
    }

    #[test]
    fn find_boundary_handles_lf_and_crlf() {
        let b = find_boundary(b"data: x\n\nrest").unwrap();
        assert_eq!(b.content, 7);
        assert_eq!(b.end, 9);

        let b = find_boundary(b"data: x\r\n\r\nrest").unwrap();
        assert_eq!(&b"data: x\r\n\r\nrest"[..b.content], b"data: x");

        assert!(find_boundary(b"data: incomplete").is_none());
    }

    #[test]
    fn roles_serialize_lowercase() {
        let json = serde_json::to_string(&Message::assistant("x")).unwrap();
        assert!(json.contains(r#""role":"assistant""#), "got {json}");
    }

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }

    /// Hits the real API. Opt-in, since it needs a key and costs money:
    /// `cargo test -- --ignored live_`
    #[tokio::test]
    #[ignore = "requires OPENROUTER_API_KEY and makes a real API call"]
    async fn live_round_trip() {
        let _ = dotenvy::dotenv();
        let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
        let model = std::env::var("OPENROUTER_MODEL")
            .unwrap_or_else(|_| crate::config::DEFAULT_MODEL.to_string());

        let client = Client::new(key, model).unwrap();
        let completion = client
            .complete(&[
                Message::system("Reply with exactly one word."),
                Message::user("What is the capital of France?"),
            ])
            .await
            .expect("live request should succeed");

        println!(
            "model replied: {:?}, usage: {:?}",
            completion.content, completion.usage
        );
        assert!(
            !completion.content.trim().is_empty(),
            "reply should not be empty"
        );
        assert!(
            completion.content.to_lowercase().contains("paris"),
            "unexpected reply: {}",
            completion.content
        );
    }

    /// Confirms a real request actually streams — more than one delta, arriving
    /// incrementally — and reassembles to a coherent reply.
    #[tokio::test]
    #[ignore = "requires OPENROUTER_API_KEY and makes a real API call"]
    async fn live_stream_arrives_incrementally() {
        let _ = dotenvy::dotenv();
        let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
        let model = std::env::var("OPENROUTER_MODEL")
            .unwrap_or_else(|_| crate::config::DEFAULT_MODEL.to_string());
        let client = Client::new(key, model).unwrap();

        let messages = [Message::user("Count from 1 to 10, space separated.")];
        let stream = client.open_stream(&messages).await.unwrap();
        futures_util::pin_mut!(stream);

        let mut deltas = 0usize;
        let mut content = String::new();
        let mut usage = None;
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                StreamEvent::Delta(d) => {
                    deltas += 1;
                    content.push_str(&d);
                }
                StreamEvent::Done { usage: u } => {
                    if u.is_some() {
                        usage = u;
                    }
                }
            }
        }

        println!("received {deltas} deltas, usage: {usage:?}\nassembled: {content:?}");
        assert!(deltas > 1, "a streamed reply should arrive in many chunks");
        assert!(content.contains('5'), "unexpected reply: {content}");
    }
}
