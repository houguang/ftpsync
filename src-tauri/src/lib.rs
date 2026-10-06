//! FTPSync 桌面应用（Tauri 层）。
//!
//! 只负责三件事：把同步内核的事件转发到前端、把前端指令转成内核对方法的调用、
//! 以及原生目录选择。所有同步逻辑都在 `sync-core` 中，与界面完全解耦。

use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

use sync_core::config::SyncConfig;
use sync_core::engine::{Engine, EventSink, LogEntry, SyncStats};

/// 把内核事件通过 Tauri 事件通道推送到前端。
struct TauriSink {
    app: AppHandle,
}

impl EventSink for TauriSink {
    fn on_log(&self, entry: &LogEntry) {
        let _ = self.app.emit("sync-log", entry);
    }

    fn on_status(&self, stats: &SyncStats) {
        let _ = self.app.emit("sync-status", stats);
    }
}

// ---------------------------------------------------------------- 配置

#[tauri::command]
fn load_config(state: State<'_, Arc<Engine>>) -> SyncConfig {
    state.config()
}

#[tauri::command]
async fn save_config(
    state: State<'_, Arc<Engine>>,
    config: SyncConfig,
) -> Result<SyncConfig, String> {
    let engine = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        engine.apply_config(config)?;
        Ok::<SyncConfig, String>(engine.config())
    })
    .await
    .map_err(|e| format!("保存配置失败: {e}"))?
}

#[tauri::command]
async fn pick_folder(app: AppHandle, title: String) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title(title)
            .blocking_pick_folder()
            .and_then(|p| p.into_path().ok())
            .map(|p| p.to_string_lossy().to_string())
    })
    .await
    .ok()
    .flatten()
}

// ---------------------------------------------------------------- 连接与同步

#[tauri::command]
async fn test_connection(config: SyncConfig) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || Engine::test_connection(&config))
        .await
        .map_err(|e| format!("连接测试失败: {e}"))?
}

#[tauri::command]
async fn start_sync(state: State<'_, Arc<Engine>>) -> Result<SyncStats, String> {
    let engine = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        engine.start()?;
        Ok::<SyncStats, String>(engine.stats())
    })
    .await
    .map_err(|e| format!("启动失败: {e}"))?
}

#[tauri::command]
async fn stop_sync(state: State<'_, Arc<Engine>>) -> Result<SyncStats, String> {
    let engine = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        engine.stop();
        engine.stats()
    })
    .await
    .map_err(|e| format!("停止失败: {e}"))
}

#[tauri::command]
fn trigger_full_sync(state: State<'_, Arc<Engine>>) -> Result<(), String> {
    state.request_full_sync()
}

// ---------------------------------------------------------------- 状态与日志

#[tauri::command]
fn get_status(state: State<'_, Arc<Engine>>) -> SyncStats {
    state.stats()
}

#[tauri::command]
fn get_logs(state: State<'_, Arc<Engine>>) -> Vec<LogEntry> {
    state.logs()
}

#[tauri::command]
fn clear_logs(state: State<'_, Arc<Engine>>) {
    state.clear_logs()
}

#[tauri::command]
fn reset_stats(state: State<'_, Arc<Engine>>) {
    state.reset_stats()
}

// ---------------------------------------------------------------- 入口

/// 构建并运行桌面应用。
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let dir = handle
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            let config_path = dir.join("ftpsync-config.json");
            let index_path = dir.join("ftpsync-index.json");

            let cfg = SyncConfig::load(&config_path);
            let auto = cfg.auto_start;
            let sink = Box::new(TauriSink {
                app: handle.clone(),
            });
            let engine = Engine::new(Some(sink), cfg, config_path, index_path);
            app.manage(Arc::clone(&engine));

            engine.log("info", format!("配置文件目录: {}", dir.display()));

            if auto {
                if let Err(e) = engine.start() {
                    engine.log("error", format!("自动启动失败: {e}"));
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            load_config,
            save_config,
            pick_folder,
            test_connection,
            start_sync,
            stop_sync,
            trigger_full_sync,
            get_status,
            get_logs,
            clear_logs,
            reset_stats
        ])
        .run(tauri::generate_context!())
        .expect("FTPSync 启动失败");
}
