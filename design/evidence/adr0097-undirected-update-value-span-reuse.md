# 無向辺の通常更新1回で、canonical側の退役領域をcounterpart側が使った

**状態: Experimental。0377のnative検証・独立レビュー・最終構造検証完了。strict clippyは既存の重複test属性で失敗。**

後続の[0378](adr0097-graph-undirected-update-cost.md)では、同じfixtureのGraphStore API区間をcanbenchで測定した。以下の実行範囲・結果は0377のnative検証を指す。fixture本体は0378でtest / canbench共有moduleへ移した。

GraphStoreの通常更新APIを1回呼び、canonical側が退役した64Bの領域からcounterpart側が34Bを取得することを確認した。退役領域が16Bしかない別条件では、34B要求をそこに収めず末尾へ割り当てた。両端の値・slot対応、他の辺、sidecarは保たれた。**手動foldやraw allocatorによる消費を挟まずに再利用する経路は、このnative fixtureで確認できた。** 実負荷での頻度やquery全体の性能は測っていない。

[0376のソース監査](adr0097-query-update-workload-audit.md)で挙げた仮説を、既存の無向辺更新テストを広げて実行した。今回の母集団は2-byte profileを使う合成fixtureの2条件だけである。通常のGraphStore操作で状態を作ったという意味であり、実運用を代表する負荷だという意味ではない。

## 結果：適合する領域があれば、counterpartの要求分は末尾に追加されなかった

`P`は大きい頂点IDを持つcanonical側、`C`は小さい頂点IDのcounterpart側を指す。両側ともforward allocatorを使う。表の単位はbytesで、tailは値slabの占有末尾である。

| 観測・要求 | fit | no-fit |
|---|---:|---:|
| Pの旧slab | 64 | 16 |
| Pのfold要求全体 | 82 | 34 |
| Cの旧slab → 要求全体 | 18 → 34 | 18 → 34 |
| 更新直前のfree | 0 | 0 |
| tailの更新前 → 後 | 84 → 166 | 36 → 104 |
| tail増分 | 82 | 68 |
| 更新後のfree（最大spanと同量、1本） | 48 | 34 |
| 更新後のlive / allocated | 118 / 118 | 70 / 70 |
| 収支と読戻しで確認した再利用量 | 34 | 0 |

fitでは、tail増分82BがPの要求だけに一致する。Cの全34Bは旧P領域の先頭`[0,34)`で読めた。no-fitではPとCの要求を合わせた68Bが末尾に追加され、Cの全データは`[70,104)`で読めた。no-fitの更新後には合計34Bがfreeになるが、Cが割り当てを受ける時点では旧Cの18Bはまだ退役していない。

再利用はallocatorのイベントログから数えたものではない。初期freeを0に固定し、tail/free/allocatedの収支、GraphStoreからの全行読戻し、既存のread APIによる値領域の内容を突き合わせた。P、C、guardの全payloadを、fitでは`[84,166)`・`[0,34)`・`[82,84)`、no-fitでは`[36,70)`・`[70,104)`・`[34,36)`で確認した。どちらも生きた領域は重ならない。

