# png_into_dll

`.png` ファイルを Windows のアイコンリソース (`RT_GROUP_ICON` / `RT_ICON`) として
`.dll` に格納するコマンドラインツールです。作成した DLL には後から `.png` / `.ico` を
追加・削除でき、ショートカットやレジストリのアイコン指定 (`icons.dll,-1` など) や
Windows の「アイコンの変更」ダイアログからそのまま利用できます。

純 Rust で PE (リソース専用 DLL) を生成するため Windows API に依存せず、
Linux / macOS 上でも DLL を作成できます。

## ビルド

```
cargo build --release
```

`target/release/png_into_dll.exe` が生成されます。

## 使い方

```
png_into_dll create  <dll> [--force]
png_into_dll add     <dll> <files...> [--group] [--id <ID|NAME>] [--keep-png]
png_into_dll list    <dll>
png_into_dll remove  <dll> <ID|NAME>...
png_into_dll extract <dll> <ID|NAME> <out.ico>
```

### 例

```powershell
# 空の DLL を作成 (add は DLL が無ければ自動的に作成するので省略可)
png_into_dll create icons.dll

# PNG を 1 つ追加 → ID 1 のアイコンになる
png_into_dll add icons.dll folder.png

# 複数の PNG をそれぞれ別のアイコンとして追加 (ID 2, 3, 4 ...)
png_into_dll add icons.dll a.png b.png c.png

# 16/32/48/256px の PNG を 1 つのマルチサイズアイコンにまとめ、名前 APP で登録
png_into_dll add icons.dll --group app16.png app32.png app48.png app256.png --id APP

# 既存の .ico ファイルを取り込む (含まれる全サイズが 1 アイコンになる)
png_into_dll add icons.dll legacy.ico --id 100

# 一覧
png_into_dll list icons.dll
#   index  id                   lang    images
#   0      APP                  0x0409 16x16 32bpp, 32x32 32bpp, 48x48 32bpp, 256x256 PNG
#   1      1                    0x0409 24x24 32bpp
#   2      100                  0x0409 16x16 4bpp, 32x32 4bpp

# 削除 / .ico として取り出し
png_into_dll remove icons.dll 1 100
png_into_dll extract icons.dll APP app.ico
```

### 格納形式

- 256px 未満の PNG は 32bit (BGRA + 1bit マスク) の DIB に変換して格納します。
  256px の画像は PNG のまま格納します (Windows Vista 以降の標準形式)。
- `--keep-png` を付けると全サイズを PNG のまま格納します (ファイルは小さくなりますが
  Vista 以降でしか表示できません)。
- 対応サイズは 1×1 〜 256×256 です。
- 入力形式は拡張子ではなく内容 (マジックナンバー) で判定します。

### アイコンの参照方法

Windows でアイコンを指定する `"path,index"` 形式では:

- 負の数 → リソース ID: `C:\path\icons.dll,-100`
- 0 以上 → `list` の `index` 列 (名前付きが先、続いて ID 昇順): `C:\path\icons.dll,0`

`--id` を省略した場合は「既存の最大の整数 ID + 1」(初回は 1) が割り当てられます。

## 制限

- `add` / `remove` で書き換えられるのは、このツールで作成したリソース専用 DLL のみです
  (`.rsrc` セクション以外を持つ DLL/EXE は破壊を防ぐため拒否します)。
  `list` / `extract` は任意の PE ファイルに対して使えます。
- アイコン以外のリソース (バージョン情報など) が DLL にあればそのまま保持されます。

## 開発

```
cargo test
cargo clippy --all-targets
```

| ファイル | 内容 |
|---|---|
| `src/pe.rs` | PE の読み込み (PE32/PE32+) と、x64 リソース専用 DLL の生成 |
| `src/icon.rs` | PNG/ICO の読み込み、DIB 変換、`GRPICONDIR` / `.ico` の入出力 |
| `src/store.rs` | DLL 内リソースのアイコン単位での追加・削除・再構成 |
| `src/resid.rs` | リソース ID (整数 / 文字列) と PE 内での並び順 |
| `src/main.rs` | CLI |
