# F2–F4：W2の実移動を検査するfixtureは、配置の前提で停止した

**状態：Blocked。F2–F4の修正は未完了。候補Rustを撤回し、着手前のbytesに戻した。commit・pushは行っていない。**

scalar と paired batch の準備経路を調べたが、必要な3辺のrun-free W2 prefixは確認できなかった。
幅宣言のlive `0→2` 拒否はnativeでも確認した。0384の原案は独立レビューで契約修正を求められた。
no-hole tinyのsidecar指摘を訂正し、公開・cursor・tiny昇格の契約をv2で具体化した。
追加の実行枠でv2の独立レビューを終え、**契約のAPPROVEを得た**。
承認対象は失敗・公開・予約の手順であり、productionと0383のfixtureは未実装・未修復である。
次は、既存ownerの準備・公開とqueue所有権を別sliceで実装・native検証する。

対象3件の元の失敗を再現した。続いて、通常のW2挿入5辺からpadding2辺を削除してdrainする
候補を試したが、3件とも**edge slab幅0・edge spill 5行**が残った。生存辺は3件である。
実移動を要求する前提が成立していないため、slot0削除以降の検査へ進めなかった。
fixture修正は停止したままである。次に必要なのは、承認済みv2を実装・検証し、
Graphの支持された操作でrun-freeなW2 edge prefixを作れることの証拠である。
policyやobserverを直す根拠は、この失敗からは得られない。

[0382のF1/F5](adr0097-maintenance-fixtures-f1-f5.md)では、inline値なしの通常5挿入がslabを作れた。
W2にも同じ前提を適用した今回の候補は失敗した。これは0382の結果を覆すものではない。

## 3件とも、対象のslot0削除前に前提が崩れた

母集団は `facade::store::tests` の次の3関数だけである。

| ID | 関数 | 着手前の最初の失敗 |
|---|---|---|
| F2 | `unordered_compaction_swap_publishes_move_and_moves_sidecars` | thirdのslot2に対して0を要求 |
| F3 | `insertion_policy_maintenance_preserves_left_pack_order` | thirdのslot2に対して1を要求 |
| F4 | `unordered_captured_at_enqueue_drains_without_ambient_table` | second/thirdの順序に対してthird/secondを要求 |

候補は3関数の共通fixtureを一つにまとめ、W2を明示的な実行tableから宣言した。
通常挿入5辺、末尾padding2辺のcanonical delete、明示的span enqueueとdrainを行い、
placementを読んだ。各sourceの観測値は同じだった。

| 観測 | 値 | 意味 |
|---|---:|---|
| degree | 3 | paddingの削除後に3辺が生存 |
| stored_edge_slots | 0 | 初期化されたedge slab prefixがない |
| edge_spill_run | 0 | 有効なrun ID。run不在という意味ではない |
| edge_spill_rows | 5 | tombstoneを含む使用長。生存5辺という意味ではない |
| inline_property_byte_width | 2 | W2のschema |
| inline_property_bytes_slab_slots | 3 | 値のprefixは3slotある |
| inline_property_bytes_overflow_log_len | 0 | 値のrunは残っていない |

値のprefixが密でも、edge runがあるためswapの前提は満たせない。queueは空になったが、
run-freeになったとはいえない。`stored_edge_slots > 0` のassertionで3件とも停止した。
候補のslot0削除、capture対照条件、exact move、sidecar/inline/reverse読取り、stale error、
後続のretained-runケースはすべて未到達である。候補の成功条件として数えない。

## W2の幅宣言は、空tinyを先にslabへ変える

確認した所有箇所は次のとおりである。productionは変更していない。

- `edge_insert.rs::insert_directed_edge_with_inline_property_bytes_and_properties` は、paired insert
  の前に `ensure_directed_edge_inline_property_width` を呼ぶ。
- `deferred.rs::ensure_directed_edge_inline_property_width` はforward/reverseそれぞれの幅宣言へ
  委譲する。強制的なedge run materializationはここにない。
- `values.rs::ensure_bucket_inline_property_byte_width_on_slot` のTiny分岐は、幅を宣言する前に
  `promote_tiny_to_slab` を呼ぶ。空bucketではtranscribeする既存edgeがない。
