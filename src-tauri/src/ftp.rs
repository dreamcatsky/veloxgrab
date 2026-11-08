//! 最小 FTP 客户端（被动模式）：LIST/SIZE/RETR，够下载器用。
//!
//! ## 范围与不做什么
//!
//! - **只做被动模式（PASV/EPSV）**：现代 FTP 服务器默认被动；主动模式
//!   （PORT）在客户端侧 NAT 下几乎不可用，做了也是废代码。
//! - **只做明文 FTP**（21 端口）：FTPS（显式/隐式 TLS）需要 TLS 会话在
//!   控制与数据两条通道上的握手与证书校验策略，rustls 的阻塞 IO 适配是
//!   另一个量级。需要 FTPS 的用户本来就该用专门工具。
//! - **控制通道用阻塞 std::net**：FTP 控制协议是「发一行、收一至多行、
//!   等状态码」的严格应答式，async 化收益为零；数据通道把阻塞
//!   `std::TcpStream` 包进 `spawn_blocking` 的同步读循环，与 direct 的
//!   写盘逻辑汇合。aria2 的 ftp 同样是独立通道实现。
//! - 匿名登录兜底：URL 无 user/pass 时 `anonymous` / 空密码。
//!
//! ## 集成边界
//!
//! `direct.rs` 的 URL 闸门放行 `ftp://` 后，`probe_ftp` / `download_ftp`
//! 由 `probe_auto` / `run_direct_download` 分流。断点续传走 direct 既有的
//! `.veloxgrab-part` 机制：REST 指令对已下偏移续传，服务器不支持 REST
//! （响应非 350）时从头下。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

/// 控制通道：一行命令 → 完整应答（多行应答读到终止行）。
struct Control {
    reader: BufReader<TcpStream>,
    stream: TcpStream,
}

#[derive(Debug)]
struct Reply {
    code: u16,
    lines: Vec<String>,
}

impl Control {
    fn connect(host: &str, port: u16, timeout: Duration) -> Result<Self, String> {
        let stream = TcpStream::connect((host, port))
            .map_err(|e| format!("FTP 连接失败: {e}"))?;
        stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut c = Self { reader, stream };
        let greet = c.read_reply()?;
        if greet.code / 100 != 2 {
            return Err(format!("FTP 服务器拒绝连接: {greet:?}"));
        }
        Ok(c)
    }

    /// 读一条完整应答：`123-...` 开头的多行应答读到 `123 ...` 终止行。
    fn read_reply(&mut self) -> Result<Reply, String> {
        let mut lines = Vec::new();
        let mut code = None;
        loop {
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .map_err(|e| format!("FTP 读应答失败: {e}"))?;
            if n == 0 {
                return Err("FTP 连接在应答中关闭".into());
            }
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            // "150 Opening..." — 前 3 字节是状态码，第 4 字节 '-' 表示续行
            if code.is_none() && line.len() >= 3 && line[..3].bytes().all(|b| b.is_ascii_digit()) {
                code = Some(line[..3].parse::<u16>().map_err(|e| e.to_string())?);
                let continued = line.as_bytes().get(3) == Some(&b'-');
                lines.push(line);
                if !continued {
                    break;
                }
            } else if code.is_some() {
                lines.push(line.clone());
                // 终止行：code + 空格开头
                if line.len() >= 4
                    && line[..3].bytes().all(|b| b.is_ascii_digit())
                    && line[..3].parse::<u16>().ok() == code
                    && line.as_bytes()[3] == b' '
                {
                    break;
                }
            } else {
                return Err(format!("FTP 应答格式异常: {line}"));
            }
        }
        Ok(Reply { code: code.unwrap_or(0), lines })
    }

    fn command(&mut self, cmd: &str) -> Result<Reply, String> {
        self.stream
            .write_all(format!("{cmd}\r\n").as_bytes())
            .map_err(|e| format!("FTP 发送命令失败: {e}"))?;
        self.read_reply()
    }

