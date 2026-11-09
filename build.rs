//! 构建脚本：把应用图标嵌入 Windows PE 资源。
//!
//! `embed-resource` 与 `winres` 在跨工具链时问题较多，统一用 `winresource`
//! （cargo 自带的 `rust-lld`/rc 封装），仅在 windows 目标生效。

#[cfg(windows)]
fn main() {
    // 构建时间戳：嵌入二进制供运行期日志做构建指纹（排障时确认版本）。
    // 注意：不声明 rerun-if-changed（默认=包内任一文件变化都重跑本脚本），
    // 保证时间戳与二进制同步刷新。
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=NYATERM_BUILD_UNIX_TS={ts}");

    winresource::WindowsResource::new()
        .set_icon("icons/icon.ico")
        .compile()
        .expect("无法编译图标资源：icons/icon.ico");
}

#[cfg(not(windows))]
fn main() {}
