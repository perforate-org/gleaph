# spillが毎回再発すると、幅33は毎回fold、幅8は多くの条件でfoldなしが低コスト

**状態: 実験。32条件の計測、対象テスト、独立Standards/Specレビュー完了。自動foldは未実装。**

各回に別bucketを新設し、targetの値が必ずspillする負荷で、value foldなし・毎回・4回ごと・8回ごとを比較した。
幅33 bytesでは毎回foldが全4条件で最少となり、foldなしより総命令数が36.11–90.37%少なかった。
幅8 bytesではfoldなしが3条件で最少だった。例外は初期次数256・各回8読出しで、4回ごとのfoldが3.87%少ない。

毎回foldには容量面の代償もある。退役したvalue prefixは反復中に再消費されず、slabの占有末尾が伸びた。
幅33・初期次数256では仮想領域が1ページ増えた。物理stable memoryの縮小はない。
値幅、読出頻度、spillの再発条件を分けて判断するための結果であり、自動foldの閾値は決めていない。

[0373の反復比較](adr0097-repeated-fold-economics.md)では、value foldの実処理は初回だけだった。
今回はその未測定条件を調べた。入力と操作数が異なるため、0373以前の総量との増減比較は行わない。

## 結果: 同じ32条件内でも、最少となる間隔は一つに定まらない

数値はWasm命令数。Nは初期target次数、Rは各回の全件読出し回数。
Neverはfoldなし、Eachは毎回、Fourは4・8回目、Eightは8回目だけvalue foldする。
すべて同一Wasm、同じ初期状態から8回を実行したtotalで、peerの新設・追記・維持処理・読出しを含む。

| 幅 / N / R | Never | Each | Four | Eight | 最少 |
| --- | ---: | ---: | ---: | ---: | --- |
| 8 / 64 / 1 | 1,682,561 | 2,977,246 | 2,068,437 | 1,956,364 | Never |
| 8 / 64 / 8 | 3,884,241 | 4,586,338 | 3,920,454 | 4,054,729 | Never |
| 8 / 256 / 1 | 2,437,454 | 5,635,867 | 3,385,188 | 3,135,909 | Never |
| 8 / 256 / 8 | 8,881,967 | 10,035,793 | 8,537,909 | 9,287,961 | Four |
| 33 / 64 / 1 | 10,672,302 | 6,818,723 | 10,017,917 | 14,157,356 | Each |
| 33 / 64 / 8 | 59,628,454 | 8,837,333 | 25,694,133 | 54,063,732 | Each |
| 33 / 256 / 1 | 35,544,584 | 19,994,519 | 30,813,834 | 46,934,722 | Each |
| 33 / 256 / 8 | 262,346,101 | 25,265,114 | 112,612,520 | 242,214,986 | Each |

幅8のEachはNeverより12.99–131.22%多い。Fourでも、N=64・R=8では0.93%多く、優位にはならなかった。
幅33ではEachがFourにも勝る。一方、EightはR=1でNeverより32.04–32.66%多く、R=8では7.67–9.33%少ない。
8回目に一度だけ移すと、それまでのrun読出費用に加えてfold費用を払い、その後に残る読出しはR回だけである。

fold呼出しは0/8/2/1回だが、費用は回数の比例倍にはならない。R=1のvalue-fold scope合計は次のとおりだった。

| 幅 / N | Each: 8回 | Four: 2回 | Eight: 1回 |
| --- | ---: | ---: | ---: |
| 8 / 64 | 1,352,212 | 434,129 | 286,108 |
| 8 / 256 | 3,485,291 | 1,131,125 | 738,487 |
| 33 / 64 | 4,121,387 | 4,331,485 | 4,706,232 |
| 33 / 256 | 16,618,510 | 15,405,126 | 15,722,112 |

幅33・N=256・R=1では、target追記scopeもNeverの3,978,093からEachの2,177,964へ減った。
全件読出しscopeは31,128,639から738,068へ減る。効果を「読出しだけの改善」とは解釈しない。
操作scopeはround scopeの内側にあり、登録処理なども含む。これらを重複加算してtotalを再構成してはいない。
全scopeの内訳とcall数は`E/comparison.json`に保存した。

### 再利用と容量: runの再利用とslabの増大が同時に起きた

nativeではpeer新設後・target追記後・fold判断後を各回記録した。途中に破壊的probeは挟んでいない。
Wasmは開始時・8回終了時・終了後probeの192レコードを記録し、nativeの対応状態とすべて一致した。
Wasmの途中状態を直接観測したという意味ではない。

