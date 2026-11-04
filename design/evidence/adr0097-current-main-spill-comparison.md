# 現状の main と spill：容量は減るが、走査コストには大きな増加が残る

**Status: Experimental — 比較計測済み。本線の値付き走査で内容検証が失敗しており、全項目合格・統合承認ではない。**

## 容量削減を再確認、挿入と走査は分けて判断する

現状の作業ツリーを同じ条件で測り直した。5,000頂点・各1辺では、LARAの16領域が要求する容量は
本線の66ページからspillの41ページへ減った。**25ページ、1.5625 MiB、37.9%の削減**である。
これは領域単位の容量であり、canister全体の物理stable memoryや課金容量が同率で減るという結果ではない。

命令数は用途によって逆方向に動く。1,024辺ハブへの挿入は22.0%減、新規256ラベルの作成・挿入は26.5%減。
一方、33ラベル×50辺の走査は昇順76.3%増、降順59.0%増だった。別の256辺走査では約4倍のコストも観測した。
spillを一律に高速・低速とは評価できない。

さらに本線の8バイト値付きハブで、降順バッチの値順序が壊れることを確認した。同じ検証はspillでは通る。
このため値付き経路の命令数は参考値とし、正しさが同等な処理の速度差とは扱わない。

以前の66→41ページという容量監査は、今回の現状比較でも再現した。過去の丸め値やCSVの表示差分を流用せず、
以下の命令数はすべて今回の2本のWasmから取得した整数値で比較している。

## 比較対象は未コミット変更を含む2つの作業ツリー

| 項目 | 本線 | spill |
|---|---|---|
| 作業ツリー | `$REPO` | `$REPO` |
| HEAD | `6d82d0fc64a8a44bbd424efd7553ea6a347eee4a` | `1d3202bc35e098b1220fecf9401503fb83abb19c` |
| LARAの未コミット変更 | `deferred.rs` / `iter.rs` のコメント | `free_span.rs` のadmission関連変更 |
| 計測Wasm SHA256 | `204813710d53046b42146b9bbdb81dbbd537e4d49f8e7e18ee55ffc94bad8104` | `daa62418a1ac8566dcac92cf0f7f5a389dea3aef084d892f99f202ed284b5a45` |

比較単位は分岐後の変更を含む作業ツリー全体であり、spill導入だけの一因子比較ではない。
既存のbenchmark fixture、入力数、compaction policyは変更していない。

### ビルド条件とロックファイル

両側で同じrustc 1.100.0-nightly、canbench 0.7.0、PocketIC runtimeを使用した。
`canbench-rs 0.7.0`、`ic-stable-structures 0.7.2`、`ic-cdk 0.20.1`、`candid 0.10.35`も共通。
release/fat-LTO/codegen-units=1、wasm32、SIMD設定は既存設定のままである。

spillの元の`Cargo.lock`は現在のmanifestに対して`--locked`でビルドできなかった。
全workspace manifestに差がなく、LARA依存168パッケージの名前・バージョン・取得元が一致することを確認して、
本線でビルドできたロックを両側の共通条件とした。元の2ロック間で、この依存集合のメタデータ差は
Windows依存参照のバージョン明記部分だった。spillのロックは各計測フェーズ終了時に元のバイト列へ復元した。

- 実際に使用した共通ロック：`f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`
- spillの元ロック：`15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2`
- 共通runtime：`2e38d2266865ae285d612bd533675073d11d0cb77b8365c203431af753ce930e`

## 領域容量は疎グラフで減り、小規模ハブではページ数が変わらない

同一の外部プローブを両側のクレート内テストとして一時登録した。
4バイト辺、segment16、`InitialCapacities::uniform(256)`、通常の`insert_edge`を用い、強制foldや
追加maintenanceは行わない。各ケースは独立した新規グラフで、まず全頂点を作成してから辺を挿入する。
各領域の容量は`Memory::size`を明示呼出しし、64 KiBページ数とバックバッファ長の一致も検査した。

