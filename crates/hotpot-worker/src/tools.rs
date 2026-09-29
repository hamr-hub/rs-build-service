//! 构建工具供给：按需下载 Linux 静态链接的 sccache，供 docker 执行器
//! bind-mount 进构建容器（官方 rust slim 镜像不自带 sccache）。
//!
//! 下载物来自 sccache 官方 GitHub release（musl 静态二进制，可在
//! Debian glibc 镜像内直接运行），缓存在数据目录，跨构建复用。
//!
//! **完整性**：下载后必须通过 SHA-256 校验才会被执行。构建服务会把这里的
//! 二进制当作 `RUSTC_WRAPPER` 注入到**用户的构建里**——未校验的下载等于
//! 把供应链信任交给一次不透明的 HTTP 响应。摘要取自 GitHub Release API 的
//! `digest` 字段（GitHub 侧计算的权威值），升级版本时同步更新下表。

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::process::Command;

/// 预置的 sccache 版本。
const SCCACHE_VERSION: &str = "0.18.0";
const CURL_RETRIES: u32 = 3;

/// 已固定摘要的 musl 资产：`(rust target, sha256 of the .tar.gz)`。
///
/// 取自 `https://api.github.com/repos/mozilla/sccache/releases/tags/v0.18.0`
/// 中各 asset 的 `digest` 字段。**新增/升级版本必须同步更新这里**——
/// 表里没有的架构一律拒绝下载，而不是「先跑起来再说」。
const PINNED_SHA256: &[(&str, &str)] = &[
    (
        "x86_64-unknown-linux-musl",
        "45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89",
    ),
    (
        "aarch64-unknown-linux-musl",
        "2b3284d5da3b46a47dc4229e75bb7b88ac4aa99c8d754fb7d2f84997e5a4354a",
    ),
];

/// 确保 Linux sccache 二进制就位；`arch` 为 daemon 报告的架构名。
/// 返回宿主机上的二进制绝对路径。
pub async fn ensure_sccache(tools_dir: &Path, arch: &str) -> Result<PathBuf, String> {
    let arch_name = match arch {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => {
            return Err(format!(
                "unsupported build architecture for sccache: {other}"
            ));
        }
    };
    let target = format!("{arch_name}-unknown-linux-musl");
    let Some(expected_sha256) = expected_sha256(&target) else {
        // 没有固定摘要就不下载：宁可没有缓存加速，也不要执行未校验的二进制。
        return Err(format!(
            "no pinned sha256 for sccache v{SCCACHE_VERSION} {target}; \
             refusing to download an unverified binary"
        ));
    };

    let dir = tools_dir.join(format!("sccache-{arch_name}-linux-musl"));
    let bin = dir.join("sccache");
    if bin.is_file() {
        return Ok(bin);
    }

    let asset = format!("sccache-v{SCCACHE_VERSION}-{target}");
    let url = format!(
        "https://github.com/mozilla/sccache/releases/download/v{SCCACHE_VERSION}/{asset}.tar.gz"
    );

    fs::create_dir_all(&dir).map_err(|e| format!("create tools dir: {e}"))?;
    let archive_path = tools_dir.join(format!(".{asset}.tar.gz"));

    let status = Command::new("curl")
        .args(["-fSL", "--retry", &CURL_RETRIES.to_string(), "-o"])
        .arg(&archive_path)
        .arg(&url)
        .status()
        .await
        .map_err(|e| format!("spawn curl failed: {e}"))?;
    if !status.success() {
        let _ = fs::remove_file(&archive_path);
        return Err(format!("download sccache from {url} failed: {status}"));
    }

    // 校验后再解包：未通过摘要校验的字节绝不进入执行路径。
    verify_sha256(&archive_path, expected_sha256)?;

    // 解包：压缩包内布局为 <asset>/sccache，只提取该二进制。
    // 用系统 tar 解包，避免为一次引导下载引入 tar/flate2 依赖
    // （Hotpot 的运行前提本就包含 docker 与 curl）。
    // 必须带 --strip-components 1：归档成员路径是 <asset>/sccache，
    // 不加会解成 dir/<asset>/sccache，而 bin 期望在 dir/sccache
    // （GNU/BSD tar 均支持该选项）。
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("--strip-components")
        .arg("1")
        .arg("-C")
        .arg(&dir)
        .arg(format!("{asset}/sccache"))
        .status()
        .await
        .map_err(|e| format!("spawn tar failed: {e}"))?;
    let _ = fs::remove_file(&archive_path);
    if !status.success() {
        return Err(format!(
            "extract {asset}/sccache from {url} failed: {status}"
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&bin)
            .map_err(|e| format!("stat sccache: {e}"))?
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        fs::set_permissions(&bin, perms).map_err(|e| format!("chmod sccache: {e}"))?;
    }
    Ok(bin)
}

/// 取某个 musl target 固定摘要。
fn expected_sha256(target: &str) -> Option<&'static str> {
    PINNED_SHA256
        .iter()
        .find(|(t, _)| *t == target)
        .map(|(_, sha)| *sha)
}

/// 校验下载文件的 SHA-256；不符则删除文件并返回错误。
fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected {
        let _ = fs::remove_file(path);
        return Err(format!(
            "sha256 mismatch for {}: expected {expected}, got {actual} (file removed)",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{expected_sha256, verify_sha256};
    use std::io::Write;

    #[test]
    fn pins_both_supported_arches() {
        assert!(expected_sha256("x86_64-unknown-linux-musl").is_some());
        assert!(expected_sha256("aarch64-unknown-linux-musl").is_some());
        // 未固定摘要的架构必须被拒绝，而不是放行。
        assert!(expected_sha256("riscv64gc-unknown-linux-musl").is_none());
    }

    #[test]
    fn rejects_digest_mismatch_and_removes_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"not the real sccache").unwrap();
        file.flush().unwrap();
        let path = file.path().to_path_buf();
        let err = verify_sha256(&path, &"0".repeat(64)).unwrap_err();
        assert!(err.contains("sha256 mismatch"), "{err}");
        assert!(!path.exists(), "校验失败的文件必须被删除");
    }

    #[test]
    fn accepts_matching_digest() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"payload").unwrap();
        file.flush().unwrap();
        // sha256("payload")
        let sha = "239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";
        verify_sha256(file.path(), sha).expect("digest should match");
    }
}