- **Value run:** 全方式で、各回のtarget追記後に非空runが存在した。選択されたfoldはすべて実処理を行った。
  Eachでは2回目に8行runを確保し、3–8回目は直前に解放した同じIDを再取得する。
  IDはN=64で65、N=256で257。Fourは5–8回目に同じrunを成長させ、8回目に解放した。
  FourのこのID不変は、生存runの成長であって解放後の再利用ではない。
- **Value prefix:** foldで置き換えた旧prefixがfree inventoryへ戻ることをnativeで毎回確認した。
  今回のfold先はいずれも既存の空きspanを使わず、新しい末尾領域を使った。
  新設peerは各回W bytesの末尾を取得し、free inventoryを消費しない。反復中の自然なslab再利用は0回だった。
- **終了後probe:** 最初に退役した32×W bytesを正確な旧offsetで取得し、poison書込み後も全生存行が一致した。
  再解放後のallocator統計・free inventory・pagesも一致する。runの共通probeは全backing byteの復元を検査する。
  これは再利用可能性の検証であり、測定中に回収された容量ではない。
- **Edge:** targetはedge runを保持し、edge foldは呼ばない。最終edge arena high-waterはN=64で308行、N=256で884行。
  edge slab宣言容量は4,112 slotsのまま、free slotsは0だった。

下表の並び順はすべてNever / Each / Four / Eight。R=1とR=8で容量状態は同じだった。
value arena行数は保持されたbackingのhigh-waterで、生存行数や幅33のblob payload総量を表さない。

| 幅 / N | 最終value tail (bytes) | 最終value free (bytes) | 最終value arena行数 |
| --- | ---: | ---: | ---: |
| 8 / 64 | 328 / 6,728 / 2,120 / 1,352 | 0 / 5,632 / 1,024 / 256 | 129 / 74 / 98 / 129 |
| 8 / 256 | 328 / 19,016 / 5,192 / 2,888 | 0 / 16,384 / 2,560 / 256 | 513 / 266 / 290 / 513 |
| 33 / 64 | 1,353 / 27,753 / 8,745 / 5,577 | 0 / 23,232 / 4,224 / 1,056 | 129 / 74 / 98 / 129 |
| 33 / 256 | 1,353 / 78,441 / 21,417 / 11,913 | 0 / 67,584 / 10,560 / 1,056 | 513 / 266 / 290 / 513 |

初期value tailは幅8で264、幅33で1,089 bytes。最終宣言容量は各値の`max(4,096, tail)`だった。
幅33・N=256のEachだけ、value slabの仮想領域（MemoryId 10）が1→2 pages、16領域合計が32→33 pagesへ増えた。
他条件の仮想領域合計は32 pagesのままで、物理backingは全条件2,049 pagesのままだった。
canbenchの32 total・440 scopeではheap/stable増分がすべて0だった。仮想領域の成長はMemoryManagerの既存割当内に収まる。

Eachはvalue arenaを抑える一方、slabの末尾と空きspanを増やす。これらの差分を足してnet memory savingsとはしない。
一時割当、peak heap、blob B-treeの空きnode量、物理メモリの縮小も、この計測からは分からない。

## 方法: 実データの別bucket書込みでtargetの末尾所有を毎回失わせた

既存の[`FoldFixture`](../../crates/ic-stable-lara/src/labeled/graph/traverse/bench/fold.rs)を拡張した。
幅W=8/33、初期次数N=64/256。初期targetは32行のvalue prefixと非空runを持ち、元のpeerが1行生存する。
各方式ともunfoldedから始め、計測前の無料foldはない。穴、削除、Tree promotionもない。

各回r=1..8に、以下を同じ順序で実行した。

1. 同じsourceのlabel `3+r`を新設し、値`9000+r`の1行を挿入して保持する。
2. targetに新しい連続slotの8行を追記する。
3. 指定周期でtarget bucketだけをvalue foldする。edge foldはしない。
4. targetの値を降順でR=1/8回、全件走査する。

peerのschema登録・行挿入も計測に含めた。新規bucketの最初のvalue spanは所有者の
`ensure_bucket_inline_property_bytes_span`から`append_byte_span`で末尾に確保される。
raw allocatorに人工的なblockerを置いてはいない。要求W bytesより小さい不適合な空きspanも作らず、
peer登録による全体compactionを避ける入力にした。

