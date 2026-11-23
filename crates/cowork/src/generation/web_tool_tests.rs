use agent::{
    AgentHost, CallAuthorization, Decision,
    tool::{BuiltinToolOutput, PreparedCall, ToolError, ToolErrorKind, collect_output},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tools::WebAccess;

use super::*;

fn host() -> Arc<CoworkHost> {
    Arc::new(CoworkHost::new(
        Arc::new(sandbox::ThreadSandbox::new(
            crate::test_support::unused_sandboxes(),
            "web",
        )),
        Default::default(),
    ))
}

fn configure(host: &CoworkHost, state: WebAccess) {
    *AsRef::<parking_lot::RwLock<WebAccess>>::as_ref(host).write() = state;
}

fn endpoint(listener: &TcpListener) -> WebAccess {
    let root = format!("http://{}", listener.local_addr().unwrap());
    WebAccess::Configured(firecrawl::Client::new(
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
        &root,
    ))
}

fn prepare(
    host: &Arc<CoworkHost>,
    name: &str,
    args: Value,
) -> (Decision, PreparedCall<CallAccess>) {
    let registry = registry();
    let tool = registry
        .get(name)
        .expect("available even when not configured");
    let call = tool.prepare(args.clone(), host).unwrap();
    let decision = host.authorize(&CallAuthorization {
        tool: &tool.descriptor,
        arguments: &args,
        access: call.access(),
        origin: &RunSource::Host,
        permissions: &(),
        grants: &[],
    });
    (decision, call)
}

async fn execute(call: PreparedCall<CallAccess>) -> Result<Value, ToolError> {
    let stream = call
        .execute(ToolCallId("web".into()), Default::default())
        .await;
    let BuiltinToolOutput::Dynamic(value) = collect_output(stream).await?.output;
    Ok(value)
}

/// Serve exactly one request and check what the adapter instructed Firecrawl to do.
async fn respond(listener: &TcpListener, path: &str, body: Value, response: Value) {
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("request reached expected endpoint")
        .unwrap();
    let mut reader = BufReader::new(&mut socket);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(line.trim_end(), format!("POST {path} HTTP/1.1"));
    let mut length = 0;
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = n.trim().parse().unwrap();
        }
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), body);
    let payload = response.to_string();
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{payload}",
                payload.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn prepared_web_calls_require_host_approval_and_a_still_enabled_service() {
    let host = host();
    host.sandbox().sandboxes().set_project(
        "web",
        sandbox::Project {
            folders: vec![],
            access: sandbox::Access::Network,
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let args = json!({"url":"https://example.org/"});
    for name in [ReadWebPage::NAME, ListSiteUrls::NAME] {
        for (state, reason) in [
            (WebAccess::Disabled, "disabled"),
            (WebAccess::MissingEndpoint, "configure"),
        ] {
            configure(&host, endpoint(&listener));
            let (decision, pending) = prepare(&host, name, args.clone());
            assert_eq!(decision, Decision::Ask);
            configure(&host, state);
            assert!(
                matches!(prepare(&host, name, args.clone()).0, Decision::Deny(message) if message.contains(reason))
            );
            assert_eq!(
                execute(pending).await.unwrap_err().kind,
                ToolErrorKind::PermissionDenied
            );
        }
    }
    let listener = listener.into_std().unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn approved_reads_keep_the_original_endpoint_and_full_partial_content() {
    let host = host();
    let a = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let b = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = "https://example.org/";
    let args = json!({"url":url});
    let request = json!({"url":url,"formats":["markdown"],"onlyMainContent":true});
    configure(&host, endpoint(&a));
    let (decision, pending) = prepare(&host, ReadWebPage::NAME, args.clone());
    assert_eq!(decision, Decision::Ask);
    configure(&host, endpoint(&b));
    let text = "long page\n".repeat(10_000);
    let (result, ()) = tokio::join!(
        execute(pending),
        respond(
            &a,
            "/v2/scrape",
            request.clone(),
            json!({"data":{"metadata":{"sourceURL":url,"title":"Docs"},"markdown":text,"warning":"Partial page"}})
        )
    );
    assert_eq!(
        result.unwrap(),
        json!({"source_url":url,"title":"Docs","markdown":text,"warning":"Partial page"})
    );
    let (_, next) = prepare(&host, ReadWebPage::NAME, args);
    let (result, ()) = tokio::join!(
        execute(next),
        respond(
            &b,
            "/v2/scrape",
            request,
            json!({"data":{"blocked":true,"markdown":" "}})
        )
    );
    assert_eq!(result.unwrap_err().kind, ToolErrorKind::Execution);
}

#[tokio::test]
async fn mapping_bounds_and_filters_results_and_distinguishes_no_matches_from_failure() {
    let host = host();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    configure(&host, endpoint(&listener));
    let url = "https://example.org/";
    let args = json!({"url":url,"filter":"docs","limit":1});
    for (links, expected) in [
        (json!([]), json!([])),
        (json!([{"url":"a"},{"url":"b"}]), json!(["a"])),
    ] {
        let (decision, call) = prepare(&host, ListSiteUrls::NAME, args.clone());
        assert_eq!(decision, Decision::Ask);
        let (result, ()) = tokio::join!(
            execute(call),
            respond(
                &listener,
                "/v2/map",
                json!({"url":url,"search":"docs","limit":1}),
                json!({"links":links})
            )
        );
        assert_eq!(result.unwrap()["urls"], expected);
    }
    drop(listener);
    let (_, call) = prepare(&host, ListSiteUrls::NAME, args);
    assert_eq!(
        execute(call).await.unwrap_err().kind,
        ToolErrorKind::Execution
    );
}