    /// PASSV 应答里的 `h1,h2,...,h6` → (ip, port)。
    fn parse_pasv(reply: &Reply) -> Result<(String, u16), String> {
        let text = reply.lines.last().unwrap_or(&String::new()).clone();
        let nums: Vec<u32> = text
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        // 至少要有 6 个数字且都是 0-255（避免从时间戳之类的杂讯里凑）
        if nums.len() < 6 || nums.iter().any(|&n| n > 255) {
            return Err(format!("PASV 应答无法解析: {text}"));
        }
        let o = nums.len() - 6;
        let ip = format!("{}.{}.{}.{}", nums[o], nums[o + 1], nums[o + 2], nums[o + 3]);
        let port = (nums[o + 4] * 256 + nums[o + 5]) as u16;
        Ok((ip, port))
    }

    /// 打开被动数据通道。EPSV 优先（防火墙友好），失败退 PASV。
    fn open_data(&mut self, host: &str) -> Result<TcpStream, String> {
        let epsv = self.command("EPSV")?;
        if epsv.code == 229 {
            // "229 Entering Extended Passive Mode (|||port|)"
            let text = epsv.lines.last().cloned().unwrap_or_default();
            let port: u16 = text
                .split('(')
                .next_back()
                .and_then(|s| s.split('|').find(|p| !p.is_empty()))
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| format!("EPSV 应答无法解析: {text}"))?;
            return TcpStream::connect((host, port)).map_err(|e| format!("FTP 数据连接失败: {e}"));
        }
        let pasv = self.command("PASV")?;
        if pasv.code != 227 {
            return Err(format!("服务器不支持被动模式: {pasv:?}"));
        }
        let (ip, port) = Self::parse_pasv(&pasv)?;
        // PASV 给的 IP 不可信（hijack 面小但存在）：连控制通道同 host 的端口。
        // 大多数服务器给的 IP 与控制通道一致；NAT 后会不同——那类环境本就
        // 需要网关级 FTP 支持，超出本实现的范围。
        let _ = ip;
        TcpStream::connect((host, port)).map_err(|e| format!("FTP 数据连接失败: {e}"))
    }
}

/// FTP URL 的解析结果（gate 已保证 scheme）。
pub struct FtpUrl {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    /// 服务器上的绝对或相对路径（保留原样，FTP 路径区分大小写）
    pub path: String,
}

pub fn parse_ftp_url(raw: &str) -> Result<FtpUrl, String> {
    let rest = raw
        .strip_prefix("ftp://")
        .ok_or_else(|| "不是 ftp:// 链接".to_string())?;
    // userinfo
    let (user, pass, rest): (String, String, &str) = match rest.split_once('@') {
        Some((userinfo, r)) => match userinfo.split_once(':') {
            Some((u, p)) => (u.to_string(), p.to_string(), r),
            None => (userinfo.to_string(), String::new(), r),
        },
        None => ("anonymous".to_string(), String::new(), rest),
    };
    // path（含可选 query 丢弃）
    let (hostport, path) = match rest.split_once('/') {
        Some((h, p)) => (h.to_string(), format!("/{p}")),
        None => (rest.to_string(), "/".to_string()),
    };
    let path = path.split(['?', '#']).next().unwrap_or(&path).to_string();
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| "FTP 端口非法".to_string())?,
        ),
        None => (hostport, 21),
    };
    if host.is_empty() {
        return Err("FTP 链接缺少主机名".into());
    }
    Ok(FtpUrl { host, port, user, pass, path })
}

