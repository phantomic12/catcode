use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn mock_provider() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request);
            let body = r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            break;
        }
    });
    (format!("http://{address}/v1"), stop, handle)
}

fn temp_workspace() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("catcode-protocol-harness-{nonce}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn read_http_request(stream: &mut std::net::TcpStream) -> String {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut header_end = None;
    while let Ok(read) = stream.read(&mut buffer) {
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if header_end.is_none() {
            header_end = bytes.windows(4).position(|window| window == b"\r\n\r\n");
        }
        if let Some(end) = header_end {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let content_length = headers
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|line| line.split_once(':'))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if bytes.len() >= end + 4 + content_length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn write_json_response(stream: &mut std::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(), body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn write_error_response(stream: &mut std::net::TcpStream, status: u16, body: &str) {
    let reason = match status {
        401 => "Unauthorized",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(), body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn write_sse_chunk(stream: &mut std::net::TcpStream, payload: &str) -> bool {
    let chunk = format!("{:x}\r\n{}\r\n", payload.len(), payload);
    stream.write_all(chunk.as_bytes()).is_ok() && stream.flush().is_ok()
}

fn scripted_stream_provider() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut chat_requests = 0_u32;
        while std::time::Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            let first_line = request.lines().next().unwrap_or_default();
            if first_line.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            if !first_line.starts_with("POST ") {
                write_json_response(&mut stream, r#"{"error":"unsupported"}"#);
                continue;
            }
            chat_requests += 1;
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
            );
            let _ = stream.flush();
            if chat_requests == 1 {
                let first = format!(
                    "data: {}\n\n",
                    serde_json::json!({"choices":[{"delta":{"content":"OLD_FIRST"}}]})
                );
                let _ = write_sse_chunk(&mut stream, &first);
                thread::sleep(Duration::from_millis(1500));
                let late = format!(
                    "data: {}\n\n",
                    serde_json::json!({"choices":[{"delta":{"content":"OLD_LATE"}}]})
                );
                let _ = write_sse_chunk(&mut stream, &late);
            } else {
                let response = format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices":[{"delta":{"content":"NEW_OK"}}]}),
                    serde_json::json!({
                        "choices":[{"delta":{},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":10,"completion_tokens":1}
                    })
                );
                let _ = write_sse_chunk(&mut stream, &response);
            }
            let _ = stream.write_all(b"0\r\n\r\n");
            let _ = stream.flush();
        }
    });
    (format!("http://{address}/v1"), stop, handle)
}

fn approval_tool_provider() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut chat_requests = 0_u32;
        while Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            let first_line = request.lines().next().unwrap_or_default();
            if first_line.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            chat_requests += 1;
            let payload = if chat_requests == 1 {
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({
                        "choices":[{
                            "delta":{"tool_calls":[{
                                "index":0,
                                "id":"call-write",
                                "type":"function",
                                "function":{
                                    "name":"write_file",
                                    "arguments":"{\"path\":\"approved.txt\",\"content\":\"approved\\n\"}"
                                }
                            }]},
                            "finish_reason":"tool_calls"
                        }],
                        "usage":{"prompt_tokens":10,"completion_tokens":5}
                    })
                )
            } else {
                format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices":[{"delta":{"content":"APPROVAL_OK"}}]}),
                    serde_json::json!({
                        "choices":[{"delta":{},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":15,"completion_tokens":2}
                    })
                )
            };
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
            );
            let _ = stream.flush();
            let _ = write_sse_chunk(&mut stream, &payload);
            let _ = stream.write_all(b"0\r\n\r\n");
            let _ = stream.flush();
        }
    });
    (format!("http://{address}/v1"), stop, handle)
}

fn retry_provider(fatal: bool) -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut posts = 0_u32;
        while Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            if request.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            posts += 1;
            if fatal {
                write_error_response(
                    &mut stream,
                    401,
                    r#"{"error":{"message":"invalid credentials"}}"#,
                );
                continue;
            }
            if posts == 1 {
                write_error_response(
                    &mut stream,
                    503,
                    r#"{"error":{"message":"temporarily unavailable"}}"#,
                );
                continue;
            }
            let payload = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices":[{"delta":{"content":"RETRY_OK"}}]}),
                serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1}})
            );
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
            let _ = write_sse_chunk(&mut stream, &payload);
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    });
    (format!("http://{address}/v1"), stop, handle)
}

/// First chat POST streams a partial delta then drops the TCP connection
/// mid-body (no terminal frame, no zero-sized chunk). Second POST completes
/// cleanly. Exercises mid-stream body-drop retry after partial tokens.
fn midstream_drop_then_success_provider() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut posts = 0_u32;
        while Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            if request.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            posts += 1;
            if posts == 1 {
                // Partial assistant text, then hard-close so the client sees a
                // body/connection read error (mirrors h2 internal-error drops).
                let partial = format!(
                    "data: {}\n\n",
                    serde_json::json!({"choices":[{"delta":{"content":"PARTIAL "}}]})
                );
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
                let _ = write_sse_chunk(&mut stream, &partial);
                // Deliberately omit the terminating 0-chunk and RST/close.
                drop(stream);
                continue;
            }
            let payload = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices":[{"delta":{"content":"FULL_OK"}}]}),
                serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1}})
            );
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
            let _ = write_sse_chunk(&mut stream, &payload);
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    });
    (format!("http://{address}/v1"), stop, handle)
}

