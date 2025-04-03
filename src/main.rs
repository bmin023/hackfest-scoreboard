use anyhow::{anyhow, Result};
use axum::{routing::get, Router};
use notify_debouncer_mini::{new_debouncer, notify, DebouncedEventKind};
use prometheus::Encoder;
use prometheus::{IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder};
use serde::Deserialize;
use serde_yaml;
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time::{self, Duration};
use tokio::{process::Command, time::timeout};

type CheckConfig = Arc<Mutex<HashMap<String, Check>>>;

#[tokio::main]
async fn main() -> Result<()> {
    let team_points = IntCounterVec::new(
        Opts::new("team_points", "How many points each team has"),
        &["team_name", "check_name"],
    )
    .unwrap();

    let team_flags = IntGaugeVec::new(
        Opts::new("team_flags", "How many flags does each team have"),
        &["team_name", "difficulty"],
    )
    .unwrap();

    let r = Registry::new();
    r.register(Box::new(team_points.clone())).unwrap();
    r.register(Box::new(team_flags.clone())).unwrap();

    let teams: Arc<HashMap<String, String>> =
        Arc::new(serde_yaml::from_reader(File::open("teams.yaml").unwrap()).unwrap());
    let checks: CheckConfig = get_check_config().await?;
    let total_flags = checks
        .lock()
        .await
        .iter()
        .fold(HashMap::new(), |mut m, (_, check)| {
            m.entry(check.difficulty.clone())
                .and_modify(|v| *v += 1)
                .or_insert(1i64);
            m
        })
        .into_iter()
        .collect::<Vec<_>>();

    {
        let (tx, rx) = std::sync::mpsc::channel();
        let checks = checks.clone();
        let mut debouncer = new_debouncer(
            Duration::from_secs(2),
            move |res: notify_debouncer_mini::DebounceEventResult| {
                match res {
                    Ok(_) => { tx.send(0).unwrap(); },
                    Err(e) => println!("Error {:?}", e),
                }
            },
        )?;
        tokio::task::spawn_blocking(move || {
            println!("Watching for changes in {}", Path::new("checks.yaml").display());
            debouncer.watcher().watch(Path::new("checks.yaml"), notify::RecursiveMode::NonRecursive).unwrap();
            loop {}
        });
        tokio::task::spawn_blocking(move || {
            loop {
                let checks = checks.clone();
                if rx.recv().is_ok() {
                    tokio::spawn(async move { try_load_check_config(checks).await } )
                } else {
                    break;
                };
            };
        });
    }
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(3));
        loop {
            interval.tick().await;
            team_flags.reset();
            let mut set = JoinSet::new();
            let uncaptured_flags = Arc::new(Mutex::new(
                total_flags.clone().into_iter().collect::<HashMap<_, _>>(),
            ));
            for (check_name, check) in checks.lock().await.iter() {
                let name = check_name.clone();
                let checkc = check.clone();
                let team_points_clone = team_points.clone();
                let team_flags_clone = team_flags.clone();
                let teams_clone = teams.clone();
                let uc_flags_clone = uncaptured_flags.clone();
                set.spawn(async move {
                    if let Ok(Some(flag)) = check_for_flag(&checkc).await {
                        if let Some(team_name) = teams_clone.get(&flag) {
                            team_points_clone
                                .with_label_values(&[team_name.as_str(), &name.as_str()])
                                .inc_by(checkc.points);
                            team_flags_clone
                                .with_label_values(&[team_name.as_str(), &checkc.difficulty])
                                .inc();
                            let mut uc_flags = uc_flags_clone.lock().await;
                            uc_flags.entry(checkc.difficulty).and_modify(|v| *v -= 1);
                        }
                    }
                });
            }
            set.join_all().await;
            let uc_flags = uncaptured_flags.lock().await;
            for (difficulty, num_flags) in uc_flags.iter() {
                if *num_flags > 0 {
                    team_flags
                        .with_label_values(&["Uncaptured", &difficulty.as_str()])
                        .set(*num_flags);
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

    println!("Running");
    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3001").await.unwrap();
    axum::serve(listener, app).await.unwrap();
    Ok(())
}

async fn get_check_config() -> Result<CheckConfig> {
    let file = File::open("checks.yaml")?;
    let checks: CheckConfig = Arc::new(Mutex::new(serde_yaml::from_reader(file)?));
    Ok(checks)
}

// try to load config, if it fails, don't do anything
async fn try_load_check_config(config: CheckConfig) {
    let file = File::open("checks.yaml");
    if let Ok(file) = file {
        let checks: serde_yaml::Result<HashMap<String, Check>> = serde_yaml::from_reader(file);
        if let Ok(checks) = checks {
            let mut config = config.lock().await;
            *config = checks;
            println!("Config loaded");
        } else {
            println!("Syntax error in config");
        }
    } else {
        println!("Failed to load config");
    }
}

#[derive(Deserialize, Clone)]
struct Check {
    check: String,
    points: u64,
    difficulty: String,
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
