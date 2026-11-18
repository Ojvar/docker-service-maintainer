use super::*;
use serde_json::json;
use std::os::unix::process::ExitStatusExt;

const T: Thresholds = Thresholds {
    warning_percent: 80.0,
    critical_percent: 90.0,
};

fn output(code: i32, stdout: &str, stderr: &str) -> Result<Output, String> {
    Ok(Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    })
}

fn entry(id: &str) -> ServerEntry {
    ServerEntry {
        id: id.into(),
        name: id.to_uppercase(),
        host: "10.0.0.1".into(),
        port: 22,
        user: "qeng".into(),
    }
}

fn lookup(vars: &[(&str, &str)]) -> Result<Settings, String> {
    let map: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Settings::from_lookup(&|k: &str| map.get(k).cloned())
}

#[test]
fn parses_inventory_and_reports_bad_lines() {
    let text = "# comment\n\
                server-1  192.168.0.121  10022  qeng  Gateway/Proxy\n\
                \n\
                server-2 10.0.0.2 22 root\n\
                Bad 10.0.0.3 22 qeng x\n\
                server-3 -oProxy=x 22 qeng x\n\
                server-4 10.0.0.4 0 qeng x\n\
                server-1 10.0.0.5 22 qeng dup\n\
                alerts 10.0.0.6 22 qeng x\n\
                server-5 10.0.0.7\n";
    let (entries, errors) = parse_inventory(text);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "Gateway/Proxy");
    assert_eq!((entries[0].port, entries[0].user.as_str()), (10022, "qeng"));
    assert_eq!(entries[1].name, "server-2");
    assert_eq!(errors.len(), 6, "{errors:?}");
    assert!(errors[0].starts_with("line 5:"));
}

#[test]
fn validates_ids_hosts_and_users() {
    assert!(is_valid_server_id("server-1") && is_valid_server_id("s_2"));
    for id in [
        "",
        "Server",
        "a/b",
        "a b",
        "refresh",
        "dashboard",
        "alerts",
        "audit",
    ] {
        assert!(!is_valid_server_id(id), "{id:?}");
    }
    assert!(
        is_valid_host("192.168.0.9") && !is_valid_host("-oProxyCommand=x") && !is_valid_host("a;b")
    );
    assert!(is_valid_user("qeng") && !is_valid_user("Root") && !is_valid_user("a@b"));
}

#[test]
fn settings_defaults_and_validation() {
    let s = lookup(&[]).unwrap();
    assert_eq!(s.data_dir, Some(PathBuf::from("data")));
    assert_eq!(
        (s.warning_percent, s.critical_percent, s.concurrency),
        (80.0, 90.0, 8)
    );
    assert!(s.alert_webhook_url.is_none());
    let s = lookup(&[
        ("SERVERS_DATA_DIR", "off"),
        ("SERVERS_WARNING_PERCENT", "70"),
        ("SERVERS_CRITICAL_PERCENT", "85"),
        ("SERVERS_CONCURRENCY", "3"),
        ("SERVERS_ALERT_WEBHOOK_URL", "https://hooks.example/x"),
    ])
    .unwrap();
    assert_eq!(s.data_dir, None);
    assert_eq!(
        (s.warning_percent, s.critical_percent, s.concurrency),
        (70.0, 85.0, 3)
    );
    assert!(
        lookup(&[("SERVERS_WARNING_PERCENT", "95")]).is_err(),
        "warning >= critical"
    );
    assert!(lookup(&[("SERVERS_CONCURRENCY", "0")]).is_err());
    assert!(lookup(&[("SERVERS_ALERT_WEBHOOK_URL", "file:///etc/passwd")]).is_err());
    assert!(lookup(&[("SERVERS_ALERT_WEBHOOK_URL", "http://a b")]).is_err());
}

#[test]
fn interprets_collector_output() {
    let ok = r#"{"id":"s","reachable":true,"duration_ms":1200,"data":{"unavailable":[]}}"#;
    assert_eq!(
        interpret(output(0, ok, "")),
        Outcome::Reached {
            data: json!({"unavailable": []}),
            duration_ms: Some(1200)
        }
    );
    let down = r#"{"id":"s","reachable":false,"duration_ms":8000,"error":"Connection timed out"}"#;
    assert_eq!(
        interpret(output(0, down, "")),
        Outcome::Failed {
            error: "Connection timed out".into(),
            duration_ms: Some(8000)
        }
    );
    match interpret(output(64, "", "unknown server id: x\n")) {
        Outcome::Failed { error, .. } => assert_eq!(error, "unknown server id: x"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        interpret(output(0, "{not json", "")),
        Outcome::Failed { .. }
    ));
    assert!(matches!(
        interpret(Err("spawn failed".into())),
        Outcome::Failed { .. }
    ));
}

