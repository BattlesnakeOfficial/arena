//! Isolated process-global subscriber test: ordinary ops tracing never enters capture.
use std::{
    io::Write,
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn product_capture_excludes_plain_ops_events() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();

    // This binary has one test. Set these before starting Tokio or the HTTP
    // server, so no concurrent environment reads race the fixture.
    unsafe {
        std::env::set_var("POSTHOG_PROJECT_TOKEN", "phc_fixture");
        std::env::set_var("POSTHOG_HOST", &host);
        std::env::set_var("RUST_LOG", "info");
        std::env::remove_var("EYES_ORG_ID");
        std::env::remove_var("EYES_APP_ID");
        for name in [
            "OTEL_EXPORTER_OTLP_HEADERS",
            "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
            "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
            "OTEL_EXPORTER_OTLP_COMPRESSION",
            "OTEL_EXPORTER_OTLP_LOGS_COMPRESSION",
            "OTEL_EXPORTER_OTLP_TRACES_COMPRESSION",
        ] {
            std::env::remove_var(name);
        }
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let captured = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let router = axum::Router::new().fallback({
            let captured = captured.clone();
            move |uri: axum::http::Uri, body: axum::body::Body| {
                let captured = captured.clone();
                async move {
                    // Read the entire request before responding. An early 2xx
                    // can cancel reqwest's send before capture gets delivered.
                    let bytes = axum::body::to_bytes(body, 10 * 1024 * 1024)
                        .await
                        .unwrap();
                    if uri.path() == "/i/v1/analytics/events" {
                        captured
                            .lock()
                            .unwrap()
                            .push(serde_json::from_slice(&bytes).unwrap());
                    }
                    axum::Json(serde_json::json!({"results": {}}))
                }
            }
        });
        let server = tokio::spawn(async move {
            axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), router)
                .await
                .unwrap();
        });

        let output = Output::default();
        let writer = output.clone();
        let telemetry = cja::setup::TracingConfig::new("arena")
            .stdout_layer(
                tracing_subscriber::fmt::layer().with_writer(move || writer.clone()),
            )
            .init()
            .unwrap();
        let user = uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
        cja::product_event!(name: "achievement_earned", user_id: Some(&user), message: "Achievement earned", properties: { first = true, slug = "winner" });
        tracing::info!(event_type = "game_phase", phase = "ready", "phase started");
        tracing::info!(event_type = "job_enqueued", job_id = 42, "job queued");
        telemetry.shutdown().await.unwrap();
        server.abort();

        let output = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("Achievement earned"), "{output}");
        assert!(output.contains("game_phase"), "{output}");
        assert!(output.contains("job_enqueued"), "{output}");
        assert!(!output.contains("capture request failed"), "{output}");
        assert!(!output.contains("capture rejected events"), "{output}");
        let bodies = captured.lock().unwrap();
        let events: Vec<_> = bodies
            .iter()
            .flat_map(|body| body["batch"].as_array().unwrap())
            .collect();
        assert_eq!(events.len(), 1, "{bodies:?}");
        assert_eq!(events[0]["event"], "achievement_earned");
        assert_eq!(events[0]["distinct_id"], user.to_string());
        assert_eq!(events[0]["properties"]["service.name"], "arena");
        assert_eq!(events[0]["properties"]["first"], true);
        assert_eq!(events[0]["properties"]["slug"], "winner");
    });
}
