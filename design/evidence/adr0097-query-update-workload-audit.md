# 通常のSETはfoldへ進むが、既存デモから更新頻度は求められない

**状態: Experimental。0376のソース監査完了。独立Standards/Specレビュー双方APPROVE、計画の最終検証passed。後続のnative検証は[0377](adr0097-undirected-update-value-span-reuse.md)を参照。**

以下の対象資料・実行範囲は0376時点の監査を指す。0377では無向辺のfit/no-fitを実行し、通常更新1回での再利用を確認した。後続の[0378](adr0097-graph-undirected-update-cost.md)では、同じfixtureのGraphStore API区間をcanbenchで測定した。query全体の性能や実更新頻度は引き続き未測定である。

GQLの実行層では、inlineプロパティの`SET`が0375と同じ値更新処理を呼ぶ。ただし、更新対象に値のspill runがある場合にfoldが必要になる。今回確認したデモのprepared queryは10件とも読出し用で、knowledgeのseedも37辺すべてが`inline_value:null`だった。これらから実負荷の更新頻度や割当要求サイズの分布は求められない。

0376で次の候補にしたのは、**無向辺の通常更新による、canonical側からcounterpart側への退役spanの再利用**である。両側が同じforward allocatorを使うため、手動のproducer foldを挟まない経路を検査できると考えた。0376ではソースから導いた仮説にとどまり、実行結果は後続の0377に分けて記録した。

[0375の実験](adr0097-update-driven-value-span-reuse.md)は、片方向のLARAで通常更新が退役spanを使うことを確認した。今回はその適用範囲をGQL実行層まで追った。0375の命令数や幅8/33の結果を、GraphStoreやquery全体の性能に読み替えてはいない。

## 結果：inline更新は共通commitに入り、辺の種類で割当先が変わる

### SETの入口は異なるが、inline bytesの更新先は一つ