| ケース | 本線のページ数 | spillのページ数 | 差 | 内容検証 |
|---|---:|---:|---:|---|
| 空グラフ | 30 | 30 | 0 | 各ケースの初期値 |
| 1,024頂点×各1辺 | 32 | 30 | −2 / −6.25% | 両側で昇順・降順の全target一致 |
| 5,000頂点×各1辺 | 66 | 41 | −25 / −37.88% | 両側で昇順・降順の全target一致 |
| 1頂点×1,024辺 | 30 | 30 | 0 | 両側で昇順・降順の全target一致 |
| 1頂点×33ラベル×50辺 | 30 | 30 | 0 | 両側で昇順・降順の全target一致 |
| 1頂点×1,024辺、8バイト値 | 30 | 30 | 0（観測値） | 本線は値の降順検証失敗、spillは全検証通過 |

空グラフの30ページは1.875 MiB。5,000頂点では4.125→2.5625 MiBとなる。
ハブのページ数が同じでも、領域内の利用量や空き容量が同じという意味ではない。

5,000頂点ケースの削減内訳は次のとおり。ページ数が変わるのは旧logに相当する2領域だけだった。

| 領域 | 本線 | spill |
|---|---:|---:|
| エッジlog / spill arena | 11ページ | 1ページ |
| 値log / spill arena | 16ページ | 1ページ |
| bucket descriptor | 15ページ | 15ページ |
| 残り13領域の合計 | 24ページ | 24ページ |

### canbenchの物理stable memoryは両側とも2,049ページ

既存のstable-backed容量ベンチ4本では、`stable_memory_increase`はすべて両側2,049ページだった。
このfixtureは16領域を標準`MemoryManager`の128ページ単位で確保するため、
`16 × 128 + 管理用1 = 2,049`ページ、128.0625 MiBとなる。今回の規模では丸めの内側に容量差が収まった。

したがって、領域ごとの要求容量削減は確認できるが、このfixtureの物理確保量削減は確認できない。
本番のmemory backendや双方向グラフ全体への換算は今回の測定対象外である。

## 命令数は26本を比較し、意味の異なる行を区別する

変化率は`(spill / main − 1) × 100`。小さい方が少ない命令数を表す。
canbenchのWasm実行命令数であり、漸近計算量、実時間、cycles課金額ではない。

### 挿入・作成

| 既存ベンチ | 本線 | spill | 変化率 | 計測範囲 |
|---|---:|---:|---:|---|
| `bench_l_cap_s_hub_s16_256e` | 2,799,308 | 2,439,233 | −12.86% | stable-backed、作成＋256辺挿入 |
| `bench_l_cap_s_sp_l1024_1e` | 367,819,539 | 289,422,646 | −21.31% | stable-backed、segment32、1,024頂点作成＋各1辺 |
| `bench_l_cap_s_sp_l32_1e` | 11,169,688 | 9,012,666 | −19.31% | stable-backed、segment32、32頂点作成＋各1辺 |
| `bench_l_cap_s_sp_s16_1024v_1e` | 236,799,688 | 202,666,079 | −14.41% | stable-backed、segment16、1,024頂点作成＋各1辺 |
| `bench_l_s2_h_ins_1024` | 10,806,638 | 8,434,241 | −21.95% | 作成済みグラフへの1,024辺挿入 |
| `bench_l_mix_hub_ins_33x50` | 64,518,523 | 59,023,165 | −8.52% | 作成＋33ラベル×50辺、skip-leaf-cascade経路 |
| `bench_l_ins_fresh_256` | 33,148,943 | 24,376,466 | −26.46% | 作成＋256個の新規ラベルに各1辺 |

### 値なし走査・選択

| 既存ベンチ | 本線 | spill | 変化率 |
|---|---:|---:|---:|
| `bench_l_mix_hub_asc_33x50` | 452,958 | 798,682 | +76.33% |
| `bench_l_mix_hub_scn_33x50` | 350,316 | 557,118 | +59.03% |
| `bench_l_s2_h_pt_d_1024` | 17,768,910 | 17,041,455 | −4.09% |
| `bench_l_s2_h_scn_d_1024` | 17,716,329 | 16,988,772 | −4.11% |
| `bench_t_v_bypass` | 12,313 | 12,250 | −0.51% |
| `bench_t_v_dense_a` | 18,310 | 75,337 | +311.45% |
| `bench_t_v_dense_d` | 18,056 | 71,317 | +294.98% |
| `bench_t_v_ebreak` | 9,339 | 29,913 | +220.30% |
| `bench_t_v_in_dense` | 88,129 | 88,367 | +0.27% |
| `bench_t_v_sparse` | 55,113 | 53,657 | −2.64% |
| `bench_t_v_window` | 9,819 | 47,504 | +383.80% |

