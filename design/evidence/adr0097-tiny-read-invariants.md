# Tiny bucketの走査幅とスロット情報の修正

## 要約：F7・F8を修正し、tiny走査の命令数も戻した

[前回監査](adr0097-native-maintenance-failure-audit.md)のF7・F8をLARAの読取処理で修正した。
削除穴の後ろに残った辺を元のスロットで読める。元の2テストは、値44の読出し、
逆向きハンドルの解決、削除後のスロット再利用まで通った。

初回の共通化では命令数が約1.34Mから約1.46Mに増えたが、再調査で読取規則を重複させずに回復した。
`tiny_scan_d2_1024`は再調査前の1,464,228から1,332,111命令へ減った（約9.02%減）。
これは穴のないdegree2・1024 bucketの比較に限る。初回修正前の1.34Mは丸め表示なので、
その状態に対する正確な改善率は出していない。

Graph storeは68件中59件成功、既存の9件失敗のまま。strict lintも既存診断で失敗している。
前回の独立レビューで出たP2のテスト不足を、同じGraph fixtureに26行追加して補った。
先行する穴を埋めた後、スロット2を削除して2へ再挿入する。追加した各段階を両方向・両順序で検査し、対象テストは成功した。
補強後の差分はStandards・Specの両担当が承認し、計画の最終構造・範囲検査も通った。0380の限定範囲は完了とする。
既存のテスト／lint失敗は残り、crate全体やbranch全体の検証完了とはしない。commit・stagingは行っていない。

対象は`spill-swap`の0380、基準HEADは`880ad86a024e30953bbc134ad946b2c2e7b6da67`。
初回0380と0379の外部証跡は凍結し、性能の再調査は`resume-r1/`、前回のレビューは`review-r2/`・`review-r3/`に分けた。
今回のP2補強と再検証は`reuse-r1/`に置く。旧台帳・入力・判定は変更しない。
commit、push、stagingは行わない。fixture前提、F6 rekey、cursor、既存lintの修正も対象外とする。

## 結果：穴の後ろの辺を元の位置で読める

### 走査幅をlive degreeから復元しない

tiny bucketは削除済みスロットを穴として残す。4本の並列辺から先頭2本を削除すると、
生きた辺は2本でも、走査すべき範囲はスロット0〜3のままである。
修正前のrank/selectはlive degreeを上限にしていたため、スロット2・3を見落としていた。

[traverse.rs](../../crates/ic-stable-lara/src/labeled/graph/traverse.rs)の
`visit_live_edge_slots_until`が、既存の`bucket_reserved_edge_slots`から範囲を取得する。
rankとselectにあった同じ上限計算を削除し、呼出側から上限を渡せなくした。
このアクセサはtinyで`tiny_used_width()`を返す。bypassとsingletonの短絡処理は維持した。

追加した`tiny_parallel_counterparts_keep_slots_after_holes`は両方向で検査する。
rankは1、selectは順に2・3・なし。通常のcounterpart解決と全件検証版も一致する。
さらにスロット2を削除し、唯一残ったスロット3を解決する。
同じfixtureで、穴がwindowの位置を消費することと、借用プロパティvisitorが最初の1件で
Breakすることも両順序で確認した。修正前はrankの期待値1に対して0を返して失敗した。

### 5つのtiny走査で、生存判定と位置付与を揃えた

辺だけを返すvisitorは、復号した辺にスロット番号を付けていなかった。
Graphの`Edge`では番号が0のままになるため、非ゼロのハンドルによる削除が失敗していた。

`visit_tiny_bucket_edges`に、topology、借用プロパティ、辺のみ、window、inline iteratorの
5経路を集約した。tinyの位置解決と復号は`read_tiny_edge_state_at_slot`が行う。
全modeを受け付ける`read_edge_state_at_slot`のTiny armも、同じ処理を呼ぶ。
その先の`decoded_edge_state_at_slot`が、生存判定とスロット・ラベル付与を1か所で行う。
既知のtiny走査は各行で全modeの位置解決を呼ばずに済む。windowには既存の`TraversalWindow`を渡す。
public API、型、依存、保存形式は増やしていない。Graph側のlookupにも特例を加えていない。