- `insert.rs::promote_tiny_to_slab` は既存のtiny rowsを転写し、既存degreeをstored prefix幅として
  公開する。空bucketの予約容量を、初期化済みedge rowsとは扱わない。
- `compact.rs::first_edge_slot_move_in_bucket` はedge runがあるbucketを移動選択から除外する。
  `slab_bucket_allows_unordered_swap` もedge run不在を要求する。ADR0097 §7はrunを残した完了を認める。

これは今回の配置と整合するソース経路である。あらゆるGraph操作でW2 prefixを作れないという
証明ではない。辺数を増やすだけの再試行、private promotion、descriptor書換え、強制fold、
observerイベントの推測生成は行わなかった。

別案の「valueless slabを先に作り、W2を後付けする」も、そのまま採用できない。
幅宣言のTree/Slab分岐は、幅不一致のときに空bucket用の `schema_unset` 条件を確認し、
不成立なら `InlinePropertyBytesWidthMismatch` を返す。後続の0→w materialization条件は
`degree > 0` を要求するため、直前の `degree == 0` を含むgateと両立しない。
この分岐順の整合性は別のschema契約の検討対象である。追加調査でlive幅宣言の拒否をnativeでも
確認したが、本稿で幅追加を許可したり、productionを修正したりはしていない。

## paired batch も、3辺のprefixを準備できなかった

scalar 案の撤回後、既存の `prepare_clean_slab_dir_buckets` と
`try_insert_batch_edges_clean_slab_with_initial_properties` を使い、初期propertyを持つ
3辺のpaired batchを試した。結果は次のrecoverable geometry errorだった。

```text
RecoverableGeometry {
  error: SpillRunDeferred {
    owner_vertex_id: VertexId(0),
    label_id: BucketLabelKey(50014)
  }
}
```

`batch-rejection-diagnostic.log` と、その入力hashに一致する
`batch-rejection-source.rs` に記録した。LARA の `preflight_overflow_log_run` は
ADR0097のrun-growth reservationがないbatchを、予約前にscalarへ差し戻す。
この拒否を無効化してfixtureを作る案は採らない。

対照として、同じGraph内の別sourceへ1辺だけのpaired batchを実行した。
edge/valueの書込みslot数はともに2で、forwardはdegree1、edge prefix1、W2 value
prefix1、edge/value runなしだった。inline marker11とforward sidecar11も読めた。
これは1辺の準備が可能だという証拠であり、3辺のprefix、移動、retained case、reverse
sidecarの検証ではない。batch全体が使えないという結論にもしていない。

live幅宣言の対照では、値を持たない5辺を先に挿入し、degree5、edge prefix5、edge runなし、
width0を確認した。実行tableへW2を指定して既存のdirected幅宣言を呼ぶと、次のエラーになった。
forwardのplacementとtopologyは変わらなかった。

```text
Forward(InlinePropertyBytesWidthMismatch {
  bucket_width: 0,
  edge_inline_property_width: 2
})
```

この2対照は `width-and-one-row-probe.log` に記録した。一時probeが使ったF2の名前で1件pass
しているが、maintenance regressionのpassではない。source、native artifact、3辺batchの
拒否sourceの対応は `continuation-evidence.json` に記録し、probe本体も外部に保存した。

次に検討するのは、幅追加の所有者の統一である。幅宣言の
`ensure_bucket_inline_property_byte_width_on_slot` と、insert用の
`ensure_bucket_inline_property_schema_for_insert_with_materialize` が別経路で同じ幅追加を
扱っている。比較を一つ移動するだけでは、このルール重複は解消しない。

先行するcontract reviewでは、LARAの既存value-stream ownerを使う案を検討する。
純粋なschema validatorには割当ての副作用を追加せず、宣言・insert・paired wrapperの
責任を区別する必要がある。両halfの失敗順序、未publish spanの解放、counterの失敗、
deferred signal、tiny promotionとslot move通知は未検証である。新しいforce-fold APIや
fixtureによるdescriptor書換えは、この検討の代替にしない。

この調査は0383の目標を置き換えない。幅宣言のowner統一と失敗契約を別sliceでレビュー・
修正した後に、3辺のrun-free W2 prefixを要求する元のfixture目標へ戻る。

