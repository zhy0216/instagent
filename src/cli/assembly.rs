//! CLI smoke regression for the shared library task runner.

#[cfg(test)]
mod tests {
    use crate::cli::fixtures::{self, Env};
    use crate::cli::handlers;
    use instagent::message::Content;
    use instagent::session::Session;
    use serde_json::Value;
    use std::path::Path;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::Mock;
    use wiremock::MockServer;

    fn fake_provider_at(plugin: &Path, base_url: &str) {
        fixtures::add_provider(plugin, fixtures::fake_openai_provider(base_url));
    }

    #[tokio::test]
    async fn run_task_end_to_end_with_fake_openai_provider() {
        let env = Env::new();
        let server = MockServer::start().await;
        let provider_dir = env.user_plugin("fakeprov");
        fake_provider_at(&provider_dir, &format!("{}/v1", server.uri()));
        env.write_config_yaml("provider: fake\nmodel: test-model\n");
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi there\"},\"finish_reason\":null}]}\n\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5}}\n\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                   data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(fixtures::sse_body(sse))
            .expect(1)
            .mount(&server)
            .await;

        handlers::run(crate::cli::RunArgs {
            task: Some("say hi".to_string()),
            cwd: Some(env.cwd.path().to_path_buf()),
            ..Default::default()
        })
        .await
        .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], "test-model");
        let tools: Vec<&str> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(tools.contains(&"shell"), "{tools:?}");
        assert!(tools.contains(&"load_skill"), "{tools:?}");

        let headers = Session::list().unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].provider, "fake");
        assert_eq!(headers[0].model, "test-model");
        let session = Session::resume(&headers[0].id).unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, instagent::message::Role::User);
        match &session.messages[1].content[0] {
            Content::Text(text) => assert_eq!(text, "hi there"),
            other => panic!("expected text, got {other:?}"),
        }
        assert_eq!(session.messages[1].usage.unwrap().output, 5);
    }
}
