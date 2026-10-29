# 反復する追記・fold・読出しでは、value-onlyが全8条件でboth-foldより低コスト

**状態: 実験。24条件の計測、対象テスト、独立レビュー完了。自動foldの閾値は決めていない。**

8件追記→fold→値の全件読出しを8回繰り返した。幅8 bytesでは、各回1読出しのvalue-onlyは
unfoldedより総命令数が25.01–29.74%多く、各回8読出しでは9.97–19.45%少なかった。
幅33 bytesでは、各回1読出しでもvalue-onlyが58.87–67.47%少ない。
この値中心の入力では、value-onlyが全8条件でboth-foldより低コストだった。

ただし、value foldが実際にrunを移すのは初回だけで、残り7回はno-opだった。
追記先がslab末尾に留まる入力の結果であり、毎回value spillが再発する負荷の結果ではない。
span再利用は確認できたが、物理stable memoryの縮小は起きていない。

[0369の単発比較](adr0097-value-only-fold-economics.md)と
[0371のretirement修復後の再計測](adr0097-value-span-retirement.md)を受けた追加実験である。
今回は[0372のbatch修復](adr0097-batch-value-span-regressions.md)を含むソースで、
維持処理を含めた反復全体を直接測った。過去の表は書き換えず、総量の増減比較にも使わない。

## 結果: 幅と読出し回数で、unfoldedとの差が逆転する

全数値はWasm命令数。Nは初期target次数、Rは各回の全件読出し回数である。
各行は3方式を同じWasm・同じ初期状態で比較している。

| 幅 / N / R | Unfolded | Value-only | Both-fold | Value-only対unfolded | Both対unfolded |
| --- | ---: | ---: | ---: | ---: | ---: |
| 8 / 64 / 1 | 1,067,044 | 1,333,939 | 3,175,600 | +25.01% | +197.61% |
| 8 / 64 / 8 | 3,261,426 | 2,936,237 | 4,572,307 | −9.97% | +40.19% |
| 8 / 256 / 1 | 1,671,033 | 2,168,043 | 4,342,493 | +29.74% | +159.87% |
| 8 / 256 / 8 | 8,096,383 | 6,521,379 | 7,957,699 | −19.45% | −1.71% |
| 33 / 64 / 1 | 10,075,149 | 3,277,208 | 5,144,817 | −67.47% | −48.94% |
| 33 / 64 / 8 | 57,607,113 | 5,256,391 | 6,948,300 | −90.88% | −87.94% |
| 33 / 256 / 1 | 35,016,002 | 14,403,179 | 16,627,904 | −58.87% | −52.51% |
| 33 / 256 / 8 | 253,980,147 | 19,636,450 | 21,497,970 | −92.27% | −91.54% |

幅8・N=256・R=8のboth-foldはunfoldedを1.71%下回るが、value-onlyには及ばない。
幅33の大きな差には、runのblob読出しをslab読出しに変える効果と、後続追記の費用差の両方が含まれる。
「読出しだけが速くなった」とは解釈しない。

R=1の操作scopeを取り出すと、幅8では追記64件の費用がvalue-onlyで増える一方、幅33では減る。
維持処理の合計も示す。scopeには各操作内の計測処理が含まれ、これらだけの和は上のtotalではない。

| 幅 / N | 追記: unfolded → value-only | Value-onlyのvalue fold 8回 | Bothのedge fold 8回 | Bothのvalue fold 8回 |
| --- | ---: | ---: | ---: | ---: |
| 8 / 64 | 721,230 → 887,504 | 172,527 | 1,867,267 | 170,821 |
| 8 / 256 | 730,279 → 894,089 | 622,052 | 2,266,378 | 622,013 |
| 33 / 64 | 3,297,855 → 1,211,858 | 1,749,098 | 1,878,629 | 1,749,164 |
| 33 / 256 | 3,624,875 → 1,281,052 | 12,341,034 | 2,272,328 | 12,338,237 |

### 初回費用を回収する時点も、実測した8回の範囲で異なる

下表は`round_1`からのscope累計が、同じ入力のunfolded累計を初めて下回る回を示す。
外側の計測処理は含まないため、export totalの損益分岐点とは区別する。
最初に下回った後は、この8回の範囲で逆転していない。回数の外挿やRの補間はしない。

