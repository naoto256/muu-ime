CLDR-derived emoji annotations in the pinned Mozc generated table

Mozc commit: 3f235b4eb6fcff7d14ef5f0fb8ee56de7ee4c732
Input: src/data/emoji/emoji_data.tsv
SHA-256: 91e8d93f5e71b7fd821b30c8d72a7f5f22214fab5d8157d009600b304fe0cf08
Last upstream update: 4517e51d53063397222adb5512c7ad972b17c181 (2025-10-23)
https://github.com/google/mozc/commit/4517e51d53063397222adb5512c7ad972b17c181

That update says CLDR 48 / Emoji 17.0. CLDR 48's final release was on
2025-10-29, so this does not establish that the generated table used the final
release XML bytes. We use the exact generated table above, not a claim to have
reproduced its upstream XML generation.

The accompanying Unicode License V3 (SPDX Unicode-3.0) is copied verbatim from:
https://github.com/unicode-org/cldr/blob/acd6d88ae493633240e19a87a721076a8a75c310/LICENSE
Git blob: 861b74f3c812088755fd185d964e82a244503bbd
The last LICENSE update before the Mozc data update (CLDR a21b3916, 2025-02-12)
has the same Git blob. Thus the license text is established independently of
the unverified exact annotations snapshot. Mozc's BSD notice is preserved in
../../dictionaries/mozc/LICENSE.txt. No fonts or emoji images are distributed.
