//! Warm target 卷：按「项目 × 工具链镜像 × 目标 triple × 构建模式」持久化
//! CARGO_TARGET_DIR，并跨构建复用。
//!
//! 为什么需要它：sccache 跳过 rustc 编译，但**构建脚本（build-script-build）
//! 是 bin crate，sccache 无法缓存，cargo 每次仍会重新编译并运行全部构建
//! 脚本**；最终 bin crate 与链接同样每次发生。实测即使 sccache 100% 命中，
//! bat 仍需 ~120s。复用 target 目录后，cargo 的 fingerprint 直接判定依赖
//! 单元（含构建脚本）为最新——只有真正变更的单元重新执行。
//!
//! 隔离边界：key 必须包含**镜像 tag**（工具链）与 **triple / mode**，不同
//! 工具链的产物绝不混用；feature / rustflags 的变化由 cargo fingerprint
//! 在同一目录内正确处理。体量由 LRU 配额回收（见 `enforce_quota`）。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hotpot_core::{ContentDigest, Result};

/// 默认总容量上限（20 GiB）：target 目录单体常达 0.5–2 GB，
/// 无界增长会很快吃光数据盘。
pub const DEFAULT_QUOTA: u64 = 20 * 1024 * 1024 * 1024;

/// 配额回收保护期：mtime 在此窗口内的条目不参与淘汰，
/// 避免删掉正在被另一个构建容器使用的目录。
pub const EVICTION_GRACE: Duration = Duration::from_secs(30 * 60);

/// 计算 warm target 的 key。
///
/// `identity` 为项目的稳定标识（git URL，或本地项目绝对路径）；先取其
/// blake3 前 16 个 hex 字符，避免 URL 里出现 `/` 等字符、也压缩长度。
/// 其余段只允许有限字符集，sanitize 后拼入，key 仍可人工辨认。
pub fn key(identity: &str, image_tag: &str, target: &str, mode: &str) -> String {
    let id = &ContentDigest::of_bytes(identity.as_bytes()).to_hex()[..16];
    format!(
        "{id}-{}-{}-{}",
        sanitize(image_tag),
        sanitize(target),
        sanitize(mode)
    )
}

/// key 对应的 warm target 目录（调用方负责 create_dir_all）。
pub fn dir(tools_dir: &Path, key: &str) -> PathBuf {
    tools_dir.join("warm-targets").join(key)
}

/// LRU 配额回收：按目录 mtime 从旧到新删除条目，直到总用量不超过 `quota`。
///
/// 返回回收的字节数。mtime 宽限期内的条目跳过（可能正被容器使用）；
/// 该函数是同步磁盘遍历，调用方须放在 spawn_blocking 中。
pub fn enforce_quota(root: &Path, quota: u64) -> Result<u64> {
    let mut entries = match fs::read_dir(root) {
        Ok(rd) => rd
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .collect::<Vec<_>>(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };

    let mut total = 0u64;
    for entry in &entries {
        total += dir_size(&entry.path());
    }
    if total <= quota {
        return Ok(0);
    }

    // mtime 旧的先淘汰；拿不到 mtime 的排到最后（不主动删信息不足的条目）。
    entries.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());

    let grace_start = std::time::SystemTime::now() - EVICTION_GRACE;
    let mut reclaimed = 0u64;
    for entry in entries {
        if total <= quota {
            break;
        }
        let young = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|mtime| mtime >= grace_start)
            .unwrap_or(true);
        if young {
            continue;
        }
        let path = entry.path();
        let size = dir_size(&path);
        // 整个 key 目录一起删；删失败（权限/挂载问题）跳过，不中断回收。
        if fs::remove_dir_all(&path).is_ok() {
            reclaimed += size;
            total = total.saturating_sub(size);
        }
    }
    Ok(reclaimed)
}

/// 递归统计目录字节数（遵循符号链接的目标内容会重复计入，target 内无符号链接）。
fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// key 段 sanitize：非 ASCII 字母数字统一为 `_`，并截断长度。
fn sanitize(segment: &str) -> String {
    let out = segment
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>();
    out.chars().take(48).collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn key_is_stable_and_path_safe() {
        let k1 = key(
            "https://github.com/BurntSushi/ripgrep.git",
            "slim-bookworm",
            "x86_64-unknown-linux-gnu",
            "release",
        );
        let k2 = key(
            "https://github.com/BurntSushi/ripgrep.git",
            "slim-bookworm",
            "x86_64-unknown-linux-gnu",
            "release",
        );
        assert_eq!(k1, k2);
        // 无路径分隔符等危险字符；身份段为 16 hex。
        assert!(
            k1.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
        assert!(k1.starts_with(|c: char| c.is_ascii_hexdigit()));

        // 任一维度变化 → key 变化。
        let k3 = key(
            "https://github.com/BurntSushi/ripgrep.git",
            "1.85-slim-bookworm",
            "x86_64-unknown-linux-gnu",
            "release",
        );
        assert_ne!(k1, k3);
    }

    #[test]
    fn quota_evicts_oldest_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("warm-targets");
        fs::create_dir_all(&root).unwrap();

        // 两个条目：旧条目 4 KiB，新条目（宽限内）12 KiB。
        let old = root.join("old-key");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("a.bin"), vec![0u8; 4 * 1024]).unwrap();
        let old_ts = std::time::SystemTime::now() - Duration::from_secs(60 * 60);
        change_mtime(&old, old_ts);

        let young = root.join("young-key");
        fs::create_dir_all(&young).unwrap();
        fs::write(young.join("b.bin"), vec![0u8; 12 * 1024]).unwrap();

        // 配额 8 KiB：旧条目可被删到配额内；新条目受宽限保护。
        let reclaimed = enforce_quota(&root, 8 * 1024).unwrap();
        assert_eq!(reclaimed, 4 * 1024);
        assert!(!old.exists(), "最旧条目应被淘汰");
        assert!(young.exists(), "宽限内条目不得淘汰");
    }

    #[test]
    fn quota_is_noop_within_limit_or_missing_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("warm-targets");
        fs::create_dir_all(root.join("k")).unwrap();
        fs::write(root.join("k").join("f"), b"x").unwrap();
        assert_eq!(enforce_quota(&root, DEFAULT_QUOTA).unwrap(), 0);
        assert_eq!(
            enforce_quota(&tmp.path().join("absent"), DEFAULT_QUOTA).unwrap(),
            0
        );
    }

    fn change_mtime(path: &Path, ts: std::time::SystemTime) {
        // 顶层目录 mtime 是排序依据；文件时间不必修改。
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(ts)).unwrap();
    }
}