nativeでは、peer新設がtailを正確にW増やし、free inventoryとtargetのprefix/run descriptorを変えないことを検査した。
続くtarget追記では毎回非空value runを要求する。期待prefix/run長は観測値から追認せず、初期Nと独立した周期式から求める。
fold後は旧prefixの退役、targetのedge状態不変、非Tree状態を検査した。元のpeerと新設8件も完全一致oracleの対象である。

最終target次数は128/320。全件読出しの合計行数はR=1で800/2,336、R=8で6,400/18,688となる。
値・neighbor・slotを入力から独立に構成した期待値と両順序で照合する。測定内は読出行数を集計し、終了後に検査する。
初期化、scratchの初期Nでのwarm-up、snapshot、完全一致assertion、破壊的probeは計測外に置いた。
その後のscratch成長、peer期待値リストの更新、周期判断、行数集計、ループとscopeの費用はtotalに含む。

totalは8回全体を囲む1つの`bench_fn`。CSVでpeer/target追記各8回、読出し8R回、fold 0/8/2/1回、
round scope各1回を確認した。native/Wasmは同じ操作helperを使う。途中にprobeを入れて再利用状態を整える処理はない。
新規export名は`cs_p{0,1,4,8}_{8,33}_{64,256}_r{1,8}`である。

scalarの`insert_edge_skip_leaf_cascade` / Insertion経路を測っている。public cascading insertion、batch、
Graph/Routerやcanister間呼出しの性能ではない。本番コード、storage layout、schema形式、MemoryId、依存関係は変更していない。

### 再現条件と検証

基点は`spill-swap`のHEAD `880ad86a024e30953bbc134ad946b2c2e7b6da67`。
既存dirty stateを固定し、今回の差分を`E/extension.patch`に保存した。
`E=/Users/yota/.local/share/gleaph/evidence/adr0097/continuous-spill-0374`。
原出力、全コマンドのcwd・引数・時間・終了状態は各`.log`と`commands.json`に残す。

- Wasm: 6,802,267 bytes、SHA256 `ccda8ec80dcdfba265bc69f5e3fd798bd358203386bb2592c001ef21173ef5cf`。
- 測定ソースSHA256: `97cee7e2c7fbd0fe08a7f0b203f35cdafcb6738e9301c068f2afd73cfec56bac`。
- CSV: 472行、SHA256 `cde75c157eadc03d46dce1d0579dadbb95f9763d357a2244791b1c4e40c9ff0f`。
- rustc/cargo 1.100.0-nightly、canbench 0.7.0、PocketIC 13.0.0、ic-stable-structures 0.7.2。
  Apple M2 Pro / Darwin arm64 / macOS 26.4.1。0373と同じ既存release/fat-LTO設定とrunnerを使用した。
- `run.py`は1コマンド最大300秒・合計590秒で制限し、cargo/canbench中だけ固定common lockを使用する。
  終了時に元のCargo.lockへ戻す。通常targetを再使用し、cold build、依存更新、`--persist`は行っていない。

Specレビューの再開後は`E/resume-1/run.py`と別台帳を用い、同じ時間上限を適用した。
ソース・Wasm・CSVのhashが不変だったため、Rust検証と計測は再実行していない。

| 検証 | 結果 / 記録 |
| --- | --- |
| `rustfmt --edition 2024 --config skip_children=true --check`（fold.rsのみ） | passed、`format-short-names.log` |
| `cargo clippy --locked --offline -p ic-stable-lara --lib --tests --features canbench --message-format=json` | exit 0、既存39警告箇所と一致、新規0。`clippy-short-names.log` / `lint-review.json` |
| `cargo test --locked --offline -p ic-stable-lara --lib --features canbench -- --nocapture fold_economics repeated_fold continuous_spill` | 12 passed / 620 filtered、`native-short-names.log` / `native-result.json` |
| `canbench cs_ --csv --show-canister-output --runtime-path /Users/yota/dev/gleaph-spill/.canbench/pocket-ic`（E/canbench-current） | 32/32条件、440 scopes、build込み107.54秒。`canbench-short-names.log` |
| `python3 E/analyze.py` | passed、call数・832 native / 192 IC snapshotの対応・page増分・hashを照合。`measured.json` / `capacity.json` |
| `lizard --csv -i -1`（保存したbefore/current fold.rs） | passed、`lizard-before.log` / `lizard-current.log` / `lizard-summary.json` |
| 独立Standardsレビュー | APPROVE、147.50秒。`review-standards-v1.md` |
| 独立Specレビュー | APPROVE、164.07秒。`resume-1/review-spec.md`。初回の200秒停止記録も保持 |
| 計画の`--phase final`検証 | passed、`resume-1/plan-final.log` |

