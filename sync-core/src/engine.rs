//! 实时同步引擎。
//!
//! 线程模型：
//! - **监听线程**：持有 `notify` 观察器，把原始文件系统事件累积到 `pending` 集合，
//!   做「静默期防抖」后统一派发为任务。
//! - **工作线程**：独占一个 FTP 会话，串行消费任务队列，保证控制连接不被并发复用。
//!
//! 增量判定依赖本地索引（相对路径 -> 大小 + 修改时间），因此无需读取远端元数据
//! 即可判断文件是否需要上传，避免了大量 MDTM/SIZE 往返。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::{EventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

use crate::config::{IgnoreMatcher, SyncConfig};
use crate::ftp::{with_client, FtpClient};

pub type Res<T> = std::result::Result<T, String>;

/// 取锁，忽略中毒状态，避免任一线程 panic 后整体不可用。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 本地文件的增量指纹。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub size: u64,
    pub mtime: u64,
}

impl FileMeta {
    pub fn from_metadata(md: &fs::Metadata) -> Self {
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            size: md.len(),
            mtime,
        }
    }
}

/// 推送给前端的日志条目。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub ts: u64,
    pub level: String,
    pub message: String,
}

/// 推送给前端的状态快照。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStats {
    pub running: bool,
    /// idle | connecting | scanning | syncing | stopped
    pub phase: String,
    pub uploaded: u64,
    pub deleted: u64,
    pub skipped: u64,
    pub failed: u64,
    /// 待处理任务数
    pub pending: usize,
    pub total_files: usize,
    pub last_error: String,
    pub last_sync_at: Option<u64>,
    pub local_dir: String,
    pub remote_dir: String,
    pub host: String,
}

impl Default for SyncStats {
    fn default() -> Self {
        Self {
            running: false,
            phase: "idle".to_string(),
            uploaded: 0,
            deleted: 0,
            skipped: 0,
            failed: 0,
            pending: 0,
            total_files: 0,
            last_error: String::new(),
            last_sync_at: None,
            local_dir: String::new(),
            remote_dir: String::new(),
            host: String::new(),
        }
    }
}

/// 工作线程消费的任务。
enum Job {
    /// 本地新增或修改（相对路径，可能是文件或目录）。
    Upsert(String),
    /// 本地删除（相对路径）。
    Delete(String),
    /// 全量同步。
    FullSync,
    /// 断开当前 FTP 会话，使新的连接参数在下次操作时生效。
    Reconnect,
    Shutdown,
}

/// 界面层实现此接口以接收引擎的日志与状态推送。
///
/// 引擎内部在工作线程中调用，实现必须是线程安全且非阻塞的。
pub trait EventSink: Send + Sync + 'static {
    fn on_log(&self, entry: &LogEntry);
    fn on_status(&self, stats: &SyncStats);
}

/// 同步引擎，作为宿主的全局状态。
pub struct Engine {
    sink: Option<Box<dyn EventSink>>,
    cfg: Mutex<SyncConfig>,
    stats: Mutex<SyncStats>,
    logs: Mutex<VecDeque<LogEntry>>,
    /// 相对路径 -> 已同步指纹
    index: Mutex<HashMap<String, FileMeta>>,
    /// 监听线程累积的原始事件路径
    pending: Arc<Mutex<HashSet<PathBuf>>>,
    job_tx: Mutex<Option<Sender<Job>>>,
    stop_flag: Arc<AtomicBool>,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    config_path: PathBuf,
    index_path: PathBuf,
}

impl Engine {
    pub fn new(
        sink: Option<Box<dyn EventSink>>,
        cfg: SyncConfig,
        config_path: PathBuf,
        index_path: PathBuf,
    ) -> Arc<Self> {
        let index = load_index(&index_path);
        Arc::new(Self {
            sink,
            cfg: Mutex::new(cfg),
            stats: Mutex::new(SyncStats::default()),
            logs: Mutex::new(VecDeque::with_capacity(256)),
            index: Mutex::new(index),
            pending: Arc::new(Mutex::new(HashSet::new())),
            job_tx: Mutex::new(None),
            stop_flag: Arc::new(AtomicBool::new(false)),
            dispatcher: Mutex::new(None),
            worker: Mutex::new(None),
            config_path,
            index_path,
        })
    }

