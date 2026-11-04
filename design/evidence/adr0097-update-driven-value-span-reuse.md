# 通常の値更新は退役spanを再利用したが、毎回foldの容量増大は残った

**状態: 実験。8条件の計測、対象テスト、独立Standards/Specレビュー、計画の最終検証完了。自動foldは未実装。**

大きいbucketのfoldで解放した値領域を、小さいbucketの通常の値更新が取得する負荷を測った。
consumerの更新8回に対し、空きspanからの割当はNever/Each/Four/Eightで0/8/1/0回だった。
値幅8/33 bytesの双方で同じ結果となり、raw allocatorを直接操作しなくても再利用が起きた。

総命令数の最少は幅8でFour（Never比−4.78%）、幅33でEach（−89.32%）だった。
再利用8回のEachでも、幅8ではNeverより8.84%高い。幅33のEachはvalue slabの仮想領域が1ページ増えた。
**再利用可能性、総コスト、占有容量は別々に判断する必要がある。** 実負荷での発生頻度や自動foldの閾値は決めていない。

[0374](adr0097-continuous-spill-fold-intervals.md)の新規peer書込みは最初のspanを末尾へ追加し、退役領域を消費しなかった。
今回は既存consumerへの更新を加えた。入力・操作数・共有run arenaの状態が異なるので、0374以前のtotalとの増減比較は行わない。

## 結果: 再利用回数だけでは最少コストの周期を選べない

producerは初期256行、consumerは16行。8回の各回に両者へ8行を追記し、consumerの値を1件更新する。
producerの全件読出しは各回8回、consumerは1回。Never/Each/Four/Eightはproducerのfold周期だけを変える。
consumer更新に伴う必要なfoldは全方式で毎回実行される。

以下は、同じ初期状態から同一Wasmで実行した8回全体の命令数。peer登録、両者の追記、fold、更新、読出しを含む。

| 幅 | Never | Each | Four | Eight | 最少 / Never比 |
| --- | ---: | ---: | ---: | ---: | --- |
| 8 | 11,328,707 | 12,329,767 | 10,787,652 | 11,740,089 | Four / −4.78% |
| 33 | 308,392,430 | 32,944,291 | 123,438,173 | 292,467,840 | Each / −89.32% |

幅8のEach/EightはNever比で+8.84%/+3.63%。幅33のFour/Eightは−59.97%/−5.16%だった。
幅33のproducer読出しscopeはNeverの266,899,539からEachの5,897,497へ減り、Eachのproducer foldは18,432,005だった。
consumer更新scopeも5,390,700から2,440,505へ変わった。ただし、共有run arenaやblob mapの状態も方式間で変わる。
この差を空きspan割当だけの効果とは解釈しない。幅8のconsumer更新scopeは逆に1,004,444→1,110,218だった。

操作scopeはround scopeに含まれるため、重複加算してtotalを再構成していない。
全scopeの命令数・call数・round別内訳は`E/comparison.json`にある。

### 要求サイズと割当経路が、再利用の有無を分けた

新規bucketの最初のspanは`append_byte_span`で末尾へ進む。既存bucketの非tail領域を置き換える場合は
`allocate_byte_span`が空き領域を先に探す。今回のconsumerは、値runを持った状態で
`update_edge_inline_property_at_slot`を呼ぶため、更新前のfoldが後者の経路を通る。

| producer周期 | consumerのfree割当 | 起きたこと |
| --- | ---: | --- |
| Never | 0/8 | producerは領域を解放しない。旧consumer領域は次の要求より小さく、peerが間に残る |
| Each | 8/8 | 初回は32Wの退役prefixへ24Wを割り当て、その後も適合する空き領域がある |
| Four | 1/8 | 4回目の32Wは48W要求に不足。8回目は288Wのproducer領域が退役し、80W要求に使われる |
| Eight | 0/8 | 8回目に解放する32Wではconsumerの80W要求に足りない |

Wは値幅（bytes）。Fourの8回目には、退役した288Wが隣接する旧consumerの48Wと結合して336Wのspanになる。
その先頭80Wは、producerが以前所有していた範囲内だった。
Eightの同じ時点では合計312Wが空いていても、最大spanは64Wだった。合計free bytesだけでは80W要求を満たせない。

Eachの8回の割当を、過去のconsumer所有区間とも照合した。1–4・7回目はproducer退役領域の初消費、5・6・8回目は
以前consumerが使って解放した範囲の再取得だった。延べ416Wの要求を、初消費216Wと再取得200Wに分けられる。
幅8では1,728/1,600 bytes、幅33では7,128/6,600 bytesである。これは割当履歴の分類であり、現在の保持量やメモリ削減量ではない。
Fourの初消費は80W、再取得は0だった。各回の実offsetと前後のfree inventoryは`E/capacity.json`に保存した。

### 再利用後もEachのslab末尾と空き領域は大きい

下表はNever / Each / Four / Eightの順。producer自身のfold先は、全ケースで空きspanを消費せず末尾へ追加された。
consumerの再利用だけでは、その成長を止めなかった。

