//! 下载目录闸门（高-1）。
//!
//! ## 要堵的是什么
//!
//! `save_dir` 是全项目唯一一条「外部可控、直接用于建目录 + 落盘 + 删文件」的
//! 路径分量：文件名一直有 `util::sanitize_filename` 兜着，目录分量此前**完全不
//! 检查**。`veloxgrab://add?url=…&dir=…\Startup` 配上可预测的文件名，就是
//! 「远程网页诱导下载器把文件写进任意目录」——一旦落到启动目录/其它软件的
//! 插件目录，等于登录自启动。攻击面有四个入口：深链、扩展（含默认接管的
//! onCreated 静默转发）、webview 的 `invoke`、以及 history.json 回灌。
//!
//! ## 判据（两级）
//!
//! 1. 词法折叠（`util::fold_path_lexically`）：消化 `.`、就地折叠 `..`；
//!    `..` 若跳出根 → 判为越界。**不存在的目录 canonicalize 会失败**，所以
//!    这一级不能省——它正是给「尚未创建的逃逸路径」准备的。
//! 2. 真实前缀比对：对「折叠后仍存在的最近祖先」做 canonicalize（解软链/
//!    junction、抹平大小写与 8.3 短名），确认它落在规范化后的根之内。
//!    两级都过才算「根内」。
//!
//! 根内 → 直接放行（保住「每次下载存到子文件夹」这类正常用法）。
//! 越界 → **不静默改写**，走一次 OS 级原生确认框（非 webview DOM——XSS 之下
//! 前端弹的确认不可信）：用户明确同意才放行并把该目录记入信任集（供续传/
//! 重下/清理沿用），否则回落全局下载目录。用户亲手选过的目录（save_settings、
//! dialog 选目录）天然是信任根，不受此闸门约束。

use std::path::{Path, PathBuf};

