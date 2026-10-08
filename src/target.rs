//! 制品 **target-triple** 的识别，以及「能否装到本机」的判定。
//!
//! 无状态工具（`galaxy-ops` / `galaxy-flow`）的制品是**平台专用二进制包**，形如
//! `<name>-<version>-<target-triple>.tar.gz`。把 x86_64 的 ELF 覆盖到 Darwin arm64 的
//! Mach-O 上**不会报错** —— 工具会静默报废。所以 `tool-copy` 在解包覆盖前，先核这一道。
//!
//! 两条纪律（都是踩过的坑，见 CR-003）：
//!
//! - **整段精确比对**：架构名取自 `split('-')` 后的**整段**，用集合成员判定，**绝不用**
//!   `contains`。`"x86_64".contains("x86")` 为真，族匹配会让 32 位 x86 宿主放行 x86_64
//!   制品（反向误放行）；反过来，组件名里本就带架构词（`wist-arm-tool-1.2.3`）也不会被
//!   误当 target-triple（假阳性）。
//! - **与词表顺序无关**：命中与否只做集合成员判定，不依赖「`arm` 必须排在 `armv7` 之后」
//!   这类隐式约定 —— 词表顺序变了也不会静默出错。
//!
//! 解析口径与发布域的正本 `wist-release::package::parse_package_name` 同源；本 crate 不依赖
//! `wist-release`（它是另一个仓 / registry 依赖），故此处自持一份并各自加测。

use std::path::Component;

/// 已知的 target-triple 架构前缀（与 `wist-release::package::KNOWN_TRIPLE_ARCHES` 同表）。
///
/// 表的**顺序不重要**：判定只做整段集合成员测试，不做前缀 / 子串匹配。
pub const KNOWN_TRIPLE_ARCHES: &[&str] = &[
    "aarch64",
    "x86_64",
    "x86_64h",
    "i686",
    "i586",
    "arm",
    "armv7",
    "armv6",
    "riscv64",
    "powerpc64",
    "powerpc64le",
    "s390x",
    "loongarch64",
];

/// 本机 target（架构 + 操作系统 + 字节序）。
///
/// 显式化（而不是到处现读 `std::env::consts`）是为了**可测**：测试可以钉一个假宿主，
/// 断言「arm64 宿主拒收 x86_64 制品」这类判定，而不受跑测机器影响。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTarget {
    /// 本机架构（与 target-triple 首段同口径：`aarch64` / `x86_64` / `x86` / `arm` / …）。
    pub arch: String,
    /// 本机操作系统（`std::env::consts::OS` 口径：`macos` / `linux` / `windows` / …）。
    pub os: String,
    /// 本机字节序（`powerpc64` 的 `le` / `be` 是两个不同 target-triple，据此收窄）。
    pub little_endian: bool,
}

impl HostTarget {
    /// 显式构造（测试 / 交叉场景用）。
    pub fn new(arch: impl Into<String>, os: impl Into<String>, little_endian: bool) -> Self {
        Self {
            arch: arch.into(),
            os: os.into(),
            little_endian,
        }
    }

    /// 探测**当前运行机器**的 target。
    pub fn detect() -> Self {
        Self::new(
            std::env::consts::ARCH,
            std::env::consts::OS,
            cfg!(target_endian = "little"),
        )
    }

    /// `arch-os` 形式的可读描述（诊断 / 错误信息用）。
    pub fn describe(&self) -> String {
        format!("{}-{}", self.arch, self.os)
    }

    /// 本机对应的 **canonical target-triple**（如 `aarch64-apple-darwin` / `x86_64-unknown-linux-gnu`）。
    ///
    /// 供向中心**声明平台**用（中心据此挑平台匹配的制品下发地址）。Linux 的 abi 段取 `gnu`：
    /// 中心按**平台家族**比对（忽略 gnu/musl），所以静态 musl 制品也能被选中。认不出的 OS
    /// 返回 `None` —— 不猜，宁可不声明。
    pub fn target_triple(&self) -> Option<String> {
        let suffix = match self.os.as_str() {
            "macos" => "apple-darwin",
            "ios" => "apple-ios",
            "linux" => "unknown-linux-gnu",
            "android" => "linux-android",
            "windows" => "pc-windows-msvc",
            "freebsd" => "unknown-freebsd",
            "netbsd" => "unknown-netbsd",
            "openbsd" => "unknown-openbsd",
            "solaris" => "unknown-solaris",
            _ => return None,
        };
        Some(format!("{}-{suffix}", self.arch))
    }
}

