use std::io::{IsTerminal, Stderr, Write};

pub struct OSC94 {
    stderr: Stderr,
    /// stderr が端末でないとき (ログファイルやパイプ) は何も書かない。
    enabled: bool,
}

impl OSC94 {
    pub fn new() -> Self {
        let stderr = std::io::stderr();
        let enabled = stderr.is_terminal();
        Self { stderr, enabled }
    }
    pub fn progress<T: TryInto<u8>>(&mut self, percent: Option<T>)
    where
        <T as std::convert::TryInto<u8>>::Error: std::fmt::Debug,
    {
        if !self.enabled {
            return;
        }
        if let Some(percent) = percent {
            self.stderr.write_all(b"\x1b]9;4;1;").ok();
            self.stderr
                .write_all(
                    percent
                        .try_into()
                        .expect("failed to cast percent")
                        .to_string()
                        .as_bytes(),
                )
                .ok();
            self.stderr.write_all(b"\x07").ok();
        } else {
            self.stderr.write_all(b"\x1b]9;4;3;0\x07").ok();
        }
    }
}

impl Drop for OSC94 {
    fn drop(&mut self) {
        // Clear the progress bar when the OSC94 struct is dropped.
        if !self.enabled {
            return;
        }
        self.stderr.write_all(b"\x1b]9;4;0;0\x07").ok();
    }
}
