//! 配置模型与忽略规则匹配。

use std::fs;
use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

/// FTP / FTPS 连接参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// true = 显式 FTPS（AUTH TLS），false = 明文 FTP。
    pub secure: bool,
    pub timeout_secs: u64,
}

impl Default for FtpConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 21,
            username: String::new(),
            password: String::new(),
            secure: false,
            timeout_secs: 30,
        }
    }
}

/// 同步任务配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncConfig {
    /// 本地监听目录（绝对路径）。
    pub local_dir: String,
    /// 远程同步根目录（FTP 服务器上的绝对路径）。
    pub remote_dir: String,
    /// 本地删除后同步删除远端（镜像删除）。
    pub mirror_delete: bool,
    /// 全量同步时清理远端存在但本地不存在的文件（严格镜像）。
    pub clean_extraneous: bool,
    /// 忽略规则（glob）。
    pub ignore_patterns: Vec<String>,
    /// 启动应用后自动开始监听。
    pub auto_start: bool,
    /// 启动监听时先执行一次全量同步。
    pub sync_on_start: bool,
    /// 事件防抖时长（毫秒）。
    pub debounce_ms: u64,
    pub ftp: FtpConfig,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            local_dir: String::new(),
            remote_dir: "/".to_string(),
            mirror_delete: true,
            clean_extraneous: true,
            ignore_patterns: vec![
                "*.tmp".into(),
                "*.swp".into(),
                "*~".into(),
                "~$*".into(),
                ".DS_Store".into(),
                "Thumbs.db".into(),
                "desktop.ini".into(),
                ".git".into(),
                ".svn".into(),
                "node_modules".into(),
            ],
            auto_start: false,
            sync_on_start: true,
            debounce_ms: 800,
            ftp: FtpConfig::default(),
        }
    }
}

impl SyncConfig {
    /// 从磁盘读取配置，失败时返回默认值。
    pub fn load(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// 写回磁盘。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| format!("序列化配置失败: {e}"))?;
        fs::write(path, text).map_err(|e| format!("写入配置文件失败: {e}"))
    }

    /// 校验必填项，返回可直接展示给用户的错误描述。
    pub fn validate(&self) -> Result<(), String> {
        if self.ftp.host.trim().is_empty() {
            return Err("请填写 FTP 服务器地址".into());
        }
        if self.ftp.port == 0 {
            return Err("FTP 端口不合法".into());
        }
        if self.local_dir.trim().is_empty() {
            return Err("请选择本地同步目录".into());
        }
        let root = Path::new(self.local_dir.trim());
        if !root.is_dir() {
            return Err(format!("本地目录不存在或不可访问: {}", self.local_dir));
        }
        if self.remote_dir.trim().is_empty() {
            return Err("请填写远程同步目录".into());
        }
        Ok(())
    }

    /// 归一化后的远程根路径（以 / 开头，无尾部斜杠）。
    pub fn remote_base(&self) -> String {
        normalize_remote(&self.remote_dir)
    }

    /// 本地根目录。
    pub fn local_root(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(self.local_dir.trim())
    }

    pub fn ignore_matcher(&self) -> IgnoreMatcher {
        IgnoreMatcher::new(&self.ignore_patterns)
    }
}

/// 归一化远程路径：统一分隔符、补前导斜杠、去尾部斜杠。
pub fn normalize_remote(input: &str) -> String {
    let mut s = input.trim().replace('\\', "/");
    if s.is_empty() {
        return "/".to_string();
    }
    if !s.starts_with('/') {
        s = format!("/{s}");
    }
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

/// 基于 glob 的忽略规则匹配器。
///
/// 每个用户规则会展开为三种形态（原样、`**/规则`、`规则/**`），
/// 这样 `*.tmp`、`node_modules`、`.git` 这类写法都能正确命中任意层级。
pub struct IgnoreMatcher {
    set: GlobSet,
    empty: bool,
}

impl IgnoreMatcher {
    pub fn new(patterns: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        let mut added = 0usize;
        for raw in patterns {
            let p = raw.trim().replace('\\', "/");
            if p.is_empty() {
                continue;
            }
            let bare = p.trim_end_matches('/').to_string();
            let candidates = [
                p.clone(),
                format!("**/{p}"),
                format!("{bare}/**"),
                format!("**/{bare}/**"),
            ];
            for cand in candidates {
                if let Ok(g) = Glob::new(&cand) {
                    builder.add(g);
                    added += 1;
                }
            }
        }
        match builder.build() {
            Ok(set) => Self {
                set,
                empty: added == 0,
            },
            Err(_) => Self {
                set: GlobSet::empty(),
                empty: true,
            },
        }
    }

    /// 判断相对路径是否应被忽略。
    pub fn is_ignored(&self, rel: &str) -> bool {
        if self.empty {
            return false;
        }
        if rel.is_empty() {
            return false;
        }
        self.set.is_match(rel)
    }
}