/// 探测：SIZE + 认证。返回 (总字节, 服务器文件名)。
pub fn probe_ftp(raw: &str, timeout: Duration) -> Result<(Option<u64>, String), String> {
    let u = parse_ftp_url(raw)?;
    let mut c = Control::connect(&u.host, u.port, timeout)?;
    login(&mut c, &u.user, &u.pass)?;
    // 二进制模式：SIZE 的语义依赖 TYPE
    let type_ok = c.command("TYPE I")?.code == 200;
    let filename = u.path.rsplit('/').next().unwrap_or("file").to_string();
    let size = c.command(&format!("SIZE {}", u.path))?;
    let total = if size.code == 213 {
        size.lines
            .last()
            .and_then(|l| l.split_whitespace().next_back())
            .and_then(|s| s.parse::<u64>().ok())
    } else if type_ok {
        // 服务器不给 SIZE：尝试 LIST 取目录项里的文件长度
        list_size(&mut c, &u.host, &u.path)?
    } else {
        None
    };
    let _ = c.command("QUIT");
    Ok((total, crate::util::sanitize_filename(&filename)))
}

/// LIST 输出里找目标文件的长度（UNIX 风格第 5 列 / DOS 风格日期后的裸数字）。
fn list_size(c: &mut Control, host: &str, path: &str) -> Result<Option<u64>, String> {
    let dir = match path.rsplit_once('/') {
        Some((d, _)) if !d.is_empty() => d.to_string(),
        _ => "/".into(),
    };
    let name = path.rsplit('/').next().unwrap_or("").to_string();
    if name.is_empty() {
        return Ok(None);
    }
    let mut data = c.open_data(host)?;
    let list = c.command(&format!("LIST {dir}"))?;
    if list.code != 150 && list.code != 125 {
        return Ok(None);
    }
    let mut buf = String::new();
    let _ = data.read_to_string(&mut buf);
    let _ = c.read_reply(); // 226
    for line in buf.lines() {
        // UNIX: -rw-r--r-- 1 owner group 12345678 ... name
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 9 && cols.last() == Some(&name.as_str()) {
            if let Ok(sz) = cols[4].parse::<u64>() {
                return Ok(Some(sz));
            }
        }
    }
    Ok(None)
}

fn login(c: &mut Control, user: &str, pass: &str) -> Result<(), String> {
    let r = c.command(&format!("USER {user}"))?;
    if r.code == 331 {
        let r = c.command(&format!("PASS {pass}"))?;
        if r.code / 100 != 2 {
            return Err("FTP 登录被拒绝（用户名或密码错误）".into());
        }
    } else if r.code / 100 != 2 {
        return Err(format!("FTP 登录被拒绝: {r:?}"));
    }
    let _ = c.command("TYPE I");
    Ok(())
}

