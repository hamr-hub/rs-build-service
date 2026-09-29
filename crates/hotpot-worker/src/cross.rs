//! 交叉编译供给：目标架构与构建机不同、或需要非默认 libc 时，在容器内
//! 安装交叉 C 工具链并配置 linker。
//!
//! 背景：`rustup target add` 只提供目标的 Rust 标准库；链接阶段仍由系统
//! cc 执行——aarch64 机的 cc 收到 `-m64` 直接报错，也没有 x86_64 的
//! glibc 开发库。因此按目标 triple 安装 `gcc-<triple>` 与
//! `libc6-dev-<arch>-cross`，并用 `CARGO_TARGET_<TRIPLE>_LINKER` 指向
//! 交叉 gcc。仅支持 Debian 系官方镜像（bookworm/trixie/bullseye）；
//! Alpine 与跨架构 musl 需要预烘焙镜像（如 cross-rs 镜像）。

/// 一次交叉供给的结果。
pub struct CrossProvision {
    /// 需要经 apt 安装的 Debian 包（空 = 无需系统包）。
    pub packages: Vec<String>,
    /// 注入构建的额外环境变量（linker / CC / CXX）。
    pub env: Vec<(String, String)>,
}

/// 宿主 C/C++ 构建工具链的供给包。
///
/// slim 镜像刻意不含 cc/make（为了体积），但大量真实 crate 的构建脚本
/// 直接调用它们（jemalloc-sys 跑 `make`；ring/openssl/libz-sys 跑 `cc`；
/// 还有 C++ 的 bindgen 配套 g++）。因此 slim 变体统一安装 build-essential
/// （gcc/g++/make/libc6-dev 的标准元包）；全量变体（bookworm/trixie…）
/// 已自带，返回空。
///
/// Alpine 不支持自动供给（apk 体系），直接报错——由调用方显式失败。
pub fn host_packages(image_tag: &str) -> Result<Vec<String>, String> {
    if image_tag.contains("alpine") {
        return Err(
            "automatic tool provisioning supports Debian-based rust images only; use a \
             *-slim-bookworm image or a prebaked Alpine image with build-base"
                .to_string(),
        );
    }
    if image_tag.contains("slim") {
        return Ok(vec!["build-essential".to_string()]);
    }
    Ok(Vec::new())
}

/// 计算 `target` triple 所需的交叉供给。
///
/// `host_arch` 为 daemon 架构（aarch64/x86_64），`image_tag` 为配置镜像
/// 的标签（用于识别 alpine）。
pub fn provision(host_arch: &str, image_tag: &str, target: &str) -> Result<CrossProvision, String> {
    let host = normalize_arch(host_arch)
        .ok_or_else(|| format!("cross compile: unsupported host architecture '{host_arch}'"))?;
    if image_tag.contains("alpine") {
        return Err(
            "automatic cross provisioning supports Debian-based rust images only; \
             use a *-slim-bookworm image or a prebaked cross image for Alpine"
                .to_string(),
        );
    }

    // wasm32：rustup 组件自带 rust-lld 与 wasi sysroot，不需要系统工具链。
    if target.starts_with("wasm32-") {
        return Ok(CrossProvision {
            packages: Vec::new(),
            env: Vec::new(),
        });
    }

    let parts: Vec<&str> = target.split('-').collect();
    if parts.len() < 2 {
        return Err(format!("invalid target triple '{target}'"));
    }
    let target_arch = normalize_arch(parts[0])
        .ok_or_else(|| format!("cross compile: unsupported target arch '{}'", parts[0]))?;
    let musl = parts.contains(&"musl");

    // 同架构 gnu：原生 cc 即可，无需任何供给。
    if target_arch == host && !musl {
        return Ok(CrossProvision {
            packages: Vec::new(),
            env: Vec::new(),
        });
    }
    // 同架构 musl：musl-gcc 包装器（Debian: musl-tools）。
    if target_arch == host {
        return Ok(CrossProvision {
            packages: vec!["musl-tools".to_string()],
            env: vec![
                env_target(target, "LINKER", "musl-gcc"),
                env_cc(target, "musl-gcc"),
            ],
        });
    }

    // 跨架构 musl：Debian 无 musl-cross 包，需要预烘焙镜像，不做半成品支持。
    if musl {
        return Err(format!(
            "cross-architecture musl target '{target}' cannot be auto-provisioned; \
             use a prebaked cross image (e.g. cross-rs) or a gnu target"
        ));
    }

    // 跨架构 gnu：gcc/g++ 交叉包 + 交叉 sysroot。
    let (gcc_pkg, gxx_pkg, libc_pkg, linker, linkerxx) = match (host, target_arch) {
        ("aarch64", "x86_64") => (
            "gcc-x86-64-linux-gnu",
            "g++-x86-64-linux-gnu",
            "libc6-dev-amd64-cross",
            "x86_64-linux-gnu-gcc",
            "x86_64-linux-gnu-g++",
        ),
        ("x86_64", "aarch64") => (
            "gcc-aarch64-linux-gnu",
            "g++-aarch64-linux-gnu",
            "libc6-dev-arm64-cross",
            "aarch64-linux-gnu-gcc",
            "aarch64-linux-gnu-g++",
        ),
        _ => {
            return Err(format!(
                "cross compile from {host} to {target_arch} is not auto-provisioned"
            ));
        }
    };
    Ok(CrossProvision {
        packages: vec![
            gcc_pkg.to_string(),
            gxx_pkg.to_string(),
            libc_pkg.to_string(),
        ],
        env: vec![
            env_target(target, "LINKER", linker),
            env_cc(target, linker),
            env_cxx(target, linkerxx),
        ],
    })
}

