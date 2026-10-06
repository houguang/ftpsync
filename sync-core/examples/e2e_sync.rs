//! 端到端自检：对一台真实的 FTP 服务器跑完整的「监听 → 上传 → 修改 → 新增 → 删除」流程。
//!
//! 运行前需准备一个可写的 FTP 账号（本地 pyftpdlib / FileZilla Server / 宝塔面板均可）：
//!
//! ```text
//! cargo run --example e2e_sync
//! ```
//!
//! 可用环境变量覆盖默认参数：
//! `E2E_FTP_HOST` `E2E_FTP_PORT` `E2E_FTP_USER` `E2E_FTP_PASS` `E2E_FTP_SECURE`
//! `E2E_LOCAL` `E2E_REMOTE`

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use sync_core::config::{FtpConfig, SyncConfig};
use sync_core::engine::Engine;
use sync_core::ftp::FtpClient;

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_string())
}

fn wait_until(label: &str, timeout: Duration, mut probe: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if probe() {
            return true;
        }
        thread::sleep(Duration::from_millis(150));
    }
    eprintln!("  [超时] {label}");
    false
}

/// 读取远端所有文件的相对路径 -> 内容。
fn remote_snapshot(cfg: &SyncConfig) -> Vec<(String, String)> {
    let mut client = match FtpClient::connect(&cfg.ftp, &cfg.remote_base()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  连接失败: {e}");
            return Vec::new();
        }
    };
    let entries = client.list_all().unwrap_or_default();
    let mut out = Vec::new();
    for entry in entries {
        let body = client
            .read_file(&entry.rel)
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_else(|_| "<读取失败>".into());
        out.push((entry.rel, body));
    }
    out.sort();
    client.quit();
    out
}

fn has_file(snapshot: &[(String, String)], rel: &str) -> bool {
    snapshot.iter().any(|(r, _)| r == rel)
}

fn content_of<'a>(snapshot: &'a [(String, String)], rel: &str) -> Option<&'a str> {
    snapshot
        .iter()
        .find(|(r, _)| r == rel)
        .map(|(_, c)| c.as_str())
}

