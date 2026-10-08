/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

fn client(http2_only: bool) -> Client {
    let mut builder = legacy::Client::builder(TokioExecutor::new());
    builder.http2_only(http2_only);
    builder.build(hyper_tls::HttpsConnector::new())
}

async fn read_http_request(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let byte = socket.read_u8().await.unwrap();
        bytes.push(byte);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let headers = String::from_utf8(bytes.clone()).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let offset = bytes.len();
    bytes.resize(offset + length, 0);
    socket.read_exact(&mut bytes[offset..]).await.unwrap();
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn http1_preserves_post_headers_body_and_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "http://{}/submit?x=1",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut socket).await;
        assert!(request.starts_with("POST /submit?x=1 HTTP/1.1\r\n"));
        assert!(request
            .to_lowercase()
            .contains("user-agent: explicit-agent\r\n"));
        assert!(request.to_lowercase().contains("x-test: request\r\n"));
        assert!(request.ends_with("\r\n\r\npayload"));
        socket.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\nX-Test: response\r\nConnection: close\r\n\r\nok").await.unwrap();
    });
    let mut request = Request::new(Method::Post, url.clone())
        .header("User-Agent", "explicit-agent")
        .unwrap()
        .header("X-Test", "request")
        .unwrap();
    request.body = Some(b"payload".to_vec());
    let response = make_request_inner(
        client(false),
        request,
        ClientSettings {
            user_agent: Some("fallback-agent".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(response.status, 201);
    assert_eq!(response.url, url);
    assert_eq!(response.request_method, Method::Post);
    assert_eq!(response.headers.get("X-Test"), Some("response"));
    assert_eq!(response.body, b"ok");
    server.await.unwrap();
}

#[tokio::test]
async fn redirects_preserve_settings_and_enforce_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let location = format!("{base}/final");
    let server = tokio::spawn(async move {
        for path in ["/start", "/final"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            assert!(request.starts_with(&format!("GET {path} HTTP/1.1")));
            assert!(request
                .to_lowercase()
                .contains("user-agent: settings-agent\r\n"));
            let reply = format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    let result = make_request_inner(
        client(false),
        Request::new(Method::Get, Url::parse(&format!("{base}/start")).unwrap()),
        ClientSettings {
            redirect_limit: 1,
            user_agent: Some("settings-agent".into()),
            ..Default::default()
        },
    )
    .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Too many redirections"));
    server.await.unwrap();
}

#[tokio::test]
async fn redirect_returns_final_url_and_body_with_timeout_disabled() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let final_url = Url::parse(&format!("{base}/final")).unwrap();
    let location = final_url.to_string();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        assert!(read_http_request(&mut first)
            .await
            .starts_with("GET /start HTTP/1.1"));
        let reply = format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        first.write_all(reply.as_bytes()).await.unwrap();
        let (mut second, _) = listener.accept().await.unwrap();
        assert!(read_http_request(&mut second)
            .await
            .starts_with("GET /final HTTP/1.1"));
        second
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
    });
    let response = timeout(
        Duration::from_secs(5),
        make_request_inner(
            client(false),
            Request::new(Method::Get, Url::parse(&format!("{base}/start")).unwrap()),
            ClientSettings {
                timeout: 0,
                ..Default::default()
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.url, final_url);
    assert_eq!(response.body, b"ok");
    server.await.unwrap();
}

#[test]
fn timeout_covers_response_body() {
    let caller = tokio::runtime::Runtime::new().unwrap();
    let backend = HyperBackend {
        runtime: tokio::runtime::Runtime::new().unwrap(),
        client: caller.block_on(async { client(false) }),
    };
    caller.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_http_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let response = timeout(
            Duration::from_secs(5),
            backend.send_request(
                Request::new(Method::Get, url),
                ClientSettings {
                    timeout: 100,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
        assert!(response
            .unwrap_err()
            .to_string()
            .contains("Request timeout"));
        server.abort();
    });
}

async fn write_frame(socket: &mut TcpStream, kind: u8, flags: u8, stream: u32, payload: &[u8]) {
    let length = payload.len() as u32;
    let mut bytes = vec![
        (length >> 16) as u8,
        (length >> 8) as u8,
        length as u8,
        kind,
        flags,
    ];
    bytes.extend_from_slice(&stream.to_be_bytes());
    bytes.extend_from_slice(payload);
    socket.write_all(&bytes).await.unwrap();
}

async fn read_frame(socket: &mut TcpStream) -> (u8, u8, u32, Vec<u8>) {
    let mut header = [0; 9];
    socket.read_exact(&mut header).await.unwrap();
    let length = ((header[0] as usize) << 16) | ((header[1] as usize) << 8) | header[2] as usize;
    let stream = u32::from_be_bytes(header[5..9].try_into().unwrap()) & 0x7fffffff;
    let mut payload = vec![0; length];
    socket.read_exact(&mut payload).await.unwrap();
    (header[3], header[4], stream, payload)
}

async fn h2_peer(listener: TcpListener, padded: bool, flood: bool) {
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut preface = [0; 24];
    socket.read_exact(&mut preface).await.unwrap();
    assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    write_frame(&mut socket, 4, 0, 0, &[]).await;
    let stream = loop {
        let (kind, flags, stream, _) = read_frame(&mut socket).await;
        if kind == 4 && flags & 1 == 0 {
            // Acknowledge the client's SETTINGS.
            write_frame(&mut socket, 4, 1, 0, &[]).await;
        } else if kind == 1 {
            break stream;
        }
    };
    // HPACK static-table entry 8 is ":status: 200".
    write_frame(&mut socket, 1, 4, stream, &[0x88]).await;
    if flood {
        let mut frames = Vec::new();
        for _ in 0..102 {
            let payload: &[u8] = if padded { &[1, 0] } else { &[] };
            frames.extend_from_slice(&[0, 0, payload.len() as u8, 0, if padded { 8 } else { 0 }]);
            frames.extend_from_slice(&stream.to_be_bytes());
            frames.extend_from_slice(payload);
        }
        socket.write_all(&frames).await.unwrap();
        loop {
            let (kind, _, _, payload) = read_frame(&mut socket).await;
            if kind == 7 {
                assert_eq!(u32::from_be_bytes(payload[4..8].try_into().unwrap()), 11);
                assert_eq!(&payload[8..], b"too_many_data_frames");
                break;
            }
        }
    } else {
        write_frame(&mut socket, 0, 0, stream, &[]).await;
        write_frame(&mut socket, 0, 0, stream, b"ok").await;
        write_frame(&mut socket, 0, 1, stream, &[]).await;
        // Keep the connection alive until the client consumes END_STREAM.
        std::future::pending::<()>().await;
    }
}

async fn rejects_empty_data_flood(padded: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(h2_peer(listener, padded, true));
    let result = timeout(
        Duration::from_secs(5),
        make_single_request(&client(true), Request::new(Method::Get, url)),
    )
    .await
    .expect("empty DATA flood must be rejected, not hang");
    assert!(result.is_err());
    timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn rejects_empty_http2_data_flood() {
    rejects_empty_data_flood(false).await;
}

#[tokio::test]
async fn rejects_padding_only_http2_data_flood() {
    rejects_empty_data_flood(true).await;
}

#[tokio::test]
async fn http2_accepts_empty_data_and_empty_end_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let server = tokio::spawn(h2_peer(listener, false, false));
    let response = timeout(
        Duration::from_secs(5),
        make_single_request(&client(true), Request::new(Method::Get, url)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.body(), b"ok");
    server.abort();
}