これらはセットアップ外の操作を測る。`pt_d_1024`と`scn_d_1024`は1,024辺を51回走査する合計であり、
1回のlookup/scanの命令数ではない。前者も早期終了する点検索ではなく、targetを比較しながら全辺を走査する。
`dense`という名前だけでは、現状の両実装が同じslab-only状態にあるとは証明できない。
今回の比較は同じ既存呼出し列に対する結果であり、物理配置を揃えたアルゴリズム単体比較ではない。

### 値に関連するfixture：正しさ同等の速度差とは未確認

| 既存ベンチ | 本線 | spill | 観測上の変化率 |
|---|---:|---:|---:|
| `bench_t_v_at_replay` | 4,989 | 3,986 | −20.10% |
| `bench_t_v_hyb` | 81,010 | 73,674 | −9.06% |
| `bench_t_v_in_hyb_ip` | 74,470 | 111,370 | +49.55% |
| `bench_t_v_pf_then_sel` | 152,753 | 266,174 | +74.25% |
| `bench_t_v_w_ip` | 155,533 | 154,677 | −0.55% |
| `bench_l_ip_log_8b_of` / `bench_l_ip_spill_8b_of` | 1,597,959 | 1,118,925 | −29.98% |

この区分には値付きfixture上のtopology-only操作も含めた。本線の順序不一致がこの6本すべてに出た、
という意味ではない。個々のfixtureの値・slot対応を今回新たに検証していないため、正当性修正を含む
同等処理の速度差とは認定しない。末尾のlog/spill名だけは、同じ計測本体の改名として対応付けた。

### 削除・compactionの複合処理

| 既存ベンチ | 本線 | spill | 扱い |
|---|---:|---:|---|
| `bench_l_s2_h_del_1024` | 127,158,550 | 44,870,295 | policyが異なるため直接の変化率を採用しない |
| `bench_l_s2_h_del_h_1024` | 72,741,140 | 21,157,667 | 観測上−70.91%。削除＋compaction＋survivor走査の合計 |

前者はspill側が`SlackMayBeDropped`を選ぶため同一作業ではない。
後者はhandle指定で削除し明示compactionを行う既存fixtureだが、移動通知の同等性は今回検証していない。
純粋な削除コストや統合条件の達成には読み替えない。

stable-backedの4本以外は既存の`VectorMemory` fixtureである。Wasm上で計算を測っていても、
本番stable-memory syscall込みのコストとは同じではない。容量プローブとも辺幅・初期容量・挿入経路が異なるため、
両表を組み合わせて1バイト削減あたりの命令コストを算出することはできない。

## 本線の降順バッチでslotと値の対応が崩れる

容量プローブの値は`target × 1009 + 7`という一意な8バイト値。
1,024辺を通常挿入した後、昇順・降順の全target列と値列を期待値と照合した。
本線は4つの値なしケースを通過し、値付きケースの昇順検証も通過したが、降順で失敗した。

- 最初の不一致は0始まり位置112。
- 期待：target912の値`920215`。
- 実際：target657の値`662920`。
- 値は1,024個揃い、期待値の並べ替えにはなっている。欠落ではなく順序・対応の問題。
- 同じプローブ、同じassertionでspillは5ケースすべて通過。

本線の`labeled/graph/traverse.rs`にある
`visit_dense_out_inline_property_batches_for_slab_prefix_next`のfast pathでは、
`raw_values`を昇順のままコピーし、降順時は`slot_indices`だけを逆順にしている。
`LabeledInlinePropertyValueBatch`の「valuesはslot_indicesと同じ順序」という契約と一致しない。
本比較では修正もassertionの緩和も行っていない。

## 計測手順・完了状況・残る判断

計測証拠の保存先を`E`とする。