#[test]
fn apply_sets_status_and_keeps_stale_data() {
    let mut state = ServersState::default();
    let e = entry("s");
    assert_eq!(begin(&mut state, std::slice::from_ref(&e)).len(), 1);
    assert!(
        begin(&mut state, std::slice::from_ref(&e)).is_empty(),
        "not started twice"
    );
    apply(
        &mut state,
        "s",
        Outcome::Reached {
            data: json!({"unavailable": ["temperatures"]}),
            duration_ms: None,
        },
    );
    assert_eq!(
        state.records["s"].status,
        ServerStatus::Online,
        "temperatures are optional"
    );
    apply(
        &mut state,
        "s",
        Outcome::Reached {
            data: json!({"unavailable": ["docker"]}),
            duration_ms: None,
        },
    );
    assert_eq!(state.records["s"].status, ServerStatus::Partial);
    apply(
        &mut state,
        "s",
        Outcome::Failed {
            error: "timeout".into(),
            duration_ms: None,
        },
    );
    let v = view(&e, state.records.get("s"), true, T, Vec::new());
    assert!(
        v.status == ServerStatus::Offline
            && v.stale
            && v.data.is_some()
            && v.last_success_at.is_some()
    );
}

#[test]
fn summary_ignores_network_shares() {
    let data = json!({
        "system": {"hostname": "h", "fqdn": "h.example", "os": "Ubuntu", "uptime_seconds": 3600},
        "cpu": {"usage_percent": 12.5, "logical_cores": 8},
        "memory": {"used_percent": 50.0},
        "disks": [
            {"mount": "/", "used_percent": 40.0, "network": false},
            {"mount": "/data", "used_percent": 85.5},
            {"mount": "/mnt/share", "used_percent": 98.5, "network": true},
        ],
        "health": {"load_1": 0.5},
        "docker": {"containers_running": 3},
    });
    let s = summarize(&data, T);
    assert_eq!(s.hostname.as_deref(), Some("h.example"));
    assert_eq!(
        (s.disk_percent, s.disk_mount.as_deref()),
        (Some(85.5), Some("/data"))
    );
    assert_eq!(s.network_shares, 1);
    assert_eq!(
        s.level,
        Level::Warning,
        "the 98.5% share does not make it critical"
    );
    assert_eq!(summarize(&json!({}), T).level, Level::Unknown);
}

#[test]
fn levels_follow_thresholds() {
    assert_eq!(level_of(&[Some(10.0), None], T), Level::Ok);
    assert_eq!(level_of(&[Some(80.0)], T), Level::Warning);
    assert_eq!(level_of(&[None, Some(90.0)], T), Level::Critical);
    let custom = Thresholds {
        warning_percent: 50.0,
        critical_percent: 60.0,
    };
    assert_eq!(level_of(&[Some(55.0)], custom), Level::Warning);
    assert_eq!(level_of(&[None], T), Level::Unknown);
}

#[test]
fn job_progress_counts_finished_servers() {
    let mut state = ServersState::default();
    let entries = [entry("a"), entry("b")];
    let job = Job {
        id: 1,
        started: Instant::now(),
        started_at: 0,
        ids: vec!["a".into(), "b".into()],
    };
    begin(&mut state, &entries);
    assert_eq!(job_status(&job, &state.records).done, 0);
    apply(
        &mut state,
        "a",
        Outcome::Failed {
            error: "x".into(),
            duration_ms: None,
        },
    );
    let status = job_status(&job, &state.records);
    assert_eq!((status.done, status.running), (1, true));
    apply(
        &mut state,
        "b",
        Outcome::Failed {
            error: "x".into(),
            duration_ms: None,
        },
    );
    assert!(!job_status(&job, &state.records).running);
    begin(&mut state, &entries[..1]);
    assert!(
        !job_status(&job, &state.records).running,
        "a single refresh after the run does not revive it"
    );
}