| 幅 | 最終value tail (bytes) | 最終value free (bytes) |
| --- | ---: | ---: |
| 8 | 3,784 / 19,144 / 8,008 / 6,344 | 2,816 / 15,872 / 4,736 / 3,072 |
| 33 | 15,609 / 78,969 / 33,033 / 26,169 | 11,616 / 65,472 / 19,536 / 12,672 |

初期tailは49W。最終value宣言容量は`max(4,096, tail)`だった。
value arenaの最終high-waterは両幅とも779 / 275 / 308 / 779行、edge arenaは全方式1,095行だった。
これらはrun backingの占有上限で、生存行数や幅33のblob payload総量ではない。
consumerのrun割当によってproducerがarena tailを失うため、0374の「初回foldまで同じrun IDで成長する」という前提も使えない。

幅33のEachのみ、value slab（MemoryId 10）が1→2 pages、16仮想領域合計が32→33 pagesとなった。
他条件は合計32 pagesのまま。物理backingは全条件2,049 pagesのままで、8 total・134 scopeのheap/stable増分もすべて0だった。
仮想領域の増大が既存のMemoryManager割当内に収まったのであり、物理メモリを縮めた結果ではない。
slab、run、blob mapの数値を足してnet memory savingsとはしない。

## 方法: producerの退役領域を、同じconsumerへの実更新で使う

既存[`FoldFixture`](../../crates/ic-stable-lara/src/labeled/graph/traverse/bench/fold.rs)を使う。
label2のproducerは32行のvalue prefixと224行のrunを持ち、label3の元peerが1行生存する。
label12のconsumerを16行で作り、値末尾を所有させる。初期freeは0、無料の事前foldはない。

各回r=1..8に以下を実行した。

1. label `3+r`（4..11）へ、値`9000+r`のpeerを1行挿入して保持する。schema登録も計測内に置く。
2. producerに連続8行、consumerにも連続8行を追記する。両者が非tailであり、追記はvalue runへ入る。
3. producerだけを指定周期でvalue foldする。edge foldは呼ばない。
4. consumer slot0のneighbor `20_010`を保ち、値を`1_000_000+r`へ更新する。毎回異なる値なので、同値書込みではない。
5. producerを降順で8回、consumerを降順で1回、全件走査する。

consumerのfoldを直接呼んだり、raw allocatorで割当先を作ったりしていない。
通常の更新APIが非空runをfoldしてから値を書き換える。最終次数はproducer320、consumer80、peerは計9行である。
読出し総行数は18,688/416。scratchの初期次数でのwarm-up、snapshot、完全一致assertionは計測外に置いた。
その後のscratch成長、peer期待値の更新、成功件数・行数集計、ループとscopeは単一`bench_fn`のtotalに含む。

nativeではpeer後・追記後・producer fold後・consumer更新後を記録した。次の点を入力由来の期待値と照合している。

- producerのprefix/run長を独立した周期式で検査し、consumer更新前のrunは8行、更新後は空とする。
- 取得した区間全体が更新前のfree spanに含まれ、更新後にはfreeと重ならないことを検査する。旧consumer領域の退役も確認する。
- 全actor・peerの生存span同士、生存spanとfree、同じarena内の生存run IDが重ならないことを検査する。
  `tail = live + free`とvertexの値領域会計も一致させる。
- producer、consumer、全peerのslot・neighbor・全value bytesを、topology / hydrated / value-onlyの両順序で完全一致検査する。
  producer fold直後、consumerを更新する前にも旧値を検査し、後続更新による破壊の見逃しを防ぐ。
- value-only操作の前後でedge geometryを照合する。consumerが末尾へ追加した後にproducerがtailを持つとは仮定しない。

新系列には途中・終了後とも破壊的probeを置いていない。共有oracleの本体を一つに抽出し、旧系列の検査を弱めずにconsumerへ適用した。
Wasmは初期・最終の48レコードを出力し、nativeの792レコード中の対応状態とすべて一致した。
Wasmの各回途中を直接観測したという意味ではない。

対象はone-orientation graphのscalar `insert_edge_skip_leaf_cascade` / Insertionと内部値更新APIである。
同じ更新処理は公開forward/reverse wrapperから呼ばれるが、その存在確認やcounterpart同期、Graph/Router、canister間呼出しはtotalに含めていない。
本番コード、storage layout、schema形式、MemoryId、依存関係は変更していない。

### 再現条件と検証

基点は`spill-swap`のHEAD `880ad86a024e30953bbc134ad946b2c2e7b6da67`。
`E=$GLEAPH_EVIDENCE/adr0097/update-driven-reuse-0375`に固定入力、差分、全出力、コマンド台帳を保存した。
0374の完了記録・計測Wasmは不変。今回の所有範囲を除く269 dirty pathsをhashで保護する。