/// First chat POST emits partial assistant text then stalls past the SSE idle
/// timeout; second POST completes. Exercises mid-stream idle-timeout retry
/// with discard_partial (same recovery path as body-drop).
fn midstream_idle_then_success_provider() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        // Idle floor is 10s for OpenAI; allow room for hang + backoff + retry.
        let deadline = Instant::now() + Duration::from_secs(45);
        let mut posts = 0_u32;
        while Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            if request.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            posts += 1;
            if posts == 1 {
                let partial = format!(
                    "data: {}\n\n",
                    serde_json::json!({"choices":[{"delta":{"content":"IDLE_PARTIAL "}}]})
                );
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: keep-alive\r\n\r\n",
                );
                let _ = write_sse_chunk(&mut stream, &partial);
                // Hold the connection open past the client's idle timeout so
                // stream_turn surfaces "stream idle timeout (...)".
                let hold_deadline = Instant::now() + Duration::from_secs(14);
                while Instant::now() < hold_deadline && !thread_stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(50));
                }
                drop(stream);
                continue;
            }
            let payload = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices":[{"delta":{"content":"IDLE_OK"}}]}),
                serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1}})
            );
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
            let _ = write_sse_chunk(&mut stream, &payload);
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    });
    (format!("http://{address}/v1"), stop, handle)
}

/// First chat POST returns HTTP 504; second completes. Confirms gateway
/// timeout stays on the retryable status path at the initial POST layer.
fn gateway_timeout_then_success_provider() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut posts = 0_u32;
        while Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            if request.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            posts += 1;
            if posts == 1 {
                write_error_response(
                    &mut stream,
                    504,
                    r#"{"error":{"message":"gateway timeout"}}"#,
                );
                continue;
            }
            let payload = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices":[{"delta":{"content":"GATE_OK"}}]}),
                serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1}})
            );
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
            let _ = write_sse_chunk(&mut stream, &payload);
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    });
    (format!("http://{address}/v1"), stop, handle)
}

fn bash_tool_provider() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut posts = 0_u32;
        while Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            if request.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            posts += 1;
            let payload = if posts == 1 {
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-bash","type":"function","function":{"name":"bash","arguments":"{\"command\":\"sleep 10\",\"timeout\":15}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":2}})
                )
            } else {
                format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices":[{"delta":{"content":"AFTER_BASH_CANCEL"}}]}),
                    serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1}})
                )
            };
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
            let _ = write_sse_chunk(&mut stream, &payload);
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    });
    (format!("http://{address}/v1"), stop, handle)
}

fn tool_wave_provider(writes: bool) -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut posts = 0_u32;
        while Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            if request.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            posts += 1;
            let payload = if posts == 1 {
                let calls = if writes {
                    serde_json::json!([
                        {"index":0,"id":"call-write-a","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"wave-a.txt\",\"content\":\"A\"}"}},
                        {"index":1,"id":"call-write-b","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"wave-b.txt\",\"content\":\"B\"}"}}
                    ])
                } else {
                    serde_json::json!([
                        {"index":0,"id":"call-read-a","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}},
                        {"index":1,"id":"call-read-b","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"b.txt\"}"}}
                    ])
                };
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({
                        "choices":[{"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}],
                        "usage":{"prompt_tokens":5,"completion_tokens":3}
                    })
                )
            } else {
                format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices":[{"delta":{"content":"WAVE_OK"}}]}),
                    serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":8,"completion_tokens":1}})
                )
            };
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
            let _ = write_sse_chunk(&mut stream, &payload);
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    });
    (format!("http://{address}/v1"), stop, handle)
}

/// Accepts both the goal planner and its speculative scout, then leaves their
/// streams open until lifecycle cancellation drops the client connections.
fn delayed_goal_provider() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut streams = Vec::new();
        while Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            if request.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
            let _ = stream.flush();
            streams.push(stream);
        }
        drop(streams);
    });
    (format!("http://{address}/v1"), stop, handle)
}

fn plugin_timeout_provider() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut posts = 0_u32;
        while Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            if request.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            posts += 1;
            let payload = if posts == 1 {
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-slow-plugin","type":"function","function":{"name":"slow_plugin","arguments":"{}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":2}})
                )
            } else {
                format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices":[{"delta":{"content":"PLUGIN_TIMEOUT_HANDLED"}}]}),
                    serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":8,"completion_tokens":1}})
                )
            };
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
            let _ = write_sse_chunk(&mut stream, &payload);
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    });
    (format!("http://{address}/v1"), stop, handle)
}

fn repeat_text_provider() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut posts = 0_u32;
        while Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            if request.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            posts += 1;
            let text = format!("BASIC_TEXT_{posts}");
            let payload = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices":[{"delta":{"content":text}}]}),
                serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":8,"completion_tokens":1}})
            );
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
            let _ = write_sse_chunk(&mut stream, &payload);
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    });
    (format!("http://{address}/v1"), stop, handle)
}

fn subagent_cancel_provider() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut posts = 0_u32;
        let mut held = Vec::new();
        while Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            if request.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            posts += 1;
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
            if posts == 1 {
                let payload = format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-child","type":"function","function":{"name":"subagent","arguments":"{\"agent\":\"scout\",\"task\":\"wait for cancellation\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":2}})
                );
                let _ = write_sse_chunk(&mut stream, &payload);
                let _ = stream.write_all(b"0\r\n\r\n");
            } else if posts == 2 {
                let _ = stream.flush();
                held.push(stream);
            } else {
                let payload = format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices":[{"delta":{"content":"AFTER_SUBAGENT_CANCEL"}}]}),
                    serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1}})
                );
                let _ = write_sse_chunk(&mut stream, &payload);
                let _ = stream.write_all(b"0\r\n\r\n");
            }
        }
        drop(held);
    });
    (format!("http://{address}/v1"), stop, handle)
}

