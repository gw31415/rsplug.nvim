# Configファイル表示仕様（Examples付き）

## 目的

-   デフォルト表示は簡潔・視認性重視（件数＋共通パス＋ベース名）
-   パスが分散する特殊ケースでも破綻しない
-   フルパスは必要時のみ表示（ケースCは例外）

------------------------------------------------------------------------

## 前提

-   入力: `Vec<PathBuf>`
-   Location = 親ディレクトリ
-   表示名 = 拡張子なしファイル名

------------------------------------------------------------------------

# ケースA：単一Location

条件:

    groups.len() == 1

### Example A-1

    Config : 12 files
             ~/.config/home-manager/nvim/rsplug
             ai better_defaults cmp denops fern filetypes
             hobbies keymaps lib tweaks ui

### Example A-2

    Config : 3 files
             ~/.config/home-manager/nvim/rsplug
             ai cmp ui

------------------------------------------------------------------------

# ケースB：複数Location

条件:

    groups.len() >= 2
    かつ ケースCでない
    かつ dominant rule未発動

### Example B-1

    Config : 9 files in 2 locations

      rsplug (8)
        ~/.config/home-manager/nvim/rsplug
        ai cmp denops fern keymaps tweaks ui lib

      local (1)
        ~/.config/nvim/local
        debug

### Example B-2

    Config : 10 files in 3 locations

      rsplug (5)
        ~/.config/.../rsplug
        ai cmp fern keymaps ui

      local (3)
        ~/.config/nvim/local
        debug test scratch

      project (2)
        ./nvim
        project_override extra

------------------------------------------------------------------------

# ケースC：完全分散

条件:

    groups.len() == N

### Example C-1

    Config : 5 files (5 locations)

      ~/.config/.../rsplug/ai.toml
      ~/.local/.../debug.toml
      ./project.toml
      /etc/rsplug/system.toml
      ~/tmp/test.toml

------------------------------------------------------------------------

# dominant prefix rule

条件:

    main_ratio >= 0.75
    かつ external_count <= 5

### Example D-1

    Config : 12 files
             ~/.config/home-manager/nvim/rsplug (10)
             ai cmp denops fern keymaps tweaks ui lib ...

             +2 external files

### Example D-2

    Config : 10 files
             ~/.config/.../rsplug (9)
             ai cmp fern keymaps ui tweaks lib ...

             +1 external file

------------------------------------------------------------------------

# ソート

    Input: ui, ai, cmp
    Output: ai cmp ui
