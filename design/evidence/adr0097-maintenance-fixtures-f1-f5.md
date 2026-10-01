# F1/F5：tiny の維持と slab の実移動を別々に検査する

**状態：F1/F5の実装、bounded native検査、独立レビュー、index-only検査は完了。別commitのhash/treeは外部 `commit/final.json` に記録する。**

F1/F5の古いfixtureは、inline値なしの3辺を入れたtiny bucketにも左詰めを要求していた。
今回の2テストは、tinyの生存辺が元のslotに残るケースと、slabで実際に辺が動くケースを
それぞれ実行する。生成物に結び付けたnative実行は**2件成功**し、既存のtiny回帰2件も成功した。
無関係な変更を除いたHEAD-plus-index候補のCargo実行でも対象2件が成功した。
worktreeのCargo経由2回はtimeoutしたため、成功とは扱わない。strict lintには既存の二重
`#[test]`が1件残る。これはF1/F5のtest-only修正であり、productionの変更ではない。

## 同じ属性検査を、異なる配置で実行する

対象は `crates/graph/src/facade/store/tests.rs` の
`forward_edge_compaction_moves_property_sidecars` と
`maintenance_without_resolved_table_compacts_order_preserving` である。
共通の検査を1引数のprivate helper `check_forward_sidecars_across_maintenance` に置き、
明示的なInsertionと解決済みlabel table不在の設定は各testが所有する。
後者はresolverの結果が `None` であることを前後に検査する。enqueue時にはどちらも
maintenance policyがInsertionであることを確認する。

| ケース | 検査前の配置 | slot0削除直後 | drain後 |
|---|---|---|---|
| tiny | 通常挿入3辺、slab幅0・spill行0、slot0/1/2 | second=1、third=2 | second=1、third=2 |
| slab | 通常挿入5辺でtinyを出る。padding2辺を削除してdrainし、正のslab幅・spill行0・degree3・slot0/1/2を確認 | second=1、third=2 | second=0、third=1 |

slab-onlyの前提はdrain完了から推測しない。placementとtopologyを別々に検査し、
対象を削除する前に確立する。enqueue直前はqueueが空で、enqueue後に仕事が存在し、
drain後にqueueが空になることも確認する。private promotionやdescriptorの直接書換えは使わない。

辺ごとにsidecar値11・22・33を設定する。slot0削除後の生存辺のID・label・slot・相対順序と、
値22・33を検査し、移動後もforward handleとreverse occurrenceの両方から同じ値を読む。
削除した辺のreverse adjacencyは空である。inline値は全ケースで空のままである。

tinyの旧third handleは引き続き33を返す。slabでは旧thirdのslot2が範囲外になり、
`GraphStoreError::CounterpartLookup(CounterpartLookupError::SourceNotFound(occurrence))`
を返すことを、元のforward occurrence全体と照合する。任意のerrorを許す検査ではない。
この読取りのerrorを所有するのは `edge_alias.rs` とLARAのcanonical counterpart解決であり、
Graphのdelete入口の `EdgeNotFound` とは異なる。

## 検査結果：native runtime は成功、Cargo timeout は未完了

証拠は `/Users/yota/.local/share/gleaph/evidence/adr0097/maintenance-fixtures-f1-f5-0382/` にある。
基準HEADは `ec33243aad441a627d372a448c458d80e6688733`。`commands.json` は各コマンドの
引数・時間上限・実時間・exit・入力hash・lock復元を記録する。Rustは通常の共有targetを使い、
記録済みcommon lockを借用した後、元のlockに戻す。

- `baseline-f1-f5.log`：変更前の正確な2件をCargoで実行し、両方ともthirdのslot2対slot1で失敗。
- `fixtures-first.log`：最初の候補は `impl Into<String>` 引数への不要な `.into()` でcompile失敗。
  その変換と、F1の置換で不要になったimportを除去した。
- `fixtures-after-compile-fix.log`：177秒でcompile完了、native実行の起動表示後に180秒上限。
  `fixtures-runtime.log`：20秒上限、Cargoのmanifest警告後に停止。両方ともlibtestの開始表示と
  runtime結果がなく、exit124の未完了検査である。停止原因は確定していない。
