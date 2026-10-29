# Native maintenance の既存失敗は、fixture と読取り不整合に分けて直す

**状態：監査完了。修正・Rust再検証は未実施。**

## 要約：10件のうち7件は物理配置の前提、2件は tiny 読取り、1件は未確定

0378で残った native test の失敗を調べた。**最初に失敗する検査で分類すると、7件は古い fixture 前提、2件は実装の読取り不整合、1件は原因未確定**である。7件にも未検証の後続条件が残るため、期待値の置換だけで閉じてはいけない。次は tiny bucket の論理スロットを返す既存の読取り境界を直し、その後に実移動を伴う maintenance fixture を整える。

対象は `facade::store::tests` の10個のテスト関数、11個の失敗登録である。hysteresis 超過テストの二重 `#[test]` が1登録を増やしている。0378の2回の実行はいずれも67登録中56成功・11失敗だった。今回の0379はそのログと凍結ソースを読む監査であり、修正も Rust コマンドの再実行もしていない。

[0378の測定報告](adr0097-graph-undirected-update-cost.md)で確認した更新コストは変更しない。本稿は、その測定時から存在する失敗を次の修正単位に分ける。

## 結果：移動しないことと、正しく読めないことを区別する

### 現在の storage 契約では、drain 完了が slab 化を保証しない