2条件はPのdegreeが異なるため、互いの性能比較には使わない。backing capacityは各条件の更新前後で同じだったが、物理メモリ削減を測ったわけではない。結果の原本は下記Eの`native-final-r2.log`、assertionは[fixture全体](../../crates/graph/src/facade/store/inline_update_fixture.rs#L26-L284)にある。

## 方法：通常insertで値runを作り、元のhandleを一度だけ更新した

[既存のW2 profile](../../crates/graph/src/facade/store/inline_update_fixture.rs#L16-L24)を使い、低ID・高ID・guardの3頂点を先に作った。高ID側に32本または8本のself-loop、低ID側に8本を挿入し、両端を結ぶ対象辺を追加する。別頂点のguard self-loopでslab末尾をずらしてから、両端に8本ずつself-loopを足した。すべて既存のGraphStore insert APIで行った。

更新直前のPは「degree41 / slab32 slot」または「degree17 / slab8 slot」、Cはどちらも「degree17 / slab9 slot」だった。slabを越える値が実際に読めることも確認し、両側に値runがある前提を置いた。古いoverflow-logのhead/lenフィールドを値runの証拠にはしていない。fitはlogical edge58本・格納row59本、no-fitは34本・35本である。

モデルは挿入入力から`(slot, neighbor, 2-byte value)`を記録する。対象辺のslotはP側32または8、C側8で、いずれも最後のslotではない。他の辺は識別できる異なる値にした。targetと無関係なself-loopにsidecarを置き、sidecar操作で値storageが変わらないことも更新直前に確認した。

元のhandleに`update_edge_inline_property_at_handle(..., &[8,0])`を一度呼ぶ。更新後は全頂点の全無向rowをモデルと照合し、slot・neighbor・値・labelを検査した。両端の値slab slot数がdegreeに達すること、edge側のplacement情報とguardが変わらないこと、targetと無関係なsidecar、およびcounterpart経由で読むtarget sidecarが保たれることも確認した。reverseの値storage集計は更新前後で同じだった。

共有commitがcanonicalを先に処理する点は[edge_profiles.rs](../../crates/graph/src/facade/store/edge_profiles.rs#L132-L151)、無向辺のforward/forward対応は[counterpart.rs](../../crates/ic-stable-lara/src/labeled/bidirectional/counterpart.rs#L309-L405)にある。値の公開・退役と再割当は[LARAの既存owner](../../crates/ic-stable-lara/src/labeled/graph/values.rs#L1217-L1395)が引き続き担う。今回追加したのはテスト側の観測だけである。

### 同じ最終値でも、再利用しない順序なら失敗した

一時的にテストだけを変え、fitの1件を実行した。4件ともコンパイル後のruntime assertionで失敗し、各変更は元に戻した。production codeは変えていない。

| 反例 | 失敗した観測 |
|---|---|
| 更新を省略 | Pのslab slot数が32のまま。期待は41 |
| 対象handleをPのself-loopへ差し替え | Cのslab slot数が9のまま。期待は17 |
| guardの挿入を省略 | guard直後のtailが82。期待は84 |
| Cのself-loopを同じ値で先に通常更新する余分な呼出しを追加 | 最終値・placement・sidecarの検査は通るが、tailが200。期待は166 |

最後の反例ではCがPの退役前にfoldするため、最終値が正しいだけでは合格しないことを確かめられた。これは追加の通常更新による反例であり、productionのcommit順序を書き換えた試験ではない。証跡は`mutations.json`と各`mutant-*.rs/.log`。その後、復元した最終ソースで10件を再実行し、すべて通った。

### 実行した検証と、変更しなかった境界

作業先Rは`$REPO`、branchは`spill-swap`、HEADは`880ad86a024e30953bbc134ad946b2c2e7b6da67`。Eは`$GLEAPH_EVIDENCE/adr0097/undirected-update-reuse-0377/`である。Eの`run.py`で各コマンドを上限300秒、累計590秒以内に制限し、`commands.json`へ記録した。

| コマンド・対象（Rから実行） | 結果・ログ |
|---|---|
| `cargo test -p gleaph-graph --lib facade::store::tests::updating_ -- --nocapture` | 変更前9件passed（`native-baseline.log`）。最終10件passed（`native-final-r2.log`） |
| `cargo test -p gleaph-graph --lib facade::store::tests::inline_property_update_fails_closed_on_missing_occurrence -- --exact --nocapture` | 別の1件passed（`native-missing-occurrence.log`） |
| 反例4件：同じcrate/libの既存無向辺テストを`--exact --nocapture`で実行 | 各1件が期待したassertionで失敗。起動先の誤記による1回の起動前失敗は反例検出に含めず、修正後の別ログを使った |
| `rustfmt --edition 2024 --check crates/graph/src/facade/store/tests.rs` | passed（`format-final.log`） |
| `cargo clippy -p gleaph-graph --lib --tests --message-format=json` | exit0。変更前後ともRust診断は重複`#[test]`のwarning1件で、新規診断0（`diagnostics.json`） |
| `cargo clippy -p gleaph-graph --lib --tests -- -D warnings` | failed。変更前2442行、変更後2647行の同じ重複属性が原因（`clippy-strict.log`）。既存箇所は変更していない |
| 計画・文書参照・差分・保護hash、独立レビュー | passed（`before-review-r2.log`、`plan-final.log`、`final-scope.log`）。Standards/Spec双方APPROVE（`standards-v1.md` / `spec-v1.md`）。読取り専用の呼出し監査もpassed（`review-audit-r3.log`） |

最終の選択テストは合計11件である。`updating_`の10件には既存の有向・無向self-loop、平行辺、indexed scalarの置換・同値更新、struct leaf、幅不一致拒否も含まれる。今回広げたfixtureは1→2、そこでの頂点作成は2→6。GraphStoreの状態はthread-localなので、fit後のfreeがno-fitを隠さないよう別テストにした。PocketIC構築、canister/federation install、canbench実行はいずれも0である。

変更対象は`tests.rs`の無向辺テスト周辺、この報告、0376報告の後続状態と参照位置、ignoredな計画だけ。開始時273 dirty pathsのうち、後続状況を追記する0376報告以外の272パスを保護した。`tests.rs`は開始時cleanのtracked fileである。cargo中だけ従来のcommon lockを借り、終了ごとに元の`Cargo.lock`へ戻した。計測用API・型・module・dependencyは追加せず、fixture helperは1引数で2ケースを共有する。旧単純テストの両端更新契約は全行検査に包含した。

[ADR0097の値slab退役記述](../adr/0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md#L204-L209)と[LARAの公開・退役契約](../storage/lara.md#L210-L228)は変わらない。production behavior、layout、public API、schema、自動fold、閾値、benchmark成果物は変更していない。commit/pushも行っていない。日本語の機械lintは既存環境の依存不足で未実施とし、手動チェックを行った（`language-review.md`）。

## 限界：GraphStoreの成立例であり、queryの速度や実頻度ではない

検証したのはnativeのGraphStore→LARA境界である。Router/GQL全体、PocketIC、upgrade、trap時のrollback、幅8/33、indexedなW2再利用、spill arena自体の解放量は今回の対象外とした。一般的なallocatorの正しさを2条件で証明したとも主張しない。

再利用を観測した合成条件から、実trafficでの更新比率、再利用率、物理メモリ削減、query命令数は求められない。0375の別条件のcanbench値と比較した百分率も出さない。strict clippyが失敗しているため、Rust検証全体が完全に通ったとは扱わない。

## So What：共通commitの成立確認は済んだ。次は測る必要のある負荷を決める

手動のproducer foldを前置きせずに退役領域を使えるか、という0376の仮説には、この2条件で答えられた。次にGQL実行やコストを測るなら、入力履歴または利用者が選ぶ負荷条件を先に定義する。その根拠がない段階で、自動foldの周期・閾値は選ばない。
