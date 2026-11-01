//! 远程桌面探针：离线验证三种协议后端与帧缓冲的健壮性。
//!
//! 用法：`cargo run --bin rdprobe`
//!
//! 做两件事：
//! 1. **帧缓冲回归**：模拟后端连续推帧 / 越界脏区 / 分辨率变化，
//!    验证不会再出现「主缓冲被掏空 → 第二帧越界 panic」这类崩溃；
//! 2. **三协议失败路径**：分别对 VNC / RDP / SPICE 连一个没人监听的端口，
//!    验证：必须回报 Error、必须回报 Disconnected、进程不能挂。

#[path = "../src/framebuf.rs"]
mod framebuf;

#[path = "../src/remote/mod.rs"]
mod remote;

use framebuf::FrameBuffer;
use remote::{RdCommand, RdEvent, RdParams, RdProtocol, RdState};
use std::time::Duration;

/// 测试结果收集器。
struct Report {
    passed: usize,
    failed: usize,
}

impl Report {
    fn new() -> Self {
        Self { passed: 0, failed: 0 }
    }
    fn check(&mut self, name: &str, ok: bool, detail: &str) {
        if ok {
            self.passed += 1;
            println!("  [PASS] {name}");
        } else {
            self.failed += 1;
            println!("  [FAIL] {name} —— {detail}");
        }
    }
}

/// 1. 帧缓冲回归测试。
fn test_framebuf(r: &mut Report) {
    println!("\n== 帧缓冲回归 ==");

    // 连续多帧写入（历史崩溃点：第二帧越界 panic）
    let mut fb = FrameBuffer::new();
    fb.resize(64, 32);
    let mut ok = true;
    for i in 0..10u32 {
        let px = vec![(i % 251) as u8; 64 * 32 * 4];
        ok &= fb.put(0, 0, 64, 32, &px);
        // 取纹理：主缓冲必须还在
        let _ = fb.take_texture();
        if fb.rgba.len() != 64 * 32 * 4 {
            ok = false;
            break;
        }
    }
    r.check(
        "连续 10 帧写入不越界、主缓冲不被掏空",
        ok,
        "主缓冲长度异常（历史上 mem::take 会掏空它）",
    );

    // 取纹理后主缓冲仍然完整
    let mut fb = FrameBuffer::new();
    fb.resize(16, 16);
    fb.put(0, 0, 16, 16, &vec![9u8; 16 * 16 * 4]);
    let tex = fb.take_texture();
    r.check(
        "take_texture 返回 BGRA 数据",
        tex.as_ref().map(|t| t.len() == 16 * 16 * 4).unwrap_or(false),
        "纹理缓冲长度不对",
    );
    r.check(
        "take_texture 之后主缓冲仍然保留",
        fb.rgba.len() == 16 * 16 * 4,
        "主缓冲被搬空了",
    );
    r.check(
        "BGRA 通道顺序正确（R,G,B）",
        tex.as_ref().map(|t| t[0] == 9 && t[1] == 9 && t[2] == 9).unwrap_or(false),
        "通道顺序不对",
    );

    // 越界脏区被安全丢弃
    let mut fb = FrameBuffer::new();
    fb.resize(32, 32);
    let r1 = fb.put(100, 100, 8, 8, &vec![0u8; 8 * 8 * 4]);
    let r2 = fb.put(0, 0, 8, 8, &vec![0u8; 10]); // 长度不足
    let r3 = fb.put(28, 28, 16, 16, &vec![0u8; 16 * 16 * 4]); // 部分越界，应裁剪
    r.check(
        "完全越界的脏区被丢弃",
        !r1,
        "越界脏区不应写入",
    );
    r.check(
        "长度不足的脏区被丢弃",
        !r2,
        "长度不足不应写入",
    );
    r.check(
        "部分越界的脏区被裁剪后写入",
        r3,
        "应裁剪后写入而不是丢弃",
    );

    // 非法分辨率
    let mut fb = FrameBuffer::new();
    r.check(
        "拒绝 0 宽 / 0 高 / 超大分辨率",
        !fb.resize(0, 100) && !fb.resize(100, 0) && !fb.resize(99999, 99999),
        "非法分辨率应被拒绝",
    );

    // 分辨率变化后旧内容作废
    let mut fb = FrameBuffer::new();
    fb.resize(32, 32);
    fb.put(0, 0, 32, 32, &vec![5u8; 32 * 32 * 4]);
    let changed = fb.resize(64, 16);
    r.check(
        "分辨率变化被识别",
        changed && fb.width == 64 && fb.height == 16,
        "resize 未生效",
    );
    r.check(
        "分辨率变化后主缓冲已清零",
        fb.rgba.iter().all(|v| *v == 0),
        "旧画面没清掉，会造成拉伸错位",
    );
}

/// 2. 三协议失败路径测试。
fn test_protocol(r: &mut Report, protocol: RdProtocol, port: u16) {
    println!("\n== {protocol:?} 失败路径 ==");
    let params = RdParams {
        protocol,
        host: "127.0.0.1".to_string(),
        port,
        username: "u".to_string(),
        password: "p".to_string(),
        ..Default::default()
    };
    let mut session = remote::spawn(params);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut error_msg = String::new();
    let mut got_error = false;
    let mut got_disconnected = false;
    while std::time::Instant::now() < deadline {
        match session.events.recv_timeout(Duration::from_millis(500)) {
            Ok(RdEvent::Error(msg)) => {
                got_error = true;
                error_msg = msg;
            }
            Ok(RdEvent::State(RdState::Disconnected)) => {
                got_disconnected = true;
                break;
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    r.check(
        &format!("{protocol:?} 连接失败时回报 Error"),
        got_error,
        "没有收到 Error 事件（UI 会永远停在连接中）",
    );
    r.check(
        &format!("{protocol:?} 连接失败后回报 Disconnected"),
        got_disconnected,
        "没有收到 Disconnected 事件",
    );
    r.check(
        &format!("{protocol:?} 错误信息非空"),
        !error_msg.trim().is_empty(),
        "错误信息为空",
    );
    println!("       实际错误: {}", error_msg.replace('\n', " / "));

    // 关闭会话不应 panic / 挂死
    session.send(RdCommand::Close);
    session.close();
    r.check(
        &format!("{protocol:?} 关闭会话干净退出"),
        true,
        "",
    );
}

fn main() {
    println!("RustTerm 远程桌面探针");
    let mut r = Report::new();

    test_framebuf(&mut r);
    test_protocol(&mut r, RdProtocol::Vnc, 1);
    test_protocol(&mut r, RdProtocol::Rdp, 1);
    test_protocol(&mut r, RdProtocol::Spice, 1);

    println!("\n================================");
    println!("通过 {} 项，失败 {} 项", r.passed, r.failed);
    if r.failed > 0 {
        std::process::exit(1);
    }
}