/// 构建容器入口脚本中「装系统包 + 准备目标 rustlib」的部分；不含 cargo。
/// 返回空串表示无需预处理（调用方直接 exec cargo）。
///
/// rustlib 缓存：`rustup target add` 需联网下载且在慢网络下耗时数分钟，
/// 把目标 rustlib 按 **rustc commit + triple** 缓存在挂载卷上，warm 构建
/// 直接本地拷贝。必须按 rustc 版本分桶——混用不同版本的 std 与 rustc
/// 会破坏版本一致性。
pub fn setup_script(packages: &[String], rustlib: Option<(&str, bool)>) -> String {
    let mut steps: Vec<String> = Vec::new();
    if !packages.is_empty() {
        // apt 的 deb 归档与索引列表都在慢网络下代价高昂，二者均持久化挂载：
        // - 官方镜像 docker-clean（DPkg::Post-Invoke）装完即 rm 所有 .deb，
        //   -o 空值无法清除该列表钩子（实测仍删除），直接删掉该配置最确定；
        //   warm 构建 apt 校验归档完整即跳过下载。
        // - 已有 *_InRelease 即跳过 apt-get update。
        // 同一部署内应固定发行版（不混用 bookworm/trixie 镜像），否则共享
        // lists 会互相覆盖。
        // 已安装的包整段跳过（含 apt-get update）——warm 构建零 apt 开销；
        // 否则每个 slim 构建都要走一次 dpkg/apt，慢网络下还可能被拖死。
        let check = packages
            .iter()
            .map(|p| {
                format!(
                    "dpkg-query -W -f='${{Status}}' {p} 2>/dev/null \
                     | grep -q 'install ok installed' || missing=\"$missing {p}\";"
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        steps.push(format!(
            "missing=\"\"; {check} if [ -n \"$missing\" ]; then \
             mkdir -p /var/cache/apt/archives/partial /var/lib/apt/lists/partial && \
             rm -f /etc/apt/apt.conf.d/docker-clean && \
             rm -f /var/cache/apt/archives/lock /var/lib/apt/lists/lock && \
             ls /var/lib/apt/lists/*_InRelease >/dev/null 2>&1 || apt-get -qq update; \
             DEBIAN_FRONTEND=noninteractive apt-get install -y \
             --no-install-recommends $missing; fi"
        ));
    }

    if let Some((target, cache)) = rustlib {
        // target 已在 API 边界做字符白名单校验（字母数字 + -_.），可直接嵌入
        // shell 文本；这里再断言一次，防止未来放宽校验后形成 shell 注入。
        debug_assert!(
            target
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        );
        if cache {
            // commit-hash 对每个发布版本唯一；缓存键 = <commit>-<triple>。
            // 注意 target 必须以**原文**进入双引号路径——shell 引号字符在变量值/
            // 双引号里都是普通字符，曾因此生成名为 `'triple'` 的路径导致 cp 失败。
            steps.push(format!(
                "vhash=$(rustc -vV | sed -n 's/^commit-hash: //p'); \
                 sysroot=$(rustc --print sysroot); \
                 dst=\"$sysroot/lib/rustlib/{target}\"; \
                 cdir=\"/opt/rustlib-cache/$vhash-{target}\"; \
                 if [ ! -d \"$dst/lib\" ]; then \
                   if [ -d \"$cdir/lib\" ]; then cp -a \"$cdir/.\" \"$dst/\"; \
                   else rustup target add {target} && mkdir -p \"$cdir\" && \
                        cp -a \"$dst/.\" \"$cdir/\"; fi; \
                 fi"
            ));
        } else {
            steps.push(format!("rustup target add {target}"));
        }
    }
    steps.join(" && ")
}

/// `CARGO_TARGET_<TRIPLE>_<SUFFIX>` 变量。
fn env_target(target: &str, suffix: &str, value: &str) -> (String, String) {
    (
        format!("CARGO_TARGET_{}_{suffix}", triple_env(target)),
        value.to_string(),
    )
}

/// `CC_<TRIPLE>`：C 依赖的 build script 据此选择交叉编译器。
fn env_cc(target: &str, value: &str) -> (String, String) {
    (format!("CC_{}", triple_env(target)), value.to_string())
}

/// `CXX_<TRIPLE>`。
fn env_cxx(target: &str, value: &str) -> (String, String) {
    (format!("CXX_{}", triple_env(target)), value.to_string())
}

/// triple → 环境变量段：大写、非字母数字统一为 `_`。
fn triple_env(target: &str) -> String {
    target
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn normalize_arch(raw: &str) -> Option<&'static str> {
    match raw {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{provision, setup_script};

    #[test]
    fn same_arch_gnu_needs_nothing() {
        let p = provision("aarch64", "slim-bookworm", "aarch64-unknown-linux-gnu").unwrap();
        assert!(p.packages.is_empty());
        assert!(p.env.is_empty());
        let p = provision("x86_64", "1.98-slim", "x86_64-unknown-linux-gnu").unwrap();
        assert!(p.packages.is_empty());
    }

    #[test]
    fn same_arch_musl_uses_musl_tools() {
        let p = provision("aarch64", "slim-bookworm", "aarch64-unknown-linux-musl").unwrap();
        assert_eq!(p.packages, vec!["musl-tools"]);
        assert!(p.env.iter().any(
            |(k, v)| k == "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER" && v == "musl-gcc"
        ));
    }

    #[test]
    fn cross_gnu_amd64_on_arm64() {
        let p = provision("aarch64", "slim-bookworm", "x86_64-unknown-linux-gnu").unwrap();
        assert!(p.packages.contains(&"gcc-x86-64-linux-gnu".to_string()));
        assert!(p.packages.contains(&"libc6-dev-amd64-cross".to_string()));
        assert!(
            p.env
                .iter()
                .any(|(k, v)| k == "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER"
                    && v == "x86_64-linux-gnu-gcc")
        );
        assert!(
            p.env
                .iter()
                .any(|(k, v)| k == "CC_X86_64_UNKNOWN_LINUX_GNU" && v == "x86_64-linux-gnu-gcc")
        );
    }

    #[test]
    fn cross_gnu_arm64_on_amd64() {
        let p = provision("x86_64", "bookworm", "aarch64-unknown-linux-gnu").unwrap();
        assert!(p.packages.contains(&"gcc-aarch64-linux-gnu".to_string()));
        assert!(p.packages.contains(&"libc6-dev-arm64-cross".to_string()));
        assert!(p.env.iter().any(
            |(k, v)| k == "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER"
                && v == "aarch64-linux-gnu-gcc"
        ));
    }

    #[test]
    fn cross_musl_is_rejected() {
        assert!(provision("aarch64", "slim-bookworm", "x86_64-unknown-linux-musl").is_err());
    }

    #[test]
    fn wasm_needs_no_system_toolchain() {
        let p = provision("aarch64", "slim-bookworm", "wasm32-unknown-unknown").unwrap();
        assert!(p.packages.is_empty());
        let p = provision("aarch64", "slim-bookworm", "wasm32-wasip1").unwrap();
        assert!(p.packages.is_empty());
    }

    #[test]
    fn alpine_and_unknown_targets_are_rejected() {
        assert!(provision("aarch64", "alpine", "x86_64-unknown-linux-gnu").is_err());
        assert!(provision("aarch64", "slim-bookworm", "riscv64gc-unknown-linux-gnu").is_err());
        assert!(provision("aarch64", "slim-bookworm", "nonsense").is_err());
    }

    #[test]
    fn host_packages_match_image_variant() {
        assert_eq!(
            super::host_packages("slim-bookworm").unwrap(),
            vec!["build-essential"]
        );
        assert_eq!(
            super::host_packages("1.85-slim").unwrap(),
            vec!["build-essential"]
        );
        // 全量镜像已自带 C 工具链。
        assert!(super::host_packages("bookworm").unwrap().is_empty());
        assert!(super::host_packages("1.98-bookworm").unwrap().is_empty());
        // Alpine 无法自动供给。
        assert!(super::host_packages("alpine").is_err());
        assert!(super::host_packages("1.98-alpine").is_err());
    }

    #[test]
    fn script_includes_packages_and_version_keyed_rustlib_cache() {
        let s = setup_script(
            &["gcc-x86-64-linux-gnu".to_string()],
            Some(("x86_64-unknown-linux-gnu", true)),
        );
        assert!(s.contains("apt-get install"));
        assert!(s.contains("/opt/rustlib-cache/$vhash-"));
        assert!(s.contains("rustup target add"));

        let s = setup_script(&[], Some(("wasm32-unknown-unknown", false)));
        assert_eq!(s, "rustup target add wasm32-unknown-unknown");

        // 无包、无目标：无需预处理。
        assert_eq!(setup_script(&[], None), "");
    }
}
