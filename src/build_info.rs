//! 本程序是哪版代码编出来的（#742）：提交号由 `build.rs` 在编译时嵌入。
//!
//! 三处可见：启动日志一行（[`crate::engine::run_main`]）、pushgateway 指标 `hft_build_info{commit}`、
//! 程序文件本身 —— [`BUILD_ID`] 原样留在二进制里，不运行程序也能 `grep -ao 'hft-build:[0-9a-z-]*'`
//! 读出来（部署脚本换版前后就这样核对，避免为查版本去执行一个实盘程序）。

/// 12 位短提交号；编译时源码有未提交改动带 `-dirty`；编译环境拿不到 git 为 `unknown`
pub const GIT_COMMIT: &str = env!("HFT_GIT_COMMIT");

/// 带固定前缀的版本标识，供在二进制文件里检索
pub const BUILD_ID: &str = concat!("hft-build:", env!("HFT_GIT_COMMIT"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_is_short_sha_or_unknown() {
        let base = GIT_COMMIT.strip_suffix("-dirty").unwrap_or(GIT_COMMIT);
        assert!(
            base == "unknown" || (base.len() == 12 && base.bytes().all(|b| b.is_ascii_hexdigit())),
            "{GIT_COMMIT}"
        );
        assert_eq!(BUILD_ID, format!("hft-build:{GIT_COMMIT}"));
    }
}