/// 下载到 `target`（调用方已处理的临时文件路径）。
///
/// `resume_from` = 已落盘字节数（>0 时先 REST）。返回本次写入的最终字节数
/// 与服务器声明的总长。服务器拒绝 REST（响应非 350）时**删除已落盘部分**
/// 从头下——半截旧数据续在变化过的文件前面比重下更糟。
pub fn download_ftp(
    raw: &str,
    target: &Path,
    resume_from: u64,
    cancel: &std::sync::atomic::AtomicBool,
    rate_kb: u64,
    mut on_bytes: impl FnMut(u64) + Send,
) -> Result<(u64, Option<u64>), String> {
    use std::sync::atomic::Ordering;
    let u = parse_ftp_url(raw)?;
    let timeout = Duration::from_secs(30);
    let mut c = Control::connect(&u.host, u.port, timeout)?;
    login(&mut c, &u.user, &u.pass)?;

    let total = c
        .command(&format!("SIZE {}", u.path))?
        .lines
        .last()
        .and_then(|l| l.split_whitespace().next_back())
        .and_then(|s| s.parse::<u64>().ok());

    let mut start = resume_from;
    if start > 0 {
        let r = c.command(&format!("REST {start}"))?;
        if r.code != 350 {
            let _ = std::fs::remove_file(target);
            start = 0;
        }
    }

    let mut data = c.open_data(&u.host)?;
    data.set_read_timeout(Some(timeout)).ok();
    let retr = c.command(&format!("RETR {}", u.path))?;
    if retr.code != 150 && retr.code != 125 {
        return Err(format!("FTP 服务器拒绝下载: {retr:?}"));
    }

    use std::io::{Seek, Write};
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(target)
        .map_err(|e| format!("创建文件失败: {e}"))?;
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start))
            .map_err(|e| format!("定位续传偏移失败: {e}"))?;
    }

    let mut written = start;
    let mut buf = [0u8; 256 * 1024];
    // 限速：1 秒滑窗（粗粒度足够——FTP 场景是阻塞线程，不影响其它任务）。
    let mut window_used: u64 = 0;
    let mut window_start = std::time::Instant::now();
    loop {
        if cancel.load(Ordering::SeqCst) {
            // 取消：已写字节留在临时文件（断点逻辑与 direct 一致）
            let _ = file.flush();
            return Err("已取消".into());
        }
        let n = data
            .read(&mut buf)
            .map_err(|e| format!("FTP 数据读取失败: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("写入失败: {e}"))?;
        written += n as u64;
        // 超预算按剩余窗补睡（阻塞线程专用，一次最多睡 1 秒）
        if rate_kb > 0 {
            window_used += n as u64;
            let budget = rate_kb.saturating_mul(1024).max(1);
            let elapsed = window_start.elapsed().as_millis() as u64;
            if elapsed < 1000 && window_used > budget {
                std::thread::sleep(std::time::Duration::from_millis(1000 - elapsed));
                window_start = std::time::Instant::now();
                window_used = 0;
            } else if elapsed >= 1000 {
                window_start = std::time::Instant::now();
                window_used = 0;
            }
        }
        on_bytes(written);
    }
    file.flush().ok();
    let end = c.read_reply()?;
    if end.code == 426 || end.code == 451 {
        return Err("FTP 传输被服务器中断".into());
    }
    // 服务器给了 SIZE 且长度不符：同 direct 的超额闸语义，不静默当成功
    if let Some(t) = total {
        if written != t {
            return Err(format!("大小校验失败：期望 {t} 字节，实际 {written} 字节"));
        }
    }
    Ok((written, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ftp_urls_with_userinfo_and_port() {
        let u = parse_ftp_url("ftp://user:pw@host.example:2121/dir/file.zip").unwrap();
        assert_eq!(u.host, "host.example");
        assert_eq!(u.port, 2121);
        assert_eq!(u.user, "user");
        assert_eq!(u.pass, "pw");
        assert_eq!(u.path, "/dir/file.zip");

        let anon = parse_ftp_url("ftp://host.example/file.iso").unwrap();
        assert_eq!(anon.user, "anonymous");
        assert_eq!(anon.port, 21);
        assert_eq!(anon.path, "/file.iso");

        // query/fragment 不进路径
        let q = parse_ftp_url("ftp://h/f.bin?x=1").unwrap();
        assert_eq!(q.path, "/f.bin");

        assert!(parse_ftp_url("http://h/x").is_err());
        assert!(parse_ftp_url("ftp:///").is_err());
    }

    #[test]
    fn pasv_reply_parsing_rejects_noise() {
        let ok = Reply { code: 227, lines: vec!["227 Entering Passive Mode (192,168,1,10,200,5)".into()] };
        assert_eq!(Control::parse_pasv(&ok).unwrap(), ("192.168.1.10".into(), 51205));
        // 数字串被杂讯凑够 6 个但含 >255：拒
        let bad = Reply { code: 227, lines: vec!["227 x (1,2,3,4,999,5) y 7,8".into()] };
        assert!(Control::parse_pasv(&bad).is_err());
        // 少于 6 个数字：拒
        let few = Reply { code: 227, lines: vec!["227 (1,2,3)".into()] };
        assert!(Control::parse_pasv(&few).is_err());
    }
}
