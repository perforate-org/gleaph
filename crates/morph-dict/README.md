# morph-dict

MeCab-format morphological dictionary engine over byte-image, offset-accessed
dictionaries. The dictionary parameter is the `ByteImage` trait (`len` +
`read_exact_at`) so the same accessors run over a heap copy, a resident-prefix /
lazy-suffix split, or a stable-memory-backed image from the companion
[`ic-morph-dict`](../ic-morph-dict) adapter.

Derived from MeCrab (github.com/cool-japan/mecrab @ 85444b5, MIT OR Apache-2.0);
see LICENSE-MECRAB and the per-file copyright headers. Ships with the
mecab-ipadic BSD license acknowledgment (LICENSE-IPADIC).

See the crate documentation (`cargo doc`) for the MPD container format and
the `DictionaryProfile` language-parameterization model.
