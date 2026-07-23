//! Snapshot build hooks.
//!
//! Build execution is isolated from repository resolution/materialization. The
//! caller supplies the already ordered dependency runtime paths and this
//! module owns the bounded subprocess execution and diagnostics.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use super::*;

const BUILD_OUTPUT_TAIL_LINES: usize = 64;
const BUILD_OUTPUT_TAIL_LINE_BYTES: usize = 4096;

/// A bounded, merged stdout/stderr tail for diagnostics after a failed build.
/// Normal output still goes straight to the progress logger.
#[derive(Default)]
struct BuildOutputTail {
    lines: VecDeque<String>,
}

impl BuildOutputTail {
    fn push(&mut self, stdtype: usize, line: String) {
        let stream = if stdtype == 2 { "stderr" } else { "stdout" };
        let line: String = line.chars().take(BUILD_OUTPUT_TAIL_LINE_BYTES).collect();
        self.lines.push_back(format!("[{stream}] {line}"));
        if self.lines.len() > BUILD_OUTPUT_TAIL_LINES {
            self.lines.pop_front();
        }
    }

    fn display(&self) -> String {
        if self.lines.is_empty() {
            "(the build command produced no output)".to_string()
        } else {
            self.lines.iter().cloned().collect::<Vec<_>>().join("\n")
        }
    }
}

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
        let output = Arc::new(Mutex::new(BuildOutputTail::default()));
        let result: Result<(), Error> = {
            let id = id.clone();
            let build = build.to_vec();
            let output_for_progress = output.clone();
            let code = execute(build.iter(), workdir.clone(), move |(stdtype, line)| {
                if let Ok(mut output) = output_for_progress.lock() {
                    output.push(stdtype, line.clone());
                }
                msg(Message::CacheBuildProgress {
                    id: id.clone(),
                    stdtype,
                    line,
                });
            })
            .await?;
            if code != 0 {
                Err(Error::BuildScriptFailed {
                    code,
                    build,
                    repo: repo_name.clone(),
                    output: output
                        .lock()
                        .map(|output| output.display())
                        .unwrap_or_else(|_| "(build output was unavailable)".to_string()),
                })
            } else {
                Ok(())
            }
        };
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