/// 制品架构与本机一致性的判定结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchVerdict {
    /// 架构可校验，且与本机一致。
    Match,
    /// 架构可与本机**明确比对**且不符 —— 装上去会让工具报废，必须拒。
    Mismatch(String),
    /// 读不出 target-triple（内容寻址名 / 非平台包等）—— **无法校验**。
    Unverifiable(String),
}

impl ArchVerdict {
    /// 是否放行（仅 [`ArchVerdict::Match`]）。
    pub fn is_match(&self) -> bool {
        matches!(self, ArchVerdict::Match)
    }

    /// 拒 / 不可校验时的可读原因。
    pub fn reason(&self) -> Option<&str> {
        match self {
            ArchVerdict::Match => None,
            ArchVerdict::Mismatch(reason) | ArchVerdict::Unverifiable(reason) => Some(reason),
        }
    }
}

/// 判定某制品的 target-triple 能否装到本机。
///
/// `triple` 为 `None`（读不出架构）时返回 [`ArchVerdict::Unverifiable`] —— 「认不出」
/// 在策略层可以被放行（见配置 `upgrade_tool_require_arch`），但**绝不**在这里默默当成 match。
pub fn check_artifact_arch(triple: Option<&str>, host: &HostTarget) -> ArchVerdict {
    let Some(triple) = triple else {
        return ArchVerdict::Unverifiable(
            "制品名里读不出 target-triple：没有可校验的架构信息".to_string(),
        );
    };
    // 架构取 target-triple 的**首段**（整段精确比，绝不 contains）。
    let arch = triple.split('-').next().unwrap_or("");
    let Some(accepted) = accepted_arches(host) else {
        return ArchVerdict::Unverifiable(format!(
            "本机架构 `{}` 不在已知 target-triple 架构表里，无法校验制品 `{triple}`",
            host.arch
        ));
    };
    if !accepted.contains(&arch) {
        return ArchVerdict::Mismatch(format!(
            "制品架构 `{arch}` 与本机 `{}` 不符（装上去会静默报废工具）",
            host.describe()
        ));
    }
    match os_of_triple(triple) {
        Some(os) if os == host.os => ArchVerdict::Match,
        Some(os) => ArchVerdict::Mismatch(format!("制品操作系统 `{os}` 与本机 `{}` 不符", host.os)),
        None => ArchVerdict::Unverifiable(format!(
            "target-triple `{triple}` 里认不出操作系统（无法确认与本机 `{}` 一致）",
            host.os
        )),
    }
}

/// 本机架构**可接受**的制品架构段（整段精确比）。`None` = 本机架构不在已知表里。
fn accepted_arches(host: &HostTarget) -> Option<Vec<&'static str>> {
    Some(match host.arch.as_str() {
        "aarch64" => vec!["aarch64"],
        // 64 位 x86：haswell 变体（`x86_64h`）能在任意 x86_64 上跑，一并接受。
        "x86_64" => vec!["x86_64", "x86_64h"],
        // 32 位 x86：**绝不**接受 `x86_64`（`"x86_64".contains("x86")` 那个反向误放行的坑）。
        "x86" => vec!["i686", "i586"],
        // 32 位 arm：同族几个变体都接受（`armv6` 制品能在 `armv7` 上跑）。
        "arm" => vec!["arm", "armv7", "armv6"],
        "riscv64" => vec!["riscv64"],
        "s390x" => vec!["s390x"],
        "loongarch64" => vec!["loongarch64"],
        // powerpc64 的 le / be 是**不同**的 target-triple，按本机字节序收窄。
        "powerpc64" => {
            if host.little_endian {
                vec!["powerpc64le"]
            } else {
                vec!["powerpc64"]
            }
        }
        // 未知宿主架构：不猜，返回 None 让判定落 Unverifiable（交由策略层决定）。
        _ => return None,
    })
}

