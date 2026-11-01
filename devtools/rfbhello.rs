//! RFB 原始握手探针：打印服务器提供的安全类型列表与协议版本。
//!
//! 用法：`cargo run --features devtools --bin rfbhello -- <host> <port>`

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let host = args.get(1).cloned().unwrap_or_else(|| "192.168.1.16".into());
    let port: u16 = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(5900);

    let mut s = TcpStream::connect((host.as_str(), port)).await?;
    let mut ver = [0u8; 12];
    s.read_exact(&mut ver).await?;
    println!("服务器版本: {:?}", String::from_utf8_lossy(&ver));

    s.write_all(b"RFB 003.008\n").await?;

    let mut count = [0u8; 1];
    s.read_exact(&mut count).await?;
    let n = count[0];
    if n == 0 {
        // 失败：后面是长度 + 原因
        let mut len = [0u8; 4];
        s.read_exact(&mut len).await?;
        let len = u32::from_be_bytes(len) as usize;
        let mut reason = vec![0u8; len];
        s.read_exact(&mut reason).await?;
        println!("认证失败: {}", String::from_utf8_lossy(&reason));
        return Ok(());
    }
    let mut types = vec![0u8; n as usize];
    s.read_exact(&mut types).await?;
    println!("安全类型数量: {n}");
    for t in &types {
        let desc = match *t {
            0 => "None(无密码)".to_string(),
            1 => "VncAuth(传统DES)".to_string(),
            2 => "RFB 3.3 明文".to_string(),
            16 => "Tight".to_string(),
            19 => "VeNCrypt".to_string(),
            129 => "RA2_256".to_string(),
            other => format!("未知({other})"),
        };
        println!("  - {t}: {desc}");
    }

    // 选一个类型试一下，看后续握手能走多远
    if types.contains(&1) {
        println!("\n尝试 VncAuth…");
        s.write_all(&[1]).await?;
        let mut challenge = [0u8; 16];
        s.read_exact(&mut challenge).await?;
        println!("  收到挑战: {challenge:02x?}");
        // 用空口令回应
        s.write_all(&[0u8; 16]).await?;
        let mut result = [0u8; 4];
        s.read_exact(&mut result).await?;
        let status = u32::from_be_bytes(result);
        println!("  认证结果: {status} ({})", if status == 0 { "成功" } else { "失败" });
        if status != 0 {
            let mut len = [0u8; 4];
            s.read_exact(&mut len).await?;
            let len = u32::from_be_bytes(len) as usize;
            let mut reason = vec![0u8; len];
            s.read_exact(&mut reason).await?;
            println!("  失败原因: {}", String::from_utf8_lossy(&reason));
        } else {
            // ServerInit
            let mut hdr = [0u8; 4];
            s.read_exact(&mut hdr).await?;
            let w = u16::from_be_bytes([hdr[0], hdr[1]]);
            let h = u16::from_be_bytes([hdr[2], hdr[3]]);
            println!("  服务器分辨率: {w}x{h}");
            let mut pf = [0u8; 16];
            s.read_exact(&mut pf).await?;
            println!("  服务器像素格式 bpp={} depth={} big_endian={} true_color={}", pf[0], pf[1], pf[2], pf[3]);
            let mut name_len = [0u8; 4];
            s.read_exact(&mut name_len).await?;
            let name_len = u32::from_be_bytes(name_len) as usize;
            let mut name = vec![0u8; name_len];
            s.read_exact(&mut name).await?;
            println!("  桌面名: {}", String::from_utf8_lossy(&name));
        }
    }

    Ok(())
}