## table解除だけでは、既存F4のpolicy消失を検査できない

`edge_inline_property_schema.rs::resolved_edge_label_with` はtest buildで、ambient tableがなくても
非zero幅のtest profileからresolved labelを合成する。元のF4はglobal W2 profileを登録している。
したがって、tableを解除したという操作だけではresolverの結果が `None` とは限らない。

再開時にはeffective drain policyも確認する必要がある。候補はglobal profileを登録せず、
実行tableだけでW2を宣言する予定だった。F4のdrainではresolver `None`・fallback Insertionを
確認し、W2の読取りtableはdrain完了後に戻す。F3はcaptured Insertionと反対のUnordered
実行contextを使ってcaptureの欠落を検知する案だった。いずれも今回は未到達であり、
policy captureのruntime検証を終えたとは扱わない。

## 実行した検査と撤回を、同じsourceに結び付けた

証拠は `/Users/yota/.local/share/gleaph/evidence/adr0097/maintenance-fixtures-f2-f4-0383/` にある。
基準HEADは `4952323c21c073652a0c7c495fb2471cd2d43918`、worktreeは
`/Users/yota/dev/gleaph-spill`、branchは `spill-swap`。
`commands.json` が引数、時間上限、exit、source hashとlock借用・復元を記録する。
通常の共有targetと記録済みcommon lockを使った。canonical lockの検証ではない。

- `plan-draft.log`：draft構造検査は成功。修正計画は未完了のまま保持する。
- `baseline-f2-f4.log`：exact3件を実行。0成功・3失敗で元の前提を再現した。
- `fixtures-first.log`：wire labelのu16とcatalog label型を比較してcompile失敗。
  共通fixtureのforward/reverse両検査を同じwire label表現へ修正した。
- `fixtures-placement-witness.log`：compile完了後、exact3件を実行。0成功・3失敗。
  3sourceのplacement dumpとslot0削除前の失敗がある。timeoutではない。
- `placement-blocker.json`：compile入力hashと候補sourceを照合した。候補sourceは
  `e9b24d20a050d95d1f09ca29789ecc4805b65390f53f6376f2b38dc986147452`、生成物は
  `0891cd3619dc8963266493dd06e854f37c76f838bf76aa436fb5c3d510964b64`。
- `withdrawn-candidate.rs` / `.patch`：失敗した候補を外部に保存。3関数以外の既存69関数と
  68個のtest属性を維持していたことを照合した。新helper内のrecording adapterは未到達だった。
- `withdraw-exact-slice-check.log` / `withdraw-exact-slice.log`：着手前→候補の差分だけを逆適用した。
  無関係なdirty hunksは破棄していない。
- `withdrawal-integrity.json`：Rust file全体が初期snapshotとbyte一致。元の279 dirty paths、
  lock、7406個の過去証拠file、HEAD、空indexを保存した。
- `restored-format-check.log` / `final-whitespace-check.log`：復元済みRustのformat checkと
  whitespace checkは成功。未trackedの本稿はrelative linkも確認した。機械的な日本語lintは
  依存不足で未実施。手動点検に留め、依存導入は行わなかった。
- `blocked-final-integrity.log`：本稿追加後も保護対象のbyte一致と空indexを再確認した。
  記録したcommandの累計は132.47秒、590秒の上限内である。

追加調査では次も実行した。hard budgetとledgerは継続し、resetしていない。最新累計は
`commands.json` の末尾を読む。

- `batch-fixture-witness.log` / `batch-rejection-diagnostic.log`：3辺batchはFAILED。
  次のgeometryを仮定して進めず、`SpillRunDeferred` の所有箇所を読んだ。
- `width-and-one-row-probe.log`：live幅追加拒否とbatch1辺の対照probeはPASS。
  F2のmaintenanceやF3/F4のpassには数えない。
- `continuation-evidence.json` / `continuation-capture.log`：両probeと3辺batchの拒否を、
  実行入力のsource hashへ結び付けた。元の他69関数と68個のtest属性も維持していた。
- `continuation-withdraw-check.log` / `continuation-withdraw.log`：追加sliceの差分だけを逆適用した。
- `continuation-restored-integrity.log`：Rust全体、279の元dirty paths、lock、7406個の過去証拠、
  HEAD、空indexを再確認した。