    // ---------------------------------------------------------------- 对外查询

    pub fn config(&self) -> SyncConfig {
        lock(&self.cfg).clone()
    }

    pub fn stats(&self) -> SyncStats {
        lock(&self.stats).clone()
    }

    pub fn logs(&self) -> Vec<LogEntry> {
        lock(&self.logs).iter().cloned().collect()
    }

    pub fn clear_logs(&self) {
        lock(&self.logs).clear();
    }

    /// 清零本次会话的统计计数（保留监听状态与上次同步时间）。
    pub fn reset_stats(&self) {
        self.set_stats(|s| {
            s.uploaded = 0;
            s.deleted = 0;
            s.skipped = 0;
            s.failed = 0;
            s.last_error.clear();
        });
        self.log("info", "统计计数已重置");
    }

    pub fn is_running(&self) -> bool {
        lock(&self.stats).running
    }

    pub fn set_config(&self, cfg: SyncConfig) {
        *lock(&self.cfg) = cfg;
    }

    /// 持久化配置；若监听范围发生变化则自动重启监听。
    pub fn apply_config(self: &Arc<Self>, cfg: SyncConfig) -> Res<()> {
        cfg.save(&self.config_path)?;
        let previous = self.config();
        let running = self.is_running();
        let scope_changed = previous.local_dir != cfg.local_dir
            || previous.ignore_patterns != cfg.ignore_patterns
            || previous.debounce_ms != cfg.debounce_ms;
        let session_changed = previous.ftp != cfg.ftp || previous.remote_dir != cfg.remote_dir;

        let host = cfg.ftp.host.clone();
        let local = cfg.local_dir.clone();
        let remote = cfg.remote_base();
        self.set_config(cfg);
        self.set_stats(|s| {
            s.host = host;
            s.local_dir = local;
            s.remote_dir = remote;
        });

        if running && scope_changed {
            self.log("info", "同步范围已变更，正在重启监听…");
            self.stop();
            self.start()?;
        } else if running && session_changed {
            // 会话缓存了连接参数与远程根目录，必须断开重建才能生效
            self.enqueue(Job::Reconnect);
        }
        Ok(())
    }

    // ---------------------------------------------------------------- 事件推送

    pub fn log(&self, level: &str, message: impl Into<String>) {
        let entry = LogEntry {
            ts: now_ms(),
            level: level.to_string(),
            message: message.into(),
        };
        {
            let mut logs = lock(&self.logs);
            logs.push_back(entry.clone());
            while logs.len() > 500 {
                logs.pop_front();
            }
        }
        if let Some(sink) = &self.sink {
            sink.on_log(&entry);
        }
    }

    pub fn set_stats<F: FnOnce(&mut SyncStats)>(&self, f: F) {
        {
            let mut s = lock(&self.stats);
            f(&mut s);
        }
        self.emit_stats();
    }

    pub fn emit_stats(&self) {
        if let Some(sink) = &self.sink {
            sink.on_status(&self.stats());
        }
    }

    pub fn set_phase(&self, phase: &str) {
        self.set_stats(|s| s.phase = phase.to_string());
    }

    pub fn bump_uploaded(&self) {
        self.set_stats(|s| s.uploaded += 1);
    }

    pub fn bump_deleted(&self) {
        self.set_stats(|s| s.deleted += 1);
    }

    pub fn bump_skipped(&self) {
        self.set_stats(|s| s.skipped += 1);
    }

    /// 记录一次失败并广播。
    pub fn fail(&self, message: &str) {
        {
            let mut s = lock(&self.stats);
            s.failed += 1;
            s.last_error = message.to_string();
        }
        self.emit_stats();
        self.log("error", message);
    }