| 操作 | 実装上の経路 |
|---|---|
| `SET e.property = value` | Router解決済みschemaのinline property idと一致すればencodeし、mutation-id付きの更新APIへ渡す。不一致ならstructの拒否条件を確認し、sidecarへ渡す。[executor.rs](../../crates/graph/src/plan/mutation/executor.rs#L606-L661) |
| `SET e = { ... }` | inlineとsidecarの値を事前分類する。既存sidecarを削除し、inline bytesがあれば同じAPIで更新した後、新しいsidecar値を書く。[executor.rs](../../crates/graph/src/plan/mutation/executor.rs#L712-L777) |

共通APIは`GraphStore::commit_update_edge_inline_property_at_occurrence`へ委譲する。ここがschema幅の検証、counterpartの解決、canonical側の選択、index-build admission、辺の値更新、派生indexへの反映を担当する。canonicalを先に更新し、それと異なるcounterpartを続けて更新する。[wrapper](../../crates/graph/src/facade/store/edge_inline_property.rs#L9-L43)、[commit owner](../../crates/graph/src/facade/store/edge_profiles.rs#L49-L175)

このcommit内部では、fenceまたはcanonical更新後の失敗を回復可能errorとして返さず、trapさせる方針がコードにある。今回確認したのはこの分岐と意図であり、record置換全体やcanister間処理の原子性を新たに実証したわけではない。[失敗境界](../../crates/graph/src/facade/store/edge_profiles.rs#L132-L151)

### 無向辺の両側は同じallocatorを使う

| 辺の種類 | canonicalとcounterpart |
|---|---|
| 有向辺（self-loopを含む） | forwardとreverseの別occurrence |
| 無向辺・異なる端点 | 両方ともforward。頂点IDが大きい側がcanonical |
| 無向self-loop | 同じoccurrenceなので、二度目の更新を省く |

これは[counterpartの選択](../../crates/ic-stable-lara/src/labeled/bidirectional/counterpart.rs#L309-L405)と[更新順序](../../crates/graph/src/facade/store/edge_profiles.rs#L138-L151)による。したがって「1 queryにつき2回foldする」と数えてはいけない。queryが処理する辺の数、辺の種類、それぞれのrunの状態を分ける必要がある。

forwardの値slabとfree-span indexesはMemoryId 10/11/12、reverseは25/26/27で、初期化時にも別のstoreとして渡される。有向辺の片側で退役したspanを、そのまま反対方向のallocatorが使うことはない。無向辺の別端点なら、同じforward allocator内で再利用する候補になる。[MemoryId定義](../../crates/graph/src/facade/stable/memory.rs#L36-L60)、[配線](../../crates/graph/src/facade/stable/memory.rs#L239-L272)

既存のGraphStoreテストには、有向辺の両方向更新、無向辺の両端更新、self-loop、平行辺の片方だけの更新を確認するassertionがある。新しいfixtureを一から作る前に、これらを再利用できる。[テスト群](../../crates/graph/src/facade/store/tests.rs#L450-L613)

## 結果：変更する値の幅と、foldが要求する領域の大きさは違う

orientation別のwrapperはlive slotを確認し、対応するLARAの値更新処理へ渡す。その処理は値runがあればfoldし、必要な値領域を確保してからslotを書き換える。[wrapper](../../crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs#L3223-L3272)、[更新owner](../../crates/ic-stable-lara/src/labeled/graph/values.rs#L1333-L1395)

`W`をschemaのbyte幅とすると、structの一つのfieldだけを変更しても、実行層は全体を再encodeして`W` bytesを渡す。field幅だけを渡すAPIではない。[structの再encode](../../crates/graph/src/plan/mutation/executor.rs#L1452-L1553)、[profileの幅](../../crates/graph-kernel/src/entry/edge_inline_property.rs#L153-L164)

foldは`bucket.degree()`行を保存し、descriptorから値runを外してからadmissionを呼ぶ。この時点の必要量は`bucket.degree() × W`である。一般のadmission式は`(bucket.degree() - run_rows) × W`だが、fold中はrunを外しているため`run_rows = 0`になる。これは必要容量であって、毎回その量を新規割当するという意味ではない。[fold](../../crates/ic-stable-lara/src/labeled/graph/compact.rs#L2483-L2515)、[必要量の算出](../../crates/ic-stable-lara/src/labeled/graph/values.rs#L1248-L1265)

admissionは次の順に分岐する。[admission owner](../../crates/ic-stable-lara/src/labeled/graph/values.rs#L1261-L1330)

1. 既存容量で足りるなら、領域を移さない。
2. 初回spanならappendする。必要時のslab compaction判定もこの分岐にある。
3. 既存spanが占有末尾にあり拡張できれば、その場で伸ばす。
4. それ以外の容量不足では、必要量全体を`allocate_byte_span`へ渡す。このallocatorは適合するfree spanを先に探し、なければappendする。

移転後は新descriptorを公開してから旧spanを退役させる。[allocator](../../crates/ic-stable-lara/src/lara/edge_inline_property.rs#L764-L792)、[公開と退役](../../crates/ic-stable-lara/src/labeled/graph/values.rs#L1217-L1236)

このため、値runのない更新、値を持たないtiny/bypass、容量内更新を、0375の「非空runをfoldして退役spanを消費する更新」と混ぜて数えない。総free bytesだけでなく、要求全体が収まる連続spanが同じallocatorにあるかを確認する必要がある。

## 結果：既存資料は入力例・契約テストであり、trafficの記録ではない

| 対象 | ソースから確認できた内容 | 分からないこと |
|---|---|---|
| social / knowledgeのprepared query | 6件 / 4件。本文はすべて読出し用。下記の全10ファイルを確認 | 実際の呼出し回数、他のqueryとの比率 |
| 両demoのgraph type | edge inline schemaの宣言なし。[social](../../demo/social/migrations/000001_social/up/00_social_graph_type.gql#L29-L36)、[knowledge](../../demo/knowledge/migrations/000001_knowledge_graph_type/up/00_knowledge_graph_type.gql#L18-L25) | 別deploymentや変更後schemaの状態 |
| knowledgeのedge seed | 37行すべて`inline_value:null`。[seed](../../demo/knowledge/seeds/edges.jsonl#L1-L37) | load後の更新履歴 |
| socialの生成関数 | 各edgeに`inline_value:null`を設定する。[loader](../../demo/social/scripts/social-load-artifact.mjs#L111-L127) | ignoredな生成済みseedや実行履歴。今回は対象外 |
| Graphのinline scalar benchmark | UINT16、別々の100頂点ペア、100 handleを一巡して直接更新する。非空run・非tail・適合free spanを保証するfixtureではない。[benchmark](../../crates/graph/src/bench/mod.rs#L3730-L3801) | query単位の分布や実頻度 |
| PocketICのscalar mutation fixture | `ROAD.distance UINT16 INLINE`。1辺のINSERT/SET/read、REMOVE拒否、sidecar共存、入力拒否のassertionがある。[fixture](../../crates/pocket-ic-tests/tests/adr0034_inline_edge_scalar_mutation.rs#L16-L225) | このrun/freespan条件での再利用。今回のruntime passも未確認 |
| structのread/schema fixture | 16-byte profileの読出しやschema競合を扱う。[read](../../crates/pocket-ic-tests/tests/adr0034_inline_edge_struct_read_access.rs#L35-L58)、[schema](../../crates/pocket-ic-tests/tests/adr0034_inline_edge_struct_schema.rs#L16-L99) | 更新頻度。schemaテストのコメントだけで、実行層のstruct更新が未実装だとも判断しない |

prepared queryの対象は、socialの[home/alice](../../demo/social/prepared/alice-home-feed.gql)、[semantic/alice](../../demo/social/prepared/alice-semantic-feed.gql)、[public](../../demo/social/prepared/public-timeline.gql)、[discovery](../../demo/social/prepared/semantic-discovery.gql)、[topic path](../../demo/social/prepared/topic-path-explanation.gql)、[home/yui](../../demo/social/prepared/yui-home-feed.gql)、knowledgeの[citation](../../demo/knowledge/prepared/citation-reach.gql)、[shortest](../../demo/knowledge/prepared/shortest-path.gql)、[team readable](../../demo/knowledge/prepared/team-readable-documents.gql)、[variable length](../../demo/knowledge/prepared/variable-length-reach.gql)である。

## 方法：静的な母集団と、実行した検証を分ける

作業対象は`gleaph-spill`の`spill-swap`、HEADは`880ad86a024e30953bbc134ad946b2c2e7b6da67`。既存のdirtyな272パスと0375の完了記録は変更対象から外した。

証跡は`/Users/yota/.local/share/gleaph/evidence/adr0097/query-update-workload-0376/`に置く。`inventory.py`はGit管理下と非ignoredのdemoファイルから、`.gql/.toml/.jsonl/.mjs/.yaml/.yml/.md`を列挙する（`public/`は除外）。demoの165ファイルと、追跡した実装・テストのhashは`inventory.json`に保存した。165は列挙数であり、全文を意味解析した件数ではない。

prepared queryは全文を読み、seedはJSON行として集計した。keyword検索は候補の確認用で、GQL parserや操作回数の集計には使っていない。実装はSETの分岐、共通commit、counterpart、fold、admission、allocatorの順に追った。アプリのscriptやデモ自体は実行していない。

今回の変更はこの報告と計画だけである。Rustのbuild・test・clippy・canbench、PocketIC、live deploymentへの接続は実施していない。既存テストを引用した箇所はassertionの確認であり、新しいruntime passではない。

文書の参照・空白・保護hash検証はpassed（`before-review-r2.log`）。独立Standards/Specレビューは双方APPROVE（`standards-v1.md` / `spec-v1.md`）、計画の`--phase final`もpassed（`plan-final.log`）。日本語の機械lintは既存環境の依存不足で未実施とし、手動チェックを行った（`language-review.md`）。

## 限界：頻度には入力履歴、再利用条件には状態の観測が必要

今回の対象資料から、時間順の実更新履歴と、その採取範囲は得られなかった。privateなログ、ignoredな生成物、任意の外部データまでは探していない。「repositoryのどこにもtraceがない」という調査結果ではない。

頻度を論じるには、まず実行順・成功/失敗・対象schemaを復元できる入力履歴と、期間・対象query・1件の数え方が必要になる。logicalなSETと物理occurrenceの更新を区別する。その履歴を再生してサイズや再利用を調べる段階では、`degree`、run使用量、既存容量、方向別のfree-span状態、間に入るinsert/readも観測する。query文の`SET`数だけでは代用できない。

0376で読んだ0375と当時の上記fixtureは、schemaに基づく通常更新、counterpart同期、退役spanの再取得を一つのassertion系列として検証してはいなかった。この不足は0377のnative統合検査で補ったが、query全体の命令数、実負荷の再利用率、物理メモリ削減、最適なfold周期は主張しない。

## So What：0376の後続案は、0377のnative検証へ進んだ

以下は0376で定め、0377で実行した検査方針である。GraphStoreの既存2-byte profileと[無向辺の両端更新テスト](../../crates/graph/src/facade/store/tests.rs#L516-L523)を基点にする。canonical側とcounterpart側の両方に非空の値runを持たせ、前者が退役させる領域に後者の要求が収まる条件を作り、通常の更新APIを一度呼ぶ。手動foldやraw allocatorによる消費は挟まない。

確認するのは、実際に先の退役領域を使ったか、両側の値とslot対応が正しいか、他の辺・sidecar・生きたspanを壊していないかである。fitしない場合も区別する。canonical側が先に更新され、両側がforward allocatorを使うという上記の実装順序を検査する仮説であり、fixtureを作るだけで再利用成功と数えない。

これはGQLの下にある共通commit境界の統合検査であり、Routerを含むGQL全体の測定でも実trafficの推定でもない。観測履歴または利用者が選んだ負荷条件が得られるまでは、合成比率から自動foldの周期・閾値を決めない。今回、production code・既存証跡・自動policyは変更せず、commit/pushも行っていない。