- `continuation-restored-format.log`：復元済みRustのformat checkはPASS。
- `continuation-restored-targets.log`：復元済みexact3件はFAILED、0成功・元の3失敗。
  probeを撤回した後の結果であり、fixtureを修復したという結論は出していない。

0383のstrict clippy、F1/F5 guard再実行、index-only runtime、staged Lizard、独立レビュー、
final plan validatorは未実施。未成立のfixtureをlanding candidateにしないため、stage・commitも行っていない。
既存の二重test属性警告は今回のcompileにも現れたが、lint成功とは扱わない。
PocketIC、canbench、full suite、benchmark artifact更新は0。productionの性能に関する結論はない。

## 0384の原案レビューは完了し、契約未承認の判定を残した

0384はソースを読むcontract reviewに限定した。Rust、API、stable layout、queue variantは
変更していない。既存のvalue-stream ownerへlive幅追加の判定を集める案を、外部証拠rootの
`width-owner/contract-v1.md` にまとめた。状態・結果、最初のwriteと後続の失敗、既存型での
表現、3案の比較を載せている。これは実装済みの契約やF2–F4の成功を示す文書ではない。

`source-manifest.json` は41入力のhash、14定義、196のexact-symbol call行を記録する。
unit testとbenchの呼出しも含むため、196のproduction経路という意味ではない。
`source-supplement.json` はgeneric定義とrepair入口を補い、16定義・197のcode参照を記録した。
repairのGraph呼出しも読む対象に加えた。関数名の対応は字句的な案内であり、ASTやcfg到達性の
証明には使っていない。両inventoryと原典を照合して、次の事実を確認した。

| 所有箇所 | ソース上の事実 | 次の実装で必要な契約 |
|---|---|---|
| declaration / active insert helper | declarationのlive分岐は到達不能。insertは別helperからmaterializeを呼ぶ | live幅追加の判定をvalues ownerの一箇所へ集める。空schema宣言とinsertの責任は分ける |
| inline / stepped materialize | 両方がwidth descriptor公開後にvertex counterの加算・u40検査を行う | 失敗し得る検査をprepareで終え、descriptorとcounterの公開を共通化する |
| directed / undirected宣言 | 一方を公開してから他方を宣言する。self-loopは一回 | 両halfをprepareしてから両方を公開する。第二halfの失敗で第一halfだけを残さない |
| steppedのfill失敗 | spanをretireした後、wrapperは元のcursorを再queueする | cursorが予約を保持するか、解放後にresetするかを一貫させる |
| tiny promotion | tombstoneを飛ばしてlive targetsを詰める。戻り値は`()`でmove通知はない | width宣言だけで既存slotを黙って変えない。穴のあるtinyへの対応を既存実装から推定しない |

上の失敗順序はソース所見であり、0384で再現したnative障害ではない。materializeのslab値は
`degree × width` のdense live-ordinal列で、edgeのtombstone-inclusiveなphysical slot列とは
異なる。ADR0088の記述と実装はこの点で一致する。古いcommentやtest名を根拠に値の長さを
`stored_slots × width` へ変えてはならない。

小さいlive slabの幅追加、空宣言、同期呼出し中の二つの未公開spanは、既存のbucket fields・
vertex counter・allocatorと一時的な予約receiptで表現できる。新しい永続width/capacity表は
提案していない。queueのV1 cursorにはpair identityや両方の準備完了状態がないため、独立した
二つのjobをdrainしてpaired declarationがatomicになったとは扱わない。最初の実装範囲では、
大きいpaired宣言はどちらも公開する前に既存のtyped signalで止める案とした。穴のあるtinyも
支持範囲外とし、昇格前に拒否する案を記した。どちらも独立承認前の案である。