/// 归一化用于比较：Windows 大小写不敏感。
/// 只在两侧都已 canonicalize（绝对、无 `..`、短名已解）之后使用。
fn norm_key(p: &Path) -> String {
    let s = p.to_string_lossy().to_string();
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

/// 词法前缀判断：`c` 是否在 `base` 之下（含相等）。Windows 不区分大小写。
fn is_lex_prefix(base: &Path, c: &Path) -> bool {
    let bc = base.components();
    let cc = c.components();
    let mut bt = bc.peekable();
    let mut ct = cc.peekable();
    loop {
        let (x, y) = (bt.peek().cloned(), ct.peek().cloned());
        let Some(xv) = x else { return true };
        let Some(yv) = y else { return false };
        if norm_component(&xv) != norm_component(&yv) {
            return false;
        }
        bt.next();
        ct.next();
    }
}

/// 单分量比较键：RootDir/Prefix 等非 Normal 分量按字符串化，Normal 走大小写规则。
fn norm_component(c: &std::path::Component<'_>) -> String {
    match c {
        std::path::Component::Normal(s) => {
            let t = s.to_string_lossy().to_string();
            if cfg!(windows) {
                t.to_ascii_lowercase()
            } else {
                t
            }
        }
        other => {
            let t = other.as_os_str().to_string_lossy().to_string();
            if cfg!(windows) {
                t.to_ascii_lowercase()
            } else {
                t
            }
        }
    }
}

/// 纯判据：候选目录 `cand` 相对根 `root` 是否落在根内、且不试图上跳。
///
/// 返回 `true` 的含义是「词法上安全」——不代表放行，调用方还要过第二级
/// canonicalize。返回 `false` 即判越界。拆出来是为了能带着构造路径单测，
/// 不必碰真实文件系统。
pub(crate) fn lexically_inside(root: &Path, cand: &Path) -> bool {
    // 根自身上跳（相对写法）没有意义，一律视为不安全
    let (c_folded, c_esc) = crate::util::fold_path_lexically(cand);
    if c_esc {
        return false;
    }
    let (r_folded, r_esc) = crate::util::fold_path_lexically(root);
    if r_esc {
        return false;
    }
    // cand 与 root 同为绝对或同为相对；相对时按同一基准拼到 root 之外无意义，
    // 交由调用方保证传进来的是绝对路径（入口已拼接/规范化）。
    if c_folded.is_absolute() != r_folded.is_absolute() {
        return false;
    }
    is_lex_prefix(&r_folded, &c_folded)
}

/// canonicalize `p` 或最近的已存在祖先（折叠掉 p 自身尾部的新建层级）。
///
/// 目的：对存在的那段拿到「解析过软链/短名」的真身，再拼回不存在的尾段。
/// 尾段里若还藏着 `..` 已在词法级被拒（lexically_inside），这里只剩新建目录名。
fn canonicalize_best_effort(p: &Path) -> Option<PathBuf> {
    if let Ok(c) = p.canonicalize() {
        return Some(c);
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(base) = cur.canonicalize() {
            let mut out = base;
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return Some(out);
        }
        let name = cur.file_name()?;
        tail.push(name.to_os_string());
        cur = cur.parent()?;
    }
}

/// 目录闸门的完整判定：`cand` 是否可当作 `root` 之下的下载目录直接放行。
pub(crate) fn is_inside(root: &Path, cand: &Path) -> bool {
    if !lexically_inside(root, cand) {
        return false;
    }
    let Some(r) = canonicalize_best_effort(root) else {
        // 连规范化根都拿不到：无从证明「在根内」，保守判越界（宁可多弹一次框）
        return false;
    };
    let Some(c) = canonicalize_best_effort(cand) else {
        return false;
    };
    is_lex_prefix(&r, &c)
}

/// 该目录是否已被用户明确认可（全局默认目录，或历史上「同意过」的信任目录）。
///
/// 比较前先做词法折叠：信任集里存的都是当年定稿的规范路径，而这里拿到的
/// cand 是 admit 里刚 join 出来、**未定稿**的原始串——不折叠的话，用户当年
/// 批准了 `D:\games`，如今传 `D:\games\..\games` 会匹配不上、对已信任的目录
/// 重复弹框。折叠对规范路径无损（幂等），两边同折即可对齐。
pub(crate) fn is_trusted(settings_root: &Path, trusted: &[String], cand: &Path) -> bool {
    let (cand_folded, _) = crate::util::fold_path_lexically(cand);
    let key = norm_key(&cand_folded);
    if norm_key(settings_root) == key {
        return true;
    }
    trusted.iter().any(|t| {
        let (tf, _) = crate::util::fold_path_lexically(Path::new(t));
        norm_key(&tf) == key
    })
}

/// 定稿：把「闸门已认可」的路径转成规范绝对路径字符串。
///
/// 存在 → 直接 canonicalize；不存在 → canonicalize 最近的已存在祖先再拼回尾段
/// （尾段只可能是新建的普通目录名：`..` 在 lexically_inside 就被拒了）。
/// 与 is_inside 的第二级共用同一套 best-effort 逻辑，保证「判定用的路径」和
/// 「回填给任务的路径」是同一个东西。拿不到定稿（比如根在断开的外置盘上且
/// 连盘符都解析不了）时报错，让建任务失败——不静默降级。
pub(crate) fn finalize(cand: &Path) -> Result<String, String> {
    let resolved = canonicalize_best_effort(cand).ok_or("路径无法解析")?;
    Ok(resolved.to_string_lossy().to_string())
}

/// 入口判定的四种结果。
pub(crate) enum Admit {
    /// 没给目录：直接用默认根
    NoDir,
    /// 根内（含相对写法按根解析后的子目录）
    Inside(PathBuf),
    /// 根外，但用户此前经原生框同意过（历史任务的续传/重下/清理走这条）
    Trusted(PathBuf),
    /// 根外且无信任记录：需要一次 OS 级确认，由调用方弹框
    Outside(PathBuf),
}

/// 对一个候选 save_dir 字符串做完整判定（不调 create_dir_all，也不弹框）。
///
/// 相对路径**按默认根解析**（`dir=work` → `<根>\work`）：这是深链 dir 参数
/// 最符合直觉的语义，同时堵掉旧行为里「相对路径静默写进进程 CWD」的暗坑——
/// 经协议拉起时 CWD 完全不受我们控制。
pub(crate) fn admit(raw: Option<&str>, root: &Path, trusted: &[String]) -> Admit {
    let Some(raw) = raw.map(str::trim).filter(|d| !d.is_empty()) else {
        return Admit::NoDir;
    };
    let p = PathBuf::from(raw);
    let cand = if p.is_absolute() { p } else { root.join(p) };
    if is_inside(root, &cand) {
        return Admit::Inside(cand);
    }
    if is_trusted(root, trusted, &cand) {
        return Admit::Trusted(cand);
    }
    Admit::Outside(cand)
}

/// 「删掉下载本体」这条路径的安全判据：`output_path` 能否作为删除目标。
///
/// 为什么需要专判据而不是直接 `remove_file(output_path)`：
/// `output_path` 是**磁盘上 history.json 里的字符串**——威胁模型里算不可信
/// 输入（手改、半截写、别的进程伪造都能把任意路径塞进来）。照单全收的话，
/// 一条伪造历史就把「删除任务」变成「删任意文件」的原语。
///
/// 判据（全部满足才放行；任一条不过就拒绝删本体、但照常删任务记录）：
/// 1. 绝对路径、词法折叠不上跳；
/// 2. 必须是普通文件，且**不是符号链接**（用 `symlink_metadata` 判链接本身，
///    不跟随——按链接删等于把删除权交给链接目标，收益为零风险无限）；
/// 3. 父目录与任务声明的保存目录 canonicalize 后必须是同一处。这一条把
///    「伪造 output_path 指向 C:\Windows\system32\xx.dll」挡死：伪造者还得
///    同时伪造 save_dir、并让用户当初批准过那个目录。
///
/// 返回 Ok(折叠后的路径) 才允许删。
pub(crate) fn check_deletable_file(output_path: &str, task_dir: &Path) -> Result<PathBuf, String> {
    let raw = output_path.trim();
    if raw.is_empty() {
        return Err("任务没有记录输出路径".into());
    }
    let p = Path::new(raw);
    if !p.is_absolute() {
        return Err("输出路径不是绝对路径".into());
    }
    let (folded, escaped) = crate::util::fold_path_lexically(p);
    if escaped {
        return Err("输出路径试图上跳出根".into());
    }
    // symlink_metadata 不跟随链接：这里要看的是「它本身是不是链接」
    let meta = std::fs::symlink_metadata(&folded).map_err(|e| format!("文件不可访问: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err("目标是符号链接，拒绝按此路径删除".into());
    }
    if !meta.is_file() {
        return Err("目标不是普通文件".into());
    }
    // 父目录与任务目录必须指向同一处（解软链、抹平短名/大小写后比对）
    let parent = folded.parent().ok_or("输出路径没有父目录")?;
    let parent_c = canonicalize_best_effort(parent).ok_or("父目录无法规范化")?;
    let dir_c = canonicalize_best_effort(task_dir).ok_or("任务目录无法规范化")?;
    if norm_key(&parent_c) != norm_key(&dir_c) {
        return Err("输出路径不在任务的保存目录内".into());
    }
    Ok(folded)
}

/// 在任务目录里按「损坏的 output_path」找回真实文件。
///
/// 为什么需要这个：yt-dlp 的 stdout 曾经按 Windows 本地代码页（cp936）输出，
/// 我们用 `from_utf8_lossy` 解码，中文文件名被毁成一串 `?`（详见
/// `ytdlp::push_common_args` 里 PYTHONUTF8 的注释）。**编码修好只对以后的任务
/// 生效**，历史记录里已经存着坏路径——不补救的话，这些任务永远删不掉文件。
///
/// 匹配策略（宁可找不到，也不能误删）：
/// 1. 用坏名字的**最长可识别片段**做前缀匹配：把替换符（`?` / U+FFFD）切成若干
///    纯 ASCII/文字片段，取最长那段（通常就是文件名开头的 `N`、`dlss` 之类）；
/// 2. 片段太短（< 3 字符）就放弃——`N` 这种能匹配上半个目录；
/// 3. 排除临时文件后缀（`.part` / `.veloxgrab-*`）；
/// 4. 候选必须唯一。有多个同名候选时返回 None，交给用户自己处理，
///    决不做「挑一个删掉」这种赌博。
pub(crate) fn find_file_for_broken_path(output_path: &str, task_dir: &Path) -> Option<PathBuf> {
    let name = Path::new(output_path).file_name()?.to_string_lossy().into_owned();
    // 只处理确实带损坏痕迹的路径；正常路径走精确匹配那条道
    if !name.contains('?') && !name.contains('\u{FFFD}') {
        return None;
    }
    // 按损坏符切段，取最长的一段做锚点
    let mut best: Option<&str> = None;
    for seg in name.split(['?', '\u{FFFD}']) {
        let seg = seg.trim();
        if seg.chars().count() < 3 {
            continue;
        }
        if best.map(|b| seg.len() > b.len()).unwrap_or(true) {
            best = Some(seg);
        }
    }
    let anchor = best?;
    let ext = Path::new(&name).extension().and_then(|e| e.to_str());

    let mut hits: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(task_dir).ok()?.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let candidate = entry.file_name().to_string_lossy().into_owned();
        // 临时产物不能当成成品删：新命名的直链临时文件带任务 id，
        // 不能只比对固定后缀（`.veloxgrab-downloading.<id>`）。
        if candidate.ends_with(".part")
            || candidate.ends_with(".veloxgrab-part")
            || candidate.ends_with(".veloxgrab-part.tmp")
            || crate::direct::is_temp_artifact(&candidate)
        {
            continue;
        }
        // 扩展名要对上，避免把同名的 .jpg 当成 .mp4 删掉
        if let Some(ext) = ext {
            if !candidate.to_ascii_lowercase().ends_with(&format!(".{}", ext.to_ascii_lowercase())) {
                continue;
            }
        }
        if candidate.contains(anchor) {
            hits.push(p);
        }
    }
    if hits.len() == 1 {
        Some(hits.remove(0))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// 词法级逐分支（不依赖文件系统，全部走构造路径）。
    #[test]
    #[cfg(windows)]
    fn lexically_inside_rejects_traversal_and_outside_roots() {
        let root = p(r"C:\Users\me\Downloads\VeloxGrab");
        // 根内 / 根本身
        assert!(lexically_inside(&root, &p(r"C:\Users\me\Downloads\VeloxGrab\sub")));
        assert!(lexically_inside(&root, &root));
        // 内部 .. 消化后仍在根内
        assert!(lexically_inside(&root, &p(r"C:\Users\me\Downloads\VeloxGrab\a\..\sub")));
        assert!(lexically_inside(&root, &p(r"C:\Users\me\Downloads\VeloxGrab\.\x")));
        // 上跳逃出根 → 拒
        assert!(!lexically_inside(&root, &p(r"C:\Users\me\Downloads\VeloxGrab\..\..\Windows")));
        // 兄弟/前缀混淆（VeloxGrabEvil 不是 VeloxGrab 的子目录）→ 拒
        assert!(!lexically_inside(&root, &p(r"C:\Users\me\Downloads\VeloxGrabEvil")));
        // 完全外部 → 拒
        assert!(!lexically_inside(&root, &p(r"D:\other")));
        // UNC → 拒（根内是盘符路径）
        assert!(!lexically_inside(&root, &p(r"\\evil\share\x")));
        // 相对 vs 绝对 → 不一致，拒
        assert!(!lexically_inside(&root, &p(r"sub\dir")));
    }

    #[test]
    #[cfg(windows)]
    fn lex_prefix_is_case_insensitive_on_windows() {
        let root = p(r"C:\Users\Me\Downloads\VeloxGrab");
        assert!(lexically_inside(&root, &p(r"c:\users\me\downloads\veloxgrab\Sub")));
    }

    /// is_inside 在真实文件系统上：根存在、目标子目录不存在时，应仍判为根内
    /// （这正是「下载到新子文件夹」的正常路径，且要能穿过 create_dir_all）。
    #[test]
    #[cfg(windows)]
    fn is_inside_accepts_nonexistent_subdir_of_real_root() {
        let dir = std::env::temp_dir().join(format!("vxg_root_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // 上次崩溃留下的残骸先清掉
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.canonicalize().unwrap();
        let fresh = root.join("newsub").join("deeper");
        assert!(is_inside(&root, &fresh), "根下尚未创建的子树必须放行");
        // 根下子树里的 .. 绕回根内：放行
        let looped = root.join("newsub").join("..").join("x");
        assert!(is_inside(&root, &looped));
        // 跳出根：拒
        let escape = root.join("..").join("Windows");
        assert!(!is_inside(&root, &escape));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(windows)]
    fn trusted_checks_are_case_insensitive() {
        let settings_root = p(r"C:\dl\VeloxGrab");
        let trusted = vec![r"D:\Games\Downloads".to_string()];
        assert!(is_trusted(&settings_root, &trusted, &p(r"C:\dl\veloxgrab")));
        assert!(is_trusted(&settings_root, &trusted, &p(r"d:\games\downloads")));
        assert!(!is_trusted(&settings_root, &trusted, &p(r"E:\other")));
        // 等价写法（折叠后回到同一个目录）必须命中信任集：当年批准了 D:\Games\Downloads，
        // 再传它的等价拼写不该重复弹框
        assert!(is_trusted(
            &settings_root,
            &trusted,
            &p(r"D:\Games\Downloads\..\Downloads"),
        ));
        assert!(is_trusted(&settings_root, &trusted, &p(r"D:\Games\Downloads\sub\..")));
        assert!(is_trusted(&settings_root, &trusted, &p(r"D:\Games\.\Downloads")));
        // 折叠后不等价的（真的指向别的目录）仍然不命中
        assert!(!is_trusted(&settings_root, &trusted, &p(r"D:\Games\Music")));
    }

    /// 删除判据：只放行「任务目录下的普通文件」，其余一律拒。
    /// 这条判据守的是「history.json 被伪造 → 删任意文件」这个升级面，
    /// 所以每个拒绝分支都单独断言。
    #[test]
    #[cfg(windows)]
    fn check_deletable_file_only_allows_plain_file_in_task_dir() {
        let base = std::env::temp_dir().join(format!("vxg_del_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let task_dir = base.join("task");
        let other_dir = base.join("other");
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::create_dir_all(&other_dir).unwrap();

        let good = task_dir.join("movie.mp4");
        std::fs::write(&good, b"x").unwrap();
        // 正常路径：任务目录下的成品文件 → 放行
        let ok = check_deletable_file(good.to_str().unwrap(), &task_dir)
            .expect("任务目录下的普通文件必须放行");
        // 返回的是**词法折叠后**的原样路径（不做 canonicalize：调用方拿它去
        // remove_file，Windows 上大小写/短名本来就等价，多一次规范化反而可能
        // 因权限差异失败）。所以这里比对的是 folded 而不是 canonicalize。
        assert_eq!(ok, good);

        // 空 / 相对路径 → 拒
        assert!(check_deletable_file("", &task_dir).is_err());
        assert!(check_deletable_file(r"movie.mp4", &task_dir).is_err());

        // 目录 → 拒（绝不是 remove_dir_all 的入口）
        assert!(check_deletable_file(task_dir.to_str().unwrap(), &task_dir).is_err());

        // 父目录 ≠ 任务目录 → 拒（伪造 output_path 指向别处）
        let outside = other_dir.join("secret.txt");
        std::fs::write(&outside, b"y").unwrap();
        assert!(check_deletable_file(outside.to_str().unwrap(), &task_dir).is_err());

        // 上跳写法 → 词法折叠判定拒
        let escape = format!(r"{}\..\..\Windows\notepad.exe", task_dir.display());
        assert!(check_deletable_file(&escape, &task_dir).is_err());

        // 不存在的文件 → 拒（交由调用方按幂等处理，不当成"删成功"）
        let missing = task_dir.join("nope.bin");
        assert!(check_deletable_file(missing.to_str().unwrap(), &task_dir).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 编码损坏的历史路径救援：只认唯一候选，模糊匹配绝不做赌博。
    #[test]
    #[cfg(windows)]
    fn find_file_for_broken_path_requires_unique_match() {
        let base = std::env::temp_dir().join(format!("vxg_repair_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // 场景一：唯一候选 → 找得到
        std::fs::write(base.join("N卡帧率提升35% 全新dlss5模型.mp4"), b"x").unwrap();
        let broken = format!(r"{}\N?????????35% ??dlss5????.mp4", base.display());
        let hit = find_file_for_broken_path(&broken, &base);
        assert!(hit.is_some(), "唯一候选必须能找回来");

        // 场景二：同锚点两个候选 → 拒绝（绝不猜）
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("N推流设置35% A.mp4"), b"x").unwrap();
        std::fs::write(base.join("N推流设置35% B.mp4"), b"x").unwrap();
        let ambiguous = format!(r"{}\N??????35% ?.mp4", base.display());
        assert!(
            find_file_for_broken_path(&ambiguous, &base).is_none(),
            "多个候选必须放弃，不能挑一个删"
        );

        // 场景三：扩展名不符 → 不匹配（不能把 .jpg 当 .mp4 删）
        std::fs::write(base.join("Z唯一锚点样例35%.jpg"), b"x").unwrap();
        let wrong_ext = format!(r"{}\Z????????35%.mp4", base.display());
        assert!(find_file_for_broken_path(&wrong_ext, &base).is_none());

        // 场景四：没损坏痕迹的路径不走这条道（避免误判正常路径）
        let clean = base.join("normal.mp4");
        assert!(find_file_for_broken_path(clean.to_str().unwrap(), &base).is_none());

        // 场景五：锚点太短（<3 字符）→ 放弃，防止匹配半个目录
        let too_short = format!(r"{}\A?.mp4", base.display());
        assert!(find_file_for_broken_path(&too_short, &base).is_none());

        let _ = std::fs::remove_dir_all(&base);
    }
}