- `native-registry-probe.log`：生成済みtest binaryに対象2件が登録されていることを確認。
- `native-binding.json`：compile前後のtest source hash一致、候補helperのsymbol、生成物のhash
  `4867643190bad1ce4d326c25667af1ef5d1825d1cd97fbff253309c6edd8d5e4`
  を記録。`native-fixtures-diagnostic.log` はその生成物を直接実行し、2成功・0失敗。
- `native-tiny-guards.log`：同じ生成物で変更していないF7/F8の2件が成功。
- `format-check.log`：対象fileのrustfmt check成功。
- `graph-clippy.log`：strict lint失敗。F9の二重 `#[test]` 1件であり、同じ既存test bodyと
  0380の `resume-r1/graph-clippy.log` に結び付けて分類する。新たな抑制や除外はない。

直接native実行は、完成済みcompileと同じsource・生成物を使ったruntime証拠である。
Cargoのtimeoutを成功に読み替えたものではなく、index projectionの実行結果でもない。
生成物の登録・起動は正常だったが、Cargo側の停止原因まではこの切分けで証明していない。

## test数を増やさず、実移動の検査を取り戻した

test属性は68のままで、対象は2関数のままである。2つのTLS graph上に、tiny/slabの計4 source
グループを作る。`GraphStore::new` はfieldを持たないfacade handleであり、helperで各1回呼ぶ。
頂点作成は対象2testの合計で8回から20回へ増える。PocketIC構築・canister/federation installは0。
それぞれのsourceで配置とqueueを検査するため、前のケースのdrainや属性を前提の代用にしない。

残る69個の既存関数は初期worktreeとbyte一致する。削除されたimportは旧F1だけが消費していた。
production、benchmark artifact、public API、storage layoutには変更がない。性能測定は行わない。
ADR0097 §7とADR0052 §7–9の実装契約は変更しない。0379監査の本文は既存の未追跡fileなので、
初期bytesを保持する。今回のF1/F5の実装状態は本稿で管理し、監査時点の失敗分類とは区別する。
その初期本文は外部証拠の `baseline/design/evidence/adr0097-native-maintenance-failure-audit.md`
に保存されている。

## 完了記録と残る境界

独立Standards/Specは同じ候補を承認した。受理した判定は `review-r2/{standards,spec}.md` と
`review-r2/audit.json` にある。各reviewerの操作は1回のfile-only checksum確認で、出力は
5722/5717文字だった。最初のpassはStandardsの補足読取りが失敗し、判定を受理していない。
同じsourceを1回の確認だけで再レビューした。自動CLI通知のskill-description短縮だけを
parserで区別し、失敗した読取りや操作制限を緩めていない。最初のeventsと監査失敗も保存した。

`index-fixtures.log` と `index-fixtures.json` は、初期のdirty hunksを除いた候補のCargo検査
2成功・0失敗を記録する。snapshotのRust hashは
`8625e296819d33cb2fe7bf2a4e53ad3883c01fad956cbf84e08b7b820c106f11`。
対象3関数のbodyは承認済みworktreeと一致し、他のHEAD関数は変更していない。
共有targetを使い、snapshotとworktreeのlockを復元した。`index-format.log` も成功した。

Lizard1.24.0でHEADと候補indexを比較した。対象3関数の合計CCNは2→14、NLOCは152→195。
増分はtiny/slab、queue、sidecar、exact errorのnative検査であり、productionの責務は増えない。
1引数helperのCCNは12。両wrapperの低下だけを改善と数えず、helper込みの増分を評価した。
`commit/lizard-review.md` はfocusedと判定する。最終indexは測定済みRust bytesと照合する。

別commitのsubjectは `test(graph): distinguish tiny no-op from slab moves`、parentは
`ec33243aad441a627d372a448c458d80e6688733`。正確なhash/treeと2fileの範囲は外部
`commit/final.json` が識別する。全体suite、canonical-lock、canister lifecycleの承認は含めない。
初期279 dirty pathsの無関係なbytesと過去4904証拠fileを保持する。pushは行わない。

F2–F4はUnordered/Insertionのcaptureを実移動で検査する。F6はexact moveとRemove/Insert postingを
揃えてからobserver修正の要否を判断する。F9/F10のhysteresis、疎なcursorの前方再走査、既存lintは
別の作業である。今回の成功をそれらの解消に数えない。
