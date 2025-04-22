use anyhow::{anyhow, Result};
use axum::{routing::get, Router};
use notify_debouncer_mini::{new_debouncer, notify};
use prometheus::Encoder;
use prometheus::{IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder };
use serde::Deserialize;
use serde_yaml;
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::task::JoinSet;
use tokio::time::{self, Duration};
use tokio::{process::Command, time::timeout};

type CheckConfig = Arc<RwLock<HashMap<String, Check>>>;

const TICK_SPEED: u64 = 30;

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

    let team_bounties = IntGaugeVec::new(
        Opts::new(
            "team_bounties",
            "How many points from bounties does each team have",
        ),
        &["team_name", "check_name"],
    )
    .unwrap();

    let check_owners = IntGaugeVec::new(
        Opts::new("check_owner", "Who owns the check"),
        &["check_name", "team_name"],
    )
    .unwrap();

    let check_bounties = IntGaugeVec::new(
        Opts::new("check_bounties", "How much is the bounty for each check"),
        &["check_name"],
    )
    .unwrap();

    let r = Registry::new();
    r.register(Box::new(team_points.clone())).unwrap();
    r.register(Box::new(team_flags.clone())).unwrap();
    r.register(Box::new(team_bounties.clone())).unwrap();
    r.register(Box::new(check_owners.clone())).unwrap();
    r.register(Box::new(check_bounties.clone())).unwrap();

    let teams: Arc<HashMap<String, String>> =
        Arc::new(serde_yaml::from_reader(File::open("teams.yaml").unwrap()).unwrap());
    let checks: CheckConfig = get_check_config().await?;
    let total_flags = checks
        .read()
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

    let (owner_tx, owner_rx) = mpsc::channel::<(String, Option<String>)>(30);

    let owners: Arc<Mutex<HashMap<String, Option<String>>>> = Arc::new(Mutex::new(
        checks
            .read()
            .await
            .iter()
            .map(|(name, _)| (name.clone(), None))
            .collect::<HashMap<_, _>>(),
    ));

    let bounties: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(
        checks
            .read()
            .await
            .iter()
            .map(|(name, _)| (name.clone(), 0))
            .collect::<HashMap<_, _>>(),
    ));

    spawn_config_watcher(checks.clone())?;

    spawn_bounty_increment(bounties.clone(), checks.clone(), check_bounties.clone());

    spawn_flag_change_watcher(
        owners.clone(),
        owner_rx,
        bounties.clone(),
        check_owners.clone(),
        team_bounties.clone(),
        check_bounties.clone(),
    );

    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(TICK_SPEED));
        loop {
            interval.tick().await;
            team_flags.reset();
            let mut set = JoinSet::new();
            let uncaptured_flags = Arc::new(Mutex::new(
                total_flags.clone().into_iter().collect::<HashMap<_, _>>(),
            ));
            for (check_name, check) in checks.read().await.iter() {
                let name = check_name.clone();
                let checkc = check.clone();
                let team_points_clone = team_points.clone();
                let team_flags_clone = team_flags.clone();
                let teams_clone = teams.clone();
                let uc_flags_clone = uncaptured_flags.clone();
                let owner_tx = owner_tx.clone();
                set.spawn(async move {
                    if let Ok(Some(flag)) = check_for_flag(&checkc).await {
                        if let Some(team_name) = teams_clone.get(&flag) {
                            owner_tx
                                .send((name.clone(), Some(team_name.clone())))
                                .await
                                .unwrap();
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
    let checks: CheckConfig = Arc::new(RwLock::new(serde_yaml::from_reader(file)?));
    Ok(checks)
}

// try to load config, if it fails, don't do anything
async fn try_load_check_config(config: CheckConfig) {
    let file = File::open("checks.yaml");
    if let Ok(file) = file {
        let checks: serde_yaml::Result<HashMap<String, Check>> = serde_yaml::from_reader(file);
        if let Ok(checks) = checks {
            let mut config = config.write().await;
            *config = checks;
            println!("Config loaded");
        } else {
            println!("Syntax error in config");
        }
    } else {
        println!("Failed to load config");
    }
}

fn spawn_bounty_increment(
    bounties: Arc<Mutex<HashMap<String, u64>>>,
    checks: CheckConfig,
    check_bounties: IntGaugeVec,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(TICK_SPEED));
        loop {
            interval.tick().await;
            let mut bounties = bounties.lock().await;
            for (check_name, check) in checks.read().await.iter() {
                if let Some(bounty) = bounties.get_mut(check_name) {
                    *bounty += check.bounty.0;
                    check_bounties
                        .with_label_values(&[check_name])
                        .set(*bounty as i64);
                }
            }
        }
    });
}

fn spawn_config_watcher(checks: CheckConfig) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(
        Duration::from_secs(2),
        move |res: notify_debouncer_mini::DebounceEventResult| match res {
            Ok(_) => {
                tx.send(0).unwrap();
            }
            Err(e) => println!("Error {:?}", e),
        },
    )?;
    tokio::task::spawn_blocking(move || {
        println!(
            "Watching for changes in {}",
            Path::new("checks.yaml").display()
        );
        debouncer
            .watcher()
            .watch(
                Path::new("checks.yaml"),
                notify::RecursiveMode::NonRecursive,
            )
            .unwrap();
        loop {}
    });
    tokio::task::spawn_blocking(move || loop {
        let checks = checks.clone();
        if rx.recv().is_ok() {
            tokio::spawn(async move { try_load_check_config(checks).await })
        } else {
            break;
        };
    });
    Ok(())
}

fn spawn_flag_change_watcher(
    owners: Arc<Mutex<HashMap<String, Option<String>>>>,
    mut owner_rx: mpsc::Receiver<(String, Option<String>)>,
    bounties: Arc<Mutex<HashMap<String, u64>>>,
    check_owners: IntGaugeVec,
    team_bounties: IntGaugeVec,
    check_bounties: IntGaugeVec,
) {
    tokio::spawn(async move {
        while let Some((check_name, new_owner)) = owner_rx.recv().await {
            let mut owners = owners.lock().await;
            if let Some(current_owner) = owners.get_mut(&check_name) {
                if *current_owner != new_owner {
                    if let Some(current_owner) = current_owner.take() {
                        check_owners
                            .with_label_values(&[&check_name, current_owner.as_str()])
                            .set(0);
                    }
                    *current_owner = new_owner.clone();
                    if let Some(new_owner) = new_owner {
                        if let Some(bounty) = bounties.lock().await.get_mut(&check_name) {
                            check_owners
                                .with_label_values(&[&check_name, new_owner.as_str()])
                                .set(1);
                            team_bounties
                                .with_label_values(&[new_owner.as_str(), &check_name])
                                .add(*bounty as i64);
                            println!("New owner for {}: {}", check_name, new_owner);
                            println!("Bounty: {}", *bounty);
                            *bounty = 0;
                            check_bounties.with_label_values(&[&check_name]).set(0);
                        }
                    }
                }
            }
        }
    });
}

#[derive(Deserialize, Clone)]
struct Bounty(u64);

impl Default for Bounty {
    fn default() -> Self {
        Bounty(1)
    }
}

#[derive(Deserialize, Clone)]
struct Check {
    check: String,
    points: u64,
    difficulty: String,
    #[serde(default)]
    bounty: Bounty,
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
