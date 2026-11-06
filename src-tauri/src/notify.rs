//! 任务完成提醒：系统 toast + 完成音效 + 闪任务栏 + 托盘提示。
//!
//! ## toast 的适用边界（如实声明）
//!
//! Windows 的 toast 需要一个注册过的 AppUserModelID（通常靠开始菜单里的
//! 快捷方式建立）。安装版有 NSIS 建立的快捷方式，toast 可靠显示；便携版、
//! 绿色版、直接双击 exe 的场景可能拿不到——结果是代码不报错、通知就是不
//! 出现。所以 toast 只是「锦上添花」的那一层，**音效与任务栏闪烁**（进程内
//! 就能保证生效）才是提醒的主干，两者都受 `notify_on_complete` 开关控制。
//!
//! ## 为什么窗口在前台时不闪 / 不弹 toast
//!
//! 用户正盯着任务列表，进度条自己会变成「已完成」，再闪任务栏、再弹 toast
//! 纯属噪音。音效不受此限：它是对「完成」的听觉确认，挂着一批下载时逐个
//! 响一声正是用户要的。

use tauri::{Manager, UserAttentionType};

use crate::engine::{SharedTasks, TaskState};

/// worker 收尾时调用。`done` 表示这次收尾是成功还是失败/取消；
/// `title` 是任务标题（toast 正文用）。
pub async fn after_worker(
    app: &tauri::AppHandle,
    tasks: &SharedTasks,
    done: bool,
    title: &str,
) {
    let enabled = app
        .try_state::<crate::AppState>()
        .map(|s| crate::settings::load_settings(s.inner()).notify_on_complete)
        .unwrap_or(false);

    // 托盘提示是静态信息，即使关掉提醒也照常更新——它不打扰任何人。
    update_tooltip(app, tasks).await;

    if !enabled || !done {
        return;
    }

    // 完成音效：不受「前台/后台」限制——挂着一批下载时逐个响一声正是用户要的。
    #[cfg(windows)]
    play_done_sound();

    let focused = app
        .get_webview_window("main")
        .map(|w| w.is_focused().unwrap_or(false))
        .unwrap_or(false);

    // 前台用户已经能看到列表里的状态变化：不闪任务栏、不弹 toast。
    if focused {
        return;
    }

    if let Some(win) = app.get_webview_window("main") {
        let _ = win.request_user_attention(Some(UserAttentionType::Informational));
    }

    use tauri_plugin_notification::NotificationExt;
    let _ = app
        .notification()
        .builder()
        .title("下载完成")
        .body(title)
        .show();
}

// ---------- 完成音效 ----------
//
// 不引入音频素材文件：两音上行（E5→A5）的短促提示音由程序合成，
// SND_MEMORY 直接从内存播放（SND_ASYNC 立即返回不阻塞 worker）。
// 注意 PlaySound 的 SND_ASYNC 要求缓冲区存活到播放结束——用 OnceLock
// 常驻合成结果，天然满足。
#[cfg(windows)]
fn play_done_sound() {
    use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY};
    static WAV: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let wav = WAV.get_or_init(build_done_wav);
    unsafe {
        PlaySoundW(wav.as_ptr() as *const u16, 0, SND_MEMORY | SND_ASYNC);
    }
}

/// 合成完成音效的 WAV 字节：16bit/44100Hz 单声道，
/// E5(659Hz) 与 A5(880Hz) 各 160ms，带淡入与指数衰减包络。
#[cfg(windows)]
fn build_done_wav() -> Vec<u8> {
    const SR: f64 = 44100.0;
    const SEG_MS: f64 = 160.0;
    let seg_len = (SR * SEG_MS / 1000.0) as usize;
    let total = seg_len * 2;
    let mut samples = Vec::with_capacity(total);
    for i in 0..total {
        let (freq, seg_t) = if i < seg_len {
            (659.25, i as f64 / seg_len as f64)
        } else {
            (880.0, (i - seg_len) as f64 / seg_len as f64)
        };
        // 包络：8ms 淡入 + 指数衰减，段间重新起振
        let attack = ((i as f64 / SR) / 0.008).min(1.0);
        let env = attack * (-seg_t * 3.5).exp();
        let v = (2.0 * std::f64::consts::PI * freq * (i as f64 / SR)).sin() * 0.30 * env;
        samples.push((v * 32767.0) as i16);
    }

    // 最小 WAV 打包（PCM 16bit 单声道）
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + samples.len() as u32 * 2).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    out.extend_from_slice(&(SR as u32).to_le_bytes());
    out.extend_from_slice(&(SR as u32 * 2.0 as u32).to_le_bytes()); // 字节率
    out.extend_from_slice(&2u16.to_le_bytes()); // 块对齐
    out.extend_from_slice(&16u16.to_le_bytes()); // 位深
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(samples.len() as u32 * 2).to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// 托盘 tooltip 跟着队列状态走。
///
/// 不显示百分比：tooltip 只有一行、刷新频率又高，跳数字反而看不清。
/// 用户要细节就点开窗口。
async fn update_tooltip(app: &tauri::AppHandle, tasks: &SharedTasks) {
    let (total, running, done) = {
        let guard = tasks.lock().await;
        let mut total = 0usize;
        let mut running = 0usize;
        let mut done = 0usize;
        for t in guard.values() {
            total += 1;
            match t.state {
                TaskState::Downloading | TaskState::Merging => running += 1,
                TaskState::Done => done += 1,
                _ => {}
            }
        }
        (total, running, done)
    };

    let text = if running > 0 {
        format!("VeloxGrab · 正在下载 {running} 个")
    } else if done > 0 && done == total {
        format!("VeloxGrab · {done} 个任务已完成")
    } else {
        "VeloxGrab".to_string()
    };

    if let Some(tray) = app.tray_by_id(crate::tray::TRAY_ID) {
        let _ = tray.set_tooltip(Some(text));
    }
}
