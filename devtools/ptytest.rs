//! 诊断：在 portable-pty（ConPTY）里运行任意命令，原样打印输出。
//! 用途：复现「rustterm 里 ping 报 传输失败。常见故障。」问题。
//!
//! 用法:
//!   cargo run --features devtools --bin ptytest -- ping 192.168.1.19 -n 2
//!   cargo run --features devtools --bin ptytest -- --env ping 192.168.1.19 -n 2   (打印继承的环境变量)

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Write;

fn main() {
    let mut log = std::fs::File::create("ptytest.log").expect("create log");
    macro_rules! say {
        ($($t:tt)*) => {{
            let _ = writeln!(&mut log, $($t)*);
            let _ = log.flush();
        }};
    }
    say!("ptytest 启动, pid={}", std::process::id());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let print_env = args.first().map(|s| s == "--env").unwrap_or(false);
    let args: Vec<String> = if print_env {
        args.into_iter().skip(1).collect()
    } else {
        args
    };
    if args.is_empty() {
        say!("错误: 缺少参数");
        std::process::exit(1);
    }

    if print_env {
        for (k, v) in std::env::vars() {
            say!("ENV {k}={v}");
        }
    }

    let mut cmd = CommandBuilder::new(&args[0]);
    for a in &args[1..] {
        cmd.arg(a);
    }

    say!("打开 PTY...");
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize { rows: 24, cols: 100, pixel_width: 0, pixel_height: 0 }) {
        Ok(p) => p,
        Err(e) => { say!("openpty 失败: {e:#?}"); std::process::exit(1); }
    };
    say!("PTY 已打开, spawn 子进程: {:?}", args);
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => { say!("spawn 失败: {e:#?}"); std::process::exit(1); }
    };
    say!("spawn 成功");
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => { say!("clone reader 失败: {e:#?}"); std::process::exit(1); }
    };
    drop(pair.slave); // 释放 slave 写端，读到 EOF

    // 看门狗：8 秒后强制收尾（ConPTY master 不会 EOF，靠它兜底）
    let mut watchdog_child = child.clone_killer();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(8));
        let _ = watchdog_child.kill();
        std::thread::sleep(std::time::Duration::from_secs(3));
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open("ptytest.log") {
            let _ = writeln!(f, "看门狗超时强制退出");
        }
        std::process::exit(2);
    });

    // 读线程：持续读，写入共享缓冲
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let buf = buf.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            use std::io::Read;
            let mut total = 0usize;
            let log = |msg: String| {
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open("ptytest.log") {
                    let _ = writeln!(f, "{msg}");
                }
            };
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => { log(format!("读到 EOF, 共 {total} 字节")); break; }
                    Ok(n) => {
                        total += n;
                        buf.lock().unwrap().extend_from_slice(&chunk[..n]);
                        log(format!("读到 {n} 字节 (累计 {total})"));
                    }
                    Err(e) => { log(format!("read 错误: {e}")); break; }
                }
            }
        });
    }

    // 主线程等子进程退出（ping -n 2 约 2 秒），然后打印
    let status = child.wait().expect("wait");
    say!("子进程退出: {status:?}");
    std::thread::sleep(std::time::Duration::from_millis(800));
    say!("=== PTY 输出 ===");
    say!("{}", String::from_utf8_lossy(&buf.lock().unwrap()));

    // 对照组 1：完整路径直接 spawn（不经 ConPTY），并剥离沙箱相关环境变量
    let out = std::process::Command::new("C:\\Windows\\System32\\ping.exe")
        .args(&args[1..])
        .env_remove("SANDBOX_CENTER_IPC_ADDRESS")
        .env_remove("SANDBOX_CENTER_UID")
        .env_remove("LSBOX_SHMEM_NAME")
        .env_remove("LSBOX_AUDIT_SHMEM")
        .env_remove("SANDBOX_IPC_TRACE_PARENT")
        .env_remove("SANDBOX_IPC_TRACE_SESSION")
        .output()
        .expect("direct spawn");
    say!("=== 完整路径直接 spawn (exit={:?}) ===", out.status.code());
    say!("{}", String::from_utf8_lossy(&out.stdout));

    // 对照组 2：进程环境诊断
    say!("=== 进程诊断 ===");
    let in_job = unsafe {
        let mut val: i32 = 0;
        let mut res: i32 = 0;
        // IsProcessInJob(GetCurrentProcess(), NULL, &res) —— 传入 NULL 检查是否在任意 Job 中
        #[link(name = "kernel32")]
        extern "system" {
            fn IsProcessInJob(h: *mut core::ffi::c_void, job: *mut core::ffi::c_void, res: *mut i32) -> i32;
        }
        IsProcessInJob(-4isize as *mut _, std::ptr::null_mut(), &mut val);
        res = val;
        res
    };
    say!("IsProcessInJob(NULL) = {in_job} (1=在某个 Job 对象中)");
    for (k, v) in std::env::vars_os() {
        say!("ENV {:?}={:?}", k, v);
    }
    say!("ptytest 正常结束");
}
