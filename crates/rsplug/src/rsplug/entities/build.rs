//! Snapshot build hooks.
//!
//! Build execution is isolated from repository resolution/materialization. The
//! caller supplies the already ordered dependency runtime paths and this
//! module owns the bounded subprocess execution and diagnostics.

use super::*;

pub(super) async fn run_repo_build(
    build: &[String],
    lua_build: Option<&str>,
    workdir: Arc<Path>,
    runtimepaths: Vec<PathBuf>,
    logid: &str,
    repo_name: &Arc<str>,
) -> Result<(), Error> {
    use crate::{
        log::{Message, msg},
        rsplug::util::execute,
    };

    let _build = super::util::resources::build().await?;
    if !build.is_empty() {
        let id = Arc::new(format!("{logid} (sh)"));
        let result: Result<(), Error> = {
            let id = id.clone();
            let build = build.to_vec();
            async {
                let code = execute(build.iter(), workdir.clone(), move |(stdtype, line)| {
                    msg(Message::CacheBuildProgress {
                        id: id.clone(),
                        stdtype,
                        line,
                    });
                })
                .await?;
                if code != 0 {
                    return Err(Error::BuildScriptFailed {
                        code,
                        build,
                        repo: repo_name.clone(),
                    });
                }
                Ok(())
            }
        }
        .await;
        msg(Message::CacheBuildFinished {
            id,
            success: result.is_ok(),
        });
        result?;
    }

    if let Some(lua_build) = lua_build {
        let id = Arc::new(format!("{logid} (lua)"));
        let result: Result<(), Error> = {
            let id = id.clone();
            async {
                let lua_build_path = create_lua_build_script(lua_build, &runtimepaths).await?;
                let code = execute(
                    lua_build_nvim_command(lua_build_path.as_os_str()),
                    workdir.clone(),
                    move |(stdtype, line)| {
                        msg(Message::CacheBuildProgress {
                            id: id.clone(),
                            stdtype,
                            line,
                        });
                    },
                )
                .await;
                let _ = tokio::fs::remove_file(&lua_build_path).await;
                let code = code?;
                if code != 0 {
                    return Err(Error::BuildLuaScriptFailed {
                        code,
                        repo: repo_name.clone(),
                    });
                }
                Ok(())
            }
        }
        .await;
        msg(Message::CacheBuildFinished {
            id,
            success: result.is_ok(),
        });
        result?;
    }
    Ok(())
}