fn cancellable_wave_provider() -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut posts = 0_u32;
        while Instant::now() < deadline && !thread_stop.load(Ordering::Relaxed) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            let request = read_http_request(&mut stream);
            if request.starts_with("GET ") {
                write_json_response(
                    &mut stream,
                    r#"{"mock-model":{"display_name":"Mock","capabilities":{"context_window":8192,"recommended_max_tokens":1024}}}"#,
                );
                continue;
            }
            posts += 1;
            let payload = if posts == 1 {
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-load-diag","type":"function","function":{"name":"load_tools","arguments":"{\"tools\":[\"diagnostics\"]}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":5,"completion_tokens":2}})
                )
            } else {
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::json!({"choices":[{"delta":{"tool_calls":[
                        {"index":0,"id":"call-diag-a","type":"function","function":{"name":"diagnostics","arguments":"{}"}},
                        {"index":1,"id":"call-diag-b","type":"function","function":{"name":"diagnostics","arguments":"{}"}}
                    ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":8,"completion_tokens":3}})
                )
            };
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n");
            let _ = write_sse_chunk(&mut stream, &payload);
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    });
    (format!("http://{address}/v1"), stop, handle)
}

struct CoreHarness {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    events: Receiver<Value>,
}

impl CoreHarness {
    fn start(workspace: &std::path::Path, base_url: &str) -> Self {
        Self::start_with_approval(workspace, base_url, "never")
    }

    fn start_with_approval(workspace: &std::path::Path, base_url: &str, approval: &str) -> Self {
        Self::start_with_options(workspace, base_url, approval, None)
    }

    fn start_with_idle_timeout(
        workspace: &std::path::Path,
        base_url: &str,
        idle_timeout_secs: u64,
    ) -> Self {
        Self::start_with_options(workspace, base_url, "never", Some(idle_timeout_secs))
    }

    fn start_with_options(
        workspace: &std::path::Path,
        base_url: &str,
        approval: &str,
        idle_timeout_secs: Option<u64>,
    ) -> Self {
        let session = workspace.join("session.jsonl");
        let config = workspace.join("config.json");
        std::fs::write(&config, "{}\n").unwrap();
        let inherited_path = std::env::var("PATH").unwrap_or_default();
        let harness_path = format!("{}:{inherited_path}", workspace.join("bin").display());
        let mut args = vec![
            "--workspace".to_string(),
            workspace.to_str().unwrap().to_string(),
            "--session".to_string(),
            session.to_str().unwrap().to_string(),
            "--config".to_string(),
            config.to_str().unwrap().to_string(),
            "--base-url".to_string(),
            base_url.to_string(),
            "--approval".to_string(),
            approval.to_string(),
            "--trust-project-plugins".to_string(),
        ];
        if let Some(secs) = idle_timeout_secs {
            args.push("--idle-timeout".to_string());
            args.push(secs.to_string());
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_core"))
            .args(&args)
            // Self-contained provider: core's first-run init stages a default
            // config (with a default provider) into ~/.config/catalyst-code
            // when that dir is absent — as in CI's clean HOME — and that staged
            // provider then takes precedence over the mock this harness points
            // at via --base-url (the legacy base_url path only applies when NO
            // providers are configured). Inject the mock as an explicit provider
            // so the harness is independent of the host's global config / first-run
            // staging, on the dev box and in CI alike.
            .env(
                "UMANS_PROVIDERS",
                format!(
                    r#"[{{"name":"protocol_harness","kind":"openai","base_url":"{base_url}","api_key_env":"PROTOCOL_HARNESS_KEY"}}]"#
                ),
            )
            .env("UMANS_ACTIVE_PROVIDER", "protocol_harness")
            .env("PROTOCOL_HARNESS_KEY", "test-key")
            .env("PATH", harness_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, events) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(event) = serde_json::from_str(&line) {
                    if sender.send(event).is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            child,
            stdin,
            events,
        }
    }

    fn send(&mut self, command: Value) {
        writeln!(self.stdin, "{command}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn send_raw(&mut self, command: &str) {
        writeln!(self.stdin, "{command}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn until(&self, event_type: &str) -> Vec<Value> {
        self.until_where(event_type, |event| event["type"] == event_type)
    }

    fn until_where(&self, description: &str, predicate: impl Fn(&Value) -> bool) -> Vec<Value> {
        self.until_where_within(description, Duration::from_secs(10), predicate)
    }

    fn until_where_within(
        &self,
        description: &str,
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Vec<Value> {
        let mut events = Vec::new();
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = self.events.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!(
                    "core did not emit {description} before timeout ({error}); events: {}",
                    serde_json::to_string(&events).unwrap()
                )
            });
            let done = predicate(&event);
            events.push(event);
            if done {
                return events;
            }
        }
    }
}

#[test]
fn invalid_command_returns_structured_error_and_transport_remains_usable() {
    let (base_url, stop_server, server) = mock_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");

    core.send_raw(r#"{"type":"send","prompt":17}"#);
    let invalid = core.until_where("structured invalid command", |event| {
        event["type"] == "error" && event["code"] == "invalid_command"
    });
    let error = invalid.last().unwrap();
    assert!(error["message"]
        .as_str()
        .is_some_and(|message| message.starts_with("bad command:")));
    assert_eq!(error["protocol_version"], 2);
    assert!(error["session_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));

    core.send(serde_json::json!({"type":"runtime_status"}));
    assert_eq!(
        core.until("runtime_status").last().unwrap()["type"],
        "runtime_status"
    );

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

impl Drop for CoreHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn real_core_negotiates_v2_over_jsonl() {
    let (base_url, stop_server, server) = mock_provider();
    let workspace = temp_workspace();
    let mut child = Command::new(env!("CARGO_BIN_EXE_core"))
        .args([
            "--workspace",
            workspace.to_str().unwrap(),
            "--base-url",
            &base_url,
            "--approval",
            "never",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"type":"init","protocol_version":2,"client":{{"name":"integration-test","version":"1","capabilities":["run_ids"]}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut saw_ready = false;
    let mut hello = None;
    for line in BufReader::new(stdout).lines().take(30) {
        let event: Value = serde_json::from_str(&line.unwrap()).unwrap();
        saw_ready |= event["type"] == "ready";
        if event["type"] == "protocol_hello" {
            hello = Some(event);
            break;
        }
    }
    let hello = hello.expect("protocol_hello from real core subprocess");
    assert!(saw_ready);
    assert_eq!(hello["protocol_version"], 2);
    assert!(hello["session_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    assert!(hello["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cap| cap == "stale_event_rejection"));

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(workspace);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
}

#[test]
fn checked_in_client_and_event_fixtures_match_real_core_negotiation() {
    let (base_url, stop_server, server) = mock_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    let command: Value =
        serde_json::from_str(include_str!("../../protocol/fixtures/init-v2.json")).unwrap();
    let expected: Value = serde_json::from_str(include_str!(
        "../../protocol/fixtures/protocol-hello-v2.json"
    ))
    .unwrap();
    core.send(command);
    let events = core.until("protocol_hello");
    let hello = events.last().unwrap();
    assert_eq!(hello["type"], expected["type"]);
    assert_eq!(hello["protocol_version"], expected["protocol_version"]);
    for capability in expected["capabilities"].as_array().unwrap() {
        assert!(hello["capabilities"]
            .as_array()
            .unwrap()
            .contains(capability));
    }
    assert!(hello["session_id"]
        .as_str()
        .is_some_and(|session| session.starts_with("session-")));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn new_session_during_stream_rejects_old_deltas_and_remains_usable() {
    let (base_url, stop_server, server) = scripted_stream_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({
        "type":"init",
        "protocol_version":2,
        "client":{"name":"lifecycle-test","version":"1","capabilities":["run_ids"]}
    }));
    let init = core.until("protocol_hello");
    assert!(
        init.iter().any(|event| {
            event["type"] == "ready"
                && event["models"]
                    .as_array()
                    .is_some_and(|models| models.iter().any(|model| model["id"] == "mock-model"))
        }),
        "mock model was not discovered: {init:?}"
    );
    let old_session = init
        .last()
        .and_then(|event| event["session_id"].as_str())
        .unwrap()
        .to_string();

    core.send(serde_json::json!({
        "type":"send", "prompt":"old run", "model":"mock-model"
    }));
    let first_stream = core.until_where("the first old-run delta", |event| {
        event["type"] == "delta" && event["text"] == "OLD_FIRST"
    });
    let old_run = first_stream
        .last()
        .and_then(|event| event["run_id"].as_str())
        .expect("old stream run id")
        .to_string();

    core.send(serde_json::json!({"type":"runtime_status"}));
    let status_events = core.until("runtime_status");
    let status = status_events.last().unwrap();
    assert_eq!(status["run_id"], old_run);
    assert!(status["resources"].as_array().is_some_and(|resources| {
        resources.iter().any(|resource| {
            resource["label"] == "foreground_agent_turn"
                && resource["run_id"] == old_run
                && resource["cancelled"] == false
        })
    }));

    core.send(serde_json::json!({"type":"new_session","path":"replacement.jsonl"}));
    let replacement_events = core.until("session_changed");
    assert!(replacement_events.iter().any(|event| {
        event["type"] == "run_cancelled"
            && event["run_id"] == old_run
            && event["reason"] == "new_session"
    }));
    let changed = replacement_events.last().unwrap();
    let new_session = changed["session_id"].as_str().unwrap();
    assert_ne!(new_session, old_session);
    assert!(!replacement_events
        .iter()
        .any(|event| event["type"] == "delta" && event["text"] == "OLD_LATE"));

    core.send(serde_json::json!({
        "type":"send", "prompt":"new run", "model":"mock-model"
    }));
    let new_events = core.until("done");
    assert!(new_events
        .iter()
        .any(|event| event["type"] == "delta" && event["text"] == "NEW_OK"));
    assert!(!new_events
        .iter()
        .any(|event| event["type"] == "delta" && event["text"] == "OLD_LATE"));
    for event in new_events
        .iter()
        .filter(|event| event.get("run_id").is_some())
    {
        assert_eq!(event["session_id"], new_session);
        assert_ne!(event["run_id"], old_run);
    }

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn abort_during_stream_cancels_old_run_and_allows_next_turn() {
    let (base_url, stop_server, server) = scripted_stream_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({
        "type":"send", "prompt":"abort this", "model":"mock-model"
    }));
    let first = core.until_where("the first abort-test delta", |event| {
        event["type"] == "delta" && event["text"] == "OLD_FIRST"
    });
    let old_run = first.last().unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    core.send(serde_json::json!({"type":"abort"}));
    let cancelled = core.until("run_cancelled");
    assert!(cancelled.iter().any(|event| {
        event["type"] == "run_cancelled" && event["run_id"] == old_run && event["reason"] == "abort"
    }));

    core.send(serde_json::json!({
        "type":"send", "prompt":"replacement", "model":"mock-model"
    }));
    let replacement = core.until("done");
    assert!(replacement
        .iter()
        .any(|event| event["type"] == "delta" && event["text"] == "NEW_OK"));
    assert!(!replacement
        .iter()
        .any(|event| event["type"] == "delta" && event["text"] == "OLD_LATE"));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn approval_executes_destructive_tool_with_bound_identity() {
    let (base_url, stop_server, server) = approval_tool_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start_with_approval(&workspace, &base_url, "always");
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    let init = core.until("protocol_hello");
    let session_id = init.last().unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    core.send(serde_json::json!({
        "type":"send", "prompt":"write the fixture", "model":"mock-model"
    }));
    let request_events = core.until("approval_request");
    let request = request_events.last().unwrap();
    assert_eq!(request["tool"], "write_file");
    assert_eq!(request["session_id"], session_id);
    assert!(request["run_id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(request["diff"]
        .as_str()
        .is_some_and(|diff| diff.contains("approved")));
    let request_id = request["request_id"].as_str().unwrap().to_string();
    core.send(serde_json::json!({
        "type":"approve", "request_id":request_id, "decision":"yes"
    }));
    let completion = core.until("done");
    assert!(completion.iter().any(|event| {
        event["type"] == "tool_result" && event["ok"] == true && event["status"] == "success"
    }));
    assert!(completion
        .iter()
        .any(|event| event["type"] == "delta" && event["text"] == "APPROVAL_OK"));
    assert_eq!(
        std::fs::read_to_string(workspace.join("approved.txt")).unwrap(),
        "approved\n"
    );

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn approval_denial_has_stable_status_and_does_not_execute_tool() {
    let (base_url, stop_server, server) = approval_tool_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start_with_approval(&workspace, &base_url, "always");
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({
        "type":"send", "prompt":"do not write the fixture", "model":"mock-model"
    }));
    let request = core.until("approval_request");
    let request_id = request.last().unwrap()["request_id"].as_str().unwrap();
    core.send(serde_json::json!({
        "type":"approve", "request_id":request_id, "decision":"no"
    }));
    let completion = core.until("done");
    assert!(completion.iter().any(|event| {
        event["type"] == "tool_result" && event["ok"] == false && event["status"] == "denied"
    }));
    assert!(!workspace.join("approved.txt").exists());

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn reset_during_approval_invalidates_request_and_prevents_execution() {
    let (base_url, stop_server, server) = approval_tool_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start_with_approval(&workspace, &base_url, "always");
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({
        "type":"send", "prompt":"request a write", "model":"mock-model"
    }));
    let request = core.until("approval_request");
    let stale_request = request.last().unwrap()["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    let old_run = request.last().unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    core.send(serde_json::json!({"type":"reset"}));
    let reset = core.until("reset");
    assert!(reset.iter().any(|event| {
        event["type"] == "run_cancelled" && event["run_id"] == old_run && event["reason"] == "reset"
    }));
    core.send(serde_json::json!({
        "type":"approve", "request_id":stale_request, "decision":"yes"
    }));
    let rejection = core.until("error");
    assert!(rejection.last().unwrap()["message"]
        .as_str()
        .is_some_and(|message| message.contains("unknown") || message.contains("expired")));
    assert!(!workspace.join("approved.txt").exists());

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn unknown_client_capability_is_tolerated_during_negotiation() {
    let (base_url, stop_server, server) = mock_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({
        "type":"init", "protocol_version":2,
        "client":{"name":"future-client","version":"99","capabilities":["future_optional_feature"]}
    }));
    let events = core.until("protocol_hello");
    let hello = events.last().unwrap();
    assert_eq!(hello["protocol_version"], 2);
    assert!(hello["capabilities"].as_array().is_some());

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn provider_retry_then_success_preserves_event_order() {
    let (base_url, stop_server, server) = retry_provider(false);
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({"type":"send","prompt":"retry","model":"mock-model"}));
    let events = core.until("done");
    let retry_index = events
        .iter()
        .position(|event| event["type"] == "http_retry")
        .unwrap();
    let delta_index = events
        .iter()
        .position(|event| event["type"] == "delta" && event["text"] == "RETRY_OK")
        .unwrap();
    let done_index = events
        .iter()
        .position(|event| event["type"] == "done")
        .unwrap();
    assert!(retry_index < delta_index && delta_index < done_index);

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn provider_midstream_body_drop_retries_after_partial_and_succeeds() {
    let (base_url, stop_server, server) = midstream_drop_then_success_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({"type":"send","prompt":"midstream","model":"mock-model"}));
    let events = core.until("done");

    let partial_index = events
        .iter()
        .position(|event| event["type"] == "delta" && event["text"] == "PARTIAL ")
        .expect("partial delta from first attempt");
    let retry = events
        .iter()
        .find(|event| event["type"] == "http_retry")
        .expect("http_retry after mid-stream body drop");
    assert_eq!(retry["discard_partial"], true);
    assert_eq!(
        retry["reason"],
        "retryable stream error after partial output"
    );
    let retry_index = events
        .iter()
        .position(|event| event["type"] == "http_retry")
        .unwrap();
    let full_index = events
        .iter()
        .position(|event| event["type"] == "delta" && event["text"] == "FULL_OK")
        .expect("FULL_OK delta from successful retry");
    assert!(partial_index < retry_index && retry_index < full_index);
    // No terminal error — the turn recovered.
    assert!(!events.iter().any(|event| event["type"] == "error"));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn provider_midstream_idle_timeout_retries_after_partial_and_succeeds() {
    let (base_url, stop_server, server) = midstream_idle_then_success_provider();
    let workspace = temp_workspace();
    // OpenAI path floors idle at 10s regardless of lower config values.
    let mut core = CoreHarness::start_with_idle_timeout(&workspace, &base_url, 10);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({"type":"send","prompt":"idle-midstream","model":"mock-model"}));
    // 10s idle hang + ~500ms backoff + second stream must finish within this.
    let events =
        core.until_where_within("done after idle retry", Duration::from_secs(30), |event| {
            event["type"] == "done"
        });

    let partial_index = events
        .iter()
        .position(|event| event["type"] == "delta" && event["text"] == "IDLE_PARTIAL ")
        .expect("partial delta before idle timeout");
    let retry = events
        .iter()
        .find(|event| event["type"] == "http_retry")
        .expect("http_retry after stream idle timeout");
    assert_eq!(retry["discard_partial"], true);
    assert_eq!(
        retry["reason"],
        "retryable stream error after partial output"
    );
    let reason = retry["reason"].as_str().unwrap_or_default();
    // The retry event reason is the user-facing class; the underlying error
    // string is not re-emitted, but the path must have classified idle timeout.
    assert!(reason.contains("partial"));
    let retry_index = events
        .iter()
        .position(|event| event["type"] == "http_retry")
        .unwrap();
    let full_index = events
        .iter()
        .position(|event| event["type"] == "delta" && event["text"] == "IDLE_OK")
        .expect("IDLE_OK delta from successful retry");
    assert!(partial_index < retry_index && retry_index < full_index);
    assert!(!events.iter().any(|event| event["type"] == "error"));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn provider_504_gateway_timeout_retries_then_succeeds() {
    let (base_url, stop_server, server) = gateway_timeout_then_success_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({"type":"send","prompt":"gate","model":"mock-model"}));
    let events = core.until("done");
    let retry_index = events
        .iter()
        .position(|event| event["type"] == "http_retry")
        .expect("http_retry on 504");
    // Initial-POST 504 retry carries status, not discard_partial.
    let retry = &events[retry_index];
    assert_eq!(retry["status"], 504);
    let delta_index = events
        .iter()
        .position(|event| event["type"] == "delta" && event["text"] == "GATE_OK")
        .expect("GATE_OK after 504 retry");
    assert!(retry_index < delta_index);
    assert!(!events.iter().any(|event| event["type"] == "error"));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn provider_authentication_error_is_fatal_and_redacted() {
    let (base_url, stop_server, server) = retry_provider(true);
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({"type":"send","prompt":"fail","model":"mock-model"}));
    let events = core.until("error");
    let error = events.last().unwrap()["message"].as_str().unwrap();
    assert!(error.contains("authentication") || error.contains("401"));
    assert!(!error.to_ascii_lowercase().contains("bearer"));
    assert!(!events.iter().any(|event| event["type"] == "http_retry"));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn abort_during_bash_cancels_subprocess_resource_promptly() {
    let (base_url, stop_server, server) = bash_tool_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({"type":"send","prompt":"run bash","model":"mock-model"}));
    let started = core.until_where("bash tool call", |event| {
        event["type"] == "tool_call" && event["name"] == "bash"
    });
    let run_id = started.last().unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let began = Instant::now();
    core.send(serde_json::json!({"type":"abort"}));
    let cancelled = core.until("run_cancelled");
    assert!(cancelled
        .iter()
        .any(|event| event["run_id"] == run_id && event["reason"] == "abort"));
    assert!(began.elapsed() < Duration::from_secs(3));
    core.send(serde_json::json!({"type":"runtime_status"}));
    let status = core.until("runtime_status");
    assert!(status.last().unwrap()["resources"]
        .as_array()
        .is_some_and(|resources| {
            !resources
                .iter()
                .any(|resource| resource["run_id"] == run_id && resource["cancelled"] == false)
        }));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn new_session_during_bash_terminates_old_process_and_new_session_works() {
    let (base_url, stop_server, server) = bash_tool_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    let init = core.until("protocol_hello");
    let old_session = init.last().unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    core.send(serde_json::json!({"type":"send","prompt":"run bash","model":"mock-model"}));
    let started = core.until_where("bash tool call", |event| {
        event["type"] == "tool_call" && event["name"] == "bash"
    });
    let old_run = started.last().unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    core.send(serde_json::json!({"type":"new_session","path":"after-bash.jsonl"}));
    let changed = core.until("session_changed");
    let new_session = changed.last().unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(old_session, new_session);
    assert!(changed.iter().any(|event| {
        event["type"] == "run_cancelled"
            && event["run_id"] == old_run
            && event["reason"] == "new_session"
    }));
    core.send(serde_json::json!({"type":"send","prompt":"continue","model":"mock-model"}));
    let completion = core.until("done");
    assert!(completion
        .iter()
        .any(|event| { event["type"] == "delta" && event["text"] == "AFTER_BASH_CANCEL" }));
    assert!(completion
        .iter()
        .filter(|event| event.get("run_id").is_some())
        .all(|event| { event["session_id"] == new_session && event["run_id"] != old_run }));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn multiple_readonly_tools_complete_as_one_ordered_wave() {
    let (base_url, stop_server, server) = tool_wave_provider(false);
    let workspace = temp_workspace();
    std::fs::write(workspace.join("a.txt"), "alpha").unwrap();
    std::fs::write(workspace.join("b.txt"), "beta").unwrap();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({"type":"send","prompt":"read both","model":"mock-model"}));
    let events = core.until("done");
    let results = events
        .iter()
        .filter(|event| event["type"] == "tool_result")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["id"], "call-read-a");
    assert_eq!(results[1]["id"], "call-read-b");
    assert!(results[0]["output"]
        .as_str()
        .is_some_and(|output| output.contains("alpha")));
    assert!(results[1]["output"]
        .as_str()
        .is_some_and(|output| output.contains("beta")));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn one_readonly_tool_failure_does_not_cancel_the_ordered_wave() {
    let (base_url, stop_server, server) = tool_wave_provider(false);
    let workspace = temp_workspace();
    std::fs::write(workspace.join("a.txt"), "alpha").unwrap();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({"type":"send","prompt":"read both","model":"mock-model"}));
    let events = core.until("done");
    let results = events
        .iter()
        .filter(|event| event["type"] == "tool_result")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["id"], "call-read-a");
    assert_eq!(results[0]["status"], "success");
    assert_eq!(results[1]["id"], "call-read-b");
    assert_eq!(results[1]["status"], "failed");
    assert!(events
        .iter()
        .any(|event| event["type"] == "delta" && event["text"] == "WAVE_OK"));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[cfg(unix)]
#[test]
fn cancellation_mid_readonly_wave_kills_owned_subprocesses_and_emits_no_results() {
    use std::os::unix::fs::PermissionsExt;

    let (base_url, stop_server, server) = cancellable_wave_provider();
    let workspace = temp_workspace();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname='wave-fixture'\nversion='0.1.0'\n",
    )
    .unwrap();
    let bin = workspace.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake_cargo = bin.join("cargo");
    std::fs::write(
        &fake_cargo,
        format!(
            "#!/bin/sh\npid_file=\"{}/diag-$$.pid\"\necho $$ > \"$pid_file\"\nexec sleep 10\n",
            workspace.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake_cargo).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fake_cargo, permissions).unwrap();

    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({
        "type":"send", "prompt":"run diagnostic wave", "model":"mock-model"
    }));
    let started = core.until_where("both diagnostic calls", |event| {
        event["type"] == "tool_call" && event["id"] == "call-diag-b"
    });
    let run_id = started
        .iter()
        .find(|event| event["type"] == "tool_call" && event["id"] == "call-diag-a")
        .and_then(|event| event["run_id"].as_str())
        .unwrap()
        .to_string();
    let pid_deadline = Instant::now() + Duration::from_secs(2);
    let pids = loop {
        let pids = std::fs::read_dir(&workspace)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("diag-"))
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .filter_map(|pid| pid.trim().parse::<u32>().ok())
            .collect::<Vec<_>>();
        if pids.len() >= 2 || Instant::now() >= pid_deadline {
            break pids;
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(pids.len(), 2, "both diagnostic subprocesses started");

    let began = Instant::now();
    core.send(serde_json::json!({"type":"abort"}));
    let cancelled = core.until("run_cancelled");
    assert!(began.elapsed() < Duration::from_secs(3));
    assert!(cancelled.iter().any(|event| {
        event["type"] == "run_cancelled" && event["run_id"] == run_id && event["reason"] == "abort"
    }));
    assert!(!cancelled.iter().any(|event| {
        event["type"] == "tool_result"
            && matches!(event["id"].as_str(), Some("call-diag-a" | "call-diag-b"))
    }));

    let gone_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if pids
            .iter()
            .all(|pid| !std::path::Path::new(&format!("/proc/{pid}")).exists())
        {
            break;
        }
        assert!(
            Instant::now() < gone_deadline,
            "diagnostic subprocess survived abort"
        );
        thread::sleep(Duration::from_millis(10));
    }

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn multiple_writes_execute_sequentially_in_model_order() {
    let (base_url, stop_server, server) = tool_wave_provider(true);
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({"type":"send","prompt":"write both","model":"mock-model"}));
    let events = core.until("done");
    let results = events
        .iter()
        .filter(|event| event["type"] == "tool_result")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["id"], "call-write-a");
    assert_eq!(results[1]["id"], "call-write-b");
    assert_eq!(
        std::fs::read_to_string(workspace.join("wave-a.txt")).unwrap(),
        "A"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("wave-b.txt")).unwrap(),
        "B"
    );

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn cancelling_goal_cancels_planner_and_speculative_subagent() {
    let (base_url, stop_server, server) = delayed_goal_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({
        "type":"start_goal",
        "goal":"exercise cancellation ownership",
        "model":"mock-model",
        "auto_deploy":true
    }));
    let started = core.until("subagent_start");
    let subagent_id = started.last().unwrap()["run_id"]
        .as_str()
        .expect("speculative scout run id")
        .to_string();
    let goal_id = started
        .iter()
        .find(|event| event["type"] == "goal_state")
        .and_then(|event| event["id"].as_str())
        .expect("goal id")
        .to_string();
    assert_eq!(
        started.last().unwrap()["parent_run_id"].as_str(),
        Some(goal_id.as_str()),
        "goal-owned subagent must identify its goal parent"
    );

    core.send(serde_json::json!({"type":"cancel_goal"}));
    let mut events = core.until_where("goal cancellation acknowledgement", |event| {
        event["type"] == "info"
            && event["message"]
                .as_str()
                .is_some_and(|message| message == "goal cancelled")
    });
    if !events.iter().any(|event| {
        event["type"] == "subagent_done"
            && event["run_id"] == subagent_id
            && event["state"] == "cancelled"
    }) {
        events.extend(core.until_where("cancelled speculative scout", |event| {
            event["type"] == "subagent_done"
                && event["run_id"] == subagent_id
                && event["state"] == "cancelled"
        }));
    }
    assert!(events
        .iter()
        .any(|event| { event["type"] == "run_cancelled" && event["reason"] == "goal_cancelled" }));
    assert!(events.iter().any(|event| {
        event["type"] == "subagent_done"
            && event["run_id"] == subagent_id
            && event["state"] == "cancelled"
    }));

    core.send(serde_json::json!({"type":"runtime_status"}));
    let status = core.until("runtime_status");
    assert!(status.last().unwrap()["resources"]
        .as_array()
        .is_some_and(|resources| resources.iter().all(|resource| {
            resource["label"] != "goal_speculative_scout" || resource["cancelled"] == true
        })));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[cfg(unix)]
#[test]
fn plugin_tool_timeout_is_bounded_and_reported_with_stable_status() {
    use std::os::unix::fs::PermissionsExt;

    let (base_url, stop_server, server) = plugin_timeout_provider();
    let workspace = temp_workspace();
    let plugin = workspace.join(".catalyst-code/plugins/slow-plugin");
    std::fs::create_dir_all(plugin.join("tools")).unwrap();
    std::fs::write(
        plugin.join("plugin.json"),
        r#"{
          "name":"slow-plugin",
          "version":"1.0.0",
          "tools":[{
            "name":"slow_plugin",
            "description":"timeout fixture",
            "parameters":{"type":"object","properties":{}},
            "script":"tools/run.sh",
            "kind":"readonly",
            "timeout_ms":200
          }]
        }"#,
    )
    .unwrap();
    let script = plugin.join("tools/run.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nsleep 5\necho '{\"ok\":true,\"output\":\"late\"}'\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();

    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    let began = Instant::now();
    core.send(serde_json::json!({
        "type":"send", "prompt":"run timeout fixture", "model":"mock-model"
    }));
    let events = core.until("done");
    assert!(began.elapsed() < Duration::from_secs(3));
    assert!(events.iter().any(|event| {
        event["type"] == "tool_result"
            && event["id"] == "call-slow-plugin"
            && event["ok"] == false
            && event["status"] == "timed_out"
            && event["output"]
                .as_str()
                .is_some_and(|output| output.contains("timed out"))
    }));
    assert!(events
        .iter()
        .any(|event| { event["type"] == "delta" && event["text"] == "PLUGIN_TIMEOUT_HANDLED" }));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn basic_text_turns_can_be_compacted_with_ordered_events() {
    let (base_url, stop_server, server) = repeat_text_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");

    for index in 1..=2 {
        core.send(serde_json::json!({
            "type":"send", "prompt":format!("turn {index}"), "model":"mock-model"
        }));
        let events = core.until("done");
        assert!(events.iter().any(|event| {
            event["type"] == "delta" && event["text"] == format!("BASIC_TEXT_{index}")
        }));
    }

    core.send(serde_json::json!({"type":"compact"}));
    let events = core.until("compacted");
    let compacting = events
        .iter()
        .position(|event| event["type"] == "compacting")
        .expect("compacting event");
    let compacted = events
        .iter()
        .position(|event| event["type"] == "compacted")
        .expect("compacted event");
    assert!(compacting < compacted);
    assert_eq!(events[compacted]["summary_chars"], 0);
    assert!(events[compacted]["after_tokens"].as_u64().is_some());

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn automatic_threshold_compaction_runs_through_turn_and_persists_entry() {
    let (base_url, stop_server, server) = repeat_text_provider();
    let workspace = temp_workspace();
    let session = workspace.join("session.jsonl");
    let large = "x".repeat(4_000);
    let mut journal = String::from("{\"_session_version\":2}\n");
    for _ in 0..12 {
        journal.push_str(&serde_json::json!({"role":"user","content":large}).to_string());
        journal.push('\n');
    }
    std::fs::write(&session, journal).unwrap();
    let artifacts = workspace.join("artifacts");
    std::fs::create_dir_all(&artifacts).unwrap();
    let artifact_path = |run_id: &str| {
        let digest = Sha256::digest(run_id.as_bytes());
        let hash = digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        artifacts.join(format!("job-{hash}.json"))
    };
    let keep_path = artifact_path("job-keep");
    let drop_path = artifact_path("job-drop");
    std::fs::write(&keep_path, r#"{"run_id":"job-keep"}"#).unwrap();
    std::fs::write(&drop_path, r#"{"run_id":"job-drop"}"#).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&session)
        .unwrap()
        .write_all(b"{\"role\":\"assistant\",\"content\":\"artifact://job-keep.json\"}\n")
        .unwrap();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({"type":"send","prompt":"trigger automatic compaction","model":"mock-model"}));
    let events = core.until("done");
    assert!(events.iter().any(|event| event["type"] == "compacting"));
    assert!(events.iter().any(|event| event["type"] == "compacted"));
    assert!(std::fs::read_to_string(&session)
        .unwrap()
        .contains("\"_compaction\""));
    assert!(
        keep_path.exists(),
        "referenced artifact must survive automatic compaction"
    );
    assert!(!drop_path.exists(), "unreferenced artifact must be shaken");
    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn aborting_parent_turn_cancels_direct_subagent() {
    let (base_url, stop_server, server) = subagent_cancel_provider();
    let workspace = temp_workspace();
    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    core.until("protocol_hello");
    core.send(serde_json::json!({
        "type":"send", "prompt":"delegate", "model":"mock-model"
    }));
    let started = core.until("subagent_start");
    let child_id = started.last().unwrap()["run_id"]
        .as_str()
        .expect("child run id")
        .to_string();
    let parent_id = started
        .iter()
        .find(|event| event["type"] == "tool_call" && event["id"] == "call-child")
        .and_then(|event| event["run_id"].as_str())
        .expect("parent run id")
        .to_string();
    assert_eq!(
        started.last().unwrap()["parent_run_id"].as_str(),
        Some(parent_id.as_str()),
        "subagent_start must identify the owning foreground run"
    );

    core.send(serde_json::json!({"type":"abort"}));
    let events = core.until("run_cancelled");
    assert!(events.iter().any(|event| {
        event["type"] == "run_cancelled"
            && event["run_id"] == parent_id
            && event["reason"] == "abort"
    }));

    core.send(serde_json::json!({"type":"runtime_status"}));
    let status = core.until("runtime_status");
    assert!(!status.iter().any(|event| {
        event["type"]
            .as_str()
            .is_some_and(|kind| kind.starts_with("subagent_"))
            && event["run_id"] == child_id
    }));
    assert!(status.last().unwrap()["resources"]
        .as_array()
        .is_some_and(|resources| resources
            .iter()
            .all(|resource| { resource["run_id"] != parent_id || resource["cancelled"] == true })));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}

#[test]
fn startup_reports_and_terminalizes_interrupted_session_run() {
    let (base_url, stop_server, server) = mock_provider();
    let workspace = temp_workspace();
    let session = workspace.join("session.jsonl");
    std::fs::write(
        &session,
        concat!(
            "{\"_session_version\":2}\n",
            "{\"role\":\"user\",\"content\":\"recover me\"}\n",
            "{\"_run\":{\"session_id\":\"session-old\",\"run_id\":\"run-crashed\",\"kind\":\"tool\",\"parent_run_id\":\"parent-old\",\"tool_call_id\":\"call-crashed\",\"state\":\"started\",\"timestamp_ms\":1}}\n",
            "{\"role\":\"assistant\",\"content\":"
        ),
    )
    .unwrap();

    let mut core = CoreHarness::start(&workspace, &base_url);
    core.send(serde_json::json!({"type":"init","protocol_version":2}));
    let recovery = core.until("session_recovered");
    let event = recovery.last().unwrap();
    assert!(event["interrupted_runs"]
        .as_array()
        .is_some_and(|runs| runs.iter().any(|run| run == "run-crashed")));
    assert!(event["interrupted_activities"]
        .as_array()
        .is_some_and(|activities| activities.iter().any(|activity| {
            activity["run_id"] == "run-crashed"
                && activity["kind"] == "tool"
                && activity["parent_run_id"] == "parent-old"
                && activity["tool_call_id"] == "call-crashed"
        })));
    assert!(event["warnings"].as_array().is_some_and(|warnings| {
        warnings.iter().any(|warning| {
            warning
                .as_str()
                .is_some_and(|text| text.contains("truncated"))
        })
    }));
    let persisted = std::fs::read_to_string(&session).unwrap();
    assert!(persisted.contains("\"run_id\":\"run-crashed\""));
    assert!(persisted.contains("\"state\":\"interrupted\""));

    drop(core);
    stop_server.store(true, Ordering::Relaxed);
    let _ = server.join();
    let _ = std::fs::remove_dir_all(workspace);
}
