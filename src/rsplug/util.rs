use std::process::Output;

use tokio::process::Command;

use super::error::Error;

type ExecuteResult<T = Vec<u8>> = Result<T, Error>;

/// 外部コマンドを実行する
pub async fn execute(cmd: &mut Command) -> ExecuteResult {
    let Output {
        stdout,
        status,
        stderr,
    } = cmd.output().await?;
    if status.success() {
        Ok(stdout)
    } else {
        Err(Error::ProcessFailed { stderr })
    }
}

pub mod git {
    //! 各種 Git 操作を行うモジュール
    use std::path::Path;

    use super::*;

    /// リポジトリが存在するかどうか
    pub async fn exists(dir: &Path) -> bool {
        matches!(
            tokio::fs::try_exists(dir.join(".git").join("HEAD")).await,
            Ok(true)
        )
    }

    /// リポジトリ初期化処理
    pub async fn init(repo: &str, dir: &Path) -> ExecuteResult<()> {
        let _ = tokio::fs::remove_dir_all(dir.join(".git")).await;
        execute(Command::new("git").current_dir(dir).arg("init")).await?;

        execute(
            Command::new("git")
                .current_dir(dir)
                .arg("remote")
                .arg("add")
                .arg("origin")
                .arg(repo),
        )
        .await?;
        Ok(())
    }

    /// リポジトリ同期処理
    pub async fn fetch(rev: &Option<String>, dir: &Path) -> ExecuteResult<()> {
        execute(
            Command::new("git")
                .current_dir(dir)
                .arg("fetch")
                .arg("--depth=1")
                .arg("origin")
                .arg(rev.as_deref().unwrap_or("HEAD")),
        )
        .await?;

        execute(
            Command::new("git")
                .current_dir(dir)
                .arg("switch")
                .arg("--detach")
                .arg("FETCH_HEAD"),
        )
        .await?;
        Ok(())
    }

    /// HEAD のハッシュ
    pub async fn head(dir: &Path) -> ExecuteResult {
        execute(
            Command::new("git")
                .current_dir(dir)
                .arg("rev-parse")
                .arg("HEAD"),
        )
        .await
    }

    /// diff の出力
    pub async fn diff(dir: &Path) -> ExecuteResult {
        execute(Command::new("git").current_dir(dir).arg("diff").arg("HEAD")).await
    }
}