    pub fn bump_queue(&self, delta: i64) {
        self.set_stats(|s| {
            s.pending = if delta >= 0 {
                s.pending + delta as usize
            } else {
                s.pending.saturating_sub((-delta) as usize)
            };
        });
    }

    // ---------------------------------------------------------------- 生命周期

    /// 启动监听与同步工作线程。
    pub fn start(self: &Arc<Self>) -> Res<()> {
        if self.is_running() {
            return Ok(());
        }
        let cfg = self.config();
        cfg.validate()?;

        // 监听必须使用绝对路径：文件系统事件是绝对路径，若根目录是相对路径，
        // 后续按前缀截取相对路径会全部失败，导致事件被静默丢弃。
        let raw_root = cfg.local_root();
        let root = std::path::absolute(&raw_root).unwrap_or(raw_root);
        let ignore = cfg.ignore_matcher();
        let debounce = Duration::from_millis(cfg.debounce_ms.clamp(120, 10_000));

        let (tx, rx) = channel::<Job>();
        *lock(&self.job_tx) = Some(tx);
        self.stop_flag.store(false, Ordering::SeqCst);

        let worker_engine = Arc::clone(self);
        *lock(&self.worker) = Some(thread::spawn(move || worker_loop(worker_engine, rx)));

        let watcher_engine = Arc::clone(self);
        let stop = Arc::clone(&self.stop_flag);
        *lock(&self.dispatcher) = Some(thread::spawn(move || {
            dispatcher_loop(watcher_engine, root, ignore, debounce, stop)
        }));

        let (host, local, remote) = (cfg.ftp.host.clone(), cfg.local_dir.clone(), cfg.remote_base());
        self.set_stats(|s| {
            s.running = true;
            s.phase = "idle".to_string();
            s.last_error.clear();
            s.host = host;
            s.local_dir = local;
            s.remote_dir = remote;
        });
        self.log("ok", "实时监听已启动");

        if cfg.sync_on_start {
            self.enqueue(Job::FullSync);
        }
        Ok(())
    }

    /// 停止监听与工作线程（会等待线程退出）。
    pub fn stop(&self) {
        if !self.is_running() {
            return;
        }
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(handle) = lock(&self.dispatcher).take() {
            let _ = handle.join();
        }
        if let Some(tx) = lock(&self.job_tx).take() {
            let _ = tx.send(Job::Shutdown);
        }
        if let Some(handle) = lock(&self.worker).take() {
            let _ = handle.join();
        }
        lock(&self.pending).clear();
        self.persist_index();
        self.set_stats(|s| {
            s.running = false;
            s.phase = "stopped".to_string();
            s.pending = 0;
        });
        self.log("warn", "实时监听已停止");
    }

    /// 请求一次全量同步。
    pub fn request_full_sync(&self) -> Res<()> {
        if !self.is_running() {
            return Err("请先启动监听".into());
        }
        self.enqueue(Job::FullSync);
        Ok(())
    }

    fn enqueue(&self, job: Job) {
        let sender = lock(&self.job_tx).clone();
        if let Some(tx) = sender {
            if tx.send(job).is_ok() {
                self.bump_queue(1);
            }
        }
    }

    fn persist_index(&self) {
        let snapshot = lock(&self.index).clone();
        save_index(&self.index_path, &snapshot);
    }

    // ---------------------------------------------------------------- 命令：连通性测试

    pub fn test_connection(cfg: &SyncConfig) -> Res<String> {
        if cfg.ftp.host.trim().is_empty() {
            return Err("请填写 FTP 服务器地址".into());
        }
        let base = cfg.remote_base();
        let mut client = FtpClient::connect(&cfg.ftp, &base)?;
        let report = client.probe()?;
        client.quit();
        Ok(report)
    }
}

// -------------------------------------------------------------------- 监听线程

