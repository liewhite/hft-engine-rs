//! 编译时把 git 提交号嵌进程序（环境变量 `HFT_GIT_COMMIT`，见 `src/build_info.rs`），
//! 回答「线上跑的是哪版代码」不再靠文件时间和日志措辞猜（#742）。
//!
//! 取值：12 位短提交号；进入程序的源码（src、Cargo.toml、Cargo.lock、build.rs）有未提交改动时
//! 加 `-dirty`；拿不到 git（无 .git 的打包环境、没装 git）记 `unknown`，不让编译失败。
use std::process::Command;

/// 参与编译、决定 dirty 标记的路径
const BUILD_INPUTS: [&str; 4] = ["src", "Cargo.toml", "Cargo.lock", "build.rs"];

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    // 源码一改就重跑（dirty 标记随之更新）；提交、切分支则靠下面的 git 文件
    for p in BUILD_INPUTS {
        println!("cargo:rerun-if-changed={p}");
    }
    let commit = match git(&["rev-parse", "--short=12", "HEAD"]) {
        Some(c) if !c.is_empty() => c,
        _ => {
            println!("cargo:rustc-env=HFT_GIT_COMMIT=unknown");
            return;
        }
    };
    // HEAD（切分支）、index（add/commit）、当前分支的 ref 文件（commit）；worktree 下 HEAD/index
    // 在 git-dir，分支 ref 在 common-dir。ref 被打包进 packed-refs 时文件不存在，cargo 会每次重跑，
    // 只是多一次 git 调用，结果仍对
    if let (Some(dir), Some(common)) = (
        git(&["rev-parse", "--absolute-git-dir"]),
        git(&["rev-parse", "--git-common-dir"]),
    ) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
        if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
            // common-dir 可能是相对路径（相对当前目录 = 包根目录，cargo 解释 rerun-if-changed 也按包根）
            println!("cargo:rerun-if-changed={common}/{r}");
        }
    }
    // --no-optional-locks：status 默认会顺手回写 index，index 一变上面的监听又让下次编译重跑
    let mut status_args = vec!["--no-optional-locks", "status", "--porcelain", "--"];
    status_args.extend(BUILD_INPUTS);
    let dirty = match git(&status_args) {
        Some(s) => !s.is_empty(),
        None => true, // 判断不了按 dirty 记，宁可多报
    };
    let suffix = if dirty { "-dirty" } else { "" };
    println!("cargo:rustc-env=HFT_GIT_COMMIT={commit}{suffix}");
}