/// 从 target-triple 的**段**里认操作系统（整段比，不 contains）。
///
/// **从右往左**扫（取最右/最具体的平台词）：`aarch64-linux-android` 取 `android` 而非 `linux`；
/// `x86_64-unknown-linux-musl` 取 `linux`；`x86_64-pc-windows-msvc` 取 `windows`。
/// 认不出返回 `None`。
fn os_of_triple(triple: &str) -> Option<&'static str> {
    for segment in triple.split('-').rev() {
        match segment {
            "darwin" => return Some("macos"),
            "linux" => return Some("linux"),
            "windows" => return Some("windows"),
            "freebsd" => return Some("freebsd"),
            "netbsd" => return Some("netbsd"),
            "openbsd" => return Some("openbsd"),
            "android" => return Some("android"),
            "ios" => return Some("ios"),
            "solaris" | "illumos" => return Some("solaris"),
            _ => {}
        }
    }
    None
}

/// 从「制品字节 + 来源」里读 target-triple：**先包内顶层目录名，再回落来源末段**。
///
/// 包内目录名是发布域的正本（`<name>-<version>-<triple>/`）；来源末段是兜底
/// （内容寻址存盘（`pkg-<hash>`）时会带原名，但更常见的是不带 —— 那就读不出）。
pub fn artifact_triple(bytes: &[u8], source: &str) -> Option<String> {
    if let Some(dir) = first_tar_entry_component(bytes)
        && let Some(triple) = parse_artifact_triple(&dir)
    {
        return Some(triple);
    }
    parse_artifact_triple(source_basename(source))
}

/// 制品字节是否为**可解**的 tar.gz（首个条目里能读出一个普通路径段）。
///
/// 用来区分「不是 tar.gz」（交给解包报「解包失败」）与「是 tar.gz 但读不出身份」（架构不可校验）。
pub fn has_tar_entry(bytes: &[u8]) -> bool {
    first_tar_entry_component(bytes).is_some()
}

/// gzip + tar 解出首个条目路径的首个普通段（如顶层目录名 `gops-v0.18.2-aarch64-apple-darwin`）。
/// 任何一步失败都返回 `None`，绝不 panic。
pub fn first_tar_entry_component(bytes: &[u8]) -> Option<String> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut entries = archive.entries().ok()?;
    let entry = entries.next()?.ok()?;
    let path = entry.path().ok()?;
    // 跳过 `./` / `/` / `..` 之类非普通段，取第一个普通目录名 / 文件名。
    path.components().find_map(|component| match component {
        Component::Normal(name) => name.to_str().map(str::to_string),
        _ => None,
    })
}

/// 从形如 `<name>-<version>[-<target-triple>][<压缩后缀>]` 的串里切出 target-triple。
///
/// 切不出返回 `None`。tolerant：包名里本就带架构词（`wist-arm-tool-1.2.3`）不会误判。
pub fn parse_artifact_triple(name: &str) -> Option<String> {
    let name = strip_archive_suffix(name);
    triple_start(name).map(|index| name[index + 1..].to_string())
}

/// target-triple 起始的 `-` 下标（其后即三元组）。
///
/// 取**最靠前**的「其前还能切出版本」的候选。加后半个条件是为了躲开包名里本来就带架构词
/// 的假阳性：`wist-arm-stack-1.2.3` 的 `-arm-` 不会被当成三元组起头（`wist` 前切不出
/// 「像版本」的段），于是版本仍能切出 `1.2.3`、架构留空。
fn triple_start(name: &str) -> Option<usize> {
    for (index, _) in name.match_indices('-') {
        let candidate = &name[index + 1..];
        let arch_head = candidate.split('-').next().unwrap_or("");
        if KNOWN_TRIPLE_ARCHES.contains(&arch_head) && version_start(&name[..index]).is_some() {
            return Some(index);
        }
    }
    None
}

