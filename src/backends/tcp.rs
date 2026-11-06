//! 原始 TCP 后端（Telnet 客户端）。
//!
//! 实现最小 Telnet 协商：
//! - 连接后主动 `IAC WILL NAWS`：声明本端支持窗口尺寸协商
//! - 收到 `IAC DO NAWS` → 回 `IAC WILL NAWS` 并开启尺寸上报
//! - 收到其他 `IAC DO x` → 回 `IAC WONT x`
//! - 收到 `IAC WILL x` → 回 `IAC DONT x`
//! - `IAC SB … IAC SE` 子协商整段丢弃
//!
//! NAWS（RFC 1073）是 Telnet 里唯一能让远端知道终端行列数的办法：
//! 不协商的话 `vi` / `top` / `htop` 这类全屏程序会按 80×24 绘制，
//! 在这个窗口里显示成一团。只有对端先给了 `DO NAWS`，客户端才可以
//! 发 `IAC SB NAWS <宽 16bit> <高 16bit> IAC SE` —— 抢发会让严格
//! 的实现直接断开连接。
//!
//! 这样对大多数网络设备的 telnet 服务（字符模式）即可交互；
//! 不支持协商的裸 TCP 服务（如 SMTP/HTTP）字节原样透传。

use anyhow::{anyhow, Result};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

use super::TermIo;

const IAC: u8 = 0xFF; // Interpret As Command
const DONT: u8 = 0xFE;
const DO: u8 = 0xFD;
const WONT: u8 = 0xFC;
const WILL: u8 = 0xFB;
const SB: u8 = 0xFA; // 子协商开始
const SE: u8 = 0xF0; // 子协商结束
const NAWS: u8 = 31; // Negotiate About Window Size

/// Telnet 协商状态（跨多次 read 保持）。
#[derive(Default)]
struct TelnetState {
    /// 对端已回 `DO NAWS`：之后才能发窗口尺寸子协商
    naws: bool,
}

