use std::borrow::Cow;
use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use tokio::fs::{self, ReadDir};

#[derive(Debug)]
pub(crate) struct DirectoryTask {
    pub(crate) absolute_path: PathBuf,
    pub(crate) relative_path: String,
}

#[derive(Debug)]
pub(crate) enum DirectoryScanResult {
    ChildDirectory(DirectoryTask),
    File(FileEntry),
}

#[derive(Debug)]
pub(crate) struct FileEntry {
    pub(crate) absolute_path: PathBuf,
    pub(crate) relative_path: String,
}

pub(crate) struct DirectoryScanStream {
    directory_reader: ReadDir,
    relative_path: String,
}

impl DirectoryTask {
    pub async fn stream(self: DirectoryTask) -> io::Result<DirectoryScanStream> {
        let directory_reader = fs::read_dir(self.absolute_path.as_path()).await?;
        Ok(DirectoryScanStream {
            directory_reader,
            relative_path: self.relative_path,
        })
    }
}

impl DirectoryScanStream {
    pub async fn next(&mut self) -> io::Result<Option<DirectoryScanResult>> {
        while let Some(entry) = self.directory_reader.next_entry().await? {
            let file_name = entry.file_name();
            let entry_name = os_str_to_utf8(file_name.as_os_str());
            let relative_path = join_relative_path(&self.relative_path, entry_name.as_ref());
            let absolute_path = entry.path();
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                return Ok(Some(DirectoryScanResult::ChildDirectory(DirectoryTask {
                    absolute_path,
                    relative_path,
                })));
            }

            if file_type.is_file() {
                return Ok(Some(DirectoryScanResult::File(FileEntry {
                    absolute_path,
                    relative_path,
                })));
            }

            if !file_type.is_symlink() {
                continue;
            }

            match fs::metadata(&absolute_path).await {
                Ok(metadata) if metadata.is_dir() => {
                    return Ok(Some(DirectoryScanResult::ChildDirectory(DirectoryTask {
                        absolute_path,
                        relative_path,
                    })));
                }
                Ok(metadata) if metadata.is_file() => {
                    return Ok(Some(DirectoryScanResult::File(FileEntry {
                        absolute_path,
                        relative_path,
                    })));
                }
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}

fn join_relative_path(base: &str, child_name: &str) -> String {
    let normalized_child = if child_name.contains('\\') {
        child_name.replace('\\', "/")
    } else {
        child_name.to_owned()
    };
    if base.is_empty() {
        return normalized_child;
    }

    let mut joined = String::with_capacity(base.len() + 1 + normalized_child.len());
    joined.push_str(base);
    joined.push('/');
    joined.push_str(&normalized_child);
    joined
}

fn os_str_to_utf8(value: &OsStr) -> Cow<'_, str> {
    value.to_string_lossy()
}