独立レビューには既存のread-only Codexを使った。最初のattemptはheredocの一時file作成を
sandboxに拒否されたと報告し、BLOCKだった。eventsにもsource-readの実行記録がないため、
原典を検査した判定とは数えない。原典を読むcommandを`python3 -c`へ変えた同一packetの
再レビューでは、checksum付きのfile-only readが一回成功した。出力は11,434文字だった。
ただし160秒で最終判定を得られず、`completed=false`・`output_sha256=null`で終了した。
この時点では再試行を止め、`review-audit.json` にevent数と出力hashの監査を保存した。
`width-owner/blocked-checkpoint.json` は279の元dirty paths、7406の過去証拠、原典42入力、
original Rust、lock、HEAD、空indexの保存とrelative linksを確認した。
0384のdraft validatorは部分完了のstatusで一度失敗したため、validatorのdraft契約に合わせて
TODOとcheckboxをpendingへ戻し、成果物の所在をStatus本文に残した。修正後の0384と変更して
いない0383のdraft検査、`git diff --check` はPASSだった。final承認を意味しない。

当時不足していた証拠は、同じpacketに対する完了済みの独立source-bound verdictだった。
最初のBLOCKはread deliveryの失敗であり、二回目の読取り成功も契約の承認ではない。
CLIのmodels-cache診断は保存したが、timeoutの原因とは断定していない。

ユーザーの再開指示後、同じpacketと原典をhashで固定し、重複したprompt文脈を減らして
read-onlyレビューを行った。`review-r3/verdict.md` の最終行は `Verdict: CHANGES_REQUIRED`。
高推論モデルの判定は69.92秒、runner全体は70.32秒で終了した。唯一のtool commandで
11,434文字の原典を読み、完了済み出力との対応を `review-r3/audit.json` で確認した。
P1が2件、P2が1件だった。test・build・benchmark・process操作をreviewerへ依頼していない。

指摘のうち、no-hole tinyで物理spanが変わるためsidecar keyが変わるという例は訂正した。
`EdgeHandle` と `EdgePropertyKey` のslotはbucket内の位置であり、slabの物理addressではない。
tinyとslabのreaderはいずれもrow-local slotを返す。穴がなければpromoterの `transcribed == i`
が各copyで成り立ち、既存handleとsidecarのkeyは保たれる。この一点を根拠にmove通知や
全live-tinyの除外を要求しない。ただしpromoterはdescriptor公開後にもcount更新で失敗し得る。
昇格を失敗原子的なprepareとして利用できるとは、まだ判断していない。

原案で残った不足は次の具体的な契約だった。その判定は `decision-blocked-v1.md` に集約した。
凍結済み `contract-v1.md` は未承認の過去candidateとして保持し、承認済み契約に読み替えない。

| 未承認の境界 | 必要な証拠・決定 |
|---|---|
| paired descriptor/counter公開 | 全prepare後に両endpointの最終row、counter、encoding、backing extentを検証し、その後の全writeがrecoverable Errを返さない具体的なowner APIと前提。単に二つのreceiptを持つことは証明にならない |
| retired spanとcursor | retainかrelease/resetかを一つに決め、最初のreserve後の失敗、後続fill、retire成功・失敗、置換cursor、cancel、bucket-slot identityを同じ所有契約で扱う。解放済みspanへの再queueや別spanのcleanupを許さない |
| tiny昇格のprepare | 先行するedge/descriptor/mate変更後の失敗でもcanonical topology、counter、slotが保たれる手順、または明示的な支持範囲。sidecarの物理address誤認で範囲を決めない |

`safe_write` はbackingが足りていればgrow失敗を返さない。この原典はpaired公開の証明候補には
なるが、両halfのpreflightとpublish手順を規定したAPIが存在する証拠にはならない。
`retire_byte_span` 自体も失敗可能で、既存allocatorにはcleanup capacityを予約する入口がある。
cleanup結果を無視して元cursorを再queueする現行経路は、安全な遷移契約として承認しない。

訂正と不足の記録も、追加のhash-bound原典で独立確認した。`review-r4/verdict.md` は
`Disposition: VERIFIED_BLOCKED`。一回のfile-only readと完了判定は `review-r4/audit.json` にある。
これは**未承認として止める判定の確認**であり、width契約・production・0383の承認ではない。
0384の許された結果である「具体的なblockerの記録」を終え、final plan検査と保護byte/link検査で
分析checkpointを閉じる。terminal証拠は `width-owner-review-final-plan.log`、
`width-owner-review-0383-draft.log`、`width-owner-review-whitespace.log`、
`width-owner/review-final-checkpoint.json`。どれかが失敗すれば閉じたとは扱わない。