| 幅 / N / R | Value-only | Both-fold |
| --- | ---: | ---: |
| 8 / 64 / 1 | 8回内では下回らない | 8回内では下回らない |
| 8 / 64 / 8 | 4回目 | 8回内では下回らない |
| 8 / 256 / 1 | 8回内では下回らない | 8回内では下回らない |
| 8 / 256 / 8 | 3回目 | 8回目 |
| 33 / 64 / 1 | 3回目 | 4回目 |
| 33 / 64 / 8 | 1回目 | 1回目 |
| 33 / 256 / 1 | 4回目 | 4回目 |
| 33 / 256 / 8 | 1回目 | 1回目 |

## 再利用: run、edge span、value prefixを分けて確認した

nativeでは各回の追記直後・fold後を観察し、途中に破壊的probeを挟まず8回を完了させた。
Wasmでは開始時・8回終了時・終了後の再利用probeを記録し、144レコードすべてがnativeの対応状態と一致した。
Wasmの途中状態を直接記録したという意味ではない。

- **Edge run:** both-foldでは毎回runを解放する。2回目以降は8行runとなり、3–8回目は直前に解放した
  同じID `33`を自然に再使用した。arenaのhigh-waterは初回追記後から増えない。
- **Edge slab:** both-foldの1–3回目は新しい容量を確保し、4回目は既存の空きspanへ移設する。
  5–8回目はその場所に留まり、隣接する空きspanから拡張部分を取得する。
  nativeでは取得範囲が処理前のfree inventoryに含まれることと、処理後のlive leafとの非重複を確認した。
  容量の横ばいだけから再利用を推定していない。
- **Value run:** value-only/bothとも初回だけ非空runをfoldする。後続追記はvalue slab末尾を伸ばすので、
  2–8回目のvalue foldはno-opになる。その7回の呼出費用もtotalに含めた。
- **Value prefix:** 初回に退役した32×W bytesは、反復中には消費されずfreeのまま残った。
  終了後のprobeでは、その正確な旧offsetをbest-fitで取得し、上書きしてもtarget/peerの値が変わらず、
  tail・仮想/物理pagesが増えないことを確認した。再解放後はfree inventoryとallocator統計が完全一致する。
  これは「再利用可能」の証拠であり、反復中の自然な再消費ではない。

初回追記は既存runをarena末尾で拡張する。サイズclassが変わってもIDは変わらないことをnativeで確認し、
終了後のrun probeには初期classではなく初回fold時のclassを使った。生存中のrunはprobe対象にしない。
共通probeは両run領域の全byteを前後で照合し、free-listの順序・link・frontierも完全に戻ることを検証する。

容量は以下のようになった。V/Bはvalue-only/both、Uはunfolded。arena行数は保持された領域のhigh-waterで、
生存runの行数ではない。すべてR=1とR=8で同じだった。

| 幅 / N | Value tail: 初期 → 最終U → 最終V/B | 最終V/Bのvalue宣言容量 | 最終V/Bのvalue free | Value arena行数: 最終U / V/B |
| --- | ---: | ---: | ---: | ---: |
| 8 / 64 | 264 → 264 → 1,288 B | 4,096 B | 256 B | 129 / 65 |
| 8 / 256 | 264 → 264 → 2,824 B | 4,096 B | 256 B | 513 / 257 |
| 33 / 64 | 1,089 → 1,089 → 5,313 B | 5,313 B | 1,056 B | 129 / 65 |
| 33 / 256 | 1,089 → 1,089 → 11,649 B | 11,649 B | 1,056 B | 513 / 257 |

Edge slabの宣言容量はU/Vで4,112 slotsのまま、bothでN=64は4,448、N=256は5,072 slotsとなる。
bothの最終freeは144/560 slots。edge arenaは3方式とも最終171/555行であり、runを解放しても縮まらない。
これらの量には領域の初期余裕や保持されたbackingが含まれる。差分を足し合わせてnet memory savingsとはしない。

全snapshotで仮想16領域の合計は32 pages、物理backingは2,049 pagesだった。
24 totalと392 scopeのheap/stable page増分もすべて0である。MemoryManagerの既存128-page bucket内の変化であり、
一時割当やpeak heap、blob B-treeの空きnode量、物理メモリ回収を測った結果ではない。

## 方法: 初期状態を揃え、fold費用を反復全体に含めた

