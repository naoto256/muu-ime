# インストールと更新

Apple Silicon（arm64）の Mac、macOS 15.0 以降に対応します。配布版のインストールに Xcode、Command Line Tools、Rust、Python、sudo は必要ありません。

## スクリプトでインストールする

GitHub Releases の最新の版を、Terminal で次の 1 行を実行してインストールします。このコマンドは main の最新のインストーラーを使います。

```sh
curl -q -fsSL --proto '=https' --proto-redir '=https' https://raw.githubusercontent.com/naoto256/muu-ime/main/install.sh | sh
```

インストーラーは最新の `SHA256SUMS` から版を決め、その版の ZIP を GitHub Releases から取得します。SHA-256、署名、bundle ID、standard 版、arm64、必要な macOS の版を確かめてから、`~/Library/Input Methods/MuuIME.app` に配置します。取得した app を検査のために実行したり、署名し直したりはしません。

配置後は、**ログアウトしてログインし直します**。これが標準の手順です。Mac の再起動でも代わりになりますが、必須ではありません。

ログインし直したら、入力ソースを追加します。

1. **システム設定 → キーボード → 入力ソース** の **編集** を開きます。
2. **＋** を押し、**日本語** から **Muu IME** を選んで追加します。

項目の名前や場所は macOS の版によって違うことがあります。入力ソースは「Muu IME ひらがな」と「Muu IME 英数」の 2 つです。

zenz は app に同梱していません。利用する場合は About から取得してください。取得しなくても、辞書だけで変換できます。

### 配布物を信頼する範囲

配布 app は **ad-hoc 署名で、Developer ID 署名・公証はありません**。信頼の起点は、この GitHub リポジトリのスクリプトと Releases です。

SHA-256 は破損や版の取り違えを検出します。同じ公開元の ZIP と SHA-256 が両方改変された場合は検出できず、公開者とは独立した署名ではありません。`codesign --verify` も、署名したバイトが保たれていることの検査で、公開者の身元や公証を証明するものではありません。

curl の取得には通常 quarantine が付きません。Safari などで手動取得した ZIP には付くことがあるため、後述の手順が異なります。また、quarantine がないだけで、あらゆる macOS の起動制限を通ることまでは保証しません。

通信先は GitHub とその CDN です。インストーラーは利用者が実行したときだけ通信し、自動の再試行や更新確認はしません。アプリ本体の通信は [データと通信](./usage/data-and-network.md) を見てください。

## 更新と版指定

更新は、インストールと同じ 1 行をもう一度実行します。更新後もログアウトしてログインし直してください。特に入力ソースの定義やアイコンの変更は、ログインし直すまで反映されないことがあります。

版とインストーラーを固定する場合は、その版のリリースに添付された `install.sh` に `--version` を渡します。以下は `0.1.0` の例です。`0.1.0` と `v0.1.0` のどちらでも指定できます。

```sh
curl -q -fsSL --proto '=https' --proto-redir '=https' https://github.com/naoto256/muu-ime/releases/download/v0.1.0/install.sh | sh -s -- --version 0.1.0
```

リリースの tag は `v0.1.0`、ZIP の名前は `MuuIME-0.1.0-macos-arm64.zip`、照合するファイル名は `SHA256SUMS` です。同じリリースの `install.sh` は、その tag のソースと同じ内容です。`SHA256SUMS` は ZIP だけの 1 行形式で、スクリプトの独立した署名にはなりません。

### 入れ替えと失敗時の復旧

新しい app は別の inode にコピーして検査し、前の app を同じディレクトリの stage に退避してから入れ替えます。配置後にも照合します。成功すると前の app はゴミ箱へ移します。ゴミ箱に移せない場合は stage に残し、その場所を表示します。

取得や検査の失敗では前の app を変更しません。入れ替えの途中で失敗した場合は前の app を戻し、新規インストールなら未検証のコピーを配置先から退けます。復元にも失敗した場合は、前の app の退避先を表示して停止します。その stage を削除せず、別の入力ソースへ切り替えてから、表示された前の app を Finder で戻してください。配置先に残る未検証のコピーと混ぜないよう、上書きコピーは避けてください。

未検証のコピーを退避した stage は、失敗時には削除せず、場所を表示します。前の app の復元と stage の中身を確認したあと、不要になった stage は手で削除できます。

強制終了や電源断では自動の復元処理は動かないため、stage の退避コピーを確認する必要があります。

ホームディレクトリ、`Library`、`Input Methods`、配置先 app とその上の階層に symlink がある環境は拒否します。symlink をたどって別の場所に配置する操作は行いません。

同時実行は `~/Library/Input Methods/.muu-install-lock` で排除します。異常終了で空の lock が残った場合は、ほかのインストール操作が終了したことを確認してから、次のように除いて再実行します。

```sh
rmdir "$HOME/Library/Input Methods/.muu-install-lock"
```

## ZIP を手で取得する場合