```text
E=$GLEAPH_EVIDENCE/adr0097/current-lara-comparison-0365
```

`before.json`は本線1,971ファイル・spill2,357ファイルの元状態とHEAD/indexを固定する。
各側の`source-inputs.tar.gz`、元の`Cargo.lock`、`worktree.patch`も保存した。
`capacity_probe.rs`のSHA256は`17f754c2aa4de20cf006f2aa0c3f6ded780db0e2dfc9d21c297021941e40083c`。
元状態、実際に使用したロック、計測Wasmを別々に保持している。

容量は各`lib.rs`に同じ外部ファイルを`#[cfg(test)]`で一時登録して、各作業ツリーから次を実行した。
登録はcanbenchのビルド前に除去した。

```sh
cargo test --locked --offline -p ic-stable-lara --lib \
  current_capacity_0365::current_capacity_and_contents -- --exact --nocapture
```

命令数は`E/main`と`E/spill`の外部設定から、6つの指定patternで取得した。
実装・fixtureは各作業ツリーのものを使い、通常の`target/canbench`にビルドした。

```sh
CARGO_TARGET_DIR=../../target/canbench cargo build --locked --offline --release \
  --target wasm32-unknown-unknown --features canbench
canbench <pattern> --csv --runtime-path <worktree>/crates/ic-stable-lara/.canbench/pocket-ic
```

patternは`bench_l_cap_s_`、`bench_l_mix_hub_`、`bench_l_s2_h_`、`bench_t_v_`、
`bench_l_ins_fresh_256`、末尾のlog/spill名の1本。実際の全コマンド・cwd・timeoutは`commands.tsv`に記録した。

| 検証・計測 | 結果 |
|---|---|
| 初回の外部テスト配置 | コンパイル失敗。単方向更新APIがcrate-privateだったため内部テストへ修正 |
| 内部配置後の初回 | fixtureのsegment32想定で失敗。両側の定義が16であることを確認して訂正 |
| 本線の訂正済み容量プローブ | **失敗**。値なし4ケース通過後、値付き降順検証に失敗 |
| spillの元ロックでの実行 | ビルド前に`--locked`で停止 |
| spillの共通ロックでの容量プローブ | **合格**。1テスト、5ケース、679件はfilter対象外 |
| standalone probe rustfmt | 合格 |
| canbenchの指定12コマンド | **全てexit 0**。各側26 totals、本線139 scopes / spill88 scopes |
| Rust/IC実行時間 | 初期失敗を含むコマンド秒数合計291秒。最長115秒。上限内 |
| 全ライブラリテスト・clippy・PocketIC E2E | 未実行。既存の`GAP-LARA-TEST-CLIPPY`を解消・免除していない |
| 元状態・HEAD/index・whitespace | 一致確認済み、両側の`git diff --check`合格 |
| Plan0365の最終validator | exit 1。容量TODOとnative/final条件が未完のため、計画は未完扱い |

全scopeのCSVとログは残しているが、scopeは入れ子で加算できず、消えたscopeをゼロコストとも扱わない。
`comparison.csv` / `comparison.json`に26行の整数値・分類・容量内訳、`lock-contract.json`に依存確認を保存した。
既存`canbench_results.yml/csv`は上書きせず、`--persist`、unfiltered canbench、instruction tracingは使っていない。

`scope.json`で元ファイル、HEAD/indexの一致を確認する。恒久的な変更はこの比較報告だけで、
本体コード・既存テスト・ロックファイル・過去の証拠を変更しない。

### 次に判断すべきこと

容量削減と挿入コスト削減はspillを残す具体的な理由になる。ただし、読み取り中心の用途には
複数ラベル走査や通常の256辺走査の増加が重い。本線の値順序不具合もあるため、
「本線の命令数を下回る」だけでは採用条件を決められない。

次の作業を選ぶなら、本線の値対応不具合を別件として修正・検証することと、実利用で重要な走査の許容範囲を
決めることを分ける。今回の計測を理由に追加最適化を自動で始めたり、統合条件を緩めたりはしない。
既存のpaged-map基準は再計測しておらず、0362の4/6、replace/tail未達はそのままである。
