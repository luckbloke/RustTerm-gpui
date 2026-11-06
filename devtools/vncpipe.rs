//! VNC 后端管线探针：本地起一个「脚本化」VNC 服务器，
//! 走 src/remote/vnc.rs 的完整 run() 管线，验证帧到底能不能解出来。
//!
//! 服务器脚本：
//! 1. RFB 3.8 握手（None 认证）
//! 2. ServerInit：分辨率 WxH，BGRA 像素格式
//! 3. 收到非增量 FramebufferUpdateRequest 后，回一帧 Raw 全屏（渐变色块）
//! 4. 之后再收到增量请求就回一小块脏矩形
//!
//! 用法：cargo run --features devtools --bin vncpipe -- out.png

#[path = "../src/framebuf.rs"]
mod framebuf;

#[path = "../src/remote/mod.rs"]
mod remote;

use anyhow::Result;
use std::io::{Read, Write};
use std::net::TcpListener;

const W: u16 = 160;
const H: u16 = 120;

fn pixel(x: u16, y: u16) -> [u8; 4] {
    // BGRA（我们请求的 bgra() 格式内存序）
    [b(4 * x % 256), b(y * 2), b(255 - x), 255]
}
fn b(v: u16) -> u8 {
    (v & 0xff) as u8
}

fn main() -> Result<()> {
    // vnc-rs 内部 tracing → log 输出，定位握手卡点
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    let out_path = std::env::args().nth(1).unwrap_or_else(|| "vncpipe.png".into());
    let listener = TcpListener::bind("127.0.0.1:15900")?;
    println!("[srv] listening 15900");

    let server = std::thread::spawn(move || -> Result<()> {
        // 后端带凭据的第一轮会被服务器拒绝（Auto 策略 + None 安全类型），
        // 第二轮无凭据才会真正握手成功 —— 循环接受连接即可。
        for _ in 0..3 {
            match serve_one(&listener) {
                Ok(true) => return Ok(()),
                Ok(false) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    });

    fn serve_one(listener: &TcpListener) -> Result<bool> {
        let (mut s, _) = listener.accept()?;
        // --- version ---
        s.write_all(b"RFB 003.008\n")?;
        let mut buf = [0u8; 12];
        s.read_exact(&mut buf)?;
        println!("[srv] version ok: {}", String::from_utf8_lossy(&buf).trim());
        // --- security types: only None ---
        s.write_all(&[1u8, 1])?;
        // 带凭据的第一轮：客户端看到只有 None 会直接断开（不发选择）
        if s.read_exact(&mut std::array::from_fn::<u8, 1, _>(|_| 0u8)).is_err() {
            println!("[srv] 客户端未选安全类型（预期内的第一轮拒绝）");
            return Ok(false);
        }
        // security result
        s.write_all(&0u32.to_be_bytes())?;
        // --- client init ---
        let mut shared = [0u8; 1];
        s.read_exact(&mut shared)?;
        println!("[srv] client init ok");
        // --- server init ---
        s.write_all(&W.to_be_bytes())?;
        s.write_all(&H.to_be_bytes())?;
        // pixel format（16 字节）：32bpp、true color；bgra 内存序 → R/G/B max 各 u16
        s.write_all(&[
            32u8, 24, 0, 1, // bpp, depth, big-endian, true-color
            0, 255, 0, 255, 0, 255, // max r/g/b（u16 BE）
            16, 8, 0, // shifts r g b
            0, 0, 0, // padding
        ])?;
        let name = b"vncpipe";
        s.write_all(&(name.len() as u32).to_be_bytes())?;
        s.write_all(name)?;
        println!("[srv] handshake done");

        let mut full_sent = false;
        loop {
            // 读客户端消息（只处理 FramebufferUpdateRequest）
            let mut msg = [0u8; 1];
            if s.read_exact(&mut msg).is_err() {
                break;
            }
            match msg[0] {
                0 => {
                    // SetPixelFormat：1 + 3 pad + 16 = 20 字节
                    s.read_exact(&mut std::array::from_fn::<u8, 19, _>(|_| 0u8))?;
                    println!("[srv] SetPixelFormat");
                }
                // FramebufferUpdateRequest：1 type + 1 incremental + 2x + 2y + 2w + 2h
                //
                // 注意消息类型是 **3**（不是 0）。把 0 当成更新请求会让
                // 服务器一直不帧，探针自己造出「黑屏」的假象。
                3 => {
                    let mut rest = [0u8; 9];
                    s.read_exact(&mut rest)?;
                    let incremental = rest[0];
                    println!("[srv] update request inc={incremental}");
                    if incremental == 0 || !full_sent {
                        full_sent = true;
                        // 全屏 Raw 更新
                        //
                        // 帧头已经写了「矩形数」，rect 体里**不能再写一次**，
                        // 否则客户端把矩形数当成 x 坐标、把坐标当成编码，
                        // 报 "Unknown VNC encoding"，一帧都解不出来。
                        let mut body = Vec::new();
                        body.extend_from_slice(&0u16.to_be_bytes());
                        body.extend_from_slice(&0u16.to_be_bytes());
                        body.extend_from_slice(&W.to_be_bytes());
                        body.extend_from_slice(&H.to_be_bytes());
                        body.extend_from_slice(&0i32.to_be_bytes()); // Raw
                        for y in 0..H {
                            for x in 0..W {
                                body.extend_from_slice(&pixel(x, y));
                            }
                        }
                        s.write_all(&0u8.to_be_bytes())?; // message type
                        s.write_all(&[0u8; 1])?; // padding
                        s.write_all(&(1u16).to_be_bytes())?; // n rects
                        s.write_all(&body)?;
                        println!("[srv] sent full raw frame");
                    } else {
                        // 增量：一块 8x8 红色矩形
                        let mut body = Vec::new();
                        body.extend_from_slice(&10u16.to_be_bytes());
                        body.extend_from_slice(&10u16.to_be_bytes());
                        body.extend_from_slice(&8u16.to_be_bytes());
                        body.extend_from_slice(&8u16.to_be_bytes());
                        body.extend_from_slice(&0i32.to_be_bytes());
                        for _ in 0..64 {
                            body.extend_from_slice(&[255u8, 0, 0, 255]); // BGRA 红
                        }
                        s.write_all(&[0u8, 0])?;
                        s.write_all(&1u16.to_be_bytes())?;
                        s.write_all(&body)?;
                    }
                }
                2 => {
                    // SetEncodings：1 + 1 pad + 2 count + 4*count
                    let mut hdr = [0u8; 3];
                    s.read_exact(&mut hdr)?;
                    let n = u16::from_be_bytes([hdr[1], hdr[2]]) as usize;
                    let mut enc = vec![0u8; n * 4];
                    s.read_exact(&mut enc)?;
                    println!("[srv] SetEncodings x{n}");
                }
                4 => {
                    // KeyEvent
                    s.read_exact(&mut std::array::from_fn::<u8, 7, _>(|_| 0u8))?;
                }
                5 => {
                    // PointerEvent
                    s.read_exact(&mut std::array::from_fn::<u8, 5, _>(|_| 0u8))?;
                }
                6 => {
                    // ClientCutText
                    let mut rest = [0u8; 7];
                    s.read_exact(&mut rest)?;
                    let len = u32::from_be_bytes([rest[3], rest[4], rest[5], rest[6]]) as usize;
                    let mut pad = vec![0u8; len];
                    s.read_exact(&mut pad)?;
                }
                other => {
                    println!("[srv] unexpected client msg {other}");
                    break;
                }
            }
        }
        Ok(true)
    }

    // ---- 客户端：走 remote::vnc::run 完整管线 ----
    let params = remote::RdParams {
        protocol: remote::RdProtocol::Vnc,
        host: "127.0.0.1".into(),
        port: 15900,
        ..Default::default()
    };
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    let cancel_cli = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_cli_bk = cancel_cli.clone();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let backend = std::thread::spawn(move || {
        rt.block_on(async {
            remote::vnc::run(&params, &ev_tx, &cmd_rx, &cancel_cli_bk).await
        })
    });

    // 收 3 秒事件
    let mut fb = vec![0u8; W as usize * H as usize * 4];
    let mut frames = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match ev_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(ev) => match ev {
                remote::RdEvent::State(s) => println!("[cli] state {s:?}"),
                remote::RdEvent::Resized { width, height } => {
                    println!("[cli] resized {width}x{height}")
                }
                remote::RdEvent::Frame { x, y, w, h, rgba } => {
                    frames += 1;
                    println!("[cli] frame #{frames} ({x},{y} {w}x{h}) bytes={} rgba[0..8]={:?}", rgba.len(), &rgba[..rgba.len().min(8)]);
                    let stride = W as usize * 4;
                    for row in 0..h as usize {
                        let dy = y as usize + row;
                        if dy >= H as usize {
                            break;
                        }
                        let sw = (w as usize * 4).min(stride - x as usize * 4);
                        let src = &rgba[row * w as usize * 4..row * w as usize * 4 + sw];
                        let dst = &mut fb[dy * stride + x as usize * 4..dy * stride + x as usize * 4 + sw];
                        dst.copy_from_slice(src);
                    }
                    // 不立即 Close：留给运行时机会调度 spawn 的任务
                }
                remote::RdEvent::Error(e) => {
                    println!("[cli] error: {e}");
                    break;
                }
                other => println!("[cli] {other:?}"),
            },
            Err(_) => continue, // 短暂无事件：继续等截止时间
        }
    }
    cancel_cli.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = cmd_tx.send(remote::RdCommand::Close);
    match backend.join() {
        Ok(Ok(())) => println!("[cli] backend: ok"),
        Ok(Err(e)) => println!("[cli] backend err: {e:#}"),
        Err(_) => println!("[cli] backend panicked"),
    }
    let _ = server.join();

    // 存 PNG 检查内容
    println!("[cli] fb[0..8]={:?}", &fb[0..8]);
    if let Some(img) = image::RgbaImage::from_raw(W as u32, H as u32, fb.clone()) {
        img.save(&out_path)?;
        println!("[cli] saved {out_path}, frames={frames}");
        let p = image::RgbaImage::from_raw(W as u32, H as u32, fb).unwrap();
        let c = p.get_pixel(5, 5);
        println!("[cli] pixel(5,5) = {:?}", c.0);
    }
    Ok(())
}