[GitHub Releases](https://github.com/naoto256/muu-ime/releases) で、**同じ版**の ZIP と `SHA256SUMS` をダウンロードします。以下は `0.1.0` の例です。

1. ダウンロードしたフォルダで SHA-256 を照合します。

   ```sh
   cd "$HOME/Downloads"
   shasum -a 256 -c SHA256SUMS
   ```

   `MuuIME-0.1.0-macos-arm64.zip: OK` と出た場合だけ先に進みます。

2. quarantine が付いている ZIP は、**照合したそのファイルだけ**から属性を外します。

   ```sh
   xattr -d com.apple.quarantine "$HOME/Downloads/MuuIME-0.1.0-macos-arm64.zip"
   ```

   属性がないというエラーなら、その ZIP には quarantine が付いていません。すでに展開した場合は、照合した ZIP から取り出した app だけを対象にします。たとえば Downloads 直下に展開した場合は次のとおりです。

   ```sh
   xattr -dr com.apple.quarantine "$HOME/Downloads/MuuIME.app"
   ```

   Downloads 全体やほかの app を対象にしないでください。

3. ZIP を新しい空のフォルダへ展開し、署名を検査します。

   ```sh
   muu_release_dir=$(mktemp -d "$HOME/Downloads/muu-ime-release.XXXXXX") &&
   ditto -x -k --norsrc --noqtn "$HOME/Downloads/MuuIME-0.1.0-macos-arm64.zip" "$muu_release_dir" &&
   codesign --verify --deep --strict --verbose=2 "$muu_release_dir/MuuIME.app"
   ```

   成功したら、次の表示を確認します。

   ```sh
   codesign -d --verbose=4 "$muu_release_dir/MuuIME.app" 2>&1
   plutil -extract CFBundleIdentifier raw -o - "$muu_release_dir/MuuIME.app/Contents/Info.plist"
   plutil -extract CFBundleShortVersionString raw -o - "$muu_release_dir/MuuIME.app/Contents/Info.plist"
   plutil -extract MuuBuildVariant raw -o - "$muu_release_dir/MuuIME.app/Contents/Info.plist"
   plutil -extract LSMinimumSystemVersion raw -o - "$muu_release_dir/MuuIME.app/Contents/Info.plist"
   ```

   署名は `Signature=adhoc`、ID は `dev.muu.inputmethod.Muu`、版は `0.1.0`、variant は `standard`、最低 OS は `15.0` です。利用する Mac が Apple Silicon で、この OS 以降であることも確認してください。

4. Finder の「移動 → フォルダへ移動」で `~/Library/Input Methods/` を開きます。前の `MuuIME.app` があれば先にゴミ箱へ移してから、新しい app をコピーしてください。既存 app の上にコピーして混ぜたり、署名し直したりはしません。
5. ログアウトしてログインし直し、入力ソースを追加します。

## アンインストール

先に別の入力ソースへ切り替え、システム設定から Muu IME の 2 つの入力ソースを外します。そのあとで実行します。

```sh
curl -q -fsSL --proto '=https' --proto-redir '=https' https://raw.githubusercontent.com/naoto256/muu-ime/main/install.sh | sh -s -- --uninstall
```

スクリプト自体の取得後、リリースの ZIP や SHA は取得しません。配置先の app が Muu IME であることを確かめ、ゴミ箱へ移します。ゴミ箱を安全に使えない場合は、同じディレクトリの stage に退避して場所を表示します。app がなければそのまま終了します。

辞書、学習、モデル、設定は読み書きせず、そのまま残します。入力ソースの削除やプロセスの停止を自動で行うものではありません。最後にログアウトしてログインし直してください。壊れた app などで bundle ID を確認できない場合は停止するため、手動で app をゴミ箱へ移してください。

## ソースからのビルド（開発者向け）

必要なものは Xcode、git、python3、rustup、`cbindgen`、`just` です。shallow clone ではビルドできません。ビルド番号に全履歴を使います。

Rust 1.96.0 に arm64 の target と、ライセンス検査用の `rust-docs` component を入れます。

```sh
rustup toolchain install 1.96.0 --target aarch64-apple-darwin --component rust-docs
CARGO_PROFILE_RELEASE_STRIP=false cargo install cbindgen just --locked
```

チェックアウトで実行します。

```sh
python3 scripts/rust_toolchain.py cargo fetch --locked --target aarch64-apple-darwin
CARGO_PROFILE_RELEASE_STRIP=false just build
CARGO_PROFILE_RELEASE_STRIP=false just install
```

最初の行だけが依存 crate の取得です。ビルド中の Cargo は offline で動きます。`CARGO_PROFILE_RELEASE_STRIP=false` は、macOS 27 のリンカと Rust 1.96 の組み合わせで、strip した proc-macro の dylib が読み込めなくなる問題を避けるためです。`just install` もビルドし直すので、同じ変数を付けます。

開発用の `just install` は、実行ファイルの SHA-256 と署名を記録・照合し、入力ソースの登録も扱います。配布用の単一 `install.sh` とは別の入口です。ソースから作った版も、インストール後はログアウトしてログインし直してください。開発用の `just uninstall` は入力ソースを無効化して app をゴミ箱へ移し、ユーザーデータは残します。

### 配布 ZIP を作る

```sh
CARGO_PROFILE_RELEASE_STRIP=false just release-package
```

clean な全履歴のチェックアウトから standard 版だけをビルドします。既存の署名・出所・bundle・ライセンスの検査後、ZIP を作り、別の場所へ展開して同じ検査を通してから公開用の 3 ファイルを置きます。

```text
build/release/v0.1.0/
  MuuIME-0.1.0-macos-arm64.zip
  SHA256SUMS
  install.sh
```

版はビルドした app の `CFBundleShortVersionString` から決めます。`SHA256SUMS` は ZIP の SHA-256 と basename を書いた 1 行です。`install.sh` は、表示される `source_commit` のソースとバイト単位で一致することを確認します。同じ版の出力は上書きしません。配り直すときは版を上げ、表示された commit に `v<版>` の tag を付け、この 3 ファイルを同じ release に置きます。すでに tag がある場合は、その tag が同じ commit を指すことも検査します。recipe は tag、release、workflow の作成やアップロードは行いません。