既存の[`FoldFixture`](../../crates/ic-stable-lara/src/labeled/graph/traverse/bench/fold.rs)を拡張した。
幅8/33、初期次数64/256、同じsourceの別labelに生存peerを1件持つ。
peerはtargetの32件目の後に挿入され、初期value prefixの末尾拡張を妨げる。
穴・削除・Tree promotionはない。各方式とも測定開始時はunfoldedで、先にfoldして費用を除外していない。

1. targetに新しい連続slotの8件を追記する。
2. unfoldedは維持処理なし、value-onlyはvalue fold、bothはedge fold→value foldを実行する。
3. targetの値を降順でR回全件走査する。以上を8回繰り返す。

最終target次数は128/320。読出行数の合計はR=1で800/2,336、R=8で6,400/18,688となる。
値・neighbor・slotは入力から別に組み立てた期待値と両順序で照合し、peerも確認する。
測定中は走査行数を集計し、測定終了後に一致を検証する。nativeの各回にも同じ追記・fold・走査helperを使う。

初期化、scratchの初期Nでのwarm-up、snapshot、完全一致のassertion、破壊的な再利用probeは計測外。
N+8以降に必要となるscratch成長は計測に含む。totalは8回全体を囲む1つの`bench_fn`である。
`round_*`と操作scopeは入れ子なので重複加算しない。行数集計、scope登録、ループ等の費用は除去していない。
追記scopeは各8回、読出scopeは8R回、選択したfold scopeは各8回であることをCSVでも検証した。

追記は既存の`insert_edge_skip_leaf_cascade` / Insertion経路を使う。
public cascading insertion、batch ingestion、Graph/Routerやcanister間呼出しの性能ではない。
edge foldはvertex全体、value foldはtarget bucketが対象であり、同じ単位の維持処理と偽っていない。
本番コード、データ所有者、API、schema、MemoryId、依存関係は変更していない。

### 再現条件と実行記録

基点は`spill-swap`のHEAD `880ad86a024e30953bbc134ad946b2c2e7b6da67`。
既存の未追跡fold.rsを含むdirty stateを固定し、今回の差分を`E/extension.patch`で分離した。
`E=/Users/yota/.local/share/gleaph/evidence/adr0097/repeated-fold-0373`。
全コマンドのcwd・引数・終了状態・時間は`commands.json`、原出力は各`.log`に残す。

- Wasm: 6,711,085 bytes、SHA256 `b6a1921d5cd8141c33bfd8b0aaa679cfa374a89ceb4af0924832c63cfb7dee8f`。
- 測定ソースSHA256: `d2a085700ac9b956b81828993ce4a64331ad4182e011c47a89421404076de76c`。
- CSV: 416行、SHA256 `7510fad7a6eca0535622719f16b1bbe94cfc9cebc945eb50dec5704841e497fb`。
- rustc/cargo 1.100.0-nightly、canbench 0.7.0、PocketIC 13.0.0、ic-stable-structures 0.7.2、
  Apple M2 Pro / Darwin arm64 / macOS 26.4.1。詳細は`pins.log`、runnerの同一性は`runtime-identity.json`。
  既存release/fat-LTO設定を再使用した。
- `run.py`は1コマンド最大300秒・合計590秒で制限し、cargo/canbench中だけ固定common lockを使用する。
  終了時には元のCargo.lockを戻す。通常targetを再使用し、cold buildや`--persist`は行っていない。

| 検証 | 結果 / 記録 |
| --- | --- |
| `rustfmt --edition 2024 --config skip_children=true --check`（fold.rsのみ） | passed、`format-final-v2.log` |
| `cargo clippy --locked --offline -p ic-stable-lara --lib --tests --features canbench --message-format=json` | exit 0、`clippy-v2.log`。既存39警告箇所と一致、新規0、`lint-review.json` |
| `cargo test --locked --offline -p ic-stable-lara --lib --features canbench -- --nocapture fold_economics repeated_fold` | 8 passed / 620 filtered、runtime 3.20秒、`native-final-v2.log` |
| `canbench bench_t_repeat_ --csv --show-canister-output --runtime-path /Users/yota/dev/gleaph-spill/.canbench/pocket-ic`（E/canbench-current） | 24/24条件、392 scopes、build込み115.83秒、`canbench-v2.log` |
| `python3 E/analyze.py` | passed、`analyze-final-v2.log`。call数・全144 IC snapshot・page増分を照合 |
| `lizard --csv -i -1`（保存したbefore/current fold.rs） | passed、`lizard-v2.log` / `lizard-review.md` |
| 計画`plans/0373-repeated-fold-measurement.md`の`--phase final`構造検証 | passed、`plan-final.log` |
| 独立した読み取り専用レビュー | Standards/SpecともAPPROVE、未解決指摘なし。`review-standards-v2.md` / `review-spec-v2.md` |