/// 监听本地目录，做静默期防抖后派发任务。
fn dispatcher_loop(
    engine: Arc<Engine>,
    root: PathBuf,
    ignore: IgnoreMatcher,
    debounce: Duration,
    stop: Arc<AtomicBool>,
) {
    let pending = Arc::clone(&engine.pending);
    let watcher_pending = Arc::clone(&pending);
    let handler_engine = Arc::clone(&engine);
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(event) => {
                // 只关心增删改，忽略访问事件
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                let mut set = lock(&watcher_pending);
                for path in event.paths {
                    set.insert(path);
                }
            }
            Err(err) => {
                handler_engine.log("warn", format!("文件监听异常: {err}"));
            }
        }
    });

    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            engine.fail(&format!("创建文件监听器失败: {e}"));
            return;
        }
    };
    if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
        engine.fail(&format!("监听目录失败 [{}]: {e}", root.display()));
        return;
    }
    engine.log("ok", format!("正在监听 {}", root.display()));

    let max_wait = debounce.saturating_mul(12);
    let mut last_len = 0usize;
    let mut last_change = Instant::now();
    let mut dirty_since: Option<Instant> = None;

    while !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(120));

        let len = lock(&pending).len();
        if len == 0 {
            last_len = 0;
            dirty_since = None;
            continue;
        }
        if dirty_since.is_none() {
            dirty_since = Some(Instant::now());
            last_change = Instant::now();
        }
        if len != last_len {
            last_len = len;
            last_change = Instant::now();
        }

        let quiet = last_change.elapsed() >= debounce;
        let stalled = dirty_since.map(|t| t.elapsed() >= max_wait).unwrap_or(false);
        if !quiet && !stalled {
            continue;
        }

        // drain 保证不丢事件
        let paths: Vec<PathBuf> = lock(&pending).drain().collect();
        last_len = 0;
        dirty_since = None;
        dispatch(&engine, &root, &ignore, paths);
    }

    drop(watcher);
    engine.log("info", "文件监听已释放");
}

/// 把原始事件路径转成同步任务。
fn dispatch(engine: &Arc<Engine>, root: &Path, ignore: &IgnoreMatcher, paths: Vec<PathBuf>) {
    let mut rels: HashSet<String> = HashSet::new();
    for path in paths {
        let rel = match strip_root(root, &path) {
            Some(r) => r,
            None => continue,
        };
        // 忽略规则命中的路径及其子路径
        let mut probe = rel.as_str();
        let mut hit = ignore.is_ignored(probe);
        while !hit {
            match probe.rfind('/') {
                Some(i) => {
                    probe = &probe[..i];
                    hit = ignore.is_ignored(probe);
                }
                None => break,
            }
        }
        if hit {
            continue;
        }
        rels.insert(rel);
    }
    if rels.is_empty() {
        return;
    }

    let sender = lock(&engine.job_tx).clone();
    let Some(tx) = sender else {
        return;
    };
    let mut sent = 0usize;
    for rel in &rels {
        let job = if root.join(rel).exists() {
            Job::Upsert(rel.clone())
        } else {
            Job::Delete(rel.clone())
        };
        if tx.send(job).is_err() {
            break;
        }
        sent += 1;
    }
    if sent > 0 {
        engine.bump_queue(sent as i64);
        engine.log("info", format!("检测到 {sent} 处变更"));
    }
}