Rust、PocketIC、benchmarkの検査は0384では未実施。機械的な日本語lintは依存不足で未実施のまま、
手動checklistとreportの型を確認した。590秒のledgerを継続し、resetしていない。
stage・commit・pushは行っていない。0383の元の3件も引き続き未修復である。
実装前に上の契約を修正して独立承認を得る。新しい永続stateが必要と分かった場合は停止する。
その後の別implementation sliceで、両halfのprepare失敗、counter、span所有権、cursor、tinyの
native red→greenを確認する。scalarのdrain/retryをpaired幅宣言のcommit手順に流用しない。

## v2を具体化し、最初のレビューは時間切れになった

ユーザーの続行指示を受け、`width-owner/contract-v2.md` を新しいreview candidateにした。
以前の原案・判定・receiptは凍結したまま残している。v2は実装済みの契約ではない。

公開手順では、両endpointのencoding、counter、backing extentを最初のcanonical write前に
検証し、commitはrecoverable Errを返さない消費型の手順とした。同じdescriptor segmentや
PMA leafを触る二つの準備は、一つの最終像へまとめる。既存のsegment rewrite、bypass、
tiny promoter、leaf pinの各ownerをprepare/reserve/publishへ揃える案であり、values側へ
storageのaddress規則を複製したり、汎用transaction engineを足したりはしない。

予約cursorは**保持する方式に決めた**。最初の成功stepはspan予約後、fillせずoffset・cursor0を
queueへ返す。後続のrecoverable Errではretireせず、同じoffset・cursorを保つ。単一のpending
materialize jobを既存stable queueの所有者とし、無関係のcanonical変更は事前に拒否する。
maintenanceはそのjobを先に一単位ずつ処理する案で、Deleteの一時的な待機やGraphの最初の
write前のadmissionも契約へ含めた。新しい永続identityやpair stateを追加する案ではない。

追加の原典で、現行enqueueが取得済み予約をoffset=MAX・cursor0へ置き換え得ること、leaf pinが
physical_start公開後にfallibleなcount更新を呼ぶこと、GraphのdeleteにはLARAより前のwriteが
あることを確認した。このため、幅の比較だけを移す実装にはしない。既存ownerの準備・公開、
queue所有権とGraph admission、幅委譲の順に、別々の実装・native検証を行う必要がある。
このr5 checkpoint時点では、手順の独立承認もruntime確認も得ていなかった。
全変更経路でadmissionを強制する実装と、0383のfixtureを成立させるruntime検証は、
後続sliceの要件として残した。

reviewer投入前の準備は二回失敗した。最初はcounts原典の終端行を誤り、次は12,541文字が
12,000文字の上限を超えた。どちらもreviewer起動前で、契約内容の判定ではない。
繰り返した入力plannerを一つの `review-r5/excerpts.json` に集約し、全行範囲と全文字数を
payload発行前に検証するようにした。旧script/logを上書きせず、重複文脈だけを減らした。
checksum、原典読取り、文字数、承認の条件は緩めていない。

r5の唯一のfile-only commandはhash検証と11,891文字の原典読取りに成功した。しかし36秒の
reviewer上限で、最終判定前に終了した。`review-r5/receipt.json` は `completed=false`、
`exit=null`、`output_sha256=null`。`review-r5/audit.json` の判定は `INCOMPLETE` である。
これは承認にも却下にも数えず、r3/r4の原案判定をv2へ流用しない。cache診断も保存したが、
timeout原因とは断定しない。

590秒の累計枠はresetしていない。原典読取り成功後の追加retryは止めた。最終document・
保護byte検査前で572.71秒を使用し、最新累計は `commands.json` に残す。現在の0384は
独立最終判定待ちのdraftで、final plan承認は未実施。draft/whitespace/linkと保護対象の照合は
`width-protocol-v2-plan-check.log`、`width-protocol-v2-0383-check.log`、
`width-protocol-v2-whitespace.log`、`width-owner/protocol-v2-checkpoint.json` を確認する。
Rust、PocketIC、benchmark、日本語machine lintは未実施。production/test bytes、旧証拠、
lock、HEAD、空indexを保存し、stage・commit・pushはしていない。このcheckpoint時点では、
凍結済みv2の独立判定を得る追加の明示的な実行枠が必要だった。

