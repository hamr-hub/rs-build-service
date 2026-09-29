//! Rust 工具链选择：解析用户指定的 channel，生成 rustup 工具链名与
//! Docker 官方镜像标签。
//!
//! 接受的写法（大小写不敏感）：
//! - `stable` / `beta` / `nightly`；
//! - `nightly-YYYY-MM-DD`（带日期的 nightly 快照）；
//! - `1.98`（两段式，补丁版本由 rustup / 镜像解析为最新）；
//! - `1.98.0`（精确版本）。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::BuildProfile;
use crate::{Error, Result};

/// 工具链通道。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolchainChannel {
    Stable,
    Beta,
    Nightly,
    /// 语义化版本；minor/patch 可为 None（两段/一段式写法）。
    Version {
        major: u32,
        minor: Option<u32>,
        patch: Option<u32>,
    },
}

/// 一次工具链请求：通道 + 可选的 nightly 日期。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainRequest {
    pub channel: ToolchainChannel,
    /// nightly 快照日期（YYYY-MM-DD）。
    pub date: Option<String>,
}

impl ToolchainRequest {
    /// rustup 可识别的工具链名（如 `stable`、`nightly-2026-01-15`、`1.98.0`）。
    pub fn rustup_spec(&self) -> String {
        match &self.channel {
            ToolchainChannel::Stable => "stable".to_string(),
            ToolchainChannel::Beta => "beta".to_string(),
            ToolchainChannel::Nightly => match &self.date {
                Some(date) => format!("nightly-{date}"),
                None => "nightly".to_string(),
            },
            ToolchainChannel::Version {
                major,
                minor,
                patch,
            } => {
                let mut s = major.to_string();
                if let Some(minor) = minor {
                    s.push('.');
                    s.push_str(&minor.to_string());
                    if let Some(patch) = patch {
                        s.push('.');
                        s.push_str(&patch.to_string());
                    }
                }
                s
            }
        }
    }

    /// 作为 cargo 代理参数（`cargo +<spec>`）。
    pub fn cargo_plus_arg(&self) -> String {
        format!("+{}", self.rustup_spec())
    }

    /// Docker 官方 `rust` 镜像标签，如 `1.98.0-slim-bookworm`。
    /// `variant` 为标签变体（`slim-bookworm` / `bookworm` / `slim` …）。
    ///
    /// Docker Hub 的官方 rust 镜像**只发布版本号标签与跟踪 stable 的浮动标签**
    /// （`slim-bookworm`、`bookworm`、`latest` 等），不存在任何 beta/nightly
    /// 标签（含带日期的 nightly）——已通过 Hub 标签列表核实。因此：
    /// - 版本号 spec → `rust:{spec}-{variant}`；
    /// - `stable` → 浮动标签 `rust:{variant}`（其内容即当前 stable）；
    /// - beta/nightly → 显式报错，提示改用版本号或 Local 执行器，
    ///   而不是静默用一个标签不存在的镜像或错误的工具链。
    pub fn docker_image(&self, variant: &str) -> std::result::Result<String, String> {
        match &self.channel {
            ToolchainChannel::Version { .. } => {
                Ok(format!("rust:{}-{variant}", self.rustup_spec()))
            }
            ToolchainChannel::Stable => Ok(format!("rust:{variant}")),
            ToolchainChannel::Beta | ToolchainChannel::Nightly => Err(format!(
                "official docker rust images publish no '{}' channel tag (no beta/nightly tags, \
                 dated nightly included); pin a version number (e.g. 1.98.0) or use the local \
                 executor",
                self.rustup_spec()
            )),
        }
    }

    /// 是否为可能不稳定的通道（beta/nightly）。
    pub fn is_prerelease(&self) -> bool {
        matches!(
            self.channel,
            ToolchainChannel::Beta | ToolchainChannel::Nightly
        )
    }
}

impl fmt::Display for ToolchainRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.rustup_spec())
    }
}

/// 解析工具链字符串。
pub fn parse_toolchain(raw: &str) -> Result<ToolchainRequest> {
    let spec = raw.trim().to_lowercase();
    if spec.is_empty() {
        return Err(Error::Invalid("toolchain must not be empty".to_string()));
    }

    // nightly-YYYY-MM-DD
    if let Some(date) = spec.strip_prefix("nightly-") {
        validate_date(date)?;
        return Ok(ToolchainRequest {
            channel: ToolchainChannel::Nightly,
            date: Some(date.to_string()),
        });
    }

    let channel = match spec.as_str() {
        "stable" => ToolchainChannel::Stable,
        "beta" => ToolchainChannel::Beta,
        "nightly" => ToolchainChannel::Nightly,
        other => {
            let version = parse_version(other)?;
            ToolchainChannel::Version {
                major: version.0,
                minor: version.1,
                patch: version.2,
            }
        }
    };
    Ok(ToolchainRequest {
        channel,
        date: None,
    })
}