- 測定ソースSHA256: `730b08ee2aba713206d4c22d885f55174c71c9baa7975f333aa8b561338ebae5`。
- Wasm: 6,865,692 bytes、SHA256 `6562d5ddbbb5bdf37294117d35932066fe9cab719ca8a7cb28ed67a92ef40c7a`。
- CSV: 142行、SHA256 `09be88ccf76051b734858ba93f8ee4ed1077748c6f62a8a0fce5f5aec5e1d304`。
- rustc/cargo 1.100.0-nightly、canbench/canbench-rs 0.7.0、PocketIC 13.0.0、ic-cdk 0.20.1、ic-stable-structures 0.7.2。
  Apple M2 Pro / macOS 26.4.1 arm64。既存release/fat-LTO、codegen-units=1、通常targetを使った。
- `run.py`は1コマンド300秒・1 user turn合計590秒で制限する。cargo/canbenchの間だけ固定common lockを使い、終了後に元へ戻す。
  cold target、依存更新、`--persist`は使わない。新規exportは`ur_{0,1,4,8}_{8,33}`の8件。
  method名合計は19,823で、20,000の制限内でinstall・実行を完了した。

| 検証 | 結果 / 記録 |
| --- | --- |
| `rustfmt --edition 2024 --config skip_children=true --check`（fold.rsのみ） | passed、`format-final.log` |
| `cargo clippy --locked --offline -p ic-stable-lara --lib --tests --features canbench --message-format=json` | exit 0、既存39警告箇所と一致、新規0。`lint-review.json` |
| `cargo test --locked --offline -p ic-stable-lara --lib --features canbench -- --nocapture fold_economics repeated_fold continuous_spill update_reuse` | 14 passed / 620 filtered、`native-final.log` |
| `canbench ur_ --csv --show-canister-output --runtime-path $REPO/.canbench/pocket-ic`（E/canbench-current） | 8/8条件、134 scopes、build込み107.05秒。`canbench-final.log` |
| `python3 E/analyze.py` / `python3 E/audit.py` | passed、call数・区間履歴・native/IC状態・保護hashを照合 |
| `lizard --csv -i -1`（凍結before/current fold.rs） | passed、`lizard-summary.json` |
| 独立Standards / Specレビュー | 両者APPROVE（101.06秒 / 116.00秒）。`review-standards-v1.md` / `review-spec-v1.md` |
| 計画の`--phase final`検証 | passed、`plan-final.log` |

新規nativeテストは`update_reuse_8`と`update_reuse_33`。各4方式を別graphで構築し、旧系列込みのgraph/MemoryManager構築は40→48回となる。
追加PocketICテストfixture/インストール呼出しは0。canbenchの8条件は1回ずつ完了した。
4変異（peer書込み欠落、producer fold欠落、consumer更新欠落、誤slot更新）は`update_reuse_8`のruntime assertionでそれぞれ失敗した。
`mutations.json`に診断とソース完全復元を記録した。最初のnative compileは`CsrVertex` trait import不足で失敗し、import追加で解消した。
このcompile失敗を変異検出やruntime passとは数えていない。

Lizard 1.24.0の比較は、未追跡fold.rsの凍結0374ソースに対する拡張差分であり、Git HEAD比較ではない。
関数数30→44、関数NLOC903→1,289、CCN合計83→124、最大CCN14→15、引数数はselfを含め最大4だった。
増分は二actorの操作と各phaseの所有権検査が中心で、共通のbucket/run/scan/完全一致検査を複製していない。
新type、汎用observer、consumer有無のmode flagは追加していない。nativeの独立周期式は、誤った実行周期を期待値側でも追認しないために残した。

## 限界: この混合比での再利用とコストを示す実験である

consumerは固定だが、peerは毎回増える。producer256/consumer16から始まる8回、値幅8/33、読出し8対1だけを測った。
実queryの頻度分布、他のサイズ比、長期の断片化、削除、batch、Tree、E2E遅延、独立run間の分散は測っていない。
Eightの最後のfold後に長く読み続けた場合の損益も、この結果からは分からない。
再利用の可否はnativeの途中状態で示し、ICでは開始・終了状態を対応させた。物理縮小、peak heap、blob mapの空きnode量は評価していない。

strict `-D warnings`は既存GAP-LARA-TEST-CLIPPYにより未達。full workspace/PocketIC suite、upgrade、instruction trace、persisted benchmark更新は対象外である。
日本語の機械lintは既知のSudachiPy offline-cache不足により未実行で、手動チェックリストと通読を用いた。

## 次の判断: 空き領域を使う書込みの頻度と要求サイズを実負荷で確認する

「解放したspanを通常の書込みが使えるか」には肯定する証拠が得られた。
ただし、適合するspanが必要であり、再利用回数が多くても総コストや占有末尾が小さくなるとは限らない。
自動化の前には、実queryがrun-backed bucketを更新する頻度、要求サイズと空きspanの分布、fold後の読出し回数を確認する必要がある。
この8条件だけで周期や閾値を固定しない。本番の自動fold、commit/pushは行っていない。