`tiny_parallel_visitors_preserve_slot_identity`は実際のGraph `Edge`を使う。
両方向×昇順・降順で、位置・相手・ラベルの並びを検査した。
位置は`[0,1,2,3] → [2,3] → [3] → [0,3]`となり、最後は最初の穴の再利用である。
生存辺の値44と逆向きハンドルも維持された。修正前は4本ともスロット0になり、最初の昇順検査で失敗した。
この元の系列だけでは削除した2の再利用を確認しておらず、SpecからP2が出た。読取修正自体の誤動作の報告ではない。
全ての元のアサーションを残し、後ろに`[0,1,3] → [0,1,2,3] → [0,1,3] → [0,1,2,3]`を追加した。
最初の2段階は穴1・2を埋め、次に2だけを削除し、最後の挿入で返る位置を2に限定する。
各変化の直後に既存の検査を呼ぶ。新しい2の逆向きハンドル解決と、生存辺3の値44も両方向で確認した。

元の`reverse_edge_compaction_preserves_canonical_sidecars`と
`unordered_default_reuses_tombstoned_slot`は本文・期待値を変更していない。
前者の成功は読取整合性を示すが、tiny上の物理的なcompaction移動を証明しない。
合法なmaintenanceのno-opはそのまま認めている。

## 方法：正しさと命令数を別々に確認した

証跡は`$GLEAPH_EVIDENCE/adr0097/tiny-read-invariants-0380/`にある。
初回の`red-counterpart.log`、`red-visitors.log`が修正前の赤信号を記録する。
再調査の`resume-r1/initial.json`は旧証跡106ファイルと283 dirty pathsを固定した。
Rust実行では0378の共通lockを借り、終了時に元の`Cargo.lock`を復元して両方のhashを確認した。
結果はその依存解決に対するものであり、元のlockを直接ビルドした結果ではない。

以下は性能修正時に再実行した結果である。ログの位置は`resume-r1/`内、作業場所は`$REPO`である。
今回変更したのはGraphのnative test本文だけで、LARAソースと性能測定のWasmは同一のまま。今回のGraph実行結果は後述する。

| コマンド・対象 | 結果 | ログ |
|---|---|---|
| `cargo test -p ic-stable-lara --lib labeled::bidirectional::counterpart::tests::` | 16成功 | `counterpart-final.log` |
| `cargo test -p ic-stable-lara --lib labeled::graph::traverse::` | 44成功。107 bytes read / 0 writeのtiny検査も成功 | `traverse-final.log` |
| `cargo test -p gleaph-graph --lib facade::store::tests:: -- --nocapture` | 59成功、既存9失敗 | `graph-final.log` |
| `cargo clippy -p ic-stable-lara --lib --tests -- -D warnings` | 既存41診断で失敗 | `lara-clippy.log` |
| `cargo clippy -p gleaph-graph --lib --tests -- -D warnings` | 既存の重複`#[test]`で失敗 | `graph-clippy.log` |
| `rustfmt --edition 2024 --check --config skip_children=true`＋変更したRust 3ファイル | 成功 | `format-final.log` |
| `canbench tiny_scan_d2_1024`（`crates/ic-stable-lara`でビルド） | 最終候補を生成、1件だけ実行 | `bench-mode-local.log` |
| `canbench tiny_scan_d2_1024 --csv --less-verbose`（外部`measure-final/`） | 1,332,111命令、heap/stable増加0/0ページ | `measure-final.log`, `measure-final/canbench_results.csv` |
| `check_perf.py`＋最終CSV | 1,350,000命令以下の検査に成功 | `perf-green.log` |

初回はGraphの11失敗のうちF7・F8だけが成功に変わった。残る9失敗は8関数で、1関数が重複登録されている。
再調査でも失敗する登録は同じで、既存のテスト本文も維持した。
新設したnative testは2本。LARAの小さなメモリ内グラフ2個とGraph store 1個を使い、
PocketIC testやcanister installは追加していない。

性能の最初の候補は共通visitorへのinline指定だった。丸め表示は1.46Mのままで、追加指定を取り下げた。
次に、既知のtiny走査が各行で全modeのpoint-readを呼ぶ箇所をソースで確認した。
tinyの位置解決を分け、従来の生存判定・metadata付与は共通処理へ移した。
最終Wasmとソースhashは`wasm-binding.json`に記録した。同じWasmを2回再測定し、
最後のCSVも1,332,111命令だった。最初の再測定は1.33Mの表示ログのみを残したため、
その回の正確な命令数は主張しない。CSVは外部に置き、
`--persist`は使わず、リポジトリの既存ベンチマーク結果を変更していない。

