# Muu IME の公開サイト

Eleventy で https://naoto256.github.io/muu-ime/ 向けの静的 HTML を作ります。
`docs/src` の本文を直接読み、`SUMMARY.md` の順で目次と前後の章を作ります。
本文の複製や、生成した Markdown は管理しません。

```sh
cd site
# .node-version に固定した Node を使います。
npm ci --ignore-scripts
npm run build
npm run check
npm run check:links
npm run preview
```

プレビュー: http://127.0.0.1:8090/muu-ime/ 。成果物は `dist/` です。
`npm run dev` は同じポートで変更を監視し、`.dev/` へ出力します。
どちらも公開やアプリのインストールを行いません。

`SITE_BASE` の既定は `/muu-ime/`、`SITE_ORIGIN` は
`https://naoto256.github.io` です。独自ドメインに変えるときは、ビルドと
プレビューの両方へ同じ値を指定します。リンク検査は通信せず、ローカルの
全ページ・アンカー・画像・CSS・JavaScript と sitemap を照合します。

ロゴはアプリの `apple/MuuIME/ModeIconArtwork/MuuComponent.svg` をそのまま
コピーします。本文・トップとも日本語のシステムフォントを使い、フォントや
スクリプトを外部サービスから読みません。

共通の OGP 画像は `src/og.png`（1200 × 630）です。編集用の原稿が
`og-source.svg` で、通常のビルドは PNG をそのままコピーします。
PNG は macOS のヒラギノを使って書き出すため、フォントの異なる環境で
再出力すると見え方が変わります。画像の生成ツールはビルドの依存に含みません。

workflow は PR のビルド検査と、main からの明示的な `workflow_dispatch`
による GitHub Pages 公開を分けます。公開にはリポジトリ側の Pages と
`github-pages` environment の設定が必要です。確認済みの同じ run の成果物を
deploy job が使います。HTML を別ブランチへコミットしません。
