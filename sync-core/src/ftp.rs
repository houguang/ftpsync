//! FTP / FTPS 客户端封装。
//!
//! 统一使用 `NativeTlsFtpStream` 一种类型：明文 FTP 时不调用 `into_secure`，
//! 这样明文与 FTPS 共用同一套代码路径，避免两套类型带来的分支复杂度。

use std::collections::HashSet;
use std::fs::File;
use std::net::ToSocketAddrs;
use std::path::Path;
use std::time::Duration;

use suppaftp::list::{File as ListFile, ListParser};
use suppaftp::native_tls::TlsConnector as TlsConnectorBuilder;
use suppaftp::types::FileType as TransferType;
use suppaftp::{Mode, NativeTlsConnector, NativeTlsFtpStream};

use crate::config::{normalize_remote, FtpConfig};

pub type Res<T> = std::result::Result<T, String>;

/// 远端目录中的一条文件记录。
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub rel: String,
    pub size: u64,
}

/// 一个已建立连接的 FTP 会话。
pub struct FtpClient {
    stream: NativeTlsFtpStream,
    base: String,
    /// 已确认存在的远程目录（相对路径），避免重复 MKD。
    known_dirs: HashSet<String>,
}

impl FtpClient {
    /// 建立连接、登录并切换到二进制被动模式。
    pub fn connect(cfg: &FtpConfig, remote_base: &str) -> Res<Self> {
        let host = cfg.host.trim().to_string();
        let addr = format!("{host}:{}", cfg.port);
        let timeout = Duration::from_secs(cfg.timeout_secs.clamp(5, 600));

        let sockaddr = addr
            .to_socket_addrs()
            .map_err(|e| format!("解析 FTP 地址失败 [{addr}]: {e}"))?
            .next()
            .ok_or_else(|| format!("无法解析 FTP 地址: {addr}"))?;

        let mut stream = NativeTlsFtpStream::connect_timeout(sockaddr, timeout)
            .map_err(|e| format!("连接 FTP 服务器失败 [{addr}]: {e}"))?;

        if cfg.secure {
            let builder = TlsConnectorBuilder::new().map_err(|e| format!("初始化 TLS 失败: {e}"))?;
            stream = stream
                .into_secure(NativeTlsConnector::from(builder), &host)
                .map_err(|e| format!("FTPS 握手失败（请确认服务器支持显式 TLS）: {e}"))?;
        }

        stream
            .login(cfg.username.as_str(), cfg.password.as_str())
            .map_err(|e| format!("登录失败（请检查账号与密码）: {e}"))?;

        stream
            .transfer_type(TransferType::Binary)
            .map_err(|e| format!("切换二进制传输模式失败: {e}"))?;

        stream.set_mode(Mode::Passive);

        let mut client = Self {
            stream,
            base: normalize_remote(remote_base),
            known_dirs: HashSet::new(),
        };
        client.ensure_dir("")?;
        Ok(client)
    }

    /// 远程根目录。
    pub fn base(&self) -> &str {
        &self.base
    }

