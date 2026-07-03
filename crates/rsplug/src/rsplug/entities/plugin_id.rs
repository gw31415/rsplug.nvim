use std::{cmp::Ordering, ops::Deref, path::Path, sync::Arc};

use crate::rsplug::util::hash;
use sailfish::runtime::Render;

/// 固定されたプラグインのID(表示や書き込み用)。
/// インストールが済んだ後に使用するのが望ましい。未インストールの PluginID は変更される可能性があるため。
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct PluginIDStr([u8; 32]);

impl Render for PluginIDStr {
    fn render(&self, b: &mut sailfish::runtime::Buffer) -> Result<(), sailfish::RenderError> {
        b.push_str(self);
        Ok(())
    }
}

impl From<PluginIDStr> for Box<[u8]> {
    fn from(val: PluginIDStr) -> Self {
        val.0.into()
    }
}

impl AsRef<str> for PluginIDStr {
    fn as_ref(&self) -> &str {
        self as &str
    }
}

impl AsRef<Path> for PluginIDStr {
    fn as_ref(&self) -> &Path {
        (self as &str).as_ref()
    }
}

impl Deref for PluginIDStr {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { str::from_utf8_unchecked(&self.0) }
    }
}

impl From<PluginIDStr> for Arc<str> {
    fn from(val: PluginIDStr) -> Self {
        Arc::from(unsafe { str::from_utf8_unchecked(&val.0) })
    }
}

impl std::fmt::Display for PluginIDStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (self as &str).fmt(f)
    }
}

/// プラグインの128bit ID。
/// `HasPluginId` trait 経由で `Hash` 実装を持つ任意の型から `.plugin_id()` で導出される。
#[derive(Hash, PartialEq, Eq, Clone, Copy)]
pub struct PluginID([u8; 16]);

impl PartialOrd for PluginID {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PluginID {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl std::fmt::Debug for PluginID {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PluginID({})", self.as_str())
    }
}

impl PluginID {
    /// 文字列表現を取得する。
    pub fn as_str(&self) -> PluginIDStr {
        PluginIDStr(hash::to_hex_bytes(self.0))
    }
}

/// 自身の [`std::hash::Hash`] 実装から [`PluginID`] を生成する。
///
/// [`std::hash::Hash`] を実装する全ての型（タプル・スライス・構造体等）に対し
/// ブランケット実装される。アドホックなID入力が必要な場合は、関連する値をタプルに
/// まとめて `.plugin_id()` を呼ぶことで、使い捨て構造体を定義する必要がない。
///
/// 注意: `Vec<T>` は内部で長さプレフィックスをハッシュに書き込むが `[T]` は書き込まない。
/// 同一性を保証するには、`Vec` に対して `.as_slice()` で `&[T]` を渡すこと。
pub(super) trait HasPluginId {
    fn plugin_id(&self) -> PluginID;
}

impl<T: std::hash::Hash + ?Sized> HasPluginId for T {
    #[inline]
    fn plugin_id(&self) -> PluginID {
        PluginID(hash::digest_hash(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_id_can_be_derived_from_hash_input_tuples() {
        let left = ("plugin/generated.lua", b"vim.g.generated = true").plugin_id();
        let right = ("plugin/generated.lua", b"vim.g.generated = true").plugin_id();

        assert_eq!(left, right);
    }

    #[test]
    fn derived_plugin_id_tracks_each_hashed_field() {
        let baseline = ("plugin/generated.lua", b"vim.g.generated = true").plugin_id();
        let different_path = ("plugin/other.lua", b"vim.g.generated = true").plugin_id();
        let different_data = ("plugin/generated.lua", b"vim.g.generated = false").plugin_id();

        assert_ne!(baseline, different_path);
        assert_ne!(baseline, different_data);
    }
}