## 同じv2の独立レビューで、契約承認を得た

ユーザーの承認を得て、累計上限を590秒から710秒へ延長した。576.02秒の使用記録を含む
ledgerは継続し、resetしていない。`width-owner/budget-extension/authorization.json` と
runner・ledger・plan・reportの元snapshotが、上限変更と以前のbytesを記録する。
この枠はcontractの独立再レビューとdocument・保護byte検査に限定した。production実装の枠ではない。

r6は同じv2と11,891文字の原典excerptを使い、reviewerを110秒、外側のrunnerを120秒に制限した。
高推論モデル、一回のchecksum付きfile-only読取り、承認条件は維持した。
`width-owner/review-r6/verdict.md` の最終行は `Verdict: APPROVE`。
reviewerは68.18秒、runner全体は68.57秒で終了し、`completed=true`・`exit=0`だった。
`review-r6/audit.json` は唯一の実行command、その成功出力、eventと完了判定の一致を確認した。
cache診断は再びstderrへ出たが、今回もtimeoutの原因とは扱わない。

独立reviewerはactionableな契約欠陥を認めなかった。全write前のbacking・encoding・counter検証、
まとめたvirtual geometry、予約保持と最初のreserve-only progress、singletonのexact retry、
Graphの最初のwrite前のadmissionを、既存フィールドで実装可能な手順として承認した。
承認は**将来の実装手順に限定**される。現コードのatomicity、全mutation経路のadmission、
runtime・性能・0383の成功は確認していない。

現在の契約判定は `width-owner/decision-approved-v2.md` にある。承認packetのhashは
`b08330854aa683a75da43815b7953315431e33a173f748e6263e54535cb2616a`、
独立verdictのhashは `9b9fae826cbcf92e163c63d626f46e360834899b9445cc65a56e7a50e222965e`。
凍結packetのcandidate headerは読取り前の状態として残し、このdecisionが現在の判定を所有する。
原案のblocked、r5のINCOMPLETE、旧script/log/receiptは上書きしない。

最初のclosure検査はすべて成功した。その後の通読で、結びの「次に契約を決める」という
見出しと冒頭の旧選択肢を、承認済み手順の実装へ直した。前のplan・report・checkerは
`width-owner/docsync-before/` に保存した。契約packetと独立判定は変更していない。
現在の0384を閉じるterminal証拠は `width-protocol-v2-r6-docsync-plan-final.log`、
`width-protocol-v2-r6-docsync-0383-draft.log`、`width-protocol-v2-r6-docsync-whitespace.log`、
`width-owner/review-extension-docsync-checkpoint.json` である。全件成功し、現在のplan・reportのbytesを
receiptへ結び付けて初めてcheckpoint完了とする。review後の累計は647.89秒、最後の検査を含む
累計は継続中の `commands.json` に記録する。279の元dirty paths、7406の過去証拠、52原典入力、
Rust、lock、HEAD、空indexと旧width証拠は保護byte検査の対象である。
Rust・PocketIC・benchmarkと機械的な日本語lintは今回も未実施。手動でreportの型とchecklistを確認し、
依存は導入していない。stage・commit・pushもしていない。

この承認を使う実装は別の明示的な実行・検証枠で行う。既存segment・tiny/leaf geometry・allocatorの
準備と失敗を返さない公開、queue所有権とGraph admission、valuesの幅委譲と共通publisherの順に進める。
各ownerのnative red→green、design同期と関連benchmarkを終えるまでlive幅宣言を有効にしない。
その後で0383の元の3件へ戻る。契約承認だけを根拠にfixtureやobserverのassertionを緩めない。

## 次は、承認済み手順を実装してW2 fixtureへ戻る

不足しているのは、Graphのcanonical操作からrun-freeなW2 edge slabとdense value prefixを作り、
それを観測できる最小fixtureである。新しい強制fold APIを足すことも、inline属性の検査を
valuelessな移動検査へ置き換えることも、今回の完了条件を満たさない。
承認済みv2に沿って、既存の配置・幅宣言の所有境界を別sliceで実装・検証してからF2–F4を再開する。
F6のindex rekeyとF9/F10のhysteresisは引き続き保留する。
