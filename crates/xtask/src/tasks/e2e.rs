//! `cargo xtask e2e`：E2E harness（P11-006 + 007~013 matrix；v0.2 档案换线 E2E-08），
//! 替代 scripts/e2e/run-e2e.ps1。
//!
//! 原则（计划 16.3）不变：整个矩阵复用同一个 warpdeck:e2e 镜像，每用例不重 build。
//!
//! 用例：
//!   1 first-run   fresh volume + setup/login + 创建实例 + 等 Healthy（P11-007）
//!   2 socks5      11081 -> trace warp=on（P11-008）
//!   3 http        18081 -> trace warp=on（P11-009）
//!   4 persistence 3 实例 + 代理认证 -> restart -> 全部恢复（P11-010）
//!   5 failure     kill 一个 warp-svc -> 池收缩仍可用 -> auto-restart（P11-011）
//!   6 gost        kill gost -> reconciler 重建（P11-012）
//!   7 no-leak     停全部实例 -> 代理必须失败（P11-013）
//!   8 profiles    多账号档案 CRUD/绑定/改绑/删除保护；ZT 需真实凭据
//!                 （env: WARP_E2E_ZT_ORG / WARP_E2E_ZT_CLIENT_ID / WARP_E2E_ZT_CLIENT_SECRET，
//!                  缺失则走零凭据路径）
//!
//! API 调用沿用 curl.exe + cookie jar 子进程（与手工验证行为一致；
//! 不引入 HTTP 客户端依赖）。compose 环境变量经进程 env 注入（优先级高于 .env）。

use std::io::Write as _;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};

use crate::common;

const PROJECT: &str = "warpdeck-e2e";
const IMAGE: &str = "warpdeck:e2e";
const PORT_WEB: u16 = 9900;
const PORT_SOCKS: u16 = 11081;
const PORT_HTTP: u16 = 18081;
const ADMIN_USER: &str = "e2e-admin";
const ADMIN_PASS: &str = "e2e-password-123";
// 认证在 E2E-04 才开启；此前 GOST 忽略 proxy-user，带上无副作用。
const PROXY_AUTH: &str = "e2e-proxy-user:e2e-proxy-pass-123";

pub struct Args {
    /// 逗号分隔用例号（如 "2,3"）；缺省全量 1..=8。
    pub only: Option<String>,
    pub no_fresh: bool,
}

struct E2e {
    cookie_jar: PathBuf,
    body_file: PathBuf,
    csrf: Option<String>,
}

struct Resp {
    status: u16,
    json: Value,
}

impl E2e {
    fn new() -> Self {
        let t = std::env::temp_dir();
        Self {
            cookie_jar: t.join("wc-e2e-cookies.jar"),
            body_file: t.join("wc-e2e-body.json"),
            csrf: None,
        }
    }

    fn api(&mut self, method: &str, path: &str, body: Option<&Value>) -> Result<Resp> {
        let uri = format!("http://127.0.0.1:{PORT_WEB}/api/v1{path}");
        let jar = self.cookie_jar.display().to_string();
        let bf = self.body_file.display().to_string();
        let mut c = Command::new("curl");
        c.arg("-s");
        if self.cookie_jar.exists() {
            c.args(["-b", &jar]);
        }
        c.args(["-c", &jar, "-H", "Content-Type: application/json"]);
        if let Some(t) = &self.csrf {
            c.args(["-H", &format!("X-CSRF-Token: {t}")]);
        }
        if let Some(b) = body {
            c.args(["--data", &b.to_string()]);
        }
        c.args(["-o", &bf, "-w", "%{http_code}", "-X", method, &uri]);
        let out = c.output().context("spawn curl")?;
        let code_str = String::from_utf8_lossy(&out.stdout);
        let status: u16 = code_str
            .trim()
            .parse()
            .with_context(|| format!("bad http code: {code_str}"))?;
        let text = std::fs::read_to_string(&self.body_file).unwrap_or_default();
        let json = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        };
        Ok(Resp { status, json })
    }

    fn setup_and_login(&mut self) -> Result<()> {
        self.api(
            "POST",
            "/setup",
            Some(&json!({"username": ADMIN_USER, "password": ADMIN_PASS})),
        )?;
        let login = self.api(
            "POST",
            "/auth/login",
            Some(&json!({"username": ADMIN_USER, "password": ADMIN_PASS})),
        )?;
        self.csrf = login
            .json
            .get("x-csrf-token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        ensure!(self.csrf.is_some(), "login response missing x-csrf-token");
        Ok(())
    }

    fn create_instance(&mut self, name: &str, profile_id: Option<i64>) -> Result<i64> {
        let mut body = json!({ "name": name });
        if let Some(pid) = profile_id {
            body["account_profile_id"] = json!(pid);
        }
        let resp = self.api("POST", "/instances", Some(&body))?;
        ensure!(
            resp.status == 201,
            "create instance '{name}' -> {} (expected 201)",
            resp.status
        );
        resp.json
            .get("id")
            .and_then(|v| v.as_i64())
            .context("instance id missing")
    }

    fn wait_instance_state(&mut self, id: i64, state: &str, timeout_secs: u64) -> Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let mut last = String::new();
        while Instant::now() < deadline {
            let resp = self.api("GET", &format!("/instances/{id}"), None)?;
            last = resp
                .json
                .get("runtime_state")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if last == state {
                return Ok(resp.json);
            }
            std::thread::sleep(Duration::from_secs(5));
        }
        bail!("instance {id} not in state '{state}' within {timeout_secs}s (last={last})")
    }

    fn stop_instance(&mut self, id: i64) {
        let _ = self.api("POST", &format!("/instances/{id}/stop"), None);
    }
}

