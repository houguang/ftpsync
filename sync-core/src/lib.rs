//! FTPSync 同步内核。
//!
//! 本 crate 不依赖任何界面框架，可独立编译、测试与嵌入其他宿主程序。
//! 界面层通过实现 [`engine::EventSink`] 接收日志与状态回调。

pub mod config;
pub mod engine;
pub mod ftp;