新規nativeテストは`repeated_fold_{8,33}_{64,256}`の4件。
既存4件を合わせてgraph/MemoryManager構築は12→24回、新しいPocketICテストfixtureは0件。
canbenchは初回と修正後に各24条件、計48実行し、それぞれfixtureを構築した。
value fold省略、edge fold省略、追記1件欠落の3変異は、`repeated_fold_8_64`の実行時assertionで失敗した。
さらにprobe後にrunを再取得して返さない変異は、従来のID・high-water・pages・生存行検査だけでは通ったが、
全byte復元検査では失敗した。`mutations.json`に診断と完全復元を記録した。compile errorは検出と数えない。

途中probeを除いて復元検査を強化した最終ソースで全24条件を再計測し、CSV全行と全snapshotの値は初回と一致した。
初回のソース・Wasm・結果は`E/v1/`、照合結果は`revision-comparison.json`に保存した。

初期のedge再利用oracleは「4回目以降は毎回、全体を空きspanへ移す」と仮定し、5回目で失敗した。
`relocate_labeled_leaf_physical_block`と`try_expand_labeled_leaf_in_place`を確認し、
移設なら全範囲、その場での拡張なら追加範囲を検証するoracleに直した。実装や入力は変えていない。
最初の集計はCLI完了文言の想定違いで失敗したが、実測ログのsummaryと24件の名前・CSVで確認し直した。
いずれも失敗記録を残しており、再計測による数字の選別ではない。

### 複雑さはbenchmarkと観測に限定した

Lizard 1.24.0、対象はfold.rsのみ。HEADにはこの未追跡ファイルがないためGit基準との差は算出せず、
凍結した着手前ソースを追加分の基準とした。indexは空で、staged reviewではない。
関数数16→26、関数NLOC449→731、CCN合計31→66。直接変更した12関数のCCNは4→39、最大3→14である。
最大はnativeの3方式×8回の観測をまとめた`check_repeated_fold`。本番コードの複雑さ増加はない。

既存fixtureを再使用し、3方式を表すprivate enumだけを追加した。新しいmoduleや汎用実験frameworkはない。
全signatureはselfを含め最大4引数。arena構築の記述は既存probeから共通helperへ移し、
追記・fold・走査と終了後probeをnative/Wasmで共有した。初期状態の完全一致oracleは維持している。
内訳と判断は`E/lizard-review.md`に記録する。macroが生成するwrapperはLizardの関数数に含まれない。

## 限界: 毎回valueがspillする負荷はまだ測っていない

この実験の対象は、1つのtargetへの連続追記と固定peer、4つの初期形状、2つの読出頻度だけである。
独立した反復runの分散・信頼区間は測っていない。削除・断片化、継続的な別bucketの書込み、Tree、大次数、
実際のquery混合比やエンドツーエンド遅延にも一般化できない。
特にvalue foldの実処理は1回なので、「valueを8回移し直しても有利」という結論には使えない。

0368と0369は操作数が異なり、両方ともretirement修復前の値である。
0371は同じ旧exportを修復後に再計測したものだが、今回とは操作順・最終次数・読出内容が異なる。
過去の単発scopeを掛け算して今回のtotalを作ってはいない。
strict `-D warnings`は既存のGAP-LARA-TEST-CLIPPYにより未達。
full workspace/PocketIC suite、upgrade、instruction trace、persisted benchmark更新は対象外である。
日本語の機械lintはoffline cacheにSudachiPyがなく実行できず、手動チェックと通読で確認した。

## 次の判断: value-only候補を残し、継続的なspill条件を別に測る

今回の値中心の入力では、both-foldを毎回実行する追加費用をvalue-onlyとの差で回収できなかった。
自動化を検討するなら、値の幅と読出頻度に加え、targetが末尾を維持できるかを区別する必要がある。
次に調べるべきなのは、別bucketの書込みでvalue spillが再発する条件と、実際のquery構成に対するfold間隔である。
この計測だけでは発火閾値を決めない。自動fold、本番の経路変更、commit/pushは行わない。

追補: [0374で継続spill下のvalue-fold間隔を比較した](adr0097-continuous-spill-fold-intervals.md)。
