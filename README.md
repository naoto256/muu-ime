<p align="center">
  <img src="apple/MuuIME/ModeIconArtwork/MuuComponent.svg" alt="Muu IME" width="96" height="96">
</p>

<h1 align="center">Muu IME</h1>

<p align="center">
  <strong>文字を、打ちたいように打つ！</strong><br>
  <sub>Moji wo Uchitai youni Utsu!</sub>
</p>

<p align="center">
  <a href="https://github.com/naoto256/muu-ime/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/naoto256/muu-ime/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/naoto256/muu-ime/releases/latest"><img alt="Release" src="https://img.shields.io/github/v/release/naoto256/muu-ime?sort=semver&amp;display_name=tag"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="macOS 15+" src="https://img.shields.io/badge/macOS-15%2B-000000.svg?logo=apple&amp;logoColor=white">
  <img alt="Apple Silicon" src="https://img.shields.io/badge/Apple%20Silicon-arm64-555555.svg">
</p>

**macOS 用の日本語入力メソッド。**

- **変換は Mac の中だけで。** 読み・候補・確定した文字列をネットワークへ送りません。
- **学習と辞書はあなたのもの。** 保存先は Mac の中。変換学習は語の番号と回数だけで、メニューからいつでも消せます。
- **アプリの通信は 2 つの場面だけ。** 郵便番号データの更新と変換モデルの取得。どちらもあなたの操作で始まります。
- **変換モデルで、もう一段賢く。** zenz は任意。文全体を読んで第一候補を選び直します。辞書だけでも変換できます。

## 動作環境

- macOS 15.0 以降
- Apple Silicon（arm64）

## インストール

Terminal で次の 1 行を実行すると、GitHub Releases の最新の版をインストールします。Xcode や Command Line Tools は不要です。

```sh
curl -q -fsSL --proto '=https' --proto-redir '=https' https://raw.githubusercontent.com/naoto256/muu-ime/main/install.sh | sh
```

Apple の公証を受けていない配布です。確認する内容は [インストールと更新](docs/src/install.md) をご覧ください。

インストール後は、**ログアウトしてログインし直し**、システム設定の入力ソースに「Muu IME ひらがな」と「Muu IME 英数」を追加してください。

更新・版指定・手動インストール・アンインストールの手順も、[インストールと更新](docs/src/install.md) にあります。

## ドキュメント

- [ドキュメントサイト](https://naoto256.github.io/muu-ime/)
- [本の目次](docs/src/SUMMARY.md)
- 開発者向け: [ソースからのビルド](docs/src/install.md)、[全体構成](docs/src/internals/architecture.md)、[ビルドとテスト](docs/src/internals/build-and-test.md)

## ライセンス

本体は [MIT License](LICENSE) です。辞書などのライセンスと帰属は [THIRD_PARTY_LICENSES/](THIRD_PARTY_LICENSES/)、変換モデルの条件は [ライセンスと既知の制限](docs/src/internals/licenses-and-limitations.md) をご覧ください。