[ADR0097 §7](../adr/0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md#L245-L277)は、辺の論理位置を変えた場合に全移動を通知する一方、移動を伴わない完了を認める。`SlackMayBeDropped` では、収容できない spill run を残してよい。tiny と tree は slab 用の left-pack / swap の対象外である。

実装でも [compact の共有入口](../../crates/ic-stable-lara/src/labeled/graph/compact.rs#L1942-L2064)がこの区別を所有する。run を残した bucket は移動選択と `stored = degree` 更新を行わない。[選択条件](../../crates/ic-stable-lara/src/labeled/graph/compact.rs#L1698-L1764)には、swap の場合、辺の run がなく、値も幅0または degree 分の密な prefix にあることが含まれる。

[Graph の finalize](../../crates/graph/src/facade/store/maintenance.rs#L155-L215)も同じ enqueue / drain に入る。別の強制 fold API ではない。また [ADR0052 §7–9](../adr/0052-per-label-adjacency-order-and-tombstone-reuse.md#L300-L355)の Unordered は並べ替えの許可であり、すべての drain で swap する保証ではない。Insertion の生存辺の相対順序、移動時の属性・index 整合性は引き続き必要である。

### 全10件の対応表

行番号と値は0378の `native-final.log` に対応する。「前提」は最初の検査が要求する物理配置を fixture が保証しないという分類であり、後続の整合性まで正常だという判定ではない。

| ID | テスト関数（`facade::store::tests::` 以下） | 最初の失敗：実測 → 期待 | 分類 |
|---|---|---|---|
| F1 | `forward_edge_compaction_moves_property_sidecars` | L1337、third の slot `2 → 1` | 前提 |
| F2 | `unordered_compaction_swap_publishes_move_and_moves_sidecars` | L1467、third の slot `2 → 0` | 前提。実行分岐は未観測 |
| F3 | `insertion_policy_maintenance_preserves_left_pack_order` | L1623、third の slot `2 → 1` | 前提。実行分岐は未観測 |
| F4 | `unordered_captured_at_enqueue_drains_without_ambient_table` | L1734、`[second, third] → [third, second]` | 前提。実行分岐は未観測 |
| F5 | `maintenance_without_resolved_table_compacts_order_preserving` | L1813、third の slot `2 → 1` | 前提 |
| F6 | `swap_compaction_rekeys_inline_scalar_index_to_new_slot` | L1979、slot 2 の Remove がない。pending は削除した slot 0 の Remove のみ | 未確定 |
| F7 | `reverse_edge_compaction_preserves_canonical_sidecars` | L2051、property 読取りで `CounterpartNotFound` | 実装不整合 |
| F8 | `unordered_default_reuses_tombstoned_slot` | L2400、再挿入前の delete が `EdgeNotFound`（owner 0、label 32768、slot 1） | 実装不整合 |
| F9 | `delete_past_hysteresis_enqueues_span_compaction_and_left_packs` | L2467、`stored_edge_slots: 5 → 7`（2登録） | 前提 |
| F10 | `delete_below_hysteresis_leaves_tombstones_for_append` | L2525、`stored_edge_slots: 5 → 16` | 前提 |

#### F1・F5：tiny の survivor を slab のように左詰めする想定が古い

[fixture](../../crates/graph/src/facade/store/tests.rs#L1273-L1356)は4-byte edge、inline 値なしの同一 label に3辺を入れ、最初の辺を削除する。F5も[同じ3辺の形](../../crates/graph/src/facade/store/tests.rs#L1755-L1830)で、maintenance の policy table を使わない。

通常 label の bucket は [tiny として生成](../../crates/ic-stable-lara/src/labeled/graph/insert.rs#L887-L950)される。tiny の削除は [used width を保って対象だけを tombstone にする](../../crates/ic-stable-lara/src/labeled/graph/remove.rs#L316-L373)。したがって `[T, second, third]` の third は slot 2 に残り、共有 compact 入口は tiny を `AdvanceBucket` で通過する。slot 1 への移動要求は現契約と合わない。

後続の property 検査は未到達である。後の修正では、tiny no-op 時に同じ handle の属性33が残る検査と、実際に slab 行を移動させて sidecar が追随する検査を分ける。F1の stale-handle rejection は「実移動で旧 slot が非 live になった」場合に保持する。F5の無指定 policy が順序を壊さない検査も残す。

#### F2・F3・F4：事前 drain を、run-free の証拠にしている

[F2](../../crates/graph/src/facade/store/tests.rs#L1358-L1509)、[F3](../../crates/graph/src/facade/store/tests.rs#L1511-L1636)、[F4](../../crates/graph/src/facade/store/tests.rs#L1638-L1753)はW2付き3辺を作り、「pre-fold」drain 後に1辺を削除して再度 drain する。fixture は辺・値の run が消えたことを検査していない。現在の非強制 fold では、drain しただけでは swap / left-pack の前提を満たせない。

F2は残った slot、F3は物理的な左詰め、F4は並べ替えそのものを要求して失敗している。F3の相対順序検査は未到達。F2の inline 値33・sidecar33・旧 handle の検査、F4の移動先 slot 検査も未到達である。

policy の消失を裏付けるソースは見つからなかった。[enqueue](../../crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs#L1572-L1605)が label ごとの policy を保存し、[drain と継続 enqueue](../../crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs#L3535-L3642)がその map を使う。Dense → Span も同じ map を引き継ぐ。ただし既存 pending item があると capture 前に dedup するため、特定の table で capture したという試験には enqueue 前の queue 状態も必要である。

**未確定なのは当該実行の分岐**である。ログには fold の成否、残存 run、移動 event、stalled / pending item がない。後の fixture では、既存の storage API で作った slab-only 状態を最初に確認し、Insertion capture / Unordered capture / table 不在を分けて検査する。別に、run を保持する合法な no-op でも辺・値・順序が保たれる検査を置く。`2`を新しい期待 slot にするだけでは coverage を失う。

#### F6：index の Remove 不在だけでは、observer の欠落と決められない

[F6の fixture](../../crates/graph/src/facade/store/tests.rs#L1833-L1991)はF32値1.5・2.5・3.5を入れ、pre-fold の pending を捨て、最初の辺を削除して drain する。失敗ログには、削除した1.5の slot 0 Remove だけがある。third が slot 2から0へ移ったという前提はコメントにしかなく、topology の移動検査はない。

[LARA drain](../../crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs#L3562-L3605)は `EdgeMoved` と `OverflowRewrite` の各移動を observer に渡す。[GraphSidecarMoveObserver](../../crates/graph/src/facade/store/helpers.rs#L24-L50)は forward のみを [relocate_edge_properties_for_move](../../crates/graph/src/facade/store/sidecar.rs#L36-L136)へ渡し、旧・新 slot の index 操作を生成する。reverse は canonical sidecar の所有者ではない。delete の移動結果も同じ relocation 関数に入る。

「移動しなかったので rekey がない」と「移動したのに通知・値読取り・posting のどこかで欠けた」は、このログだけでは区別できない。最小の後続 witness は、移動前後の topology、exact move event、値3.5、physical index 106・epoch 1・property 911・owner・label・slot・payload を揃えた Remove/Insert 一式である。削除分は別に検査する。`any(Remove at 2)` を弱めたり、通知を Graph 側で推測生成したりしない。

#### F7：tiny の live 件数を検索幅に使うため、穴の後ろを見落とす

[F7](../../crates/graph/src/facade/store/tests.rs#L1994-L2083)の reverse bucket は3辺の先頭を削除した形になる。別 label の辺は別 bucket に属する。tiny には slot 1・2の2辺が残るが、used width は3のままである。Dense maintenance から Span に進んでも tiny の論理位置は変わらない。

ところが [select_live_edge_by_neighbor_ordinal](../../crates/ic-stable-lara/src/labeled/graph/traverse.rs#L424-L493)は placement 情報の `stored=0, spill=0` を見て、検索幅を `degree=2` にする。[placement 情報](../../crates/ic-stable-lara/src/labeled/graph/bucket.rs#L774-L817)の0は「slab を占有しない」という意味であり、tiny の論理幅ではない。検索範囲 `0..2` は slot 2の third を含まない。property 読取りは [counterpart_of](../../crates/ic-stable-lara/src/labeled/bidirectional/counterpart.rs#L335-L407)で失敗し、sidecar 本体の値に到達しない。

同じ幅の再構成は [count_preceding_live_edges_with_neighbor](../../crates/ic-stable-lara/src/labeled/graph/traverse.rs#L326-L389)にもある。修正は select だけへの条件追加で終えず、rank/select 両方を、すでに tiny の used width を扱う [visit_edges](../../crates/ic-stable-lara/src/labeled/graph/traverse.rs#L790-L856)と同じ論理走査契約へ寄せる必要がある。verified counterpart はその visitor を使う別経路であり、fast / verified の比較も必要になる。

エラーの `slot_index=u32::MAX` は [候補がないときの診断用値](../../crates/ic-stable-lara/src/labeled/bidirectional/counterpart.rs#L230-L257)である。不正な handle が保存された証拠ではない。runtime の descriptor dump はないが、fixture → tiny 生成 → tombstone 保持 → 検索上限のソース経路が観測エラーを説明する。後続の reverse handle 検査は未到達で、F8と同じ読取り問題にも注意が要る。

#### F8：再利用の検査より前に、visitor が slot 1を失う

[F8](../../crates/graph/src/facade/store/tests.rs#L2373-L2409)は source 0、target 1に同じ default label の3辺を入れる。source は末尾頂点ではないため [homogeneous bypass の条件](../../crates/ic-stable-lara/src/labeled/graph/bypass.rs#L22-L32)を満たさず、inline 値なしの tiny bucket を使う。finalize の Span drain は tiny を移動しない。

[visit_edges_for_label_impl の tiny 両方向](../../crates/ic-stable-lara/src/labeled/graph/traverse.rs#L2263-L2294)は `E::read_from` の結果に label だけを付け、slot を付けていない。[Graph の Edge decode](../../crates/graph-kernel/src/entry/edge.rs#L88-L102)は slot 0で初期化する。したがって3辺とも slot 0として [lookup_forward_out_edge](../../crates/graph/src/facade/store/lookup.rs#L13-L49)へ渡り、要求した slot 1に一致しない。逆向きの lookup も同じ visitor に依存する。[delete の入口](../../crates/graph/src/facade/store/adjacency.rs#L48-L73)は書込み前に `EdgeNotFound` を返す。

一方、[inline-property-ref visitor の tiny 経路](../../crates/ic-stable-lara/src/labeled/graph/traverse.rs#L1977-L2027)と topology visitor、point read は slot を付ける。同じ tiny decode / traversal の契約が複数箇所で食い違っている。既存の正しい走査経路へ統一するのが修正境界であり、Graph lookup ごとの slot 補正は不要である。forward / reverse、昇順 / 降順、slot 0以外、穴と並列辺を含めて比較する。

再挿入も「slot 1を再利用した」という assertion も実行されていない。再利用失敗とは分類しない。後の修正ではこの assertion を維持し、tiny と slab の再利用を別の前提で検査する。

#### F9・F10：`stored_edge_slots` は総辺数ではない

[2つの hysteresis fixture](../../crates/graph/src/facade/store/tests.rs#L2412-L2530)はInsertion label に16辺を入れ、finalize 後を16-slot slab とみなして9辺または8辺を削除する。だが finalize は run-free を保証しない。実測はそれぞれ degree 7・8の検査を通過した後、prefix の幅5で失敗している。

[remove-side gate](../../crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs#L1607-L1696)の算式は `stored >= degree && stored - degree > stored / 2` である。degree に spill の live rows が含まれる状態では、`16 - 削除数` から slab tombstone 数を決められない。幅5はrunを残す配置と整合するが、ログにrun長や削除前の配置はない。閾値超過・非超過をこの2件が実証したとはいえず、run-backed churn に対する gate の十分性も本監査では証明していない。

後続 fixture は削除前に slab-only・stored 16を確立し、8削除時と9削除時の queue admission / drain、および残存辺のID・順序を検査する。別に run-backed delete の読取り整合性を検査する。期待値7・16を観測値5へ置換して済ませない。二重登録の除去は後の test 修正に含め、期待実行件数を明示する。

### active design 文書にも同期漏れがある

[bulk-ingest-finalize.md](../storage/bulk-ingest-finalize.md#L21-L71)には旧 log の dense 条件や、native full drain で reclaimed state を観測できるという説明が残る。queue 完了と run-free を区別するADR0097 §7との同期が必要である。[ADR0052のSlice 6説明](../adr/0052-per-label-adjacency-order-and-tombstone-reuse.md#L92-L104)も「table がなくても swap する」と読めるため、適用可能な slab という条件を明記する必要がある。

ADR0097は冒頭が `proposed` のまま、§7に implemented の追記を持つ。本稿は該当追記と実装を照合し、ADR全体が完成したとは扱わない。これらは今回発見した文書上の課題であり、契約を勝手に変更しない。実装・test を直す後続 slice で、関連節の現行条件と計画部分を同期する。

## 方法：2回の既存ログを、同じソース状態に結び付けて読んだ

監査対象は `spill-swap` の HEAD `880ad86a024e30953bbc134ad946b2c2e7b6da67` と、0378完了時の278 dirty pathsである。主な証拠は次のとおり。

- [0378 native-final.log](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/native-final.log)、[native-baseline.log](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/native-baseline.log)：元の失敗、最初のpanic、67件の登録結果。
- [0378 commands-snapshot.json](../../../../.local/share/gleaph/evidence/adr0097/graph-undirected-update-cost-0378/commands-snapshot.json)：`cargo test -p gleaph-graph --lib facade::store::tests:: -- --nocapture` は各回 exit 101。共通の実行用lockに結び付いている。0379はlockを借用していない。
- [0379 inventory.json](../../../../.local/share/gleaph/evidence/adr0097/native-failure-audit-0379/inventory.json)：ログのstatus多重集合、10関数・11失敗、panic位置、現在のsource hash、保護対象を照合。0378で別の更新fixtureを抽出したため、2回のsourceファイル全体のhashは同一ではない。最終runと現在のsourceを結び付け、失敗テスト群の保存は0378の検査記録で確認した。

各テストを全体で読み、setup → canonical mutation / read → maintenance → 最初の失敗の順で所有関数を追った。初期仮説と反証条件も区別した。

| 仮説 | 調べた予測 | 結果 |
|---|---|---|
| slab-only / 必須移動という fixture 前提が古い | tiny除外・非強制foldがあれば、drain完了だけから移動を要求できない | F1–F5・F9–F10の検査前提に該当。F2–F4の実分岐は未観測 |
| enqueueした policy が drain で失われる | map を保存しない、継続で落とす、ambient tableから再解決する経路がある | 調べたSpan / Dense両経路にはない。dedup前提は次のfixtureで検査する |
| canonical位置とsidecar / index消費が食い違う | 実移動と通知・derived stateの差を観測できる | F6は観測不足。F7・F8では、derived stateより前のtiny読取りに具体的な不整合がある |

日本語は依存不足の機械lintを再試行せず、手動チェックリストで確認した。[language-environment.json](../../../../.local/share/gleaph/evidence/adr0097/native-failure-audit-0379/language-environment.json)と[確認記録](../../../../.local/share/gleaph/evidence/adr0097/native-failure-audit-0379/language-review.md)に区別を残す。

**監査状態：完了。** 初回の Standards / Spec は各225秒で打ち切られた。[実行記録](../../../../.local/share/gleaph/evidence/adr0097/native-failure-audit-0379/independent-reviews.log)と[診断記録](../../../../.local/share/gleaph/evidence/adr0097/native-failure-audit-0379/review-diagnostics.log)は保存したまま、継続依頼後に重複出力を省いて再開した。[Standards](../../../../.local/share/gleaph/evidence/adr0097/native-failure-audit-0379/review-r2/standards.md)・[Spec](../../../../.local/share/gleaph/evidence/adr0097/native-failure-audit-0379/review-r2/spec.md)とも最終判定は APPROVE。[レビュー監査](../../../../.local/share/gleaph/evidence/adr0097/native-failure-audit-0379/review-r2/audit.json)で各1回のfile-only読取りを確認した。監査用許可リストの修正も[記録](../../../../.local/share/gleaph/evidence/adr0097/native-failure-audit-0379/review-r2/policy-correction.json)し、レビュー結果や本文の分類は変更していない。[最終計画検査](../../../../.local/share/gleaph/evidence/adr0097/native-failure-audit-0379/plan-final-r2.log)と `review-r2/final.json` に完了証拠を残す。ソース、テスト、既存文書、lock、benchmark artifact は変更していない。

## 限界：最初のpanicより後を成功扱いしない

mode / prefix / run / queue / move event のruntime計測は追加していない。F7・F8の原因はログに一致するソース経路からの結論であり、修正後のgreenや新しい最小再現はまだない。F6は特に、移動がなかったこともobserver欠落も断定できない。

既存テストの再実行、fmt、clippy、PocketIC、canbench、mutant、benchmark artifact更新はすべて未実施である。56件の既存成功も、未到達だった各assertionの代用にはしない。新しいtest関数、PocketIC構築、canister installは0件。性能やcanister lifecycleへの結論は追加しない。

## So What：tiny の読取りを先に統一し、実移動の検査を取り戻す

最初の修正候補は **LARAのtiny論理走査契約**である。F7のrank/selectとF8のedge-only visitorを、既存のused-width・slot付き読取りに揃える。両症状をGraph側の個別補正で隠さず、同じ契約を使う対称経路を先に整理する。canonical propertyの44、正しいreverse handle、並列辺のdelete/reuseを回帰条件にする。

その後、no-opが合法なmaintenanceと、実移動を確実に発生させるfixtureを分ける。F6はこの移動の観測を得てから通知側の修正要否を決める。fixtureの更新には生存辺の同一性、値、順序、sidecar、exact postingを残す。今回の成果は失敗の分類と修正境界であり、11失敗の解消ではない。
