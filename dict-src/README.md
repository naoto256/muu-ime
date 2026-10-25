`provenance.toml` に、辞書データの取得元、固定した版、ライセンス、加工の説明と補助データの SHA-256 を記録します。辞書は `just dict-build` で生成し、`mozc-oss.sha256` の値と照合します。`just dict-reproducibility` で、同じ入力からバイト単位で同じ辞書ができることを確認します。