/// 第一个「像版本号」的段在串中的字节下标，用于跳过包名前缀。
fn version_start(name: &str) -> Option<usize> {
    let mut offset = 0;
    for segment in name.split('-') {
        if looks_like_version(segment) {
            return Some(offset);
        }
        offset += segment.len() + 1;
    }
    None
}

/// 段是否像版本号：可选 `v` 前缀 + 至少 `N.N`（`1234`、`2024-10` 这类不算）。
fn looks_like_version(segment: &str) -> bool {
    let rest = segment.strip_prefix(['v', 'V']).unwrap_or(segment);
    let mut parts = rest.split('.');
    let (Some(head), Some(second)) = (parts.next(), parts.next()) else {
        return false;
    };
    !head.is_empty()
        && head.chars().all(|ch| ch.is_ascii_digit())
        && second.chars().next().is_some_and(|ch| ch.is_ascii_digit())
}

/// 剥掉常见压缩 / 归档后缀（只剥一层，够用）。
fn strip_archive_suffix(name: &str) -> &str {
    for suffix in [
        ".tar.gz", ".tar.bz2", ".tar.xz", ".tgz", ".tar", ".gz", ".zip", ".bin",
    ] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            return stripped;
        }
    }
    name
}

/// 取来源的末段（路径 / URL 的文件名），剥掉查询串 / fragment 与尾斜杠。
///
/// crate 内共享：`tool_install` 据它推内容寻址摘要前缀（`pkg-<hex16>`）。
pub(crate) fn source_basename(source: &str) -> &str {
    let without_query = source.split(['?', '#']).next().unwrap_or(source);
    let trimmed = without_query.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_gz_with_entry(entry: &str, payload: &[u8]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, entry, payload)
                .expect("append tar entry");
            builder.finish().expect("finish tar");
        }
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_bytes).expect("gzip write");
        encoder.finish().expect("gzip finish")
    }

    #[test]
    fn parse_detects_a_trailing_triple() {
        for (name, expected) in [
            ("gops-v0.18.2-aarch64-apple-darwin", "aarch64-apple-darwin"),
            (
                "galaxy-flow-v0.16.1-alpha-x86_64-unknown-linux-musl.tar.gz",
                "x86_64-unknown-linux-musl",
            ),
            (
                "gx-0.15.1-x86_64-unknown-linux-gnu",
                "x86_64-unknown-linux-gnu",
            ),
            (
                "wist-arm-tool-1.2.3-armv7-unknown-linux-gnueabihf",
                "armv7-unknown-linux-gnueabihf",
            ),
        ] {
            assert_eq!(
                parse_artifact_triple(name).as_deref(),
                Some(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn parse_ignores_an_arch_word_inside_the_component_name() {
        // 假阳性防护：`-arm-` 只是组件名的一部分，不是 target-triple。
        for name in [
            "wist-arm-tool-1.2.3.tar.gz",
            "/opt/pkgs/wist-arm-stack-1.2.3.tar.gz",
            "https://x/wist-arm-stack-1.2.3.tar.gz",
        ] {
            assert_eq!(parse_artifact_triple(name), None, "{name}");
        }
    }

    #[test]
    fn parse_returns_none_without_a_version_or_triple() {
        for name in [
            "pkg-955e0dc75215c3a6",        // 内容寻址：无版本、无架构
            "wist-stack-2024-10.tar.gz",   // 裸日期段不算版本
            "wist-stack-build1234.tar.gz", // 无版本
            "gops",                        // 裸二进制名
            "thing.tar.gz",
        ] {
            assert_eq!(parse_artifact_triple(name), None, "{name}");
        }
    }

    #[test]
    fn artifact_triple_prefers_the_package_dir_then_falls_back_to_the_source() {
        let bytes = tar_gz_with_entry("gops-v0.18.2-aarch64-apple-darwin/gops", b"bin");
        // 包内目录名是正本。
        assert_eq!(
            artifact_triple(&bytes, "https://x/pkg-955e0dc75215c3a6").as_deref(),
            Some("aarch64-apple-darwin")
        );
        // 包内读不出（首条目不带身份）→ 回落来源末段。
        let plain = tar_gz_with_entry("pkg/gops", b"bin");
        assert_eq!(
            artifact_triple(
                &plain,
                "https://x/gops-v0.18.2-x86_64-unknown-linux-musl.tar.gz?sig=1"
            )
            .as_deref(),
            Some("x86_64-unknown-linux-musl")
        );
        // 两处都读不出 → None。
        assert_eq!(artifact_triple(&plain, "pkg-955e0dc75215c3a6"), None);
    }

    #[test]
    fn a_matching_arch_and_os_passes() {
        let host = HostTarget::new("aarch64", "macos", true);
        assert_eq!(
            check_artifact_arch(Some("aarch64-apple-darwin"), &host),
            ArchVerdict::Match
        );
        let linux = HostTarget::new("x86_64", "linux", true);
        // musl 与 gnu 都是合法的 Linux target（静态 musl 二进制在任何 glibc Linux 上跑）。
        for triple in ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"] {
            assert_eq!(
                check_artifact_arch(Some(triple), &linux),
                ArchVerdict::Match
            );
        }
    }

    #[test]
    fn a_mismatched_arch_is_rejected() {
        // 正是报告的破坏场景：arm64 宿主装到 x86_64 制品。
        let host = HostTarget::new("aarch64", "macos", true);
        let verdict = check_artifact_arch(Some("x86_64-unknown-linux-musl"), &host);
        assert!(matches!(verdict, ArchVerdict::Mismatch(_)), "{verdict:?}");
        assert!(verdict.reason().unwrap().contains("x86_64"), "{verdict:?}");
        // 反向：x86_64 宿主不接受 arm64 制品。
        let linux = HostTarget::new("x86_64", "linux", true);
        assert!(matches!(
            check_artifact_arch(Some("aarch64-unknown-linux-gnu"), &linux),
            ArchVerdict::Mismatch(_)
        ));
    }

    #[test]
    fn a_32_bit_x86_host_never_accepts_an_x86_64_artifact() {
        // 反向误放行防护：`"x86_64".contains("x86")` 为真，族匹配会误放行 —— 整段比就不会。
        let host = HostTarget::new("x86", "linux", true);
        assert!(matches!(
            check_artifact_arch(Some("x86_64-unknown-linux-gnu"), &host),
            ArchVerdict::Mismatch(_)
        ));
        // 真正 32 位的三元组才放行。
        assert_eq!(
            check_artifact_arch(Some("i686-unknown-linux-gnu"), &host),
            ArchVerdict::Match
        );
    }

    #[test]
    fn the_same_arch_but_a_different_os_is_rejected() {
        let host = HostTarget::new("aarch64", "macos", true);
        // 同为 aarch64，但制品是 Linux → 不能装到 macOS。
        assert!(matches!(
            check_artifact_arch(Some("aarch64-unknown-linux-musl"), &host),
            ArchVerdict::Mismatch(_)
        ));
        // macOS 上的 x86_64 制品（Rosetta 场景）依旧按错架构拒（本机是 arm64）。
        assert!(matches!(
            check_artifact_arch(Some("x86_64-apple-darwin"), &host),
            ArchVerdict::Mismatch(_)
        ));
    }

    #[test]
    fn powerpc64_variants_follow_endianness() {
        let be = HostTarget::new("powerpc64", "linux", false);
        assert_eq!(
            check_artifact_arch(Some("powerpc64-unknown-linux-gnu"), &be),
            ArchVerdict::Match
        );
        assert!(matches!(
            check_artifact_arch(Some("powerpc64le-unknown-linux-gnu"), &be),
            ArchVerdict::Mismatch(_)
        ));
        let le = HostTarget::new("powerpc64", "linux", true);
        assert_eq!(
            check_artifact_arch(Some("powerpc64le-unknown-linux-gnu"), &le),
            ArchVerdict::Match
        );
    }

    #[test]
    fn an_unreadable_or_unrecognized_triple_is_unverifiable() {
        let host = HostTarget::new("aarch64", "macos", true);
        assert!(matches!(
            check_artifact_arch(None, &host),
            ArchVerdict::Unverifiable(_)
        ));
        // 认得出架构、认不出操作系统 → 也不放行（不可校验）。
        assert!(matches!(
            check_artifact_arch(Some("aarch64-unknown-none"), &host),
            ArchVerdict::Unverifiable(_)
        ));
        // 未知宿主架构 → 不可校验（交由策略层）。
        let odd = HostTarget::new("sparc64", "linux", false);
        assert!(matches!(
            check_artifact_arch(Some("aarch64-apple-darwin"), &odd),
            ArchVerdict::Unverifiable(_)
        ));
    }

    #[test]
    fn has_tar_entry_distinguishes_non_tarballs() {
        let ok = tar_gz_with_entry("pkg/gops", b"bin");
        assert!(has_tar_entry(&ok));
        assert!(!has_tar_entry(b"this is not gzip"));
    }

    #[test]
    fn parse_handles_haswell_i686_and_prerelease_versions() {
        for (name, expected) in [
            ("gops-1.2.3-x86_64h-apple-darwin", "x86_64h-apple-darwin"),
            (
                "gops-1.2.3-i686-unknown-linux-gnu",
                "i686-unknown-linux-gnu",
            ),
            (
                "gx-v0.2.0-beta.1-x86_64-unknown-linux-gnu.tar.gz",
                "x86_64-unknown-linux-gnu",
            ),
        ] {
            assert_eq!(
                parse_artifact_triple(name).as_deref(),
                Some(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn parse_ignores_arch_words_in_the_name_without_a_version() {
        // 无版本号时，`-arm-` / `-armv7-` 段即便命中词表也不当三元组（不给假阳性留口）。
        for name in [
            "wist-armv7-agent",
            "wist-armv6-agent",
            "wist-arm-tool",
            "wist-x86_64-tool",
        ] {
            assert_eq!(parse_artifact_triple(name), None, "{name}");
        }
    }

    #[test]
    fn os_is_taken_from_the_rightmost_platform_segment() {
        // `aarch64-linux-android` 要认 android（最右），而不是先撞上的 linux。
        let android = HostTarget::new("aarch64", "android", true);
        assert_eq!(
            check_artifact_arch(Some("aarch64-linux-android"), &android),
            ArchVerdict::Match
        );
        // 桌面 Linux 上装 android 制品 → 不符。
        let linux = HostTarget::new("aarch64", "linux", true);
        assert!(matches!(
            check_artifact_arch(Some("aarch64-linux-android"), &linux),
            ArchVerdict::Mismatch(_)
        ));
        // Windows（gnu 与 msvc 都归 windows）。
        let windows = HostTarget::new("x86_64", "windows", true);
        for triple in ["x86_64-pc-windows-gnu", "x86_64-pc-windows-msvc"] {
            assert_eq!(
                check_artifact_arch(Some(triple), &windows),
                ArchVerdict::Match
            );
        }
    }

    #[test]
    fn an_aarch64_host_rejects_32_bit_arm_artifacts() {
        let host = HostTarget::new("aarch64", "macos", true);
        assert!(matches!(
            check_artifact_arch(Some("armv7-unknown-linux-gnueabihf"), &host),
            ArchVerdict::Mismatch(_)
        ));
        assert!(matches!(
            check_artifact_arch(Some("arm-unknown-linux-gnueabi"), &host),
            ArchVerdict::Mismatch(_)
        ));
    }

    #[test]
    fn an_x86_64_host_accepts_the_haswell_variant() {
        let host = HostTarget::new("x86_64", "macos", true);
        assert_eq!(
            check_artifact_arch(Some("x86_64h-apple-darwin"), &host),
            ArchVerdict::Match
        );
    }

    #[test]
    fn target_triple_names_the_platform_for_the_center() {
        // 声明给中心的 canonical 三元组：macOS arm64 / Linux x86_64 / Linux arm64。
        assert_eq!(
            HostTarget::new("aarch64", "macos", true)
                .target_triple()
                .as_deref(),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(
            HostTarget::new("x86_64", "linux", true)
                .target_triple()
                .as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
        // 认不出的 OS 不猜。
        assert_eq!(
            HostTarget::new("sparc64", "plan9", false).target_triple(),
            None
        );
    }
}