[ADR0096 §5](../adr/0096-inline-tiny-mode-for-low-degree-label-buckets.md#5-per-path-rules-dispatch-table)も、
走査幅と読取処理の担当箇所に合わせた。保存形式とplacement policyは変わらない。

## 限界：費用の内訳とcursorの不具合は未検証

命令トレースは30秒で打ち切られ、成果物を得られなかった（`trace-before.log`、exit 124）。
したがって命令別の原因割合は特定していない。位置解決を分けた候補で費用が戻ったという測定結果と、
ソース上の呼出経路から判断している。inline指定だけで減らなかった理由も断定しない。

1,350,000の上限は、初回修正前の丸め表示1.34Mから決めた保守的な検査値である。
最終候補はこの値を下回るが、rank/select、Graph削除、穴の多いbucketや実トラフィックの費用を示さない。
canbenchの古い保存済み基準に対する改善率は、この変更の前後差には使っていない。
全workspace、全LARA suite、PocketIC、未選択のベンチマークも実行していない。

cursorのページ読取がtinyのplacement情報を幅0と解釈する別経路は、今回修正していない。
[GAP-TINY-CURSOR-EXTENT](../implementation-gaps.md#gap-tiny-cursor-extent--cursor-paging-still-treats-tiny-placement-as-empty)
に担当箇所と次の検査条件を記録した。F6の移動・rekeyとfixture前提の修正も後続作業である。

初回の独立レビューは両役とも175秒で終わらず、判定を得られなかった。その記録は凍結したまま残す。
性能再調査時のレビューも両役が220秒で打ち切られ、最終判定を得られなかった。
性能修正を含む0380全体を対象とし、Standardsは5回、Specは6回のfile-only閲覧を行った。
両者の45入力のhash確認は成功したが、内容の承認ではない。
呼出監査では書込み・検証実行はなく、Standardsの途中メッセージ1件は指示違反として記録した。
新しい入力スナップショット、実行記録、監査は`resume-r1/review-r1/`にある。

旧CLIログにはmodel-cacheのschema診断があった。今回は一時的な別のCodex homeを使い、
その診断は出ず、共用cacheも変更しなかった。それでも判定が返らなかったため、
旧診断をタイムアウト原因とは断定しない。一時homeは削除済みである。

性能再調査の観測時間は約526秒／590秒。その継続回では残り時間にレビューを収められず、再試行しなかった。
最初のdraft検査は成功したが、作業途中のdraft検査は完了項目を認めないphase規則で失敗した。
当時の最終検査は承認の未完了を理由に不合格だった。

ユーザーが前回の独立レビュー実行を許可したため、その継続回で新しい590秒枠を使った。凍結した旧台帳は変更せず、
同じ実行制限を持つrunnerを新しい証跡ディレクトリに置いた。入力は140,157 bytesに減らし、
各担当の閲覧をhash確認1回と補助閲覧1回に絞った。基準はHEADとの差分ではなく、凍結した0380着手前の状態である。
Rustコードとテストは変更せず、ビルド・テスト・ベンチマークも再実行していない。

`review-r2/`では両担当が承認を返したが、補助閲覧の出力が改行を含め12001文字になった。
12000文字の上限を1文字超えるため、この承認は採用しない。`print`の改行も収まるよう本文を11500文字以下に制限した。
同じレビュー実行処理、同一のソースと入力資料で`review-r3/`を実行した。時間枠はそのまま引き継ぎ、再設定していない。

`review-r3/`ではStandardsが約52秒で`APPROVE`、Specが約108秒で`CHANGES_REQUIRED`を返した。
両担当とも34入力のhashが一致し、2回のfile-only閲覧が成功した。書込み、検証実行、途中メッセージ、
出力量超過はなかった。実行記録と監査は`review-r3/standards.md`、`spec.md`、`audit.json`にある。
レビューの手順は合格したが、対象コードの承認は揃っていない。

SpecのP2は`crates/graph/src/facade/store/tests.rs:2458-2466`にある。
スロット2を削除した後の挿入は0を再利用する。0・1だけを再利用し、2は再利用しない実装でも今の検査を通れる。
同じfixtureで前の穴を埋め、2を削除してから2に再挿入し、各段階の両方向・両順序を確認する必要がある。
計画の「非ゼロの生存辺を削除し、その位置を再利用する」という約束を狭めて解決してはいけない。
その継続回はレビューまでとし、テスト追加を行わなかった。計画の再検証と閉鎖を未完了に戻し、最終構造検査も不合格だった。

今回はユーザーの許可を受け、上述の同じ位置2を再利用する検査を追加した。fixtureは既存のGraphStore 1個・頂点3個のまま。
新しいtest、helper、PocketIC constructor、canister installは増やさず、挿入3回と削除1回を追加しただけである。
`reuse-r1/initial.json`で326の旧証跡ファイルと既存283 dirty pathsを固定し、新しい590秒枠で実行した。

| 今回のコマンド（作業場所は同じ） | 結果 | `reuse-r1/`内のログ |
|---|---|---|
| `rustfmt --edition 2024 --check --config skip_children=true crates/graph/src/facade/store/tests.rs` | 成功 | `format.log` |
| `cargo test -p gleaph-graph --lib tiny_parallel_visitors_preserve_slot_identity -- --nocapture` | 1成功。新しい位置2の再利用まで到達 | `graph-focused.log` |
| `cargo test -p gleaph-graph --lib facade::store::tests:: -- --nocapture` | 59成功、同じ9登録の既存失敗 | `graph-final.log` |
| `cargo clippy -p gleaph-graph --lib --tests -- -D warnings` | 既存の重複`#[test]`1件だけで失敗 | `graph-clippy.log` |

Rust実行では今回も同じ共通lockを借り、元の`Cargo.lock`に復元した。新しいGraph実行は補強後のソースhashに結び付ける。
LARAの16・44件と1,332,111命令の性能値は元の記録を使う。今回はそのソース・Wasm・fixtureを変えておらず、
LARAテストやcanbenchは再実行していない。Graphの既存9失敗とstrict lintの失敗は成功扱いにしない。

最初の補強後レビューも両担当から`APPROVE`が返ったが、Specのhash確認コマンドのstdoutがCLIイベントに残らなかった。
exit 0だけでは照合結果を確認できず、監査は不合格とした。欠落の原因は特定していない。
hash不一致で失敗する検証にし、照合した全hashも出力する形へ変えた。同じレビュー実行処理、同一のソース・差分・資料を使い、
合格条件を緩めずに再確認した。最初の判定は閉鎖の根拠にしていない。

採用した記録は`reuse-r1/review-confirm/`にある。Standardsが約48秒、Specが約43秒で`APPROVE`を返した。
両担当とも41入力が一致し、file-only閲覧は2回・1回。書込み・検証実行・途中メッセージ・出力量超過はなかった。
両者が、同じ位置2を再利用するアサーションと両方向・両順序の検査によってP2が解消したと確認した。
入力は169,121 bytesで、凍結した0380着手前からの全差分を含む。新しいテストだけを見た承認ではない。

[計画0380](../../plans/0380-tiny-read-invariants.md)の閉鎖項目と最終条件を完了にし、
`validate_plan.py plans/0380-tiny-read-invariants.md --phase final`は成功した（`reuse-r1/plan-final.log`）。
最終範囲検査も、今回の変更がGraph test本文26行と計画・報告の3ファイルだけであること、元のF7・F8本文、
旧証跡326ファイル・0379関連66ファイル、既存283 dirty pathsの集合、元のlockとHEAD/indexが保たれたことを確認した。
記録は`reuse-r1/final-scope.log`と`final.json`に残す。レビュー後の変更は計画・報告の状態記録のみで、Rustコードは同一である。

## So What：0380を閉じ、commitの対象範囲を分ける

今回の2件はGraph側の特例なしで直り、初回の共通化で増えたtiny走査費用も検査上限以下に戻った。
不足していた同じ非ゼロ位置の再利用も、元のアサーションを減らさずに対象テストと独立再レビューで確認した。
0380の限定範囲は完了した。ただしworktreeには283のdirty pathsがあり、この承認はその全てを対象にしていない。
次はcommitへ含める変更範囲を決める。未承認の変更を混ぜず、fixture前提は別の変更で直す。
F6 rekeyは辺が動いたこととmove eventを先に確認し、cursorは独立した読取APIの検査として扱う。
