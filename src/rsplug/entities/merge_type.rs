pub enum MergeType {
    /// 競合したらマージしない
    Conflict,
    /// 競合したら上書きする
    Overwrite,
    // /// プログラム的にマージする
    // Merge(Arc<dyn Fn(Vec<u8>, Vec<u8>) -> Result<Vec<u8>, MergeType>>),
}
