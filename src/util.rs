pub mod git {
    //! 各種 Git 操作を行うモジュール

    use std::{path::Path, process::Output};

    use crate::error::MainResult;

    /// リポジトリが存在するかどうか
    pub async fn exists(dir: &Path) -> bool {
        matches!(
            tokio::fs::try_exists(dir.join(".git").join("HEAD")).await,
            Ok(true)
        )
    }

    /// リポジトリ初期化処理
    pub async fn init(repo: String, dir: &Path) -> MainResult {
        let _ = tokio::fs::remove_dir_all(dir.join(".git")).await;
        tokio::process::Command::new("git")
            .current_dir(dir)
            .arg("init")
            .spawn()?
            .wait()
            .await?;

        tokio::process::Command::new("git")
            .current_dir(dir)
            .arg("remote")
            .arg("add")
            .arg("origin")
            .arg(repo)
            .spawn()?
            .wait()
            .await?;
        Ok(())
    }

    /// リポジトリ同期処理
    pub async fn fetch(rev: &Option<String>, dir: &Path) -> MainResult {
        let rev: &[&str] = if let Some(rev) = rev { &[rev] } else { &[] };
        tokio::process::Command::new("git")
            .current_dir(dir)
            .arg("fetch")
            .arg("--depth=1")
            .arg("origin")
            .args(rev)
            .spawn()?
            .wait()
            .await?;

        tokio::process::Command::new("git")
            .current_dir(dir)
            .arg("switch")
            .arg("--detach")
            .arg("FETCH_HEAD")
            .spawn()?
            .wait()
            .await?;
        Ok(())
    }

    /// HEAD のハッシュ
    pub async fn head(dir: &Path) -> Option<Vec<u8>> {
        let Ok(Output { stdout, status, .. }) = tokio::process::Command::new("git")
            .current_dir(dir)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .await
        else {
            return None;
        };
        if status.success() { Some(stdout) } else { None }
    }

    /// diff の出力
    pub async fn diff(dir: &Path) -> Option<Vec<u8>> {
        let Ok(Output { stdout, status, .. }) = tokio::process::Command::new("git")
            .current_dir(dir)
            .arg("diff")
            .arg("HEAD")
            .output()
            .await
        else {
            return None;
        };
        if status.success() { Some(stdout) } else { None }
    }
}
