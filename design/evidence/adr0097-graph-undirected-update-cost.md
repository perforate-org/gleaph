# 無向辺の通常更新1回で、命令数と値slabの追加量を測った

**状態: Experimental。0378の測定・独立レビュー・最終検証完了。既存のnative失敗11件は未解決。**

0377と同じW2の合成fixtureで、GraphStoreの無向辺更新1回をcanbenchで測った。fit条件は321,020命令、値slab末尾の増分82B、再利用34Bだった。no-fit条件は207,048命令、末尾増分68B、再利用0Bだった。各条件を3回測り、値は同じだった。**これは条件ごとの更新コストであり、再利用だけの高速化率ではない。** この数字から自動foldの周期・閾値は選ばない。

[0377](adr0097-undirected-update-value-span-reuse.md)のnative検証に、通常更新APIの計測区間を加えた。対象は2-byteのRawU16 profile、inline indexを構成しない2条件だけである。fixtureの作成・事前読出し・結果検査は計測外に置いた。Router/GQL全体や実trafficの更新頻度は対象にしていない。

## 結果：fitでは要求116Bのうち34Bを既存領域で賄った

`P`は頂点IDが大きいcanonical側、`C`は小さいcounterpart側で、どちらもforward allocatorを使う。tailは値slabの占有末尾を表す。命令数は丸めたCLI表示ではなく、同じ呼出しの`BenchResult.total.instructions`から記録した。

| 条件（P / C degree） | 命令数 #1 | #2 | #3 | tailの前→後 | tail増分 | 再利用 |
|---|---:|---:|---:|---:|---:|---:|
| fit（41 / 17） | 321,020 | 321,020 | 321,020 | 84→166B | 82B | 34B |
| no-fit（17 / 17） | 207,048 | 207,048 | 207,048 | 36→104B | 68B | 0B |

fitのfold要求はPが82B、Cが34Bだった。末尾への追加はPの82Bだけで、Cの全34Bは旧P領域の先頭で読めた。no-fitでは旧P領域が16Bと小さく、両端の要求34Bずつが末尾に追加された。**2条件はPのdegreeも違うため、命令数の差を再利用の損得とは解釈できない。** 最小値・最大値は各行の値に一致するが、3回の一致から統計的な安定性までは主張しない。

両条件とも更新直前のfreeは0Bだった。更新後のfreeはfitが48B、no-fitが34Bで、それぞれ1本のspanになった。live / allocatedは118 / 118Bと70 / 70Bだった。再利用量はallocatorイベント数ではなく、退役した旧P・旧Cのbytesとfreeの増減から求め、tailの収支と全payloadの読戻しで照合した。

計測区間のheap増分・stable memory増分はいずれも0ページだった。これは追加ページが不要だったという観測であり、物理メモリを解放したことや、fixture全体のメモリ消費が0であることを意味しない。

原本は[全6観測と実行物のhash](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/measurements.json)、[1回目](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/canbench-1.log)、[2回目](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/canbench-2.log)、[3回目](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/canbench-3.log)にある。

## 方法：一つのfixtureを共有し、API呼出しだけを囲んだ

