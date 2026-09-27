# Repository guidance

- TOMLとJSON lockからLua/Neovimのpackを生成する。出力の決定性とlock/cache identityを維持する。
- lazy-loadingを変更するときは生成packを比較し、リポジトリのNeowrightスキルで実際のNeovim挙動を確認する。
- Rust変更には影響に応じて `cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all -- --check` を使う。`cargo check -q` は使わない。
- コミット・push・履歴変更はユーザーが明示的に依頼した範囲で行い、秘密情報を出力しない。
