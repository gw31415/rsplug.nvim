use std::{
    collections::BTreeSet,
    mem::MaybeUninit,
    ops::{Add, AddAssign, Deref},
    path::Path,
};

use itertools::Itertools;
use xxhash_rust::xxh3::xxh3_128;

struct PackageIDStr([u8; 32]);

impl AsRef<str> for PackageIDStr {
    fn as_ref(&self) -> &str {
        self as &str
    }
}

impl AsRef<Path> for PackageIDStr {
    fn as_ref(&self) -> &Path {
        (self as &str).as_ref()
    }
}

impl Deref for PackageIDStr {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { str::from_utf8_unchecked(&self.0) }
    }
}

/// パッケージID。ディレクトリ名として使用される。
#[derive(Hash, Clone)]
pub struct PackageID(pub(super) BTreeSet<[u8; 16]>);

impl PackageID {
    /// 文字列に変換
    pub fn into_str(self) -> impl AsRef<Path> + AsRef<str> + Deref<Target = str> {
        const TABLE: &[u8; 16] = b"0123456789abcdef";
        let PackageID(inner) = self;
        let bytes = inner.into_iter().flatten().collect_vec();
        let hash: [u8; 16] = xxh3_128(&bytes).to_ne_bytes();
        let mut res = const { [MaybeUninit::<u8>::uninit(); 32] };
        for (i, b) in hash.iter().enumerate() {
            let i = i << 1;
            unsafe {
                res.get_mut(i)
                    .unwrap_unchecked()
                    .write(TABLE[(b / 16u8) as usize]);
                res.get_mut(i + 1)
                    .unwrap_unchecked()
                    .write(TABLE[(b % 16u8) as usize]);
            }
        }
        PackageIDStr(unsafe { std::mem::transmute::<[MaybeUninit<u8>; 32], [u8; 32]>(res) })
    }
}

impl Add for PackageID {
    type Output = Self;
    fn add(mut self, rhs: Self) -> Self::Output {
        self += rhs;
        self
    }
}

impl AddAssign for PackageID {
    fn add_assign(&mut self, rhs: Self) {
        self.0.extend(rhs.0);
    }
}