/// 按目标工具链解析配置镜像，得到实际使用的官方 `rust` 镜像引用。
///
/// `rust:1.98-slim-bookworm` + `1.85` → `rust:1.85-slim-bookworm`；
/// `rust:slim-bookworm` + `stable` → `rust:slim-bookworm`（浮动 stable）。
/// 非官方镜像、缺标签、或请求通道没有官方标签（beta/nightly）时返回 Err，
/// 由调用方显式报错，而不是静默用错工具链。
pub fn resolve_rust_image(
    image: &str,
    toolchain: &ToolchainRequest,
) -> std::result::Result<String, String> {
    let (repo, tag) = image
        .split_once(':')
        .ok_or_else(|| format!("image '{image}' is not an official rust image (missing tag)"))?;
    if repo != "rust" {
        return Err(format!(
            "image '{image}' is not an official rust image; pass --docker-image to pin a matching \
             image"
        ));
    }
    // 变体推导必须只剥离**工具链版本段**：`slim-bookworm` 首段 "slim" 不是版本，
    // 按首个 '-' 切会误得变体 "bookworm"（拉成无 slim 的数 GB 全量镜像）。
    let spec_prefix = format!("{}-", toolchain.rustup_spec());
    let variant = tag
        .strip_prefix(&spec_prefix)
        .unwrap_or_else(|| match tag.split_once('-') {
            Some((head, rest)) if parse_toolchain(head).is_ok() => rest,
            _ => tag,
        });
    toolchain.docker_image(variant)
}

/// 解析 X / X.Y / X.Y.Z 形式的版本号。
fn parse_version(s: &str) -> Result<(u32, Option<u32>, Option<u32>)> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 3 {
        return Err(Error::Invalid(format!(
            "invalid rust version '{s}': expected MAJOR[.MINOR[.PATCH]]"
        )));
    }
    let mut nums = Vec::with_capacity(3);
    for part in parts {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return Err(Error::Invalid(format!(
                "invalid rust version '{s}': components must be integers"
            )));
        }
        nums.push(part.parse::<u32>().map_err(|_| {
            Error::Invalid(format!(
                "invalid rust version '{s}': component out of range"
            ))
        })?);
    }
    Ok((nums[0], nums.get(1).copied(), nums.get(2).copied()))
}

/// 校验 YYYY-MM-DD 日期形状（不验证日历正确性）。
fn validate_date(date: &str) -> Result<()> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3
        || parts[0].len() != 4
        || parts[1].len() != 2
        || parts[2].len() != 2
        || !parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
    {
        return Err(Error::Invalid(format!(
            "invalid nightly date '{date}': expected YYYY-MM-DD"
        )));
    }
    Ok(())
}

/// 单个字段长度上限（features / flags / target 等，防御超长输入）。
const MAX_FIELD_LEN: usize = 256;
/// 单次构建允许的 features / 附加参数条数上限。
const MAX_ITEMS: usize = 64;

/// 在系统边界校验构建 profile：工具链写法、target 三元组形状、列表规模。
pub fn validate_profile(profile: &BuildProfile) -> Result<()> {
    if let Some(raw) = &profile.toolchain {
        parse_toolchain(raw)?;
    }
    if let Some(target) = &profile.target {
        validate_target(target)?;
    }
    validate_items("features", &profile.features)?;
    validate_items("cargo_flags", &profile.cargo_flags)?;
    Ok(())
}

/// 校验 rustc target 三元组的基本形状（至少两段、合法字符）。
fn validate_target(target: &str) -> Result<()> {
    if target.is_empty() || target.len() > MAX_FIELD_LEN {
        return Err(Error::Invalid(format!(
            "target triple must be 1-{MAX_FIELD_LEN} chars"
        )));
    }
    let valid = target.split('-').count() >= 2
        && target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid {
        return Err(Error::Invalid(format!("invalid target triple '{target}'")));
    }
    Ok(())
}