#[test]
fn alert_transitions() {
    use ServerStatus::*;
    let k = |b, a| alerts::transitions(b, a);
    assert_eq!(
        k((Online, Level::Ok), (Offline, Level::Ok)),
        vec![AlertKind::Offline]
    );
    assert_eq!(
        k((Offline, Level::Ok), (Offline, Level::Ok)),
        vec![],
        "no repeat while offline"
    );
    assert_eq!(
        k((Offline, Level::Ok), (Online, Level::Ok)),
        vec![AlertKind::Recovered]
    );
    assert_eq!(
        k((Online, Level::Ok), (Online, Level::Warning)),
        vec![AlertKind::Warning]
    );
    assert_eq!(
        k((Online, Level::Warning), (Online, Level::Critical)),
        vec![AlertKind::Critical]
    );
    assert_eq!(
        k((Online, Level::Critical), (Online, Level::Warning)),
        vec![],
        "downgrade is quiet"
    );
    assert_eq!(
        k((Online, Level::Critical), (Online, Level::Ok)),
        vec![AlertKind::Resolved]
    );
    assert_eq!(
        k((Unknown, Level::Unknown), (Online, Level::Critical)),
        vec![AlertKind::Critical]
    );
    assert_eq!(k((Unknown, Level::Unknown), (Online, Level::Ok)), vec![]);
}

#[test]
fn alert_delivery_and_cooldown() {
    let mut state = ServersState::default();
    let off = Settings::default();
    let ev = alerts::new_event(
        &mut state,
        &off,
        &entry("s"),
        AlertKind::Offline,
        None,
        Some("timeout"),
    );
    assert_eq!(ev.delivery, "not_configured");
    assert!(ev.message.contains("unreachable: timeout"));
    let on = Settings {
        alert_webhook_url: Some("https://hooks.example/x".into()),
        ..Settings::default()
    };
    let first = alerts::new_event(&mut state, &on, &entry("s"), AlertKind::Offline, None, None);
    let second = alerts::new_event(&mut state, &on, &entry("s"), AlertKind::Offline, None, None);
    let other = alerts::new_event(&mut state, &on, &entry("t"), AlertKind::Offline, None, None);
    assert_eq!(first.delivery, alerts::PENDING);
    assert_eq!(second.delivery, "suppressed (cooldown)");
    assert_eq!(other.delivery, alerts::PENDING, "cooldown is per server");
    assert!(first.id < second.id && state.alerts.len() == 4);
    let body: Value = serde_json::from_str(&alerts::payload(&first)).unwrap();
    assert_eq!(body["event"], "offline");
    assert_eq!(body["server"]["id"], "s");
}

#[test]
fn downsample_averages_and_keeps_worst_status() {
    let points: Vec<HistoryPoint> = (0..10)
        .map(|i| HistoryPoint {
            t: i,
            status: if i == 3 {
                ServerStatus::Offline
            } else {
                ServerStatus::Online
            },
            cpu: Some(f64::from(i as i32)),
            mem: None,
            disk: Some(50.0),
            load_1: None,
        })
        .collect();
    let out = downsample(points.clone(), 5);
    assert_eq!(out.len(), 5);
    assert_eq!((out[0].t, out[0].cpu), (1, Some(0.5)));
    assert_eq!(out[1].status, ServerStatus::Offline);
    assert_eq!(out[0].mem, None);
    assert_eq!(downsample(points, 20).len(), 10);
}

#[test]
fn state_and_history_survive_restart() {
    let dir = crate::TestDir::new();
    let settings = Settings {
        data_dir: Some(dir.path().join("data")),
        ..Settings::default()
    };
    let servers = Arc::new(Servers::new(settings.clone()));
    let data = json!({"cpu": {"usage_percent": 95.0}, "memory": {"used_percent": 10.0}, "unavailable": []});
    begin(&mut servers.lock(), &[entry("s")]);
    servers.finish(
        &entry("s"),
        Outcome::Reached {
            data,
            duration_ms: Some(5),
        },
        PathBuf::from("/nonexistent"),
    );
    servers.audit(&Client::default(), "refresh", Some("s"), None, "started");
    drop(servers);

    let reloaded = Servers::new(settings);
    let state = reloaded.lock();
    assert_eq!(state.records["s"].status, ServerStatus::Online);
    assert_eq!(state.history["s"].len(), 1);
    assert_eq!(state.history["s"][0].cpu, Some(95.0));
    assert_eq!(state.alerts.len(), 1, "critical alert recorded");
    assert_eq!(state.alerts[0].kind, AlertKind::Critical);
    assert_eq!(state.audit.len(), 1);
    assert!(!state.records["s"].collecting);
}

#[test]
fn truncates_long_errors() {
    let long = "x".repeat(MAX_ERROR_LEN + 10);
    assert_eq!(truncate(&long).chars().count(), MAX_ERROR_LEN + 1);
    assert_eq!(truncate("  short \n"), "short");
}