    /// 把相对路径拼成远程绝对路径。
    pub fn remote_path(&self, rel: &str) -> String {
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            self.base.clone()
        } else if self.base == "/" {
            format!("/{rel}")
        } else {
            format!("{}/{}", self.base, rel)
        }
    }

    /// 连通性自检：打印服务器当前目录并列出同步根目录内容。
    pub fn probe(&mut self) -> Res<String> {
        let pwd = self.stream.pwd().unwrap_or_else(|_| "未知".to_string());
        let entries = self.list_dir("")?;
        Ok(format!(
            "连接正常。服务器登录目录 {}，同步根目录 [{}] 下有 {} 个条目",
            pwd,
            self.base,
            entries.len()
        ))
    }

    /// 递归确保远程目录存在（相对路径，"" 表示根目录本身）。
    pub fn ensure_dir(&mut self, rel: &str) -> Res<()> {
        let rel = rel.trim_matches('/').replace('\\', "/");
        if rel.is_empty() {
            if !self.known_dirs.contains("") {
                self.mkdir_if_needed("")?;
                self.known_dirs.insert(String::new());
            }
            return Ok(());
        }
        let mut cur = String::new();
        for seg in rel.split('/') {
            if seg.is_empty() || seg == "." {
                continue;
            }
            if !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(seg);
            if self.known_dirs.contains(&cur) {
                continue;
            }
            self.mkdir_if_needed(&cur)?;
            self.known_dirs.insert(cur.clone());
        }
        Ok(())
    }

    fn mkdir_if_needed(&mut self, rel: &str) -> Res<()> {
        let path = self.remote_path(rel);
        if self.stream.cwd(&path).is_ok() {
            return Ok(());
        }
        match self.stream.mkdir(&path) {
            Ok(_) => Ok(()),
            Err(e) => {
                // 可能是并发下已存在，二次确认
                if self.stream.cwd(&path).is_ok() {
                    Ok(())
                } else {
                    Err(format!("创建远程目录失败 [{path}]: {e}"))
                }
            }
        }
    }

    /// 上传单个文件，自动创建其父目录。
    pub fn upload(&mut self, rel: &str, local: &Path) -> Res<u64> {
        let rel = rel.trim_matches('/').replace('\\', "/");
        let parent = match rel.rfind('/') {
            Some(i) => rel[..i].to_string(),
            None => String::new(),
        };
        self.ensure_dir(&parent)?;

        let target = self.remote_path(&rel);
        let mut file = File::open(local)
            .map_err(|e| format!("读取本地文件失败 [{}]: {e}", local.display()))?;
        self.stream
            .put_file(&target, &mut file)
            .map_err(|e| format!("上传失败 [{rel}]: {e}"))
    }

    /// 读取远程文件内容（用于自检与校验）。
    pub fn read_file(&mut self, rel: &str) -> Res<Vec<u8>> {
        let path = self.remote_path(rel);
        let cursor = self
            .stream
            .retr_as_buffer(&path)
            .map_err(|e| format!("下载失败 [{path}]: {e}"))?;
        Ok(cursor.into_inner())
    }

    /// 递归列出远程根目录下的所有文件。
    pub fn list_all(&mut self) -> Res<Vec<RemoteEntry>> {
        let mut out = Vec::new();
        self.walk("", &mut out)?;
        Ok(out)
    }

    pub fn delete_file(&mut self, rel: &str) -> Res<()> {        let path = self.remote_path(rel);
        self.stream
            .rm(&path)
            .map_err(|e| format!("删除远程文件失败 [{path}]: {e}"))
    }

    pub fn remove_dir(&mut self, rel: &str) -> Res<()> {
        let path = self.remote_path(rel);
        match self.stream.rmdir(&path) {
            Ok(_) => {
                self.known_dirs.remove(rel.trim_matches('/'));
                Ok(())
            }
            Err(e) => Err(format!("删除远程目录失败 [{path}]: {e}")),
        }
    }

    /// 列出某个远程目录（相对路径）下的条目。
    fn list_dir(&mut self, rel: &str) -> Res<Vec<ListFile>> {
        let path = self.remote_path(rel);
        self.stream
            .cwd(&path)
            .map_err(|e| format!("进入远程目录失败 [{path}]: {e}"))?;
        let lines = self
            .stream
            .list(None)
            .map_err(|e| format!("列出远程目录失败 [{path}]: {e}"))?;

        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            // 优先 POSIX 格式，失败则尝试 DOS（Windows FTP 服务）格式
            let parsed =
                ListParser::parse_posix(&line).or_else(|_| ListParser::parse_dos(&line));
            if let Ok(entry) = parsed {
                let name = entry.name();
                if name == "." || name == ".." {
                    continue;
                }
                out.push(entry);
            }
        }
        Ok(out)
    }

    /// 递归遍历远程目录，收集所有普通文件。
    pub fn walk(&mut self, rel: &str, out: &mut Vec<RemoteEntry>) -> Res<()> {
        let entries = self.list_dir(rel)?;
        for entry in entries {
            let name = entry.name().to_string();
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if entry.is_directory() {
                self.walk(&child, out)?;
            } else if entry.is_file() {
                out.push(RemoteEntry {
                    rel: child,
                    size: entry.size() as u64,
                });
            }
        }
        Ok(())
    }

    /// 优雅关闭连接。
    pub fn quit(&mut self) {
        let _ = self.stream.quit();
    }
}

/// 在给定会话上执行操作；失败时自动重连并重试一次。
///
/// 长连接 FTP 在空闲后容易被服务器断开，这个包装让上层逻辑无需关心重连。
pub fn with_client<T>(
    slot: &mut Option<FtpClient>,
    cfg: &FtpConfig,
    remote_base: &str,
    op: impl Fn(&mut FtpClient) -> Res<T>,
) -> Res<T> {
    if slot.is_none() {
        *slot = Some(FtpClient::connect(cfg, remote_base)?);
    }
    let client = slot.as_mut().expect("client just ensured");
    match op(client) {
        Ok(value) => Ok(value),
        Err(first) => {
            *slot = None;
            let mut fresh = FtpClient::connect(cfg, remote_base)
                .map_err(|e| format!("{first}（重连失败: {e}）"))?;
            let retried = op(&mut fresh);
            *slot = Some(fresh);
            retried.map_err(|e| format!("{first}（重连重试仍失败: {e}）"))
        }
    }
}