fn validate_items(name: &str, items: &[String]) -> Result<()> {
    if items.len() > MAX_ITEMS {
        return Err(Error::Invalid(format!("too many {name} (max {MAX_ITEMS})")));
    }
    if items
        .iter()
        .any(|i| i.is_empty() || i.len() > MAX_FIELD_LEN)
    {
        return Err(Error::Invalid(format!(
            "{name} entries must be 1-{MAX_FIELD_LEN} chars"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channels() {
        assert_eq!(
            parse_toolchain("stable").unwrap().channel,
            ToolchainChannel::Stable
        );
        assert_eq!(
            parse_toolchain("BETA").unwrap().channel,
            ToolchainChannel::Beta
        );
        assert_eq!(
            parse_toolchain(" nightly ").unwrap().channel,
            ToolchainChannel::Nightly
        );
    }

    #[test]
    fn parses_versions() {
        let req = parse_toolchain("1.98").unwrap();
        assert_eq!(
            req.channel,
            ToolchainChannel::Version {
                major: 1,
                minor: Some(98),
                patch: None,
            }
        );
        let req = parse_toolchain("1.98.0").unwrap();
        assert_eq!(
            req.channel,
            ToolchainChannel::Version {
                major: 1,
                minor: Some(98),
                patch: Some(0),
            }
        );
        let req = parse_toolchain("42").unwrap();
        assert_eq!(
            req.channel,
            ToolchainChannel::Version {
                major: 42,
                minor: None,
                patch: None,
            }
        );
    }

    #[test]
    fn parses_dated_nightly() {
        let req = parse_toolchain("nightly-2026-01-15").unwrap();
        assert_eq!(req.channel, ToolchainChannel::Nightly);
        assert_eq!(req.date.as_deref(), Some("2026-01-15"));
        assert_eq!(req.rustup_spec(), "nightly-2026-01-15");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_toolchain("").is_err());
        assert!(parse_toolchain("rust").is_err());
        assert!(parse_toolchain("1.98.x").is_err());
        assert!(parse_toolchain("1.2.3.4").is_err());
        assert!(parse_toolchain("nightly-2026-1-15").is_err());
        assert!(parse_toolchain("-1.0").is_err());
    }

    #[test]
    fn builds_rustup_and_docker_names() {
        assert_eq!(parse_toolchain("1.98.0").unwrap().rustup_spec(), "1.98.0");
        assert_eq!(parse_toolchain("1.98").unwrap().cargo_plus_arg(), "+1.98");
        // stable 映射到跟踪 stable 的浮动标签（官方不发布 stable-slim-bookworm）。
        assert_eq!(
            parse_toolchain("stable")
                .unwrap()
                .docker_image("slim-bookworm")
                .unwrap(),
            "rust:slim-bookworm"
        );
        assert_eq!(
            parse_toolchain("1.98.0")
                .unwrap()
                .docker_image("slim-bookworm")
                .unwrap(),
            "rust:1.98.0-slim-bookworm"
        );
        // beta/nightly 无任何官方镜像标签，必须显式报错。
        assert!(
            parse_toolchain("nightly")
                .unwrap()
                .docker_image("slim-bookworm")
                .is_err()
        );
        assert!(
            parse_toolchain("nightly-2026-01-15")
                .unwrap()
                .docker_image("slim-bookworm")
                .is_err()
        );
        assert!(
            parse_toolchain("beta")
                .unwrap()
                .docker_image("bookworm")
                .is_err()
        );
    }

    #[test]
    fn resolves_rust_image_variants() {
        fn resolve(image: &str, spec: &str) -> std::result::Result<String, String> {
            resolve_rust_image(image, &parse_toolchain(spec).unwrap())
        }
        // `slim-bookworm` 无版本段，曾被误切成 "bookworm"。
        assert_eq!(
            resolve("rust:slim-bookworm", "1.85").unwrap(),
            "rust:1.85-slim-bookworm"
        );
        assert_eq!(
            resolve("rust:bookworm", "1.98").unwrap(),
            "rust:1.98-bookworm"
        );
        assert_eq!(
            resolve("rust:1.98-slim-bookworm", "1.85").unwrap(),
            "rust:1.85-slim-bookworm"
        );
        // stable 只能落到跟踪 stable 的浮动标签。
        assert_eq!(
            resolve("rust:slim-bookworm", "stable").unwrap(),
            "rust:slim-bookworm"
        );
        assert_eq!(
            resolve("rust:1.98-slim-bookworm", "stable").unwrap(),
            "rust:slim-bookworm"
        );
        // beta/nightly 无官方标签；非官方仓库/缺标签显式拒绝。
        assert!(resolve("rust:slim-bookworm", "beta").is_err());
        assert!(resolve("rust:slim-bookworm", "nightly").is_err());
        assert!(resolve("rust:slim-bookworm", "nightly-2026-01-15").is_err());
        assert!(resolve("myregistry/rust:slim", "1.85").is_err());
        assert!(resolve("rust", "1.85").is_err());
    }

    #[test]
    fn validates_profiles() {
        let mut profile = BuildProfile::default();
        assert!(validate_profile(&profile).is_ok());

        profile.toolchain = Some("1.85.0".to_string());
        profile.target = Some("aarch64-unknown-linux-gnu".to_string());
        assert!(validate_profile(&profile).is_ok());

        profile.toolchain = Some("bogus".to_string());
        assert!(validate_profile(&profile).is_err());

        profile.toolchain = None;
        profile.target = Some("noseparator".to_string());
        assert!(validate_profile(&profile).is_err());
    }
}
