use std::sync::{Arc, Mutex};

use super::{Config, GlobalClient, contract, telemetry};

#[tokio::test]
async fn test_existing_requests_recheck_opt_out_without_extra_requests()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let preferences = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&preferences);
    let service = axum::Router::new().route(
        "/rift/rest/v1/capabilities",
        axum::routing::get(move |headers: axum::http::HeaderMap| {
            let received = Arc::clone(&received);
            async move {
                received.lock().expect("preferences").push(
                    headers
                        .get(reqwest::header::DNT)
                        .map(|value| value.to_str().expect("DNT").to_owned()),
                );
                ([("content-type", "application/json")], "{}")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move { axum::serve(listener, service).await });
    let mut config = Config {
        endpoint: format!("http://{address}/rift/rest"),
        ..Config::default()
    };
    config.telemetry = telemetry::Policy::for_test(root.path(), home.path());
    let client = GlobalClient::new_with_token(config, None)?;
    let project = root.path().join("rift.toml");
    std::fs::create_dir(home.path().join(".rift"))?;
    let global = home.path().join(telemetry::USER_CONFIGURATION_FILE);
    for (path, content) in [
        (&project, None),
        (&project, Some("[telemetry]\ndisabled = true\n")),
        (&project, Some("[telemetry]\ndisabled = false\n")),
        (&global, Some("[telemetry]\ndisabled = true\n")),
        (&global, Some("[broken")),
    ] {
        if let Some(content) = content {
            std::fs::write(path, content)?;
        }
        client
            .send_once::<()>(contract::Endpoint::Capabilities, None, None, None, 1024)
            .await?;
    }
    assert_eq!(
        *preferences.lock().expect("preferences"),
        [
            None,
            Some("1".to_owned()),
            None,
            Some("1".to_owned()),
            Some("1".to_owned())
        ],
    );
    assert_eq!(std::fs::read_dir(home.path().join(".rift"))?.count(), 1);
    task.abort();
    assert!(
        task.await
            .expect_err("server task cancelled")
            .is_cancelled()
    );
    Ok(())
}
