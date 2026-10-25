# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-10-26

> 文字を、打ちたいように打つ！

Moji wo Uchitai youni Utsu!

macOS 用の日本語入力メソッドです。

- 読み・候補・確定した文字列をネットワークへ送りません。
- 学習と辞書は Mac の中に保存し、変換学習は語の番号と回数だけを記録します。
- 郵便番号データの更新と変換モデルの取得は、あなたの操作で始まります。
- zenz は任意です。文全体を読んで第一候補を選び直します。辞書だけでも変換できます。

動作環境は **macOS 15.0 以降、Apple Silicon（arm64）** です。

Terminal で次の 1 行を実行すると、最新の版をインストールします。

```sh
curl -q -fsSL --proto '=https' --proto-redir '=https' https://raw.githubusercontent.com/naoto256/muu-ime/main/install.sh | sh
```

Apple の公証を受けていない配布です。インストール前の確認事項と、版を指定する方法は [ドキュメント](https://naoto256.github.io/muu-ime/) をご覧ください。

インストール後は、ログアウトしてログインし直し、システム設定の入力ソースに「Muu IME ひらがな」と「Muu IME 英数」を追加してください。

[0.1.0]: https://github.com/naoto256/muu-ime/releases/tag/v0.1.0
