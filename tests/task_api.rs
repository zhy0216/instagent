//! Invoke the complete lifecycle through the library, without CLI or signals.
use instagent::agent::task::{self, DiagnosticCode, RunRequest, RunStatus, TaskInput};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn run(request: RunRequest, cancel: CancellationToken) -> task::RunReport {
    fn require_send<T: Send>(future: T) -> T {
        future
    }
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx); // Results must remain complete even with no progress observer.
    require_send(task::run(request, cancel, tx)).await
}

#[tokio::test]
async fn library_task_lifecycle_results_resume_requirements_and_cancellation() {
    // This integration binary has one environment-using test; all state is local.
    let root = tempfile::tempdir().unwrap();
    for (key, directory) in [("CONFIG", "config"), ("DATA", "data"), ("AGENTS", "agents")] {
        std::fs::create_dir(root.path().join(directory)).unwrap();
        std::env::set_var(format!("INSTAGENT_{key}_DIR"), root.path().join(directory));
    }
    std::env::remove_var("INSTAGENT_PROVIDER");
    std::env::remove_var("INSTAGENT_MODEL");
    let plugin = root.path().join("plugin");
    std::fs::create_dir_all(plugin.join("dev.instagent/providers")).unwrap();
    std::fs::write(plugin.join("plugin.json"), serde_json::json!({"$schema":instagent::plugin::manifest::PLUGIN_SCHEMA_URL,"name":"fixture","version":"1.0.0"}).to_string()).unwrap();
    let server = MockServer::start().await;
    std::fs::write(plugin.join("dev.instagent/providers/fake.json"), serde_json::json!({"name":"fake","engine":"openai","base_url":format!("{}/v1",server.uri())}).to_string()).unwrap();
    let response = ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
        .set_body_string("data: {\"choices\":[{\"delta\":{\"content\":\"exact answer\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n");
    Mock::given(method("POST"))
        .respond_with(response.clone())
        .mount(&server)
        .await;
    let mut request = RunRequest::new(TaskInput::Text("first".into()));
    request.cwd = Some(root.path().join("work"));
    request.provider = Some("fake".into());
    request.model = Some("model".into());
    request.plugin_paths.push(plugin);
    request.capabilities.plugins = Some(vec!["fixture".into()]);
    request.capabilities.tools = Some(vec!["read".into()]);
    let first = run(request.clone(), CancellationToken::new()).await;
    assert_eq!(first.status, RunStatus::Completed, "{first:?}");
    assert_eq!(first.output, "exact answer");
    let body: serde_json::Value =
        serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(body["tools"][0]["function"]["name"], "read");
    let mut resume = request.clone();
    resume.resume = first.session_id.clone();
    resume.input = TaskInput::Text("continue".into());
    let second = run(resume, CancellationToken::new()).await;
    assert_eq!(second.status, RunStatus::Completed, "{second:?}");
    assert_eq!(second.session_id, first.session_id);
    assert_eq!(
        instagent::session::Session::resume(first.session_id.as_deref().unwrap())
            .unwrap()
            .messages
            .len(),
        4
    );

    server.reset().await;
    let mut missing = request.clone();
    missing.capabilities.required_tools = vec!["shell".into()];
    let report = run(missing, CancellationToken::new()).await;
    assert_eq!(report.status, RunStatus::Failed);
    assert!(report.session_id.is_none());
    assert!(report
        .diagnostics
        .iter()
        .any(|d| d.code == DiagnosticCode::RequiredToolMissing && d.source == "shell"));
    assert!(server.received_requests().await.unwrap().is_empty());

    Mock::given(method("POST"))
        .respond_with(response.set_delay(Duration::from_secs(10)))
        .mount(&server)
        .await;
    let mut timed = request.clone();
    timed.timeout_secs = 1;
    let parent = CancellationToken::new();
    let report = run(timed, parent.clone()).await;
    assert_eq!(report.status, RunStatus::TimedOut);
    assert!(
        !parent.is_cancelled(),
        "task deadline must not cancel its host"
    );
    assert!(report.output.is_empty() && report.usage.is_none());
    let cancel = CancellationToken::new();
    let before = server.received_requests().await.unwrap().len();
    let cancel_after_request = async {
        loop {
            if server.received_requests().await.unwrap().len() > before {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel.cancel();
    };
    let (report, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(run(request, cancel.clone()), cancel_after_request)
    })
    .await
    .unwrap();
    assert_eq!(report.status, RunStatus::Cancelled);
    assert!(report.output.is_empty());
}