/// 把路径规范化为 `/` 分隔、去掉 Windows `\\?\` 前缀的字符串。
fn norm_slash(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('\\', "/");
    if let Some(rest) = s.strip_prefix("//?/") {
        s = rest.to_string();
    }
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

/// 从事件绝对路径中截出相对监听根目录的部分（Windows 下大小写不敏感）。
fn strip_root(root: &Path, path: &Path) -> Option<String> {
    let root_s = norm_slash(root);
    let path_s = norm_slash(path);
    if root_s.is_empty() || path_s.len() < root_s.len() {
        return None;
    }
    if !path_s.is_char_boundary(root_s.len()) {
        return None;
    }
    let (head, tail) = path_s.split_at(root_s.len());
    if !head.eq_ignore_ascii_case(&root_s) {
        return None;
    }
    let rest = tail.trim_start_matches('/');
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

// -------------------------------------------------------------------- 工作线程

fn worker_loop(engine: Arc<Engine>, rx: Receiver<Job>) {
    let mut client: Option<FtpClient> = None;
    let mut last_persist = Instant::now();

    while let Ok(job) = rx.recv() {
        match job {
            Job::Shutdown => break,
            Job::FullSync => {
                do_full_sync(&engine, &mut client);
                engine.bump_queue(-1);
                engine.persist_index();
                last_persist = Instant::now();
            }
            Job::Upsert(rel) => {
                do_upsert(&engine, &mut client, &rel);
                engine.bump_queue(-1);
                if last_persist.elapsed() >= Duration::from_millis(1500) {
                    engine.persist_index();
                    last_persist = Instant::now();
                }
            }
            Job::Delete(rel) => {
                do_delete(&engine, &mut client, &rel);
                engine.bump_queue(-1);
                if last_persist.elapsed() >= Duration::from_millis(1500) {
                    engine.persist_index();
                    last_persist = Instant::now();
                }
            }
            Job::Reconnect => {
                if let Some(mut c) = client.take() {
                    c.quit();
                }
                engine.log("info", "连接参数已更新，FTP 会话将在下次操作时重建");
                engine.bump_queue(-1);
            }
        }
    }

    if let Some(mut c) = client.take() {
        c.quit();
    }
    engine.persist_index();
    engine.log("info", "同步工作线程已退出");
}

/// 处理本地新增/修改。
fn do_upsert(engine: &Arc<Engine>, client: &mut Option<FtpClient>, rel: &str) {
    let cfg = engine.config();
    if cfg.ignore_matcher().is_ignored(rel) {
        return;
    }
    let root = cfg.local_root();
    let abs = root.join(rel);

    let md = match fs::metadata(&abs) {
        Ok(m) => m,
        // 已被删除，转为删除任务
        Err(_) => {
            do_delete(engine, client, rel);
            return;
        }
    };

    if md.is_dir() {
        if let Err(e) = with_client(client, &cfg.ftp, &cfg.remote_dir, |c| c.ensure_dir(rel)) {
            engine.fail(&e);
            return;
        }
        let files = scan_local(&root, &abs, &cfg.ignore_matcher());
        for (child_rel, (_, child_abs)) in files {
            upload_file(engine, client, &cfg, &child_rel, &child_abs);
        }
    } else if md.is_file() {
        upload_file(engine, client, &cfg, rel, &abs);
    }
}

/// 上传单个文件（先比对索引，未变化则跳过）。
fn upload_file(
    engine: &Arc<Engine>,
    client: &mut Option<FtpClient>,
    cfg: &SyncConfig,
    rel: &str,
    abs: &Path,
) {
    let md = match fs::metadata(abs) {
        Ok(m) => m,
        Err(_) => return,
    };
    let meta = FileMeta::from_metadata(&md);

    {
        let index = lock(&engine.index);
        if let Some(prev) = index.get(rel) {
            if *prev == meta {
                engine.bump_skipped();
                return;
            }
        }
    }

    match with_client(client, &cfg.ftp, &cfg.remote_dir, |c| c.upload(rel, abs)) {
        Ok(size) => {
            lock(&engine.index).insert(rel.to_string(), meta);
            engine.bump_uploaded();
            engine.log("ok", format!("↑ {rel}  ({size} 字节)"));
        }
        Err(e) => engine.fail(&e),
    }
}

/// 判断 FTP 错误是否属于「目标已不存在」。
///
/// 删除操作遇到这种情况属于正常竞态（远端被外部删除、或本地索引残留），
/// 不应计入失败统计，否则会持续产生无意义的错误噪声。
fn is_missing_remote(err: &str) -> bool {
    let lower = err.to_lowercase();
    if lower.contains("permission") || lower.contains("denied") {
        return false;
    }
    (lower.contains("550")
        || lower.contains("not found")
        || lower.contains("no such")
        || lower.contains("does not exist"))
        && !lower.contains("login")
}

/// 处理本地删除（含目录级联）。
fn do_delete(engine: &Arc<Engine>, client: &mut Option<FtpClient>, rel: &str) {
    let cfg = engine.config();

    let files: Vec<String> = {
        let index = lock(&engine.index);
        let prefix = format!("{rel}/");
        let mut v: Vec<String> = index
            .keys()
            .filter(|k| k.as_str() == rel || k.starts_with(&prefix))
            .cloned()
            .collect();
        v.sort();
        v
    };

    if !cfg.mirror_delete {
        let mut index = lock(&engine.index);
        for f in &files {
            index.remove(f);
        }
        drop(index);
        engine.log(
            "warn",
            format!("本地已删除 {rel}（镜像删除已关闭，远端保留）"),
        );
        return;
    }

    if files.is_empty() {
        // 可能是一个空目录
        let _ = with_client(client, &cfg.ftp, &cfg.remote_dir, |c| c.remove_dir(rel));
        return;
    }

    let mut dirs: HashSet<String> = HashSet::new();
    for file in &files {
        match with_client(client, &cfg.ftp, &cfg.remote_dir, |c| c.delete_file(file)) {
            Ok(_) => {
                lock(&engine.index).remove(file);
                engine.bump_deleted();
                engine.log("ok", format!("✕ {file}"));
            }
            Err(e) if is_missing_remote(&e) => {
                lock(&engine.index).remove(file);
                engine.log("info", format!("· {file} 远端已不存在，跳过删除"));
            }
            Err(e) => engine.fail(&e),
        }
        let mut parent = file.as_str();
        while let Some(i) = parent.rfind('/') {
            parent = &parent[..i];
            if !parent.is_empty() {
                dirs.insert(parent.to_string());
            }
        }
    }

    // 由深到浅删除空目录（非空目录删除会失败，属预期）
    let mut ordered: Vec<String> = dirs.into_iter().collect();
    ordered.sort_by(|a, b| {
        b.matches('/')
            .count()
            .cmp(&a.matches('/').count())
            .then(b.len().cmp(&a.len()))
    });
    for dir in ordered {
        let _ = with_client(client, &cfg.ftp, &cfg.remote_dir, |c| c.remove_dir(&dir));
    }
}

/// 全量同步：扫描本地 -> 上传差异 -> 清理远端失效与多余文件。
fn do_full_sync(engine: &Arc<Engine>, client: &mut Option<FtpClient>) {
    let cfg = engine.config();
    engine.set_phase("scanning");
    engine.log("info", "开始全量同步：扫描本地目录…");

    let root = cfg.local_root();
    let ignore = cfg.ignore_matcher();
    let locals = scan_local(&root, &root, &ignore);
    engine.set_stats(|s| s.total_files = locals.len());
    engine.log("info", format!("本地共 {} 个文件", locals.len()));

    // 计算差异
    let mut todo: Vec<(String, PathBuf, FileMeta)> = Vec::new();
    {
        let index = lock(&engine.index);
        for (rel, (meta, abs)) in &locals {
            match index.get(rel) {
                Some(prev) if prev == meta => {}
                _ => todo.push((rel.clone(), abs.clone(), *meta)),
            }
        }
    }
    let total = todo.len();
    engine.set_phase("syncing");
    engine.set_stats(|s| s.pending = total);
    engine.log("info", format!("需要上传 {total} 个文件"));

    for (i, (rel, abs, meta)) in todo.iter().enumerate() {
        match with_client(client, &cfg.ftp, &cfg.remote_dir, |c| c.upload(rel, abs)) {
            Ok(_) => {
                lock(&engine.index).insert(rel.clone(), *meta);
                engine.set_stats(|s| {
                    s.uploaded += 1;
                    s.pending = total.saturating_sub(i + 1);
                });
            }
            Err(e) => {
                engine.fail(&e);
                engine.set_stats(|s| s.pending = total.saturating_sub(i + 1));
            }
        }
        if total > 50 && (i + 1) % 50 == 0 {
            engine.log("info", format!("上传进度 {}/{}", i + 1, total));
        }
    }
    if total > 0 {
        engine.log("ok", format!("差异上传完成，共 {total} 个文件"));
    }

    // 清理：本地已不存在的文件
    if cfg.mirror_delete {
        let gone: Vec<String> = {
            let index = lock(&engine.index);
            index
                .keys()
                .filter(|k| !locals.contains_key(*k))
                .cloned()
                .collect()
        };
        if !gone.is_empty() {
            engine.log("info", format!("清理远端已失效文件 {} 个", gone.len()));
            for rel in gone {
                do_delete(engine, client, &rel);
            }
        }
    }

    // 清理：远端存在但本地不存在的文件
    if cfg.mirror_delete && cfg.clean_extraneous {
        match with_client(client, &cfg.ftp, &cfg.remote_dir, |c| {
            let mut list = Vec::new();
            c.walk("", &mut list)?;
            Ok(list)
        }) {
            Ok(list) => {
                let extra: Vec<crate::ftp::RemoteEntry> = list
                    .into_iter()
                    .filter(|e| !locals.contains_key(&e.rel))
                    .collect();
                if extra.is_empty() {
                    engine.log("info", "远端无多余文件");
                } else {
                    engine.log("warn", format!("清理远端多余文件 {} 个", extra.len()));
                    let mut dirs: HashSet<String> = HashSet::new();
                    for entry in extra {
                        let rel = entry.rel;
                        match with_client(client, &cfg.ftp, &cfg.remote_dir, |c| c.delete_file(&rel))
                        {
                            Ok(_) => {
                                engine.bump_deleted();
                                engine.log(
                                    "ok",
                                    format!("✕ {rel}（远端多余，{} 字节）", entry.size),
                                );
                            }
                            Err(e) if is_missing_remote(&e) => {
                                engine.log("info", format!("· {rel} 远端已不存在，跳过删除"));
                            }
                            Err(e) => engine.fail(&e),
                        }
                        let mut parent = rel.as_str();
                        while let Some(i) = parent.rfind('/') {
                            parent = &parent[..i];
                            if !parent.is_empty() {
                                dirs.insert(parent.to_string());
                            }
                        }
                    }
                    let mut ordered: Vec<String> = dirs.into_iter().collect();
                    ordered.sort_by(|a, b| {
                        b.matches('/')
                            .count()
                            .cmp(&a.matches('/').count())
                            .then(b.len().cmp(&a.len()))
                    });
                    for dir in ordered {
                        let _ = with_client(client, &cfg.ftp, &cfg.remote_dir, |c| c.remove_dir(&dir));
                    }
                }
            }
            Err(e) => engine.log("warn", format!("远端目录列举失败，跳过多余文件清理: {e}")),
        }
    }

    engine.set_stats(|s| {
        s.pending = 0;
        s.last_sync_at = Some(now_ms());
    });
    engine.set_phase("idle");
    engine.log("ok", "全量同步完成");
}

// -------------------------------------------------------------------- 工具函数

/// 递归扫描本地目录，返回 相对路径 -> (指纹, 绝对路径)。
fn scan_local(
    root: &Path,
    dir: &Path,
    ignore: &IgnoreMatcher,
) -> HashMap<String, (FileMeta, PathBuf)> {
    let mut out = HashMap::new();
    scan_into(root, dir, ignore, &mut out);
    out
}

fn scan_into(
    root: &Path,
    dir: &Path,
    ignore: &IgnoreMatcher,
    out: &mut HashMap<String, (FileMeta, PathBuf)>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if rel.is_empty() || ignore.is_ignored(&rel) {
            continue;
        }
        if file_type.is_dir() {
            scan_into(root, &path, ignore, out);
        } else if file_type.is_file() {
            if let Ok(md) = entry.metadata() {
                out.insert(rel, (FileMeta::from_metadata(&md), path));
            }
        }
    }
}

fn load_index(path: &Path) -> HashMap<String, FileMeta> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn save_index(path: &Path, map: &HashMap<String, FileMeta>) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(map) {
        let _ = fs::write(path, text);
    }
}