fn assert(msg: &str, cond: bool) -> Result<()> {
    if cond {
        println!("  PASS: {msg}");
        Ok(())
    } else {
        println!("  FAIL: {msg}");
        bail!("E2E assertion failed: {msg}")
    }
}

/// compose 带超时（输出直通实时显示；超时强杀子进程）。
fn compose(items: &[&str], timeout: Duration, repo: &std::path::Path) -> Result<()> {
    println!("+ docker compose -p {PROJECT} {}", items.join(" "));
    let mut child = Command::new("docker")
        .args(["compose", "-p", PROJECT])
        .args(items)
        .current_dir(repo)
        .env("WARPDECK_IMAGE", IMAGE)
        .env("WEB_HOST_PORT", PORT_WEB.to_string())
        .env("SOCKS5_HOST_PORT", PORT_SOCKS.to_string())
        .env("HTTP_HOST_PORT", PORT_HTTP.to_string())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn docker compose")?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(st) => {
                ensure!(st.success(), "docker compose {} failed", items.join(" "));
                return Ok(());
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    bail!(
                        "docker compose {} timed out (>{:?})",
                        items.join(" "),
                        timeout
                    );
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }
}

fn wait_container_healthy(timeout_secs: u64) -> Result<()> {
    let container = format!("{PROJECT}-warpdeck-1");
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut last = String::new();
    while Instant::now() < deadline {
        last = common::capture(
            "docker",
            &[
                "inspect".into(),
                "--format".into(),
                "{{.State.Health.Status}}".into(),
                container.clone(),
            ],
        )
        .unwrap_or_default();
        if last.trim() == "healthy" {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    bail!(
        "container not healthy within {timeout_secs}s (state={})",
        last.trim()
    )
}

/// 容器健康 ≠ GOST 已监听：等 TCP 可连（与服务端 apply→probe 语义一致），
/// 防 E2E-04 竞态。
fn wait_proxy_listeners(timeout_secs: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    for port in [PORT_SOCKS, PORT_HTTP] {
        let mut ready = false;
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        ensure!(
            ready,
            "proxy listener :{port} not open within {timeout_secs}s"
        );
    }
    Ok(())
}

fn get_trace(proto: &str, timeout_secs: u64) -> Option<String> {
    let (arg, port) = if proto == "socks5" {
        ("--socks5-hostname", PORT_SOCKS)
    } else {
        ("-x", PORT_HTTP)
    };
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            &timeout_secs.to_string(),
            arg,
            &format!("127.0.0.1:{port}"),
            "--proxy-user",
            PROXY_AUTH,
            "https://cloudflare.com/cdn-cgi/trace",
            "-o",
            "-",
        ])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 数据面有界重试（~60s）：reconciler 启动期会以期望配置重启一次 GOST，
/// 存在 accept 后 EOF 的窗口，勿把重启窗口误判为故障。
fn assert_warp_on(proto: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut trace: Option<String> = None;
    while Instant::now() < deadline {
        trace = get_trace(proto, 30);
        if trace.as_deref().is_some_and(|t| t.contains("warp=on")) {
            break;
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    let t = trace.clone().unwrap_or_default();
    assert(&format!("{proto} trace reachable"), trace.is_some())?;
    assert(&format!("{proto} trace warp=on"), t.contains("warp=on"))
}

fn trace_with_auth(timeout_secs: u64) -> Option<String> {
    // 与 get_trace 相同（当前实现恒带 --proxy-user），显式命名提升用例可读性。
    get_trace("socks5", timeout_secs)
}

pub fn run(args: &Args) -> Result<()> {
    let only: Vec<u32> = match &args.only {
        Some(s) => s.split(',').filter_map(|p| p.trim().parse().ok()).collect(),
        None => vec![1, 2, 3, 4, 5, 6, 7, 8],
    };
    let has = |n: u32| only.contains(&n);

    let repo = common::repo_root()?;
    let mut e2e = E2e::new();
    let mut results: Vec<String> = Vec::new();

    println!(
        "== E2E setup ({PROJECT}, image={IMAGE}, ports {PORT_WEB}/{PORT_SOCKS}/{PORT_HTTP}) =="
    );
    if args.no_fresh {
        println!("  (reuse existing environment)");
    } else {
        println!("  fresh environment...");
        compose(&["down", "-v"], Duration::from_secs(300), &repo)?;
        compose(&["up", "-d"], Duration::from_secs(300), &repo)?;
    }
    wait_container_healthy(180)?;
    e2e.setup_and_login()?;
    println!("  admin setup + login OK");

    let mut ids: Vec<i64> = Vec::new();

    // ---------- E2E-01 ----------
    if has(1) {
        println!(
            "== E2E-01 first run: fresh volume, setup, login, create instance, wait healthy =="
        );
        let id = e2e.create_instance("e2e-a", None)?;
        ids.push(id);
        let view = e2e.wait_instance_state(id, "healthy", 360)?;
        let exit_ip = view.get("exit_ip").and_then(|v| v.as_str()).unwrap_or("");
        let colo = view.get("colo").and_then(|v| v.as_str()).unwrap_or("");
        assert(
            &format!("instance {id} Healthy (exit_ip={exit_ip} colo={colo})"),
            true,
        )?;
        results.push("PASS E2E-01 first run".into());
    }

    // ---------- E2E-02 / 03 ----------
    if has(2) {
        println!("== E2E-02 socks5 -> warp=on ==");
        wait_proxy_listeners(90)?;
        assert_warp_on("socks5")?;
        results.push("PASS E2E-02 socks5 warp=on".into());
    }
    if has(3) {
        println!("== E2E-03 http -> warp=on ==");
        assert_warp_on("http")?;
        results.push("PASS E2E-03 http warp=on".into());
    }

    // ---------- E2E-04 ----------
    if has(4) {
        println!("== E2E-04 restart persistence ==");
        let b = e2e.create_instance("e2e-b", None)?;
        let c = e2e.create_instance("e2e-c", None)?;
        ids.push(b);
        ids.push(c);
        e2e.wait_instance_state(b, "healthy", 360)?;
        e2e.wait_instance_state(c, "healthy", 360)?;
        let cfg = e2e.api(
            "PUT",
            "/proxy",
            Some(&json!({
                "socks5_enabled": true,
                "http_enabled": true,
                "auth_enabled": true,
                "username": "e2e-proxy-user",
                "password": "e2e-proxy-pass-123"
            })),
        )?;
        assert(
            "proxy config saved (201/200)",
            cfg.status == 200 || cfg.status == 201,
        )?;
        let view = e2e.api("GET", "/proxy", None)?;
        assert(
            "proxy auth_configured persisted",
            view.json.get("auth_configured").and_then(|v| v.as_bool()) == Some(true),
        )?;
        compose(&["restart"], Duration::from_secs(300), &repo)?;
        wait_container_healthy(180)?;
        for id in &ids {
            let v = e2e.wait_instance_state(*id, "healthy", 360)?;
            assert(
                &format!("instance {id} recovered after restart"),
                v.get("runtime_state").and_then(|x| x.as_str()) == Some("healthy"),
            )?;
        }
        wait_proxy_listeners(90)?;
        let trace = trace_with_auth(30).unwrap_or_default();
        assert(
            "socks5 trace (with auth) still warp=on after restart",
            trace.contains("warp=on"),
        )?;
        results.push("PASS E2E-04 restart persistence".into());
    }

    // ---------- E2E-05 ----------
    if has(5) {
        println!("== E2E-05 kill one warp-svc -> pool shrinks -> proxy alive -> auto-restart ==");
        let container = format!("{PROJECT}-warpdeck-1");
        common::run(
            "docker",
            &[
                "exec".into(),
                container,
                "bash".into(),
                "-c".into(),
                "pgrep -f 'warp-svc --accept-tos' | head -n 1 | xargs -r kill -9".into(),
            ],
        )?;
        assert("one warp-svc killed inside container", true)?;
        std::thread::sleep(Duration::from_secs(5));
        let mut ok = false;
        for _ in 0..10 {
            if let Some(t) = trace_with_auth(30) {
                if t.contains("warp=on") {
                    ok = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_secs(6));
        }
        assert("proxy still works after instance kill (pool shrank)", ok)?;
        for id in &ids {
            e2e.wait_instance_state(*id, "healthy", 360)?;
            assert(&format!("instance {id} auto-restarted healthy"), true)?;
        }
        results.push("PASS E2E-05 instance failure".into());
    }

    // ---------- E2E-06 ----------
    if has(6) {
        println!("== E2E-06 kill gost -> reconciler restart -> trace recovers ==");
        let container = format!("{PROJECT}-warpdeck-1");
        common::run(
            "docker",
            &[
                "exec".into(),
                container,
                "bash".into(),
                "-c".into(),
                "pkill -9 -f 'gost -C'".into(),
            ],
        )?;
        assert("gost killed inside container", true)?;
        let mut ok = false;
        for _ in 0..15 {
            std::thread::sleep(Duration::from_secs(6));
            if let Some(t) = trace_with_auth(30) {
                if t.contains("warp=on") {
                    ok = true;
                    break;
                }
            }
        }
        assert(
            "gost auto-recovered (reconciler restart + listener probe)",
            ok,
        )?;
        results.push("PASS E2E-06 gost failure".into());
    }

    // ---------- E2E-07 ----------
    if has(7) {
        println!("== E2E-07 stop all instances -> proxy must fail (no direct leak) ==");
        if ids.is_empty() {
            let list = e2e.api("GET", "/instances", None)?;
            if let Value::Array(items) = &list.json {
                for it in items {
                    if let Some(id) = it.get("id").and_then(|v| v.as_i64()) {
                        e2e.stop_instance(id);
                    }
                }
            }
        } else {
            for id in &ids {
                e2e.stop_instance(*id);
            }
        }
        std::thread::sleep(Duration::from_secs(12));
        let leaked = trace_with_auth(20);
        assert(
            "proxy request FAILS with no healthy upstream",
            leaked.is_none(),
        )?;
        let direct = Command::new("curl")
            .args([
                "-fsSL",
                "--max-time",
                "15",
                "https://cloudflare.com/cdn-cgi/trace",
                "-o",
                "-",
            ])
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| ());
        assert(
            "direct (non-proxy) internet still works (environment sanity)",
            direct.is_some(),
        )?;
        results.push("PASS E2E-07 no direct leak".into());
    }

    // ---------- E2E-08 ----------
    if has(8) {
        println!("== E2E-08 account profiles: CRUD(masked) + binding + rebind auto-restart + delete protection ==");
        wait_proxy_listeners(90)?;

        let acc = e2e.api("GET", "/accounts", None)?;
        assert("GET /accounts -> 200", acc.status == 200)?;
        let def_list: Vec<&Value> = acc
            .json
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|x| x.get("default").and_then(|v| v.as_bool()) == Some(true))
                    .collect()
            })
            .unwrap_or_default();
        assert(
            "default free profile present (id=1, mode=free)",
            def_list.len() == 1
                && def_list[0].get("id").and_then(|v| v.as_i64()) == Some(1)
                && def_list[0].get("mode").and_then(|v| v.as_str()) == Some("free"),
        )?;

        let zt_org = std::env::var("WARP_E2E_ZT_ORG")
            .ok()
            .filter(|s| !s.is_empty());
        let zt_id = std::env::var("WARP_E2E_ZT_CLIENT_ID")
            .ok()
            .filter(|s| !s.is_empty());
        let zt_secret = std::env::var("WARP_E2E_ZT_CLIENT_SECRET")
            .ok()
            .filter(|s| !s.is_empty());
        let zt_available = zt_org.is_some() && zt_id.is_some() && zt_secret.is_some();

        if zt_available {
            run_zt_line(
                &mut e2e,
                zt_org.as_deref(),
                zt_id.as_deref(),
                zt_secret.as_deref(),
                &mut results,
            )?;
        } else {
            println!("  SKIP: Zero Trust 换线未提供 WARP_E2E_ZT_* 凭据（只验证默认档路径）");
            let free_id = e2e.create_instance("e2e-free", None)?;
            let free_id2 = e2e.create_instance("e2e-free2", None)?;
            e2e.wait_instance_state(free_id, "healthy", 360)?;
            e2e.wait_instance_state(free_id2, "healthy", 360)?;
            let del = e2e.api("DELETE", "/accounts/1", None)?;
            assert(
                "delete default profile -> 409 (protected)",
                del.status == 409,
            )?;
            let rb = e2e.api(
                "PATCH",
                &format!("/instances/{free_id}"),
                Some(&json!({"account_profile_id": null})),
            )?;
            assert("PATCH rebind (explicit null) -> 200", rb.status == 200)?;
            let v = e2e.wait_instance_state(free_id, "healthy", 360)?;
            assert(
                "rebind to default profile effective",
                v.pointer("/account/profile_id").and_then(|x| x.as_i64()) == Some(1),
            )?;
            e2e.stop_instance(free_id);
            e2e.stop_instance(free_id2);
            results.push("PASS E2E-08 account profiles (default/free line, ZT skipped)".into());
        }
    }

    println!("\n========== E2E summary ==========");
    let mut out = std::io::stdout().lock();
    for r in &results {
        writeln!(out, "  {r}")?;
    }
    writeln!(out, "=================================")?;
    ensure!(
        results.iter().all(|r| !r.starts_with("FAIL")),
        "E2E failures present"
    );
    println!("ALL E2E PASSED");
    Ok(())
}

