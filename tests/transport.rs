//! Transport failures a mock HTTP server can't produce: stalled and broken bodies.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kunobi_decision::{Client, Error, RetryPolicy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How the raw server answers a given attempt (1-based).
#[derive(Clone, Copy)]
enum Reply {
    /// Send headers promising a body, then go silent.
    StallBody(u16),
    /// Send headers and half the body, then drop the connection.
    BreakBody(u16),
    /// A complete `{"models":[]}` response.
    Models,
}

/// Serve one connection per attempt, answering according to `plan`; returns the base URL.
async fn serve(plan: fn(usize) -> Reply) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let attempt = counter.fetch_add(1, Ordering::SeqCst) + 1;
            tokio::spawn(async move {
                let mut request = vec![0u8; 8192];
                let _ = socket.read(&mut request).await;
                let head = |status: u16, len: usize| {
                    format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {len}\r\nx-typesafe-request-id: req_{attempt}\r\nconnection: close\r\n\r\n"
                    )
                };
                match plan(attempt) {
                    Reply::StallBody(status) => {
                        let _ = socket.write_all(head(status, 100).as_bytes()).await;
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    Reply::BreakBody(status) => {
                        let _ = socket.write_all(head(status, 100).as_bytes()).await;
                        let _ = socket.write_all(b"[").await;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Reply::Models => {
                        let body = r#"{"models":[]}"#;
                        let _ = socket.write_all(head(200, body.len()).as_bytes()).await;
                        let _ = socket.write_all(body.as_bytes()).await;
                    }
                }
            });
        }
    });
    (url, attempts)
}

fn client(url: &str, retry: RetryPolicy) -> Client {
    Client::builder()
        .api_key("k")
        .base_url(url)
        .timeout(Duration::from_millis(200))
        .retry(RetryPolicy {
            backoff_initial: Duration::ZERO,
            ..retry
        })
        .build()
        .unwrap()
}

#[tokio::test]
async fn a_stalled_body_times_out_for_success_and_error_statuses() {
    for status in [200, 503] {
        let (url, attempts) = match status {
            200 => serve(|_| Reply::StallBody(200)).await,
            _ => serve(|_| Reply::StallBody(503)).await,
        };
        let client = client(
            &url,
            RetryPolicy {
                max_retries: 1,
                ..RetryPolicy::default()
            },
        );

        let started = Instant::now();
        let err = client.models().list().await.unwrap_err();
        assert!(err.is_timeout(), "status {status}: {err:?}");
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "status {status}");
        assert!(started.elapsed() < Duration::from_secs(5));

        let err = client
            .models()
            .list()
            .max_retries(0)
            .raw()
            .await
            .unwrap_err();
        assert!(err.is_timeout(), "raw path, status {status}: {err:?}");
    }
}

#[tokio::test]
async fn a_broken_body_is_a_connection_error_and_honors_the_policy() {
    for status in [200, 503] {
        let (url, attempts) = match status {
            200 => serve(|_| Reply::BreakBody(200)).await,
            _ => serve(|_| Reply::BreakBody(503)).await,
        };
        let policy = RetryPolicy {
            retry_connection_errors: false,
            ..RetryPolicy::default()
        };
        let err = client(&url, policy).models().list().await.unwrap_err();
        assert!(
            matches!(err, Error::Connection { .. }),
            "status {status}: {err:?}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "status {status}");
    }
}

#[tokio::test]
async fn a_broken_body_is_retried_and_reports_the_successful_attempt() {
    let (url, attempts) = serve(|attempt| {
        if attempt == 1 {
            Reply::BreakBody(503)
        } else {
            Reply::Models
        }
    })
    .await;
    let response = client(
        &url,
        RetryPolicy {
            max_retries: 1,
            ..RetryPolicy::default()
        },
    )
    .models()
    .list()
    .with_response()
    .await
    .unwrap();
    assert!(response.data.is_empty());
    assert_eq!(response.request_id.as_deref(), Some("req_2"));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}