pub fn spawn(host: &str, port: u16) -> Result<TermIo> {
    let addr = format!("{host}:{port}");
    let stream =
        TcpStream::connect(&addr).map_err(|e| anyhow!("连接 {addr} 失败: {e}"))?;
    stream.set_nodelay(true).ok();
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .ok();

    // 协商应答、键盘输入、NAWS 子协商都会往同一个 socket 写。
    // 三者各持一个 try_clone 的话，多个线程的 write_all 可能交错，
    // 对端看到的就是半条命令 + 半个按键 —— 用一把锁串起来。
    let write = Arc::new(Mutex::new(stream));
    let read_stream = write
        .lock()
        .map_err(|_| anyhow!("TCP 写锁已中毒"))?
        .try_clone()?;

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (output_tx, output_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    let naws = Arc::new(AtomicBool::new(false));
    // 最后一次窗口尺寸。首帧尺寸往往在 NAWS 协商完成之前就到了，
    // 读线程在协商成功的那一刻靠它把尺寸补发出去。
    let last_size: Arc<Mutex<Option<(u16, u16)>>> = Arc::new(Mutex::new(None));

    // 主动声明支持 NAWS。服务器若也支持，会回 IAC DO NAWS。
    if let Ok(mut w) = write.lock() {
        let _ = w.write_all(&[IAC, WILL, NAWS]);
        let _ = w.flush();
    }

    // 读线程：Telnet 协商过滤后送 UI
    let write_read = write.clone();
    let naws_read = naws.clone();
    let size_read = last_size.clone();
    std::thread::spawn(move || {
        let mut reader = read_stream;
        let mut buf = [0u8; 8192];
        let mut pending: Vec<u8> = Vec::with_capacity(8192);
        let mut st = TelnetState::default();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    let (clean, reply, consumed) = filter_telnet(&pending, &mut st);
                    if consumed > 0 {
                        pending.drain(..consumed);
                    }
                    // 这一次读到 DO NAWS 才置位：swap 返回旧值，
                    // 旧值为 false 说明「刚刚」协商成功，需要补发首帧尺寸
                    let just_enabled = st.naws && !naws_read.swap(true, Ordering::Relaxed);
                    if !reply.is_empty() || just_enabled {
                        if let Ok(mut w) = write_read.lock() {
                            if !reply.is_empty() {
                                let _ = w.write_all(&reply);
                            }
                            if just_enabled {
                                if let Ok(slot) = size_read.lock() {
                                    if let Some((cols, rows)) = *slot {
                                        let _ = w.write_all(&naws_message(cols, rows));
                                    }
                                }
                            }
                            let _ = w.flush();
                        }
                    }
                    if !clean.is_empty() && output_tx.send(clean).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = output_tx
            .send("\r\n\x1b[90m[TCP 连接已关闭]\x1b[0m".as_bytes().to_vec());
    });

    // 写线程
    let write_input = write.clone();
    std::thread::spawn(move || {
        while let Some(data) = input_rx.blocking_recv() {
            let Ok(mut w) = write_input.lock() else { break };
            if w.write_all(&data).is_err() || w.flush().is_err() {
                break;
            }
        }
    });

    // 尺寸线程：协商成功后把 cols/rows 以 NAWS 子协商发出去。
    //
    // 协商是异步的（要等对端回 DO NAWS），而尺寸随时会变：
    // 所以这里既缓存最新值给读线程补发，也在协商已就绪时立即发。
    let write_resize = write.clone();
    let size_slot = last_size.clone();
    std::thread::spawn(move || {
        while let Some((cols, rows)) = resize_rx.blocking_recv() {
            if let Ok(mut slot) = size_slot.lock() {
                *slot = Some((cols, rows));
            }
            if !naws.load(Ordering::Relaxed) {
                continue;
            }
            let Ok(mut w) = write_resize.lock() else { break };
            if w.write_all(&naws_message(cols, rows)).is_err() || w.flush().is_err() {
                break;
            }
        }
    });

    Ok(TermIo {
        input_tx,
        resize_tx,
        output_rx,
        ssh: None,
    })
}

/// 构造 NAWS 窗口尺寸子协商（RFC 1073）。
///
/// `IAC SB NAWS <宽 16bit 大端> <高 16bit 大端> IAC SE`；按 RFC，尺寸 0 视为 1。
fn naws_message(cols: u16, rows: u16) -> [u8; 9] {
    let cols = cols.max(1);
    let rows = rows.max(1);
    [
        IAC,
        SB,
        NAWS,
        (cols >> 8) as u8,
        (cols & 0xff) as u8,
        (rows >> 8) as u8,
        (rows & 0xff) as u8,
        IAC,
        SE,
    ]
}

/// 从 `buf` 中剥离 Telnet 命令。
///
/// 返回 `(可见数据, 需要回给对端的应答, 已消费字节数)`。
/// 末尾不完整的命令不消费，等待下一次读取补齐。
fn filter_telnet(buf: &[u8], st: &mut TelnetState) -> (Vec<u8>, Vec<u8>, usize) {
    let mut out = Vec::with_capacity(buf.len());
    let mut reply = Vec::new();
    let mut i = 0usize;
    while i < buf.len() {
        let b = buf[i];
        if b != IAC {
            out.push(b);
            i += 1;
            continue;
        }
        if i + 1 >= buf.len() {
            break; // 只剩 IAC，等更多数据
        }
        let cmd = buf[i + 1];
        match cmd {
            IAC => {
                out.push(IAC); // 转义的字面 0xFF
                i += 2;
            }
            DO | DONT | WILL | WONT => {
                if i + 2 >= buf.len() {
                    break;
                }
                let opt = buf[i + 2];
                match cmd {
                    // NAWS：这是唯一我们愿意接受的选项，其余一律拒绝
                    DO if opt == NAWS => {
                        st.naws = true;
                        reply.extend_from_slice(&[IAC, WILL, NAWS]);
                    }
                    DO => reply.extend_from_slice(&[IAC, WONT, opt]),
                    WILL => reply.extend_from_slice(&[IAC, DONT, opt]),
                    _ => {}
                }
                i += 3;
            }
            SB => {
                // 跳过子协商直到 IAC SE
                let mut j = i + 2;
                let mut closed = false;
                while j + 1 < buf.len() {
                    if buf[j] == IAC && buf[j + 1] == SE {
                        j += 2;
                        closed = true;
                        break;
                    }
                    j += 1;
                }
                if !closed {
                    break; // 子协商未结束，等更多数据
                }
                i = j;
            }
            _ => i += 2, // 其他两字节命令
        }
    }
    (out, reply, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_plain_text() {
        let mut st = TelnetState::default();
        let (out, reply, n) = filter_telnet(b"hello", &mut st);
        assert_eq!(out, b"hello");
        assert!(reply.is_empty());
        assert_eq!(n, 5);
    }

    #[test]
    fn answers_negotiation() {
        // IAC DO ECHO(1) 后跟 "abc"
        let mut st = TelnetState::default();
        let data = [IAC, DO, 1, b'a', b'b', b'c'];
        let (out, reply, n) = filter_telnet(&data, &mut st);
        assert_eq!(out, b"abc");
        assert_eq!(reply, vec![IAC, WONT, 1]);
        assert_eq!(n, 6);
    }

    #[test]
    fn accepts_naws() {
        // 对端回 DO NAWS：必须回 WILL NAWS，并置位状态
        let mut st = TelnetState::default();
        let data = [IAC, DO, NAWS];
        let (out, reply, n) = filter_telnet(&data, &mut st);
        assert!(out.is_empty());
        assert_eq!(reply, vec![IAC, WILL, NAWS]);
        assert!(st.naws);
        assert_eq!(n, 3);
    }

    #[test]
    fn keeps_incomplete_command() {
        let mut st = TelnetState::default();
        let (_, _, n) = filter_telnet(&[b'x', IAC], &mut st);
        assert_eq!(n, 1, "末尾不完整的 IAC 不应被消费");
    }
}
