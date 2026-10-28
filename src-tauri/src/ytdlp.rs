use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;

use crate::settings::Settings;

/// 单次探测（`yt-dlp -J`）的总时限。
///
/// 不给上限的话，半开连接或需要人机验证的页面能让它挂十几分钟；
/// 前端此时「解析」按钮是 disabled 的，用户既不能取消也不能重试。
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);

/// 由 lib.rs 在 setup 阶段注入的 Tauri 资源目录。
/// 安装版里 yt-dlp / ffmpeg 会随资源被放到 $INSTDIR 或其 bin/ 子目录。
static RESOURCE_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn set_resource_dir(dir: PathBuf) {
    let _ = RESOURCE_DIR.set(dir);
}

/// 资源目录（安装后即 $INSTDIR）。扩展目录、附带工具都从这里找。
pub fn resource_dir() -> Option<&'static PathBuf> {
    RESOURCE_DIR.get()
}

/// 在资源目录里按若干相对路径找一个可执行文件
fn probe_resource_dir(names: &[&str]) -> Option<PathBuf> {
    let dir = resource_dir()?;
    for name in names {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// 可执行文件路径缓存。
///
/// 这两个路径在进程生命周期内不会变，但 `push_common_args` 每次拼命令都会问一次
/// ffmpeg 在哪——找不到时那是一次完整的 PATH 遍历（Windows 上常见 30~60 个目录，
/// 每个一次 `is_file()`）。每次探测、每次下载都要付这个成本，缓存一次即可。
///
/// 代价：运行中把 ffmpeg 装进 PATH 不会被发现，需要重启（与 README 的说明一致）。
static YTDLP_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
static FFMPEG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// yt-dlp 可执行文件的定位策略：
/// 1. 环境变量 VELOXGRAB_YTDLP
/// 2. Tauri 资源目录（安装版）
/// 3. 程序目录旁的 bin/yt-dlp.exe（便携/打包分发用）
/// 4. PATH 中的 yt-dlp
pub fn resolve_ytdlp() -> Result<PathBuf, String> {
    YTDLP_PATH
        .get_or_init(resolve_ytdlp_uncached)
        .clone()
        .ok_or_else(|| {
            "未找到 yt-dlp。请把 yt-dlp.exe 放到程序目录，或设置环境变量 VELOXGRAB_YTDLP 指向它。".to_string()
        })
}

fn resolve_ytdlp_uncached() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("VELOXGRAB_YTDLP") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let exe_name = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
    if let Some(p) = probe_resource_dir(&[
        "bin/yt-dlp.exe",
        "yt-dlp.exe",
        "bin/yt-dlp",
        "yt-dlp",
    ]) {
        return Some(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["yt-dlp.exe", "bin/yt-dlp.exe"] {
                let cand = dir.join(name);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    // PATH 查找
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join(exe_name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

pub fn resolve_ffmpeg() -> Option<PathBuf> {
    FFMPEG_PATH.get_or_init(resolve_ffmpeg_uncached).clone()
}

fn resolve_ffmpeg_uncached() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("VELOXGRAB_FFMPEG") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    if let Some(p) = probe_resource_dir(&[
        "bin/ffmpeg.exe",
        "ffmpeg.exe",
        "bin/ffmpeg",
        "ffmpeg",
    ]) {
        return Some(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["ffmpeg.exe", "bin/ffmpeg.exe"] {
                let cand = dir.join(name);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    let exe_name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join(exe_name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// 组装通用的网络/认证参数，probe 与 download 共用，保证行为一致。
///
/// `with_cookies` 控制是否带 Cookie。之所以要把 Cookie 单独摘出来，
/// 是因为它并非总是有益的：
///
/// * 大部分公开视频不带 Cookie 就能解析，带上反而多一次可能失败的步骤；
/// * 从 Chrome 127 / Edge 127 起，浏览器启用了 App-Bound Encryption，
///   yt-dlp 读不到 Cookie 库会直接报
///   `Could not copy Chrome cookie database` 或 `Failed to decrypt with DPAPI`，
///   导致**本来能下载的视频也解析失败**。
///
/// 所以正确姿势是按需降级：先不带 Cookie 试，失败了再带上。
pub fn push_common_args(cmd: &mut Command, settings: &Settings, with_cookies: bool, url: &str) {
    cmd.arg("--no-warnings");
    cmd.arg("--no-playlist");
    // 尽力让 yt-dlp 用 UTF-8 输出。**它不是可靠防线**，实测（v2026.08.19 的
    // PyInstaller 独立 exe，简中系统）设了这两个变量仍然吐 cp936 字节：
    //   UTF-8 解 → "N��֡������35% ..."
    //   GBK   解 → "N卡帧率提升35% 全新dlss5模型再次优化"
    // 真正的防线是 decode.rs 的「先严格 UTF-8，再按系统代码页」两步解码。
    // 保留这两行只是给某些能识别它的发行版留个机会，无害。
    cmd.env("PYTHONUTF8", "1");
    cmd.env("PYTHONIOENCODING", "utf-8");

    if with_cookies {
        // Cookie：自定义文件优先，其次浏览器导入
        if !settings.cookies_file.trim().is_empty() {
            cmd.arg("--cookies").arg(&settings.cookies_file);
        } else if settings.cookies_from_browser != "none"
            && !settings.cookies_from_browser.trim().is_empty()
        {
            cmd.arg("--cookies-from-browser")
                .arg(&settings.cookies_from_browser);
        }
    }

    if let Some(ff) = resolve_ffmpeg() {
        cmd.arg("--ffmpeg-location").arg(ff);
    }

    // 全局限速：yt-dlp 的 --limit-rate 接受 500K / 4M 这类写法
    if settings.rate_limit_kb > 0 {
        cmd.arg("--limit-rate")
            .arg(format!("{}K", settings.rate_limit_kb));
    }
    if let Some(p) = settings.proxy_url() {
        cmd.arg("--proxy").arg(p);
    }
    // HuggingFace：带 token 走 Authorization 头。
    //
    // 格式是 `--add-headers FIELD:VALUE`（旧文档里的 `--add-header`）。写成
    // `huggingface.co:Authorization:Bearer …` 会被当成字段名 `huggingface.co`，
    // 服务端根本收不到 Authorization——要下 gated 仓库只会一直 401。
    // host 判定必须走 is_huggingface_url：`contains("huggingface.co")` 会把
    // 查询串里带域名的任意 URL 也判中，等于把 token 交给攻击者。
    if let Some(tok) = settings.hf_token_trimmed() {
        if crate::util::is_huggingface_url(url) {
            cmd.arg("--add-headers")
                .arg(format!("Authorization:Bearer {tok}"));
        }
    }
}

/// 用户是否配置了任何 Cookie 来源
pub fn has_cookie_source(settings: &Settings) -> bool {
    !settings.cookies_file.trim().is_empty()
        || (settings.cookies_from_browser != "none"
            && !settings.cookies_from_browser.trim().is_empty())
}

/// 把 yt-dlp 的 stderr 压成几行可读的错误摘要
fn summarize_err(stderr: &[u8]) -> String {
    // 用 decode_output 而不是 from_utf8_lossy：yt-dlp 的错误信息里常带中文
    // 文件名/标题（例如"文件已存在"），lossy 会把这部分变成问号，用户看到的
    // 报错就是一堆 `?`，无从判断到底卡在哪个文件上。见 decode.rs。
    let err = crate::decode::decode_output(stderr);
    err.lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

/// 解析出的单个可下载格式
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormatOption {
    pub format_id: String,
    pub label: String,
    pub ext: String,
    pub resolution: String,
    /// 视频高度（像素）。排序与推荐都用它。
    ///
    /// 不能用 `resolution` 反解：yt-dlp 给的是 `"1920x1080"` 这种 WxH 格式，
    /// 早期实现按 `"1080p"` 解析，结果恒为 0，排序完全失效。
    #[serde(default)]
    pub height: u32,
    pub filesize: Option<u64>,
    pub has_video: bool,
    pub has_audio: bool,
    pub fps: Option<f64>,
    /// 是否为界面上的默认推荐项（最高分辨率且兼容性最好）
    #[serde(default)]
    pub recommended: bool,
}

/// 探测结果，返回给前端
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    pub id: String,
    pub title: String,
    pub uploader: Option<String>,
    pub thumbnail: Option<String>,
    pub duration: Option<f64>,
    pub webpage_url: String,
    pub extractor: String,
    pub formats: Vec<FormatOption>,
    pub best_audio_format: Option<String>,
    pub has_video_stream: bool,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    format_id: Option<String>,
    ext: Option<String>,
    resolution: Option<String>,
    height: Option<u32>,
    vcodec: Option<String>,
    acodec: Option<String>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
    fps: Option<f64>,
    format_note: Option<String>,
    /// 音频平均码率（kbps）
    abr: Option<f64>,
    /// 总码率（kbps），abr 缺失时的兜底
    tbr: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawInfo {
    id: Option<String>,
    title: Option<String>,
    uploader: Option<String>,
    thumbnail: Option<String>,
    duration: Option<f64>,
    webpage_url: Option<String>,
    extractor: Option<String>,
    #[serde(default)]
    formats: Vec<RawFormat>,
}

fn has_codec(c: &Option<String>) -> bool {
    match c {
        Some(v) => !v.is_empty() && v != "none",
        None => false,
    }
}

/// 纯音频流打分：(是否有码率信息, 码率 kbps, 体积字节)。
///
/// 码率优先、体积兜底。只看 filesize 的话，HLS/DASH 源普遍不给这个字段，
/// 所有候选都是 0 分，最终永远选中列表里第一个（往往是低码率音轨）。
fn audio_score(abr: Option<f64>, tbr: Option<f64>, size: Option<u64>) -> (bool, u64, u64) {
    let kbps = abr
        .or(tbr)
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| v.round() as u64);
    (kbps.is_some(), kbps.unwrap_or(0), size.unwrap_or(0))
}

fn build_label(f: &RawFormat, has_video: bool) -> String {
    if !has_video {
        let abr = f.format_note.clone().unwrap_or_default();
        let ext = f.ext.clone().unwrap_or_default();
        return if abr.is_empty() {
            format!("{ext} 音频")
        } else {
            format!("{ext} {abr}")
        };
    }
    let res = f
        .resolution
        .clone()
        .filter(|r| r != "audio only" && !r.is_empty())
        .or_else(|| f.height.map(|h| format!("{h}p")))
        .unwrap_or_else(|| "unknown".into());
    let mut label = res;
    if let Some(fps) = f.fps {
        if fps >= 50.0 {
            label.push_str(&format!("{}", fps.round() as u32));
            label.push('f');
        }
    }
    if let Some(ext) = &f.ext {
        label.push_str(&format!(" · {ext}"));
    }
    if f.vcodec.as_deref() == Some("h264") {
        label.push_str(" · H264");
    } else if let Some(vc) = &f.vcodec {
        if vc.starts_with("avc1") {
            label.push_str(" · H264");
        } else if vc.starts_with("hev1") || vc.starts_with("hvc1") {
            label.push_str(" · H265");
        } else if vc.starts_with("av01") {
            label.push_str(" · AV1");
        }
    }
    label
}

/// 跑一次 yt-dlp -J，返回 stdout 原始字节
async fn run_probe(
    ytdlp: &std::path::Path,
    url: &str,
    settings: &Settings,
    with_cookies: bool,
) -> Result<Vec<u8>, String> {
    let mut cmd = Command::new(ytdlp);
    cmd.arg("-J");
    push_common_args(&mut cmd, settings, with_cookies, url);
    cmd.arg("--").arg(url);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    // 超时丢弃 future 时把子进程一起带走，别留下孤儿 yt-dlp
    cmd.kill_on_drop(true);

    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            format!(
                "解析超时（{} 秒）：站点无响应或需要人机验证",
                PROBE_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("启动 yt-dlp 失败: {e}"))?;

    if !output.status.success() {
        let tail = summarize_err(&output.stderr);
        return Err(if tail.is_empty() {
            "yt-dlp 未返回有效信息".into()
        } else {
            tail
        });
    }
    Ok(output.stdout)
}

/// 用 yt-dlp -J 抓取元数据并构造可选格式列表。
///
/// 解析分两轮：
/// 1. 先不带 Cookie —— 覆盖绝大多数公开视频，也不会被浏览器加密问题拖累；
/// 2. 第一轮失败且用户配了 Cookie 时，再带上 Cookie 重试（登录墙/年龄限制才需要）。
///
/// 早期版本无条件带 Cookie，一旦浏览器指纹保护让 yt-dlp 读不了 Cookie 库，
/// 连公开视频都会解析失败，表现为「视频无法解析下载地址」。
pub async fn probe(url: &str, settings: &Settings) -> Result<MediaInfo, String> {
    let ytdlp = resolve_ytdlp()?;

    let first = run_probe(&ytdlp, url, settings, false).await;
    let output = match first {
        Ok(o) => o,
        Err(plain_err) => {
            if has_cookie_source(settings) {
                match run_probe(&ytdlp, url, settings, true).await {
                    Ok(o) => o,
                    // 带上 Cookie 还是不行：把两轮的错误都给出来，
                    // 否则用户只看到 Cookie 的错误，会误以为是 Cookie 的问题
                    Err(cookie_err) => {
                        return Err(format!(
                            "解析失败（不带 Cookie）：\n{plain_err}\n\n解析失败（带 Cookie）：\n{cookie_err}"
                        ))
                    }
                }
            } else {
                return Err(format!("解析失败：\n{plain_err}"));
            }
        }
    };

    let raw: RawInfo =
        serde_json::from_slice(&output).map_err(|e| format!("解析 JSON 失败: {e}"))?;

    let mut video_formats: Vec<FormatOption> = Vec::new();
    // 打分三元组：(是否有码率信息, 码率 kbps, 体积字节)
    let mut best_audio: Option<((bool, u64, u64), String)> = None;
    let mut has_video_stream = false;

    for f in &raw.formats {
        let Some(fid) = f.format_id.clone().filter(|s| !s.is_empty()) else {
            continue;
        };
        let hv = has_codec(&f.vcodec);
        let ha = has_codec(&f.acodec);
        if hv {
            has_video_stream = true;
        }

        let size = f.filesize.or(f.filesize_approx);

        if hv {
            // 跳过纯图片/无高度的可疑项
            if f.height.is_none() && f.resolution.is_none() {
                continue;
            }
            video_formats.push(FormatOption {
                format_id: fid,
                label: build_label(f, true),
                ext: f.ext.clone().unwrap_or_default(),
                resolution: f
                    .resolution
                    .clone()
                    .or_else(|| f.height.map(|h| format!("{h}p")))
                    .unwrap_or_default(),
                // 优先用 yt-dlp 直接给的 height，缺失时才从字符串兜底解析
                height: f
                    .height
                    .unwrap_or_else(|| parse_height(f.resolution.as_deref().unwrap_or(""))),
                filesize: size,
                has_video: true,
                has_audio: ha,
                fps: f.fps,
                recommended: false,
            });
        } else if ha {
            // 记住质量最好的纯音频流，供合并用。码率优先，体积兜底。
            let score = audio_score(f.abr, f.tbr, size);
            if best_audio
                .as_ref()
                .map(|(s, _)| score > *s)
                .unwrap_or(true)
            {
                best_audio = Some((score, fid));
            }
        }
    }

    // 排序策略：分辨率高度降序 → 编码兼容性（H264 > H265 > AV1 > 其他）→ 体积降序
    // 这样同分辨率下默认首选兼容性最好的 H.264，用户仍可按体积挑 H.265/AV1。
    let codec_rank = |label: &str| -> u8 {
        if label.contains("H264") {
            0
        } else if label.contains("H265") {
            1
        } else if label.contains("AV1") {
            2
        } else {
            3
        }
    };
    video_formats.sort_by(|a, b| {
        b.height
            .cmp(&a.height)
            .then(codec_rank(&a.label).cmp(&codec_rank(&b.label)))
            .then(b.filesize.unwrap_or(0).cmp(&a.filesize.unwrap_or(0)))
    });
    video_formats.dedup_by(|a, b| a.format_id == b.format_id);

    // 标记推荐项：最高分辨率下第一个 H.264；没有 H.264 就取排在最前的那个
    let top_height = video_formats.first().map(|f| f.height).unwrap_or(0);
    let pick = video_formats
        .iter()
        .position(|f| f.height == top_height && f.label.contains("H264"))
        .unwrap_or(0);
    if let Some(f) = video_formats.get_mut(pick) {
        f.recommended = true;
    }

    if video_formats.is_empty() {
        return Err("该链接未解析到可下载的视频格式（可能需要登录 Cookie，或该站点不受支持）。".into());
    }

    let id = raw.id.unwrap_or_else(|| "unknown".into());
    let title = raw
        .title
        .unwrap_or_else(|| format!("video_{id}"));

    Ok(MediaInfo {
        id,
        title,
        uploader: raw.uploader,
        thumbnail: raw.thumbnail,
        duration: raw.duration,
        webpage_url: raw.webpage_url.unwrap_or_else(|| url.to_string()),
        extractor: raw.extractor.unwrap_or_else(|| "generic".into()),
        formats: video_formats,
        best_audio_format: best_audio.map(|(_, f)| f),
        has_video_stream,
    })
}

/// 从 `resolution` 字符串里取高度，仅作为 `height` 字段缺失时的兜底。
///
/// yt-dlp 对视频格式给的是 `"1920x1080"`（WxH），不是 `"1080p"`。
/// 早期实现只 `trim_end_matches('p')` 再 parse，对 `"1920x1080"` 恒返回 0，
/// 导致分辨率排序和「推荐」选项全部失效。
fn parse_height(res: &str) -> u32 {
    let core = res.split_once('x').map(|(_, h)| h).unwrap_or(res);
    let digits: String = core.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_height_handles_wxh_and_p_suffix() {
        // yt-dlp 对视频格式给的是 WxH，不是 1080p——这是排序失效的根因
        assert_eq!(parse_height("1920x1080"), 1080);
        assert_eq!(parse_height("1280x720"), 720);
        assert_eq!(parse_height("3840x2160"), 2160);
        // 兼容 p 后缀与帧率后缀
        assert_eq!(parse_height("1080p"), 1080);
        assert_eq!(parse_height("1080p60"), 1080);
        // 非法输入不能 panic
        assert_eq!(parse_height("audio only"), 0);
        assert_eq!(parse_height(""), 0);
    }

    #[test]
    fn audio_score_prefers_bitrate_over_size() {
        // HLS 源：都没有 filesize，只有码率。高码率必须胜出。
        let hi = audio_score(Some(128.0), None, None);
        let lo = audio_score(Some(64.0), None, None);
        assert!(hi > lo);

        // 有码率的总是胜过只有体积的（哪怕体积看起来很大）
        let br = audio_score(Some(48.0), None, None);
        let size_only = audio_score(None, None, Some(50 * 1024 * 1024));
        assert!(br > size_only);

        // abr 缺失时退回 tbr
        assert_eq!(audio_score(None, Some(160.0), None), (true, 160, 0));

        // 两者都没有才按体积比
        let big = audio_score(None, None, Some(10_000));
        let small = audio_score(None, None, Some(1_000));
        assert!(big > small);

        // 非法值不能当成有效码率
        assert_eq!(audio_score(Some(f64::NAN), None, None), (false, 0, 0));
        assert_eq!(audio_score(Some(-1.0), None, None), (false, 0, 0));
    }

    fn args_for(url: &str, token: &str) -> Vec<String> {
        let settings = Settings {
            hf_token: token.into(),
            ..Settings::default()
        };
        let mut cmd = Command::new("yt-dlp");
        push_common_args(&mut cmd, &settings, false, url);
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn hf_token_is_sent_as_authorization_header() {
        let args = args_for("https://huggingface.co/org/repo/resolve/main/x.bin", "hf_secret");
        let i = args
            .iter()
            .position(|a| a == "--add-headers")
            .expect("HF 链接必须带认证头");
        // 关键回归：FIELD:VALUE，不是 HOST:Name:Value
        assert_eq!(args[i + 1], "Authorization:Bearer hf_secret");
    }

    #[test]
    fn hf_token_is_not_sent_to_lookalike_hosts() {
        // 没有 token 时当然不加
        assert!(!args_for("https://huggingface.co/x", "").iter().any(|a| a == "--add-headers"));
        // 查询串里出现域名不算
        assert!(!args_for("https://evil.example/?next=huggingface.co", "hf_secret")
            .iter()
            .any(|a| a == "--add-headers"));
        // 相似域名 / 后缀伪造不算
        assert!(!args_for("https://huggingface.co.evil.example/x", "hf_secret")
            .iter()
            .any(|a| a == "--add-headers"));
        assert!(!args_for("https://nothuggingface.co/x", "hf_secret")
            .iter()
            .any(|a| a == "--add-headers"));
        // 子域仍然要带上
        assert!(args_for("https://cdn-lfs.huggingface.co/x", "hf_secret")
            .iter()
            .any(|a| a == "--add-headers"));
    }
}
