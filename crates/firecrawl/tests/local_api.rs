use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

use firecrawl::{Client, Error, Format, MapRequest, ScrapeRequest};
use serde_json::{Value, json};

// One exchange with a loopback stand-in: verify the wire request, not just
// whether an independently constructed serde value has the right fields.
fn endpoint(method_path: &'static str, body: Value, status: u16, reply: Value) -> Client {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let root = format!("http://{}/", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim_end(), format!("{method_path} HTTP/1.1"));
        let mut length = 0;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        if !body.is_null() {
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), body);
        }
        let payload = reply.to_string();
        write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}", payload.len()).unwrap();
    });
    Client::new(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        &root,
    )
}

#[tokio::test]
async fn scrape_and_map_use_distinct_v2_envelopes() {
    let url = "https://example.com";
    let client = endpoint(
        "POST /v2/scrape",
        json!({"url":url,"formats":["rawHtml"],"onlyMainContent":false}),
        200,
        json!({"success":true,"data":{"rawHtml":"<p>Hi</p>", "warning":"partial page"}}),
    );
    let doc = client
        .scrape(&ScrapeRequest {
            url: url.into(),
            formats: vec![Format::RawHtml],
            only_main_content: Some(false),
        })
        .await
        .unwrap();
    assert_eq!(doc.raw_html.as_deref(), Some("<p>Hi</p>"));
    assert_eq!(doc.warning.as_deref(), Some("partial page"));
    let client = endpoint(
        "POST /v2/map",
        json!({"url":url,"limit":5}),
        200,
        json!({"success":true,"links":[{"url":"https://example.com/docs"}]}),
    );
    let links = client
        .map(&MapRequest {
            url: url.into(),
            search: None,
            limit: Some(5),
        })
        .await
        .unwrap();
    assert_eq!(links[0].url, "https://example.com/docs");
}

#[tokio::test]
async fn health_is_unversioned_and_preserves_api_errors() {
    let client = endpoint(
        "GET /health",
        Value::Null,
        401,
        json!({"success":false,"error":"Invalid token","code":"UNAUTHORIZED"}),
    );
    let Error::Api {
        status,
        code,
        message,
    } = client.health().await.unwrap_err()
    else {
        panic!("expected API error")
    };
    assert_eq!(status, 401);
    assert_eq!(code.as_deref(), Some("UNAUTHORIZED"));
    assert_eq!(message, "Invalid token");
}
