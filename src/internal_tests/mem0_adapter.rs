use crate::memory_client::{Memory, MemoryOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn mem0_search_hydrates_hits_without_per_record_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let request_count = Arc::new(AtomicUsize::new(0));
    let server_count = Arc::clone(&request_count);
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(350);
        while Instant::now() < deadline {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            server_count.fetch_add(1, Ordering::SeqCst);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let path = request_line.split_whitespace().nth(1).unwrap_or_default();
            let mut content_length = 0usize;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    break;
                }
                if let Some(value) = header
                    .strip_prefix("content-length:")
                    .or_else(|| header.strip_prefix("Content-Length:"))
                {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            let route = path.split('?').next().unwrap_or_default();
            let response = if route.ends_with("/search") {
                serde_json::json!({"data": [{
                    "id": "memory-1",
                    "content": "Alice likes tea",
                    "score": 0.9,
                    "created_at": "2026-01-01T00:00:00Z",
                    "role": "user",
                    "user_id": "alice",
                    "session_id": "run-1",
                    "metadata": {"kind": "mem0-json:\"preference\""},
                    "expires_at_ms": null,
                    "supersedes_id": null,
                    "relation_evidence": [],
                    "comparison_operands": [],
                    "claim_states": []
                }]})
            } else {
                serde_json::json!({"id":"memory-1","content":"user: Alice likes tea","role":"user","user_id":"alice","session_id":"run-1","created_at":"2026-01-01T00:00:00Z","expires_at_ms":null,"supersedes_id":null,"metadata":{}})
            };
            let response = serde_json::to_vec(&response).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            )
            .unwrap();
            stream.write_all(&response).unwrap();
        }
    });

    let client = Memory::remote(
        format!("http://{address}"),
        MemoryOptions {
            request_timeout: Duration::from_secs(2),
            ..Default::default()
        },
    )
    .unwrap();
    let result = crate::mem0_search_with_client(&client, "tea", "alice", 10, None, None).unwrap();
    assert_eq!(result["results"].as_array().unwrap().len(), 1);
    assert_eq!(result["results"][0]["metadata"]["kind"], "preference");
    server.join().unwrap();
    assert_eq!(request_count.load(Ordering::SeqCst), 1);
}
