use anyhow::{anyhow, Result};
use axum::{routing::get, Router};
use prometheus::Encoder;
use prometheus::{IntCounterVec, Opts, Registry, TextEncoder};
use serde::Deserialize;
use serde_yaml;
use std::collections::HashMap;
use std::fs::File;
use tokio::time::{self, Duration};
use tokio::{process::Command, time::timeout};

#[tokio::main]
async fn main() -> Result<()> {
    let counter = IntCounterVec::new(
        Opts::new("team_points", "How many points each team has"),
        &["team_name", "check_name"],
    )
    .unwrap();

    let r = Registry::new();
    r.register(Box::new(counter.clone())).unwrap();

    let mut interval = time::interval(Duration::from_secs(3));
    tokio::spawn(async move {
        let teams: HashMap<String, String> =
            serde_yaml::from_reader(File::open("teams.yaml").unwrap()).unwrap();
        let checks: HashMap<String, Check> =
            serde_yaml::from_reader(File::open("checks.yaml").unwrap()).unwrap();
        loop {
            interval.tick().await;
            for (check_name, check) in checks.iter() {
                if let Ok(Some(flag)) = check_for_flag(&check).await {
                    if let Some(team_name) = teams.get(&flag) {
                        counter.with_label_values(&[team_name.as_str(),&check_name.as_str()]).inc_by(check.points);
                    }
                }
            }
        }
    });

    // build our application with a single route
    let app = Router::new().route(
        "/metrics",
        get(|| async move {
            let metric_families = r.gather();
            let mut buffer = vec![];
            let encoder = TextEncoder::new();
            encoder.encode(&metric_families, &mut buffer).unwrap();
            String::from_utf8(buffer).unwrap()
        }),
    );

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3001").await.unwrap();
    axum::serve(listener, app).await.unwrap();
    Ok(())
}

#[derive(Deserialize)]
struct Check {
    check: String,
    points: u64,
}

async fn check_for_flag(check: &Check) -> Result<Option<String>> {
    let path = std::env::var("PATH").unwrap_or("/usr/bin:/bin:/usr/local/bin".to_string());
    let output = Command::new("bash")
        .arg("-c")
        .arg(&check.check)
        .env_clear()
        .env("PATH", path)
        .output();
    let Ok(res) = timeout(Duration::from_secs(5), output).await else {
        println!("'{}' timed out", &check.check);
        return Ok(None);
    };
    let Ok(res) = res else {
        println!("'{}' failed to run", &check.check);
        return Err(anyhow!("'{}' failed to run", &check.check));
    };
    let stdout = String::from_utf8_lossy(&res.stdout).to_string();
    if !stdout.is_empty() && res.status.success() {
        Ok(Some(stdout.trim().to_string()))
    } else {
        if !res.status.success() {
            println!("'{}' exited with bad status code", &check.check);
        }
        Ok(None)
    }
}