新規nativeテストは`continuous_spill_{8,33}_{64,256}`の4件。各テストが4方式を構築するため、
既存分と合わせたgraph/MemoryManager構築は24→40回となる。追加PocketICテストfixtureは0件。
canbenchは32条件を1回ずつ完了した。最初の試行はinstallで拒否され、benchmark実行には到達していない。

peer書込み省略、fold省略、FourをEight周期に変更、target追記1件欠落の4変異は、
`continuous_spill_8_64`の実行時assertionでそれぞれ失敗した。compile errorを検出とは数えていない。
`mutations.json`は完全復元と変異を適用した基準ソースを記録する。その後の変更は新規export名とその説明commentだけで、
変換前後の一致を`export-rename.json`に残した。最終の短縮名ソースで12テストと32条件を再検証した。

最初のWasmはビルドに成功したが、method名の合計が20,239文字となり、PocketIC 13.0.0の20,000文字制限でinstallに失敗した。
新規prefix `bench_t_spill_`だけを`cs_`に短縮した。各benchmarkには計測用とtracing用の2 query exportsがあり、
最終合計は19,535文字になった。既存名、処理、計測条件は変えていない。
拒否ログとWasmは`canbench.log` / `rejected-name-limit.wasm`に保存した。将来さらにexportを増やす際は残りの名長予算に注意が必要である。

### 複雑さ: 既存fixtureと検証経路の拡張に留めた

Lizard 1.24.0の対象はfold.rsだけ。未追跡ファイルのためGit基準の差は作らず、凍結した0373完了時ソースと比較した。
関数数26→30、関数NLOC731→903、CCN合計66→83。直接変更した6関数のCCNは7→24、最大4→9。
ファイル全体の最大CCNは14、引数数はselfを含め最大4のまま。macro生成wrapperはLizardの関数数に含まれない。

追加したのはprivateな4択周期、1行peerの登録、計測ループ、nativeの各回oracleである。
固定peerの期待値をリストへ拡張し、既存の完全一致oracleと最終probeを全生存peerへ適用した。
操作・snapshot・probeを新しく複製してはいない。古い計測系列も引き続き必要なので削除しない。
nativeの独立周期式は、壊れた`due()`を期待値側まで共有して誤って通すことを避けるために残した。
汎用observerや新module、本番の自動policyは導入していない。

## 限界: 新規bucketが増え続ける8回の負荷である

この入力は、固定された二つのbucketを交互に上書きする負荷ではない。source上のlabelと生存peerが毎回増える。
その登録費用とallocator状態を全方式へ同じように課しているが、固定bucketの再書込みへ結果を流用できない。
Eightは8回目のfold直後で観測を終えるため、その後に何回読めば費用を回収するかも未測定である。

対象はW=8/33、N=64/256、R=1/8、8回の有限な行列だけ。独立run間の分散・信頼区間、
削除、他の断片化、Tree、大次数、実query混合比、E2E遅延は測っていない。Rや周期の補間・外挿はしない。
再利用可能なspanが増えることと、長時間の空間効率・物理メモリ回収を同一視しない。

strict `-D warnings`は既存GAP-LARA-TEST-CLIPPYにより未達。full workspace/PocketIC suite、upgrade、
instruction trace、persisted benchmark更新は対象外である。日本語の機械lintはSudachiPyのoffline cacheがないため未実行で、
手動チェックリストと通読を用いた。

## 次の判断: 間隔だけでなく、幅・読出し・空きspanの消費先を条件にする

継続spill下でも幅33のvalue-only候補は残る。ただし、毎回foldの命令数上の優位には、退役spanの蓄積が伴った。
幅8では毎回foldを選ぶ根拠は得られず、4回ごとの改善も一つのN/R条件に限られる。
自動化へ進む前に、実際の入力でこの末尾喪失が起きる頻度と、退役spanを再消費する書込みを確認する必要がある。
この結果だけで普遍的な周期や発火閾値は選ばない。本番の自動fold実装、commit/pushは行わない。

後続の[通常の値更新による退役span再利用の実験](adr0097-update-driven-value-span-reuse.md)では、既存consumerへの更新を加えた負荷を調べた。