nativeとWasmで[同じfixture・assertion](../../crates/graph/src/facade/store/inline_update_fixture.rs#L26-L284)を使う。W2 profile helperも移し、旧定義は削除した。[moduleの読み込み](../../crates/graph/src/facade/store.rs#L32-L38)はtestまたはcanbench用Wasmに限る。通常のproduction buildには入らない。

0377と同じく、低ID・高ID・guardの3頂点を作り、通常insertで両端に非空の値runを残す。guardにより旧spanが非tailになる。初期tail/free、degree、slab slot数、全row、既知の旧P領域を更新前に検査する。対象のslotはP側32または8、C側8で、いずれも末尾slotではない。targetと無関係な辺には別々のsidecarを置く。

[計測境界](../../crates/graph/src/facade/store/inline_update_fixture.rs#L178-L190)では、入力を`black_box`に通してから、一つだけ定義した更新closureを`bench_fn`内で一度呼ぶ。対象は`update_edge_inline_property_at_handle`であり、[共通commit](../../crates/graph/src/facade/store/edge_profiles.rs#L49-L175)のschema幅検証、counterpart解決、canonical先行の両端fold・値更新を含む。APIの各内部段階を個別には計測していない。

区間にはResultを保持する小さな処理とSDKの計測境界が含まれる。成功確認の`expect`、全行・sidecar・placementの検査、tail/free/allocatedの取得、領域の読戻し、ログ出力は区間外である。先行する別の更新、手動fold、raw allocatorによる消費は加えていない。ログは全assertionの成功後に出す。

更新後にはP・Cの値slab slot数がdegreeに達したことを検査する。入力から作ったモデルを使い、全rowのslot・neighbor・label・値、targetと無関係なsidecar、counterpart経由のtarget sidecarを照合する。guardのplacement、reverse側の集計、backing capacityが変わらないことも確認する。最後にP・C・guardの全payloadを、重ならない既知の範囲から読む。setupとこれらのpostconditionが移動前と同じことは[照合結果](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/checks.json)にも記録した。0377の反例4件は再実行したとは扱わない。

[fit / no-fitのentrypoint](../../crates/graph/src/facade/store/inline_update_fixture.rs#L286-L296)は別々にした。`GraphStore::new()`は状態を初期化しないため、同じ状態で2条件を続けない。canbench-rs 0.7.0のraw macroはqueryをexportする。今回はそのローカルquery harnessで測り、各queryで初期tail0・更新直前free0などのassertionが通った。replicated updateのingress、合意形成、Candidのdispatch/返却処理は計測していない。

### 再現手順と検証結果

Rは`/Users/yota/dev/gleaph-spill`、branchは`spill-swap`、HEADは`880ad86a024e30953bbc134ad946b2c2e7b6da67`。Eは`/Users/yota/.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/`である。canbench CLI / SDKは0.7.0、Rustは1.100.0-nightlyを使った（[詳細](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/environment.log)）。

計測はRの`crates/graph`から次を3回実行した。既存の[canbench.yml](../../crates/graph/canbench.yml)でrelease wasm32をbuildし、2ケースだけを選択した。`canbench_lara`、instruction tracing、runtime整合性チェックの無効化は使っていない。

```sh
canbench graph_undirected_inline_update_ --show-canister-output
```

cargo/canbench実行中だけ従来のcommon lockを借り、終了ごとに元の`Cargo.lock`へ戻した。各コマンドの使用lockと前後のsource hashは[実行台帳](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/commands.json)にある。上限は1コマンド300秒、累計590秒で、最初のcanbenchはbuildを含め約163秒、後の2回は約8秒と7秒だった。外部terminalへの切替えや別targetでの再buildはしていない。

| 検証 | 結果 |
|---|---|
| `cargo test -p gleaph-graph --lib facade::store::tests:: -- --nocapture` | 変更前後とも56 passed / 11 failed、全67件の名前・結果・重複数が一致。更新10件とmissing-occurrenceの1件はpassed |
| `cargo clippy -p gleaph-graph --lib --tests --message-format=json` | 前後ともexit0。Rust診断は同じ重複`#[test]`のwarning1件、新規0 |
| `rustfmt --edition 2024 --check --config skip_children=true`（変更したRust 3ファイル） | passed |
| canbenchの選択2ケース × 3回 | 全6観測成功。すべての検査後に、命令数と同じ更新の領域収支を記録 |
| 機械的な移動照合、Lizard | setup/postconditionは同一。近隣テストの変更は未使用引数の除去だけ |
| 独立Standards / Specレビュー、計画・最終scope検証 | [Standards](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/standards-v1.md) / [Spec](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/spec-v1.md)ともAPPROVE。読取り専用呼出しの[再監査](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/review-audit.json)と最終検証もpassed（plan-final.log / final.json）。最初の監査はdict.fromkeysの分類漏れで失敗し、全呼出しを確認して監査側だけ修正した |

広めに選んだnativeテスト群は**変更前から失敗している**。内訳はcompaction・slot位置・sidecarを検査する10種類で、重複登録された1種類を含め11件となる。[変更前ログ](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/native-baseline.log)と[変更後ログ](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/native-final.log)に残した。失敗するassertionは削除・弱化していない。strict clippyは0378では実行せず、0377で失敗した重複属性も残した。canbenchのbuildにもmanifest等のwarningがあるため、全検証成功・warning-freeとは報告しない。

Lizard 1.24.0の比較基点は開始時のdirty worktreeで、HEADとの差分全体ではない。共有fixtureのNLOCは216→249、CCNは14→16だった。二つのclosureを足した計測用コードの字句的な増加を含み、値や領域の検査条件は同じである。新しいbench関数は各NLOC3 / CCN1で、W2 helperの未使用引数は2→1へ減らした。moduleを分けて同じ検査を二重に持つことは避けた。[詳細](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/lizard-summary.json)

変更したworktreeの範囲はRust 3ファイル、0376/0377の参照・後続状態、この報告、ignoredな計画に限る。開始時275 dirty pathsのうち272を保護した。`store.rs`は開始時cleanで、追加は共有fixtureとこの報告の2ファイルである。HEADと空のindexは維持した。

nativeのreuseケース数は2→2、そこでの頂点数も計6のまま。新しいbenchは2ケースで、それぞれ3頂点を作る。PocketICのアプリテストやfederation install helperは追加していないが、canbench自身のローカルruntimeは実行した。`--persist`を使っていないため、`canbench_results.yml`は変更していない。全benchの実行・成果物の更新は今回の範囲外である。

## 限界：準備済み状態のAPIコストであり、再利用だけの差分ではない

事前読出しを含むfixtureで、一つの未fold状態を一度更新した結果である。cold-start、継続的な更新列、実入力分布、inline index付きの再利用、別の値幅や大きなdegreeを代表しない。fold単体のコスト、全GQL query、upgradeやtrap rollback、spill arenaの解放量も測っていない。

nativeの既存失敗は未解決で、今回の範囲で正当化・修復したものではない。日本語の機械lintは、確認したPythonでSudachiPy / sudachidict_coreを利用できないため未実施とし、環境追加なしで手動チェックを行った（[環境](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/language-environment.json)、[チェック記録](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/language-review.md)）。

## So What：更新コストと追加bytesの組を得た。周期の判断は別の問いになる

0377の成立確認に加え、「P / C degreeが41 / 17なら約32.1万命令で更新し、Cの34Bを既存領域から賄った」と記録できた。再利用を成立させるために、先行する別のfoldを呼ぶ必要はこの条件ではなかった。

次に再利用の純粋な性能差を問うなら、degreeやrun状態をそろえた比較条件が要る。実運用での効果やfold周期を問うなら、更新履歴と呼出し頻度が別途必要になる。0375の別系列との百分率、自動fold・閾値の提案は行わない。productionの動作・API・schema・layoutと依存関係は変更せず、commit/pushも行っていない。