fn main() -> ExitCode {
    let host = env_or("E2E_FTP_HOST", "127.0.0.1");
    let port: u16 = env_or("E2E_FTP_PORT", "2121").parse().unwrap_or(2121);
    let username = env_or("E2E_FTP_USER", "test");
    let password = env_or("E2E_FTP_PASS", "test123");
    let secure = env_or("E2E_FTP_SECURE", "false") == "true";
    let local = PathBuf::from(env_or("E2E_LOCAL", "target/e2e/local"));
    let remote_dir = env_or("E2E_REMOTE", "/remote");

    println!("=== FTPSync 端到端自检 ===");
    println!("服务器 : {host}:{port}  (FTPS={secure})");
    println!("本地   : {}", local.display());
    println!("远程   : {remote_dir}\n");

    // ---- 准备本地样本 ----
    if local.exists() {
        let _ = fs::remove_dir_all(&local);
    }
    fs::create_dir_all(local.join("sub")).expect("创建本地目录失败");
    fs::write(local.join("a.txt"), "hello v1").unwrap();
    fs::write(local.join("sub").join("b.txt"), "bee").unwrap();
    fs::write(local.join("skip.tmp"), "should be ignored").unwrap();

    let cfg = SyncConfig {
        local_dir: local.to_string_lossy().to_string(),
        remote_dir: remote_dir.clone(),
        mirror_delete: true,
        clean_extraneous: true,
        ignore_patterns: vec!["*.tmp".into()],
        auto_start: false,
        sync_on_start: true,
        debounce_ms: 300,
        ftp: FtpConfig {
            host: host.clone(),
            port,
            username,
            password,
            secure,
            timeout_secs: 20,
        },
    };

    // ---- 连通性 ----
    match Engine::test_connection(&cfg) {
        Ok(report) => println!("[1/6] 连通性检查 ... 通过\n      {report}"),
        Err(e) => {
            eprintln!("[1/6] 连通性检查 ... 失败: {e}");
            return ExitCode::FAILURE;
        }
    }

    // ---- 清空远端，确保起点干净 ----
    {
        let mut client = FtpClient::connect(&cfg.ftp, &cfg.remote_base()).expect("连接失败");
        for entry in client.list_all().unwrap_or_default() {
            let _ = client.delete_file(&entry.rel);
        }
        client.quit();
    }

    let work = PathBuf::from("target/e2e");
    fs::create_dir_all(&work).unwrap();
    // 清掉上一次运行的索引，保证自检从干净状态开始
    let _ = fs::remove_file(work.join("index.json"));
    let engine = Engine::new(
        None,
        cfg.clone(),
        work.join("config.json"),
        work.join("index.json"),
    );

    let mut passed = 0usize;
    let mut failed = 0usize;

    // ---- 2. 全量同步 ----
    engine.start().expect("启动同步失败");
    let synced = wait_until("等待首次全量同步完成", Duration::from_secs(40), || {
        let s = engine.stats();
        s.phase == "idle" && s.last_sync_at.is_some()
    });
    let snapshot = remote_snapshot(&cfg);
    let ok = synced
        && has_file(&snapshot, "a.txt")
        && has_file(&snapshot, "sub/b.txt")
        && !has_file(&snapshot, "skip.tmp");
    println!(
        "[2/6] 首次全量同步 ... {}\n      远端内容: {:?}",
        verdict(ok),
        snapshot.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>()
    );
    record(ok, &mut passed, &mut failed);

    // ---- 3. 修改文件 ----
    fs::write(local.join("a.txt"), "hello v2").unwrap();
    let ok = wait_until("等待修改生效", Duration::from_secs(25), || {
        content_of(&remote_snapshot(&cfg), "a.txt") == Some("hello v2")
    });
    println!("[3/6] 修改文件后自动上传 ... {}", verdict(ok));
    record(ok, &mut passed, &mut failed);

    // ---- 4. 新增文件 ----
    fs::write(local.join("new.txt"), "brand new").unwrap();
    let ok = wait_until("等待新增文件上传", Duration::from_secs(25), || {
        has_file(&remote_snapshot(&cfg), "new.txt")
    });
    println!("[4/6] 新增文件自动上传 ... {}", verdict(ok));
    record(ok, &mut passed, &mut failed);

    // ---- 5. 删除文件（镜像删除） ----
    fs::remove_file(local.join("sub").join("b.txt")).unwrap();
    let ok = wait_until("等待镜像删除生效", Duration::from_secs(25), || {
        !has_file(&remote_snapshot(&cfg), "sub/b.txt")
    });
    println!("[5/6] 本地删除同步到远端 ... {}", verdict(ok));
    record(ok, &mut passed, &mut failed);

    // ---- 6. 清理远端多余文件 ----
    {
        let orphan = work.join("orphan.txt");
        fs::write(&orphan, "i am not local").unwrap();
        let mut client = FtpClient::connect(&cfg.ftp, &cfg.remote_base()).unwrap();
        client.upload("orphan.txt", &orphan).unwrap();
        client.quit();
    }
    engine.request_full_sync().unwrap();
    let ok = wait_until("等待严格镜像清理", Duration::from_secs(40), || {
        let s = engine.stats();
        s.phase == "idle" && !has_file(&remote_snapshot(&cfg), "orphan.txt")
    });
    println!("[6/6] 严格镜像清理远端多余文件 ... {}", verdict(ok));
    record(ok, &mut passed, &mut failed);

    let stats = engine.stats();
    println!("\n--- 引擎日志 ---");
    for entry in engine.logs() {
        println!("  [{}] {}", entry.level, entry.message);
    }
    engine.stop();

    println!("\n统计: 上传 {} · 删除 {} · 跳过 {} · 失败 {}",
        stats.uploaded, stats.deleted, stats.skipped, stats.failed);
    if stats.failed > 0 {
        println!("最近错误: {}", stats.last_error);
    }
    println!("\n结果: {passed} 项通过, {failed} 项失败");

    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn verdict(ok: bool) -> &'static str {
    if ok {
        "通过"
    } else {
        "失败"
    }
}

fn record(ok: bool, passed: &mut usize, failed: &mut usize) {
    if ok {
        *passed += 1;
    } else {
        *failed += 1;
    }
}