/// E2E-08 ZT 换线全路径（真实 service token）。
#[allow(clippy::too_many_lines)]
fn run_zt_line(
    e2e: &mut E2e,
    org: Option<&str>,
    client_id: Option<&str>,
    secret: Option<&str>,
    results: &mut Vec<String>,
) -> Result<()> {
    let resp = e2e.api(
        "POST",
        "/accounts",
        Some(&json!({
            "name": "e2e-zero-trust",
            "mode": "zero_trust",
            "zero_trust_org": org,
            "client_id": client_id,
            "client_secret": secret
        })),
    )?;
    assert("create zero_trust profile -> 201", resp.status == 201)?;
    let zt_pid = resp
        .json
        .get("id")
        .and_then(|v| v.as_i64())
        .context("profile id missing")?;
    assert(
        "created profile has masked secrets (no plaintext)",
        resp.json.get("client_id").is_none() && resp.json.get("client_secret").is_none(),
    )?;

    let free_id = e2e.create_instance("e2e-free", None)?;
    let zt_inst = e2e.create_instance("e2e-zt", Some(zt_pid))?;
    e2e.wait_instance_state(free_id, "healthy", 360)?;
    e2e.wait_instance_state(zt_inst, "healthy", 360)?;
    let v_free = e2e.api("GET", &format!("/instances/{free_id}"), None)?;
    let v_zt = e2e.api("GET", &format!("/instances/{zt_inst}"), None)?;
    assert(
        "free instance bound to default profile (account.profile_id=1)",
        v_free
            .json
            .pointer("/account/profile_id")
            .and_then(|v| v.as_i64())
            == Some(1),
    )?;
    assert(
        "zt instance bound to created profile",
        v_zt.json
            .pointer("/account/profile_id")
            .and_then(|v| v.as_i64())
            == Some(zt_pid),
    )?;
    assert(
        "zt instance exit ip present",
        v_zt.json.get("exit_ip").is_some_and(|v| !v.is_null()),
    )?;

    // 改绑：zt 实例解绑回默认档 -> restart_pending -> 自动重启生效。
    let rb = e2e.api(
        "PATCH",
        &format!("/instances/{zt_inst}"),
        Some(&json!({"account_profile_id": null})),
    )?;
    assert(
        "PATCH rebind (explicit null = unbind) -> 200",
        rb.status == 200,
    )?;
    let v = e2e.wait_instance_state(zt_inst, "healthy", 360)?;
    assert(
        "rebind took effect via auto-restart (account.profile_id=1)",
        v.pointer("/account/profile_id").and_then(|x| x.as_i64()) == Some(1),
    )?;

    // 删除保护：重绑一个实例制造 409，再清理后 204。
    let free_id2 = e2e.create_instance("e2e-free2", Some(zt_pid))?;
    e2e.wait_instance_state(free_id2, "healthy", 360)?;
    let del = e2e.api("DELETE", &format!("/accounts/{zt_pid}"), None)?;
    assert(
        "delete bound profile -> 409 (referenced)",
        del.status == 409,
    )?;
    e2e.api("DELETE", &format!("/instances/{free_id2}"), None)?;
    std::thread::sleep(Duration::from_secs(3));
    let del2 = e2e.api("DELETE", &format!("/accounts/{zt_pid}"), None)?;
    assert("delete unbound profile -> 204", del2.status == 204)?;

    // 数据面换线证据：ZT 档案在场时 socks5 依旧 warp=on。
    std::thread::sleep(Duration::from_secs(3));
    assert_warp_on("socks5")?;
    results.push("PASS E2E-08 account profiles (zero_trust line)".into());
    Ok(())
}
