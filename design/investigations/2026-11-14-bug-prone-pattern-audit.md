# バグ誘発パターン監査: DRY / newtype / enum / wildcard / カプセル化 / RAII

Date: 2026-11-14 02:20 UTC+09:00
Author: Gleaph agent
Status: 静的監査（第 1 回）。修正は未実施。P1/P2 はコード経路を読んで確認し、実行での再現は行っていない。
Scope: `crates/` の 56 crate、`sdk/`、`examples/` の Rust 実装（テスト・ベンチは原則対象外）。約 672,000 行 / 1,073 ファイル。

## Goal

「正しさを型・所有者・単一経路で守れているか」という観点から、次の 6 分類で現行実装を洗い出す。

1. **DRY/SSOT** — 同じドメイン規則・定数・スキーマ・エンコーディング・検証が複数箇所にある。
2. **NEWTYPE 欠如** — 識別子・オフセット・長さ・世代・序数が生の `u32`/`u64`/`String`/タプルで流れ、取り違えを型が止めない。
3. **ENUM 欠如** — `bool` フラグ、文字列モード、マジックナンバー、矛盾可能な並行フラグ。
4. **WILDCARD** — enum への `match` の `_ =>` が、追加された variant を黙って既定（多くは「成功」「許可」）に落とす。
5. **ENCAP** — `pub` フィールド、コンストラクタ迂回、view を迂回する第二経路、同じ操作の複数経路。
6. **RAII 欠如** — 確保と解放の手動対、失敗点ごとのロールバック、義務状態を派生物から読む、順序規則がコメントだけ。

## Method

- まず機械的に候補を出した:
  `cargo clippy --workspace --lib --bins` に制限 lint
  （`wildcard_enum_match_arm`, `match_wildcard_for_single_variants`, `wildcard_in_or_patterns`,
  `cast_possible_truncation`, `cast_sign_loss`, `cast_possible_wrap`, `struct_excessive_bools`,
  `fn_params_excessive_bools`, `mem_forget`, `todo`, `exhaustive_enums`, `exhaustive_structs`）を付けて実行。
  結果は `target/audit/clippy.json`（gitignored）に保存。集計:
  - `wildcard_enum_match_arm`: **523 件 / 147 ファイル**
  - `cast_possible_truncation`: 755、`cast_sign_loss`: 103、`cast_possible_wrap`: 61
  - `struct_excessive_bools`: 5、`fn_params_excessive_bools`: 1
  - enum 以外も含む `_ =>` 腕は全体で 1,160、`matches!(...)` 内の `_` は 716
- 上記を 7 セクション（A: ストレージ基盤、B: graph、C: GQL、D: Router 入口/認可、E: Router 永続状態、F: カニスター/プロビジョニング、G: kernel/index/SDK/CLI/UI）に分け、
  各セクションで候補の周辺 30〜200 行を読み、設計文書（`design/adr/0100`, `0027`, `0035`, `0068`, `0074`, `0075`, `0082`, `0097`〜`0102` ほか）と照合した。
- **P1/P2 はリードが再読して確認**した。P3 の一部と「追加候補」はセクション担当の報告のみで、リード未再検証と明記する。
- 制約: 読み取り専用。ファイル変更・`cargo test`・PocketIC 実行はしていない。したがって P1 は「コード経路として成立する」ことの確認であり、実データでの再現ではない。

## 結論

- **P1 9 件、P2/P3 約 80 件（セクション別表）、横断 P3 16 件**。うち、実行時に実際に誤動作しうる P1 は
  認可 2 件（D-01, F-01）、状態破壊 2 件（F-03, A-01）、越境/監査 1 件（F-02）、
  永続証拠のリーク 1 件（E-01）、回復停止 1 件（E-02）、公開契約のドリフト 1 件（G-02）、
  多リソース状態機械 1 件（F-04）。
- 支配的なバグ族は 3 つ:
  1. **「同じ契約に家が 2 つ以上ある」**（wire 型、定数、ID 型、anchor 判定、サブプラン走査、ポリシー降下サイト）。
     コンパイル時に結合点が無いため、片側だけ直しても緑のままドリフトする。
  2. **「義務・完了・所有を派生物から読む」**（compaction で消える shard ベクタから retirement 義務を読む E-01、
     `done` と `next` の二重表現 E-03、生きた run を id 範囲だけで判定する A-01）。
  3. **「失敗点ごとの手動後始末」**（A-02, F-05, F-07, G-01, G-07, D-07）。
     `LtbBlockReservation` / `OwnedValueRoot` / `CanonicalSegmentGuard` という正解の型が既にあるのに、
     同型の局面で裸の値とコメントが使われている。
- 既存の `design/implementation-gaps.md` が **RESOLVED** と記録している
  GAP-2026-09-22-001（scalar retirement）は、E-01 のとおり実装としては成立していない。
  この記録は「解決済み」の再確認が必要である。

---

## P1 指摘

### P1-01 条件付き GRANT の述語が、プランナ生成の `IndexScan` / `IndexIntersection` で無言で落ちる
- 分類: WILDCARD + ENCAP（認可境界）
- 場所: `crates/router/src/policy_pushdown.rs:536-731`（`decide_site`、`_ => none()` は `:729`）、
  `crates/router/src/authz.rs:1755-1783`（`covers` は特権の有無だけを見る）、
  `crates/gql-planner/src/planner/match_plan/path/filters.rs:819-847`（索引等値で `IndexScan`/`IndexIntersection` を emit）
- 現状: ポリシー述語を計画へ降ろす `decide_site` は `NodeScan { label: Some(..) }`（`:551`）と
  `ExpandFilter { var_len: None }`（`:692`）の 2 腕しか持たない。プランナは索引済みプロパティの等値で
  `IndexScan` を出すが、これは `_ => none()` に落ちる。`lower_into_plan` は `()` を返し、
  「適用されたグループが 0 件」を検出する witness も無い。
- なぜバグか: 条件付き GRANT の認可判定は「特権行を持つこと」だけで、述語は出力フィルタとして
  後段で適用される設計（`gql.rs:3787-3793` のコメント）。フィルタ降下が起きないと、
  **GRANT の条件を満たさない行がそのまま返る**（fail-open）。例: `Person` に `visibility` 条件付き GRANT があり、
  `email` に索引があるとき `MATCH (p:Person) WHERE p.email=$e RETURN p` は `IndexScan` になり、
  `visibility` 条件が無視される。既存テストはすべて手組みの `NodeScan` から始めており、
  プランナ生成の `IndexScan` を入力にしたテストが 1 本も無い（`policy_pushdown.rs:1745-1785` は
  lowering 自身が `NodeScan` から `IndexScan` を作るケースだけを見る）。
- 修正方向: `IndexScan` / `ConditionalIndexScan` / `IndexIntersection` の腕を追加し、ラベル束縛が
  復元できない場合は doc どおりのガード式（`NOT IsLabeled(v,L) OR …`）を挿入する。
  `lower_into_plan` は「未適用グループ」を返し、呼び出し側（`gql.rs:3804`, `prepared.rs:963`）が
  fail-closed にする。索引→ラベルの解決は `authz.rs` の既存解決器と共有する。
- 確度: リード確認済み（腕の欠落・`covers` の述語非依存・プランナの emit をコードで確認）。
  攻撃成立の最終確認には索引付きグラフでの統合テストが必要。

### P1-02 text-canister の書き込み・全文検索エンドポイントが無認可
- 分類: ENCAP（認可境界）/ DRY（同じ境界の 2 契約）
- 場所: `crates/text-canister/src/lib.rs:206`（`ingest_text`）、`:217`（`delete_docs`）、`:229`（`search`）。
  対照は `:237`（`search_candidates`）と `:245`（`admin_merge_step`）の
  `#[query(guard = "guards::guard_controller")]` / `#[update(guard = ...)]`。
- 現状: `ingest_text` / `delete_docs` は素の `#[update]`、`search` は素の `#[query]`。
  canister 内で `msg_caller` を読むのは `guards.rs` だけで、`state::enqueue_ingest` は caller を引数に取らない。
- なぜバグか: IC の canister エンドポイントは誰でも呼べる。任意の principal が派生テキスト索引へ
  文書を注入・削除でき（検索結果の汚染・消去）、全文検索結果も任意 principal に返る。
  同じ操作族のキー限定検索 `search_candidates` が controller-only であるのに、
  キー無限定の `search` と書き込みが無認可というのは、同一境界に対する 2 つの契約である。
- 修正方向: 3 エンドポイントに `guards::guard_controller` を付ける。将来 Provision relay を許すなら
  `authorize_relay` と同型の純関数で認可行列を 1 箇所に持つ。
- 確度: リード確認済み（guard 属性と caller 参照の不在）。

### P1-03 `AccountStore::insert` が拒否パスで既存アカウントを上書きする
- 分類: ENCAP（状態破壊）/ 非 RAII
- 場所: `crates/account/src/stable/store.rs:21-28`
- 現状:
  ```rust
  let existed = memory::ACCOUNTS.with_borrow_mut(|map| map.insert(id, account).is_some());
  if existed { return Err(AccountError::AlreadyExists); }
  ```
  `insert` を先に実行してから存在判定するため、`Err` を返した時点で map の中身は新しいアカウントに置き換わっている。
- なぜバグか: 既存オーナーの個人アカウントに対して `create_account_with_caller` が呼ばれると
  `AlreadyExists` を返しつつ、`routers` を含む既存レコードが空の新レコードで破壊される。
  呼び出し側はエラーを見て「何も起きていない」と判断するのが自然で、成功応答なしに状態が失われる。
  既存テスト（`crates/account/src/canister.rs:279-282`）は同名で再作成するため差分が出ず、この破壊を検出しない。
- 修正方向: 先に `get` で存在確認してから `insert`。または store に compare-and-insert を 1 primitive として置く。
- 確度: リード確認済み。

### P1-04 Provision が `deployment_id` / `authorized_caller` を呼び出し元に束縛しない
- 分類: ENCAP（認可境界）+ NEWTYPE 欠如
- 場所: `crates/provision/src/canister/mod.rs:236-249`（唯一の認可は `deployment_store.contains(&caller)`）、
  `:127-153`（`req.deployment_id` / `req.authorized_caller` をそのまま永続化）、
  `:230`（`is_authorized_caller` は「非 anonymous が記録されているか」）。
  契約側は `crates/provision/src/types.rs:55-57`（「`deployment_id = issuer = caller` は常に一致」）と
  `design/adr/0035-provision-canister-and-issuance-protocol.md:60`。
- 現状: grant 集合への所属だけを見て、`req.deployment_id == caller` も
  `req.authorized_caller == caller` も検査しない。`authorized_caller` の参照は定義・記録・表示の 3 箇所のみ。
- なぜバグか: 付与された issuer A が `deployment_id = B` を名乗る request を送れる。
  `(deployment, logical_resource)` の intent lock は `deployment_id` で名前空間が決まるため、
  他 deployment の intent 名前空間を占有できる（恒久的 lock 枯渇。P2-05 と合成すると回復不能）。
  `authorized_caller` には任意 principal を記録でき、監査と `is_authorized_caller` 表示が嘘になる。
- 修正方向: 境界で `req.deployment_id == caller.to_text()` と `req.authorized_caller == caller` を検証するか、
  両フィールドを wire から削除して caller から導出する。`DeploymentId` / `RequestId` を newtype 化する。
- 確度: リード確認済み（検証コードの不在）。

### P1-05 scalar mutation の retirement 義務が、完了時に自分自身の遷移で消され、二度と retire されない
- 分類: DRY/SSOT（義務を派生物から読む）+ RAII
- 場所: `crates/router/src/facade/stable/label_stats.rs:1041-1054`（`scalar_retirement_pending`）、
  `crates/router/src/facade/store/idempotency.rs:2347-2362`（完了時に `compact_completed_record`）、
  `:320-341`（`scalar_clear_shards`）、`crates/router/src/gql.rs:6111-6124`（inline retirement を試みる）、
  `:6571`（recovery 側）、`crates/router/src/facade/store/idempotency.rs:271`（TTL pin）。
- 現状: `scalar_retirement_pending()` は
  `Scalar { retirement_acked: false } && is_terminal() && completed_row_count.is_some() && shards().len() == 1`
  を要求する。しかし scalar の shard を全完了させて `completed_row_count` を立てる唯一の生産経路 `record_scalar_shard_progress` は、
  同じ no-await 領域で `compact_completed_record(&mut record)` を呼び、`Scalar { shards }` を空にする。
  つまり「terminal かつ shards 1 件」という retirement 待ち状態は、その状態を作る関数の中で同時に破壊される。
  単一 shard の通常 INSERT では、capture 時点で `projection_advanced=false` のため `is_terminal()` が false で
  `gql.rs:6121` の inline retirement は発火せず、その後 `gql.rs:5467` で `ProjectionAdvanced` を記録した瞬間に
  compaction が走る。以降 `recover_mutation_record` は `is_terminal()==true` /
  `scalar_retirement_pending()==false` で即 `Ok(())` を返す。
- なぜバグか: Graph の journal entry は `Active` のまま誰も retire しない
  （`retire_scalar_record` の呼び出しは `gql.rs:6121` と `:6571` のみ）。さらに TTL eviction の pin
  （`idempotency.rs:271`）も同じ述語が false になるため効かず、
  7 日後に Router レコードだけが消える。結果は「Graph 側に永久に回収不能な Active evidence が残り、
  Router 側に retire する主体がいない」。`scalar_retirement_receipt`（`idempotency.rs:2223-2234`）も
  compaction 後は receipt を構成できない。PocketIC テスト `adr0027_scalar_journal_retention.rs` は
  Graph の `retire_scalar_mutation` を直接叩いており、この Router 経路を検証していない。
- 修正方向: 「retirement 義務」を shard ベクタ（compaction で消える派生物）から読むのをやめ、
  `Scalar` に独立した義務フィールド（例 `retirement: Option<RetirementOwed>`）として完了時に一度だけ書く。
  述語はそのフィールドだけを見る。`is_terminal` / `router_mutation_completed_row_count` /
  `scalar_retirement_pending` に散った「完了」導出も 1 つに統合する。
- 確度: リード確認済み（コード経路は全数読了。実行での再現は未実施）。

### P1-06 回復/保持タイマの論理状態が、メッセージ中断で「armed のまま timer 無し」に固定される
- 分類: RAII 欠如（中断時の状態復元）
- 場所: `crates/router/src/recovery.rs:84-112`（`request_arm` / `begin_pass` / `finish_pass`）、
  `crates/router/src/retention.rs:50-78`（`request_arm` / `begin_tick` / `finish_tick`）、
  `crates/router/src/lib.rs:123-126,148-152`（init/post_upgrade のみ arm）。
- 現状: `on_tick` は `begin_pass()` で `timer_armed=false, running=true` にしてから
  `run_recovery_pass().await` を実行し、戻ってきた後 `finish_pass()` で再アームを決める。
  `arm_if_needed` は `timer_armed==true` なら `AlreadyScheduled` を返して**何もスケジュールしない**。
  `running==true` なら `Latched` を返して**これもスケジュールしない**。
- なぜバグか: `run_recovery_pass` は 6 レーンを 1 メッセージで順に await する
  （projection / reclaim / effect / vector outbox / drop / retirement。加えて
  `read_unique_mutation_effects` は空ページまで頁ループするため上限が効かない）。
  trap・命令数超過・panic で中断すると `finish_pass` に到達しない。IC のメッセージ原子性により
  ヒープは開始時点へ巻き戻るので `timer_armed=true` が復元されるが、one-shot タイマ自体は
  既に消費済みで存在しない。以降の `arm_if_needed` は `AlreadyScheduled` を返し続け、
  ADR 0029/0030 の回復レーンが**恒久的に沈黙**する。retention も同形。
  具体的な trap 候補: `index_retirement.rs:175-176` の
  `expect("a stored retirement key is a valid PhysicalIndexId")`、
  `facade/stable/index_retirement.rs:102` の `decode_one(...).expect(...)`。
- 修正方向: in-flight タイマを RAII 値（`Drop` で「armed でない・running でない」を保証する型）にし、
  `arm_if_needed` が `running` を見たときに次のスケジュールを確約する。
  「論理 armed と実 TimerId が食い違ったら再スケジュールする」整合関数を毎 tick 入口に置く。
  6 レーンは 1 tick 1 レーンに分割して中断確率自体を下げる。
- 確度: リード確認済み（構造）。発火は trap 条件付きで、通常運用での再現は未確認。

### P1-07 bulk-load の wire 契約が 2 crate で二重定義され、既に 3 形状ドリフトしている（SDK が古い側を公開）
- 分類: DRY/SSOT（公開契約）
- 場所: `crates/router-wire/src/types.rs:673-680`（`BulkLoadChunkV1`）、`:719-746`（`BulkLoadResponse`）、
  `:770-776`（`BulkLoadChunkReceiptV1`）、
  `crates/bulk-load-api/src/lib.rs:134-154`（`Updates` variant）、`:199-226`（`Updated` variant）、
  `:240-246`（`updated_row_count`）、
  `sdk/canister/rust/src/lib.rs:201`（`bulk_load(command: BulkLoadCommand)` は router-wire 型）、
  `sdk/client/rust/src/lib.rs:78`（`pub use gleaph_router_wire::types::*`）、
  `crates/router/src/types.rs:31-36`（Router は bulk-load-api を re-export）。
- 現状: 同じ名前の公開 Candid 型が両 crate にあり、両方の crate doc が所有権を主張している
  （`router-wire/src/lib.rs:3-6` と `bulk-load-api/src/lib.rs:4-6`）。Router の実 entrypoint は
  `gleaph_bulk_load_api` を使う。SDK は `gleaph_router_wire::types::*` を公開面にしている。
  既にドリフトしている: router-wire の `BulkLoadChunkV1` に `Updates` が無い、
  `BulkLoadResponse` に `Updated` が無い、`BulkLoadChunkReceiptV1` に `updated_row_count` が無い。
  `AtomicInsertOperationV1` / `AtomicInsertEdgeV1` / `AtomicInsertEndpointV1` / `BulkLoadUpdateV1` は
  bulk-load-api にしか存在しない。
- なぜバグか: SDK 経由では update mode の bulk load を**表現できない**（`Updates` chunk を組めない）。
  Router は `Updated` / `updated_row_count` を返すため、SDK の decode は unknown variant で失敗し、
  `BulkLoadStatusPage.receipts` は `updated_row_count` を**黙って落とす**
  （Candid の record は余剰フィールドを読み飛ばす）。DRY 違反が「レビューで気付く」段階を越え、
  公開 API の意味が片側だけ欠けている段階に達している。
- 修正方向: `gleaph_bulk_load_api` を唯一の所有者とし、`router-wire` の当該型を削除して
  `pub use gleaph_bulk_load_api::{...}` に置換する。両 SDK の公開面はそのまま保たれる。
- 確度: リード確認済み（型定義と Router の戻り値型を突き合わせ済み。Candid 署名そのものは未確認）。

### P1-08 公開済み descriptor が解放済み run を指しうる（release → publish の順序と、範囲だけの生存検査）
- 分類: DRY/SSOT（順序規則の二実装）+ RAII
- 場所: `crates/ic-stable-lara/src/labeled/graph/value_plane.rs:259-298`
  （`publish_bucket_descriptor` は `spill_mint(run.get())` で id 範囲だけを見る）、
  `crates/ic-stable-lara/src/labeled/graph/remove.rs:509-535`（release → publish）、
  `value_plane.rs:178-186`（`release_run` は best-effort。release 成功時だけ descriptor フィールドを消す）、
  `crates/ic-stable-lara/src/labeled/graph/layout.rs:222-264`（逆順 = publish → release を明文化している側）。
- 現状: `publish_bucket_descriptor` の doc は「arena が保持しない run を名指す descriptor は publish できない」と
  書くが、実装は `SpillRunId::mint`（範囲検査）だけ。free list 上の run id は範囲内なので検査を通る。
  削除経路は `release_run()` で run を解放してから、後段 `remove.rs:535` で descriptor を publish する。
  コメントは「release が唯一の失敗点」を前提にしているが、publish 側の `write_label_bucket_slot`
  は `slab.write_slot` の失敗を返しうる。
- なぜバグか: publish が `Err` を返すと、(1) values run は free list に連結され、
  ヘッダ語は「次の free run id」に化けている、(2) 永続 descriptor は依然としてその run id を持つ、
  (3) その後の読みは `spill_mint(成功) → spill_used_len(run)` で free list のリンク値を
  used length として読む。`read_rows` は `used` を上限に `data_address(run, index)` を読むため、
  範囲外の arena 行（別バケットの行、未初期化行）を「このバケットの値」として返しうる。
  ADR 0100 の表が「A descriptor naming a released run — checked nowhere」として修正済みと主張している
  defect そのものが、範囲検査に置き換わっただけで生存している。
  同じ「run を差し替える」操作で `layout.rs:222-225` は正しい順序（publish → release）を明文化しており、
  同一規則が 2 つの順序で実装されている。
- 修正方向: `SpillRunStore` に liveness を答えられる API（`is_live(run)` / `ActiveRun` トークン）を足し、
  publish の検査を範囲ではなく生存に変える。削除経路も publish → release の順にし、
  release を `OwnedSpillRun`（Drop で解放、公開時に disarm）に包む。
- 確度: リード確認済み（範囲検査のみ / publish が `Err` を返しうる / release が先、の 3 点）。
  grow 失敗の再現は未実施。

### P1-09 多リソース Provision job で状態遷移が全拒否され、`let _ =` が hard gate 契約を握り潰す
- 分類: ENUM 欠如（状態機械）+ DRY
- 場所: `crates/provision/src/canister/mod.rs:362-451`（`let _ = store.advance_state(...)` は
  `:375, :386, :395, :400, :413, :424, :450`）、
  `crates/provision/src/types.rs:161-202`（遷移表、`advance_state` は hard gate と明記）、
  `crates/provision/src/stable/store.rs:234-277`（`advance_state`）。
- 現状: `deploy_job_resources` はリソースごとに同じ key に対して
  `CreatePending → CanisterCreated → InstallPending → Installed` を繰り返すが、遷移表は
  1 リソース分の線形列しか持たない（`(Installed, CreatePending)` は不正）。
  2 個目以降のリソースでは 4 回の遷移がすべて `Err(InvalidTransition)` になり、戻り値が `let _ =` で
  捨てられて処理は続行する。
- なぜバグか: 2 リソース以上の request（テスト `canister/tests.rs:2666-2667` が実際に 2 リソースを送る）で、
  `active_resource_index` は 0 のまま、`completed_effect_count` は 2 個目以降の効果を数えず、
  永続状態は「1 個目の resource の `Installed`」を指したまま実効果（2 個目の canister 作成・インストール）が進む。
  将来「永続状態から再開する」処理（ADR 0035 が要求）を足した瞬間に、実行済み効果を再実行する。
- 修正方向: 遷移表を（リソース単位の状態 × ジョブ全体の状態）に分けるか、リソース完了時に
  `active_resource_index` を進める明示的な `finish_resource(index)` を store に持たせ、
  `advance_state` の `Err` は伝播させる（`let _ =` を禁止する）。
- 確度: リード確認済み（遷移表とループ、2 リソーステストの存在）。

---

## P2/P3 指摘（セクション別）

### ストレージ基盤（A）

| # | 分類 | 場所 | 内容 |
| --- | --- | --- | --- |
| A-02 | RAII | `ic-stable-lara/src/labeled/graph/layout.rs:226-262` | `rebuild_run` が `spill_repack` で確保した `fresh` / `new_values` を裸の `SpillRunId` で保持する。`with_edge_run` / `with_values_run` / `publish_bucket_descriptor` のどこかで `Err` になると、確保・充填済みの run が誰にも参照されず解放もされない。arena は固定容量なので失敗の累積が `ArenaExhausted` と tree 昇格を早める。`OwnedValueRoot` と同じ型に寄せる。 |
| A-03 | WILDCARD | `ic-stable-lara/src/labeled/record.rs:761-780, 841-864` | 範囲外 run id を release ビルドで `None`（= run なし）に丸める。`debug_assert!` のみ。破損 descriptor が「行の無言欠落」になる。`try_read_from` は fail-closed なので、decode とアクセサで不正の定義が二重。 |
| A-04 | WILDCARD | `ic-stable-lara/src/labeled/bidirectional/deferred.rs:1650-1666, 1673-1684` | `resolve_bucket_slot` の `_ => None` が `Ok(Missing)` と `Err(op error)` を同一視し、呼び出し側は `unwrap_or(signal.bucket_slot)` で古い slot に materialize を enqueue する。`Err` を伝播させる。 |
| A-05 | ENCAP/ENUM | `ic-stable-lara/src/labeled/record.rs:302-325, 509-521, 1142-1153` | `with_tree_mode(bool)` / `with_tiny_used_width` / `with_stored_slots` が無検証。tiny に tree ビットを立てた行は再オープンで拒否され、`with_stored_slots` は release で tiny ペイロードを破壊する。`BucketResidency { Tiny, Slab, Tree }` に。 |
| A-06 | DRY/NEWTYPE | `ic-stable-lara/src/lara/edge/free_span.rs:27-32, 88, 1501, 1540, 1574, 1990-1997` | `FreeSpan { pub start_slot: u64, pub len: u64 }` に不変条件の所有者が無い。区間終端の導出が 4 通り（`checked_add` / 裸 `+` / `saturating_add`）で、`len==0` は `size_class(0)` のアンダーフローになる。`SpanId = u64` も start_slot と別型でない。 |
| A-07 | ENUM/SSOT | `ic-stable-lara/src/labeled/spill_run_store.rs:60-66, 271-318` | class 表が `MIN_ROWS` / `MAX_ROWS` / `CLASS_COUNT` の 3 定数と計算インデックスの暗黙一致に依存。`class_index` は `debug_assert` のみ、`free_head(class)` は境界検査なし。定数を広げると別クラスの free list と静かに混線する。 |
| A-08 | NEWTYPE | `ic-stable-lara/src/labeled/spill_run_store.rs:99-137` | 公開境界は `SpillRunId` なのに、同一モジュール内部は `type RunId = u32`。`data_address(run, index)` の 2 引数が同型で、sentinel 混同を型が防がない。 |
| A-09 | DRY | `ic-stable-lara/src/labeled/record.rs:920-943` | `inline_property_bytes_slab_slots()`（導出）と `..._allocated()`（保存値）が同名形状で別の数を返す。tree/tiny で 0 と非 0 が入れ替わる。 |
| A-10 | DRY | `ic-stable-paged-ordered-map/src/lib.rs:617-631, 720-741, 1085-1090` | `page_count` が「キャッシュされた `initial_pages`」と「image プローブ」の 2 経路。等価性が暗黙で、`EditImage` に読み取り専用ステージが入ると split が古い count で検査する。 |
| A-11 | ENCAP | `morph-dict/src/container.rs:88-127` | `validate` がエントリ名の重複を許し、`entry()` は find-first。同じ名前の 2 エントリで先勝ち。`validate` に一意性検査を追加。 |
| A-12 | NEWTYPE | `ic-stable-lara/src/labeled/tree_csr.rs:176-197` | `usize → u32` の失敗を `u32::MAX` に、バイト乗算を飽和に丸める。飽和値が「巨大な正当値」として下流に流れる。 |
| A-13 | ENUM | `ic-stable-lara/src/labeled/record.rs:625-660, 343-350` | tiny 行の妥当性が書き込み時の `E::BYTES` 依存で、decode は `TINY_MAX_DEGREE=4` しか見ない。edge 幅が変わると `init` は成功するのに特定行が壊れる。 |
| A-14 | ENCAP | `ic-stable-paged-ordered-map/src/lib.rs:1800-1856` | `validate()` が「free list 上のページが active 鎖にも居る」ことを検査しない。allocate が生きたページを返しうる。 |

追加候補（未検証）: `spill_run_store.rs:442` の `grow_in_place` が free list 再利用より先に id 容量で拒否する、`bucket_label_key.rs:64-78` の `& 0x7FFF` マスク、`row_plane.rs:86-90` の `Run { logical: None }` 再導出、`promote.rs:713-722` の手動 LIFO 解放。

### graph（B）

| # | 分類 | 場所 | 内容 |
| --- | --- | --- | --- |
| B-01 | DRY | `graph/src/plan/mutation/executor.rs:133` / `:326` | 約 190 行の op dispatch が sync / async でほぼ行単位コピー。本番は async のみだが、`label_stats_delta_*` 等のテストは同期版を直接呼ぶ（`:2267, 2335, 2436, 2480, 2519, 3418, 4403`）。片方だけの修正が緑のまま通る。`mutation_id` も `0` と `unwrap_or(0)` で既に非対称。 |
| B-02 | SSOT | `graph/src/plan/mutation/executor.rs:224, 412`（sync/async）、`graph/src/facade/batch_write.rs:256-267` | `hot_forward_vertices` が物理 forward owner ではなくプランの `src` を数える。`(a)<-[e]-(b)` を 2 本で誤発火し、実際に膨らんだ b ではなく a を compact 対象にする。batch 経路は `source_vertex_id`（物理 owner）を数えており非対称。 |
| B-03 | DRY | `graph/src/plan_wire_guard.rs:65, 77`、`graph/src/plan/query/executor.rs:228`、`graph/src/gql_run.rs:319` | 「seed でスキップ可能な anchor op」の規則が 4 実装。wire guard は `ConditionalIndexScan` を含むが executor/gql_run は含まない。seed 付き `ConditionalIndexScan` 先頭プランで anchor が実行され、`bind_local_index_hits` の直積で行が増幅しうる。 |
| B-04 | ENCAP/DRY | `graph/src/property/index_dispatch.rs:251`、`graph/src/facade/mutation_executor.rs:279-291` | bulk 頂点プロパティ書き込みだけが TEXT 派生を自分の責務にせず、呼び出し側が補填している。2 人目の呼び出し元で TEXT 索引が静かに古くなる。 |
| B-05 | DRY | `graph/src/plan/mutation/executor.rs:1032`、`graph/src/gql_run.rs:819` | label stats delta の集約規則（0 早期 return、同一 id 加算、合計 0 の retain）が完全に同一実装で 2 箇所。`LabelStatsDelta::merge` に 1 本化。 |
| B-06 | DRY | `graph/src/facade/store/helpers.rs:88`、`graph/src/facade/store/edge_scan.rs:996` | undirected の canonical owner 規則（`max(a,b)`）を 2 サイトが再実装。正規は `counterpart.rs:316`。 |
| B-07 | ENUM | `graph/src/facade/store/edge_insert.rs:57`、`helpers.rs:115, 142-148` | directedness が `bool` で内部境界を流れ、既存の `EdgeDirectedness` を迂回。`true/false` の取り違えを型が止めない。 |
| B-08 | NEWTYPE | `graph/src/plan/expr_evaluator.rs:605, 747`、`gql-value/src/types.rs:186` | `DECIMAL(p,s)` の `scale: Option<u64>` を検証なしで `as u32`。`DECIMAL(10, 4294967298)` が scale 2 として扱われる。 |
| B-09 | 対称性 | `graph/src/plan/query/edge_vector_kernel.rs:51` / `:74` | 同じ inline vector 述語で、単発 score は長すぎる payload を切り、バッチは候補ゼロにする。次元不一致の意味が 2 つ。 |
| B-10 | ENCAP/残骸 | `graph-kernel/src/entry/vertex.rs:38-56`、`graph/src/facade/stable/vertex_labels.rs:105-150` | `primary_label_id()` / `has_label_sidecar()` が常に `None`/`false`。`set_labels` も no-op の `with_primary_label_id` を呼び「行にも書いた」体裁を残す。superseded path の削除を。 |
| B-11 | WILDCARD | `graph/src/federation/expand.rs:29, 139` | 7 値 `EdgeDirection` の `_` が複合方向を `Outgoing` / `Directed` に潰す。現状 `#[expect(dead_code)]` だが、cross-shard expand 配線時に undirected バケットを引かない。 |
| B-12 | WILDCARD | `graph/src/plan_wire_guard.rs:61` | `ensure_execution_mode_matches_plans` の `_ => Ok(())`。現状 2 値なので外側の網羅 match が守るが、3 値目で「query 経路の DML 許可」に倒れる。 |

追加候補（未検証）: `facade/store/adjacency.rs:300-310` の `journal_edge_insert` が 6 引数すべて `_` で `Ok(())`、`index/*_pending.rs` の同型 5 本、`layout/session.rs:246-330` の `LayoutFlight` drop で engine が戻らない、`handlers.rs:1271, 1352` の `next_index = index as u32`。

### GQL（C）

| # | 分類 | 場所 | 内容 |
| --- | --- | --- | --- |
| C-01 | WILDCARD/DRY | `gql-planner/src/planner.rs:194, 234` | 未知の `Statement`（`CreateGraph` / `DropGraph` / `CreateGraphType` / `DropGraphType` / `Session` / `ExplainAuthorization`）を `Ok(PhysicalPlan::default())` に落とす。同じ dispatch 表が options 有無で 2 回。現状 Router/Graph が上流で分岐するため直ちに誤動作しないが、planner 公開 API では「未実装が成功」になる。 |
| C-02 | DRY/SSOT | `gql-planner/src/text_scan.rs:18-19`、`gql-planner/src/planner/validate.rs:13-23`、`router/src/gql_text_scan.rs:83-111` | 「未融合 `text_score` は plan validation で拒否」と doc に書きながら、planner の検証は `GLEAPH.VECTOR` だけ。実際の拒否は Router。planner の公開 API 経路では fail-closed が成立しない。 |
| C-03 | ENCAP | `gql-planner/src/planner/validate.rs:195-202`、`plan.rs:30-34`、`anchor.rs:600-621` | 汎用 planner に Gleaph 名前空間・Router 認可段階（`PROPERTY_FILTER_STAGE_POLICY_CHAIN = usize::MAX`）・Router カタログ名形式が入る。`design/gql/layers.md` と `AGENTS.md` の層契約、`extension-syntax.md` の planner 所有記述が矛盾。 |
| C-04 | WILDCARD | `gql/src/parser/types.rs:773-781` | 型位置の未知識別子を `ExtensionType` として受理し、ホスト側の解決点が INLINE 文脈にしかない。綴り間違いが構文エラーにならず、型検査は警告止まり。 |
| C-05 | DRY | `gql/src/validate.rs:70-80`、`type_check/diagnostics.rs:60-74`、`type_check.rs:210-223` | 「どの警告を致命エラーにするか」が 3 段（validate のホワイトリスト、diagnostics のホワイトリスト、code→severity 表）に分散。 |
| C-06 | DRY | `gql/src/type_check/infer.rs:451-523`、`gql/src/numeric_ops.rs:90-104`、`type_check/types.rs:325-336` | 型互換・数値昇格の規則が 4 実装。`infer_add(Int64, Decimal)` は Float64 だが実行時は Decimal（`Value::is_float` は Decimal を含まない）。`wider_int_vt` は符号を無視するので `Int8 + Uint64` が Uint64 と推論され、実行時は Int64。静と動が乖離。 |
| C-07 | ENCAP | `gql-planner/src/plan.rs:236-247` | `PhysicalPlan` の全フィールドが `pub` + `Default`。派生の `output` / `binding_layout` を迂回して任意の構造体リテラルを組める（`wire/bundle.rs:196-204`、`router/src/gql_search.rs:2243` ほか）。`hydrates_all_row_bindings` が「空だから全部」に化ける。 |
| C-09 | WILDCARD | `gql-planner/src/output_schema.rs:131-137, 260` | `EdgeIndexScan` を `OutputBindingKind::Vertex` として登録。`variable` は辺を束縛する。現状は materialize のフォールバックで誤値は出ないが、kind を信頼する消費側が増えた瞬間に辺を頂点として扱う。`binding_layout` と二重の登録表。 |

注: `Create OR REPLACE ... IF NOT EXISTS` の同時受理（C-08 相当）は
`design/investigations/2026-11-11-iso-conformance-audit.md` の F2 で「validate が拒否する」と
記録済みであり、欠陥ではなく既知の層分担とする。

### Router 入口/認可（D）

| # | 分類 | 場所 | 内容 |
| --- | --- | --- | --- |
| D-02 | WILDCARD | `router/src/graph_context.rs:90-109` | `SESSION SET SCHEMA` / `TIME ZONE` / `PARAMETER` と `RESET SCHEMA` 等が `_ => Ok(current)` で無言に無視され、既定グラフで実行される。`GraphParameter` だけは明示エラー。 |
| D-03 | DRY/SSOT | `router/src/general_routing.rs:79, 92, 111, 122-146` | `node_labels` を構築するが一度も読まない。エッジ端点を実行時ラベル名とだけ比較するため、ラベル集合付きの型を端点に持つエッジが無言で捨てられる。全プロパティ型が固定 `"string"`。 |
| D-04 | DRY/WILDCARD | `router/src/use_graph.rs:570-601`、`router/src/bulk_ingest_finalize.rs:84-105`、`router/src/policy_pushdown.rs:841-852`、`router/src/authz.rs:1293` | サブプラン走査が 4 実装。`SemiApply` の被覆が gql-planner と食い違う。`UseGraph` が `SemiApply` 配下に入ると単一グラフとして dispatch されうる。 |
| D-05 | ENCAP | `router/src/graph_context.rs:11-14`、`router/src/gql.rs:3639-3641` | `ResolvedGraphContext { pub graph_id }` を解決器を迂回して構築できる。bulk-load の bound-vertex 経路が実際に直接構築する。 |
| D-06 | WILDCARD | `router/src/types.rs:204` | 完了 receipt 投影の `_ => None`。新しい receipt 保持 variant を「receipt 無し成功」に落とし、採番済み ID を失う。現行 variant は正しい。 |
| D-07 | RAII | `router/src/gql_grants.rs:153-182, 1027-1060` | 全 statement を plan してから `apply(action)?` をループ。`READ` は複数行に lower されるため、途中失敗で先頭行だけ書けた状態が残る。「この後は失敗しない」はコメントだけ。現状の失敗要因は限定されるが、`AuthWriteError` に容量系が増えた瞬間に現実化。 |
| D-08 | ENUM | `router/src/execution_path.rs:21-52` | query/update 不整合検査を無効化する `force: bool` が 6 経路に配線され、`true` は 0 件。将来 `true` を渡すと検査が静かに消える。 |
| D-09 | NEWTYPE/DRY | `router/src/gql.rs:1030-1044, 1494-1525` | `(shard_id, vertex_id) ↔ u64` の手書きパックが 3 箇所。戻しは `as u32` の切り詰め。`PackedPostingKey` に 1 本化。 |
| D-10 | NEWTYPE | `auth/src/lib.rs:264`、`router/src/facade/stable/prepared_catalog.rs:26-34`、`router/src/prepared.rs:356-370` | prepared query 名が認可キー・安定キー・メタデータ照合キーを兼ねるのに生 `String`。空名・長さ・正規化の検証が無く、失効は完全一致。`PreparedQueryName` に。 |
| D-11 | DRY | `router/src/gql.rs:3320-3398, 3489-3535` | vector / text / ordinary index / constraint の DDL 入口 5 段（parse → authorize → mode → graph 解決 → execute）が 4 回逐語コピー。`IndexDdlLane` enum で差分だけ表す。 |

追加候補（未検証）: `authz.rs:312-392` の property-read 走査が planner の `property_uses()` の部分写し（サブプラン非再帰・`_ => {}`）、plan cache の `PreflightContext` が本番で生成されない、`api/inference.rs:302-321` が内部エラー文字列を返す、`general_routing.rs:66` の `_ => "any"`。

### Router 永続状態（E）

| # | 分類 | 場所 | 内容 |
| --- | --- | --- | --- |
| E-03 | ENUM/DRY | `graph-kernel/src/federation/index_posting_purge.rs:58-65`、`router/src/index_retirement.rs:107-113` | `IndexPostingPurgeStepResult { next, done }` は `done = next.is_none()` の派生値なのに両方を wire に載せ、Router は `next` だけ読む。`ConstraintPurgeOutcome { cursor, done }` も同型。`enum PurgeStep { More(cursor), Done }` に。 |
| E-04 | NEWTYPE/DRY | `router/src/recovery.rs:166-167, 186-187, 288-404` | 別 keyspace のカーソルが同じ `Cell<Option<u64>>`。lap の EOF 判定と `_LAP_FOUND` の意味が 6 レーンで手書きされ、既に揃っていない。`LaneCursor<K>` + `BoundedLapDriver<K>` に。 |
| E-05 | NEWTYPE | `router/src/facade/store/schema_migration/index.rs:53-61, 906-942` | `topology_epoch` / `lifecycle_catalog_epoch` / `prepared_catalog_epoch` がすべて裸 `u64`。同じ関数内で 2 つを別フィールドに振り分けており、取り違えが安全側に倒れない。 |
| E-06 | DRY | `router/src/text_sync.rs:17-46`、`text-canister/src/lib.rs:83-92` | `FlushReport` を Router が `FlushReportMirror` として手書き複製し「field order is the wire contract」とコメントで担保。text-canister は Router の依存に無い。decode 失敗は best-effort に飲まれる。 |
| E-07 | DRY | `router/src/facade/store/label_stats_projection.rs:96-139` | ラベル統計を集約値と per-shard 値で二重保持。`live_count == Σ shard live` の検証が無く、負 delta は `saturating_sub` で 0 に潰れる。COUNT fast path が集約値、ルーティングが per-shard 値を読む。 |
| E-08 | ENCAP/ENUM | `router/src/facade/stable/label_stats.rs:1229-1280, 1213-1227` | `RouterMutationShardV1` の `completed` / `projection_advanced` が独立 pub フィールド + pub セッタ。`completed=false, projection_advanced=true` が表現可能で、decode 時検証も無い。3 状態 enum に。 |
| E-09 | ENUM | `router/src/facade/stable/label_stats.rs:1021-1027` | `shards()` が非 Scalar payload を「shards 空」に潰す。`Option<&[...]>` にして呼び手に variant を意識させる。 |
| E-10 | ENUM | `router/src/facade/stable/unique_effect_pending.rs:101-107, 236-258` | `Quarantined` が状態ではなく「過去に orphan だった」履歴フラグ。実効的な意味は `next_retry_ns` が持つ。`enum Disposition { Active, Backoff { until_ns } }` に。 |
| E-11 | WILDCARD | `router/src/index_catalog.rs:866-879` | `value_type_is_container` の `_ => false` が「未知 → 索引可能」に倒れる。スカラを列挙し `_ => true`（拒否）に反転。 |
| E-12 | ENCAP | `router/src/peer_sync.rs:7-21` | 未実装の peer 同期が `Ok(())` を返し、doc は現在形で「伝える」と書く。成功を装う no-op。 |

### カニスター/プロビジョニング（F）

| # | 分類 | 場所 | 内容 |
| --- | --- | --- | --- |
| F-05 | RAII | `provision/src/stable/store.rs:157-217, 464-515`、`provision/src/canister/mod.rs:380-434` | intent lock / Map2 owner は `complete_graph_registration` の `Completed` でしか消えない。`release_intent_lock` / `clear_intent_locks_for_record` / `remove` は本番呼び出しゼロ。失敗 job は `Failed` で terminal になり、lock と作成済み canister（cycles）を残す。`JobState` に `CleanupPending` が無く、resume も無い（ADR 0035 と乖離）。 |
| F-07 | RAII/ENUM | `provision/src/canister/mod.rs:1093-1109`、`stable/artifact.rs:163` | `ArtifactUploadState::Failed` が terminal で、以後その ArtifactId へのチャンク投入を無条件拒否。upload 行を消すのは成功時のみ。1 バイト破損で artifact identity 全体が使用不能。エラー型も `ChunkHashMismatch` で原因と無関係。 |
| F-08 | ENCAP | `account/src/stable/store.rs:36-80` | 最後の owner を Member に落とす / 削除する経路を拒否しない。owner ゼロの Org は全操作が `NotAuthorized` になり回復不能。`InvalidRole` variant は未使用。 |
| F-09 | DRY | `text-canister/src/init.rs:11-30`、`graph-kernel/src/provisioning/init_args.rs:72-95` | `TextCanisterInitArgs` が 2 箇所に同一定義。doc が「Mirrors ... field exactly」と手動同期を宣言。Candid の record 幅部分型により、フィールド追加は受信側で黙って無視される。vector-canister は共有型を re-export しており、crate 内で規則が 2 種。 |
| F-10 | DRY | `artifact-api/src/types.rs:27-39, 74, 106, 184, 285`、`provision/src/types.rs:358, 387, 436, 445-447` | `CanisterKind` / `ArtifactId` / `ArtifactMetadata` / `ArtifactUploadState` / `ArtifactError` / `Release*` と 4 定数が二重定義。参照行番号は既に stale（`398-407` → 現行 `358`）。`ArtifactId` のフィールド順が両者で異なり、derive `Ord`/`Hash` の意味がずれる。 |
| F-11 | WILDCARD | `text-canister/src/state.rs:296-308, 1269-1283` | 永続 `dict_state: u8` + マジックナンバー。未知値は status では `Finalized`、ingest ゲートでは `!= FINALIZED` で拒否という自己矛盾。`DictState` enum + 単一述語に。 |
| F-13 | DRY | `text-canister/src/state.rs:640-648`、`backfill.rs:269-273`、`analyzer_mecab.rs:67-81` | 「登録済み analyzer id 集合」が 3 実装。過去に片側だけ更新された履歴がコメントに残る。 |
| F-14 | DRY | `instruction-budget/src/lib.rs:21, 66-70` | `plane_budget(plane)` が `plane` を無視し、`MAX_DYNAMIC_UPDATE_INSTRUCTIONS` と同一値を別式で再導出。 |
| F-15 | ENCAP | `provision/src/canister/mod.rs:108, 212, 230` | `is_authorized_caller` は「レコードに非 anonymous の `authorized_caller` があるか」で、呼び出し元と無関係。`caller` 引数は捨てられている。P1-04 と合成すると監査の嘘になる。 |

### kernel / index / SDK / CLI / UI（G）

| # | 分類 | 場所 | 内容 |
| --- | --- | --- | --- |
| G-01 | RAII | `cli/src/network.rs:286-300, 342-376` | `spawn_launcher` は spawn 後に status 待ちし、timeout / gateway 不達で `Err` を返すが `Child` を kill せず、PID file も書かない（`:355` は ready 後のみ）。再実行時 `:286` の稼働中ガードは PID file を根拠にするため発動せず、`:296` が稼働中プロセスの state dir を削除する。PocketIC state の破壊とポート二重取り。 |
| G-03 | DRY | `graph-kernel/src/federation/shard_id.rs:15, 99, 173, 246` | `ShardId` / `IndexClusterId` / `VectorIndexId` / `TextIndexId` が各約 90 行のほぼ同一実装。`checked_add` は `ShardId` だけ、`u64` 変換も `ShardId` だけ。stable key の互換性を 4 箇所で維持する必要がある。 |
| G-04 | NEWTYPE | `graph-kernel/src/index.rs:349-351, 438-441, 457-461, 485, 519` | 同じ wire 面で `ShardId` と生 `u32` が混在。`Vec<u32>` の理由コメント（nested newtype が往復しない）は、同ファイル `:1224-1237` の回帰テストと `#[serde(transparent)]` により既に失効。 |
| G-05 | NEWTYPE | `graph-kernel/src/plan_exec.rs:23, 26`、`federation.rs:67` | `MutationId = u64` / `ShardEventSeq = u64` / `LocalVertexId = u32` がエイリアス。doc は「0 は予約」と宣言するが、`ClaimId::new` / `EffectId::new` は 0 を受ける。`Option<MutationId>` は `Some(0)` を許す。`PhysicalIndexId` が良い前例。 |
| G-06 | ENCAP | `graph-index/src/guards.rs:6-9, 34-37, 65-68` | caller guard が native で無条件 `Ok(())`。認可ロジックは wasm ビルドにしか無く、native テストでは一度も実行されない。guard 本体を target 非依存の純関数に切り出す。 |
| G-07 | RAII | `cli/src/load.rs:1066-1079, 1282-1292` | `read_state_file` が ENOENT 以外の I/O エラーも `Ok(None)` に丸める。Completed 分岐は `None` だと digest 比較を飛ばして `Skipped` を返すため、artifact が変わっていても「ロード済み」で成功する。 |
| G-08 | DRY/WILDCARD | `gleaph-console/src/main.rs:570-589` | `ThemeChoice` の順序とラベルを console が再実装し、逆写像は `_ => Dark`。`label()` は呼ばれない。 |
| G-09 | DRY | `graph-view-web/src/lib.rs:148-153, 165-209` | node index をラベル文字列に往復させ、`parse::<usize>().unwrap_or_default()` で失敗を node 0 に潰す。index を 4 箇所で線形探索。 |
| G-10 | ENUM | `graph-index/src/facade/stable/memory.rs:58-79` | `initialized: bool` + 未初期化時のダミー `graph_id=0` / `group=0`。「未設定」と「graph 0 で設定済み」が別表現。 |
| G-11 | ENUM | `cli/src/decision.rs:51-58, 159-206` | `JudgeMode::resolve` が `Auto` を含む同じ型を返し、`build_request` は `Auto` を Noul として黙って処理する。`ResolvedJudgeMode` に。 |
| G-12 | ENUM/SSOT | `graph-kernel/src/vector_index.rs:876-892, 975-990`、`vector-canister/src/facade/store/maintenance_step.rs:238-243` | 走査カーソルを `Option<Vec<u8>> + bool` で表現し、2 つの doc が同じフィールドについて異なる規約を述べる。`enum ScanCursor { Start, At(...), Done }` に。 |
| G-13 | ENUM/DRY | `graph-kernel/src/provisioning/mod.rs:45-89`、`graph/src/facade/stable/index_pending_floor.rs:16-21, 63` | stable key の u8 タグを encode / decode で二重管理。片方だけ更新すると既存 key が別 variant として復号される。`#[repr(u8)]` + `tag()` に。 |

### 横断（複数 crate にまたがる重複）

| # | 分類 | 場所 | 内容 |
| --- | --- | --- | --- |
| X-01 | DRY | `router/src/prepared.rs:48` / `cli/src/prepared.rs:42` | `MAX_PREPARED_BATCH = 32` が 2 箇所。CLI 側は「Keep in sync with the Router constant」とコメント。CLI は `router` に依存できないが `router-wire` には依存できる。 |
| X-02 | DRY | `router/src/gql_text_scan.rs:60` / `text-canister/src/state.rs:159` | `MAX_CANDIDATE_KEYS = 256` が「mirrors the canister」として 2 箇所。Router が admission し、canister が実行時拒否する。 |
| X-03 | DRY | `auth/src/lib.rs:875` / `gql/src/parser/statement.rs:1843` | ADR 0082 の chain hop 上限 2 が、auth の `pub const` と gql parser のローカル const に二重定義。parser が通り auth が拒否する（または逆）ドリフトになりうる。 |
| X-04 | DRY | `artifact-api` ↔ `provision` ↔ `ingress-client` | artifact/release/audit の wire 型が 3 crate に重複。`ingress-client/src/wire.rs:119` の `BootstrapAuthAction` は provision と variant 順が異なる。 |
| X-05 | ENUM | `graph/src/canister/handlers.rs:1529-1534, 314, 327, 1285` | `execute_plan_impl(input, drain_outbox_after, check_journal, write_incomplete_journal, write_complete_journal)` の 4 bool。3 呼び出ししかなく、`write_incomplete_journal` は全経路 false（死んだフラグ）。`run_wire_plans_last_read_row_count` も `check_journal` / `write_journal` の 2 bool を受け、層ごとに `write_incomplete` / `write_journal` / `write_complete` と呼び名が変わる。`PlanExecutionMode` enum に。 |
| X-06 | WILDCARD | `bulk-load-api/src/lib.rs:518-539` | `validate_batch_endpoint` の `_ => Ok(())`。将来 `AtomicInsertEndpointV1` に variant が増えると検証を素通りする。現行 2 値では正しい。 |
| X-07 | NEWTYPE | `graph-kernel/src/index.rs:156-178` | `IndexPostingMutation` の `property_id: u32` / `label_id: u16` / `vertex_id: u32` が生値。同じファイルの `IndexBuildTarget` は `PropertyId` newtype を使う。 |

---

## P3（局所・命名・残骸。修正は P1/P2 の後）

| # | 場所 | 内容 |
| --- | --- | --- |
| P3-01 | `gql-planner/src/executor_contract.rs:29-75, 172-183` | doc は `gleaph_gql_executor::execute_ops` と同期せよと言うが、その crate は存在しない。`check_op` は未知 op を「対応済み」に落とし、production から呼ばれない。 |
| P3-02 | `gql/src/validate/session_graph.rs:22-35` | `SeedGuard::drop` が前値を復元せず `None` にする。現状ネストしないため実害なし。 |
| P3-03 | `gql/src/validate.rs:150`、`gql/src/type_check.rs:274` | DDL/Session 前後でバインディングを空に落とす wildcard。仕様意図の確認が必要。 |
| P3-04 | `gql-planner/src/planner.rs:910-942` | 末尾が `Project`/`Materialize` でないと隠し変数追加が黙って no-op。 |
| P3-05 | `graph-kernel/src/federation/global_edge_id.rs:73` | `u32::from_le_bytes(label) as u16` が上位 16 bit を切り捨て、16 byte id の一意性を壊す。 |
| P3-06 | `graph-kernel/src/vector_index.rs` / `IndexEqualSpec` | `Default` + pub フィールドで edge 意図が vertex arm に化ける。 |
| P3-07 | `graph-kernel/src/index.rs:662-669` | `IndexIntersectionResult::vertices()` が `Edges` に空 Vec を返す（エラーでも panic でもない）。 |
| P3-08 | `graph/src/gql_run.rs:481` ほか | `build_statement_plan` 経由の DDL/Session は C-01 と同じ。Router では上流で分岐済み。 |
| P3-09 | `graph/src/plan/query/executor/scan/streaming.rs:61-107` | 「行数を保存する op」のホワイトリストが 2 つあり、`KeepBindings` がどちらにも無い。 |
| P3-10 | `authz.rs:390` | `collect_property_reading_vars_in_op` の `_ => {}` がサブプランを再帰しない。 |
| P3-11 | `router/src/api/inference.rs:302-321` | 内部エラー文字列を `AskFailure.message` に載せる。 |
| P3-12 | `general_routing.rs:66` | `_ => "any"` が未知 `SemanticType` を wire 上 any に落とす。 |
| P3-13 | `cli/src/main.rs:48-55` | `CliError::exit_code` の `_ => 1`。 |
| P3-14 | `cli/src/config.rs:105` | `GLEAPH_*` の typo を `_ => {}` で無視（TOML は deny_unknown_fields で厳格）。 |
| P3-15 | `text-analyzer-spike` / `morph-dict` | analyzer の型が spike と本体で二重。 |
| P3-16 | `graph-view-rune/src/panel.rs:386-406` | layout の dirty 管理が `moving` で 2 回分岐（同一条件の重複）。 |

---

## 横断分析

### 1. 「同じ契約に家が 2 つ以上ある」— 最も多い

公開 wire 型（P1-07, F-09, F-10, E-06, X-04）、定数（X-01〜X-03）、
ID 型（G-03, G-04, G-05, A-06, A-08）、anchor 判定（B-03）、
サブプラン走査（D-04）、ポリシー降下サイト（P1-01）、
index DDL 入口（D-11）、mutation executor（B-01）、
`LabelStatsDelta` の merge（B-05）、undirected owner（B-06）。

これらは「片方だけ直してもコンパイルが通り、テストも緑」という共通性質を持つ。
AGENTS.md の DRY 節が「drift がバグを生む」と書いているとおり、
今回の P1 のうち 3 件（D-01, G-02, F-10 系）は既にドリフトが実在する。

**原則**: 契約は 1 つの neutral crate（`*-api` / `*-wire` / `graph-kernel`）に置き、
canister crate と client crate の双方が re-export する。定数はその crate の `pub const` を参照する。
「Mirrors ...」というコメントは同期の証明ではない。

### 2. 「義務・完了・所有を派生物から読む」

E-01（compaction で消える shard ベクタから retirement 義務を読む）、
E-03（`done` と `next`）、A-01（run の生存を id 範囲で判定）、
E-07（集約値と per-shard 値）、A-09（導出 slot 数と保存 slot 数）。

**原則**: 義務は義務として独立したフィールド/型に持つ。
完了は enum 1 つで表す。生存は free list のリンクではなく liveness API で答える。

### 3. `bool` モード

X-05（journal 4 bool）、D-08（`force`）、B-07（directedness bool）、
F-04（`initialized`）、E-08（completed/projection_advanced）、
G-12（cursor + exhausted）、A-05（tree mode bool）、C-08（or_replace / if_not_exists）。

**原則**: 相互排他な状態は enum、独立した事実は bool のままでよいが、
「同時に true になってはならない」ものを並行 bool にしない。

### 4. 失敗点ごとの手動後始末

A-02（repack run のリーク）、F-05（intent lock / canister）、F-07（Failed upload）、
G-01（launcher）、G-07（state file）、D-07（GRANT の部分適用）、E-02（timer）。

**原則**: 確保したものは Drop で返す（`LtbBlockReservation` / `OwnedValueRoot` が前例）。
「この後は失敗しない」はコメントではなく型で表す。

---

## 修正優先順位（提案）

1. **認可**: P1-01（条件付き GRANT の IndexScan）、P1-02（text-canister の guard）、P1-04（Provision の deployment 束縛）。
   いずれも fail-open で、テストが緑のまま残る種類。
2. **状態破壊**: P1-03（account の上書き）、P1-08（released run を指す descriptor）。
   どちらも「エラーを返したのに状態が変わっている」形。
3. **永続証拠と回復**: P1-05（scalar retirement）、P1-06（timer latch）。
   ADR 0027/0029 の前提を壊しており、運用で静かに蓄積する。
4. **公開契約**: P1-07（bulk-load）、F-09/F-10（artifact/text init）、X-01〜X-03（定数）、G-03/G-04（ID）。
   neutral crate への移設と re-export で、振る舞いを変えずに結合点を作れる。
5. **構造**: B-01（sync/async executor）、E-08（scalar shard 状態）、F-04（多リソース状態機械）、D-01 の witness。
6. 上記の後で P2 の局所修正と P3 の掃除。

---

## 設計文書とコードの不一致（AGENTS.md の要求により列挙）

1. **AGENTS.md / `design/gql/layers.md`** は「Gleaph 固有を `gql` / `gql-planner` に入れない」と書くが、
   `gql` は `feature = "gleaph"` で GRANT/REVOKE・EXPLAIN AUTHORIZATION・`extract_grant_chain` を持ち、
   `gql-planner` は `PlanOp::TextScan`・`PROPERTY_FILTER_STAGE_POLICY_CHAIN`・`gleaph.vector` 判定を持つ。
   `design/gql/extension-syntax.md:993-996` は planner が lower すると明記しており、2 つの設計文書が矛盾する。
2. `crates/gql-planner/CLAUDE.md` は「PlanOp 33 variants」と書くが、`plan.rs` の実体は 38。
3. `crates/gql-planner/src/executor_contract.rs:1-5` は存在しない `gleaph_gql_executor` を参照する。
4. `crates/text-canister/src/init.rs:17` は analyzer を「∈ {1, 2}」と書くが、実際は 0〜3 の 4 値。
5. `crates/artifact-api/src/types.rs` の provision 行番号引用（`398-407`, `428-441`, `481-483`）は
   現行の `358`, `387`, `445-447` と一致しない。
6. `crates/graph-kernel/src/index.rs:349-351` の「nested newtype が往復しないので `Vec<u32>`」は、
   同ファイル `:1224-1237` の回帰テストと `#[serde(transparent)]` により失効している。
7. `design/adr/0035-provision-canister-and-issuance-protocol.md:90-102` は
   resume と `CleanupPending` を要求するが、`JobState`（`provision/src/types.rs:131-141`）に
   `CleanupPending` は無く、resume 経路も無い（P1-09 / P2-05）。
8. `graph-kernel/src/vector_index.rs:883` と `:977-979` は、同じ `cursor`/`exhausted` の関係について
   異なる規約を述べている。
9. `design/implementation-gaps.md` の GAP-2026-09-22-001 は RESOLVED と記録するが、
   P1-05 のとおり Router の自動 retirement は成立していない。
10. 複数の ID 型（`GraphId`, `IndexNameId`, `ConstraintNameId` など）が「0 is reserved」と書きながら
    `Default` を derive し、`Default::default()` がちょうど予約値を生成する。強制点は型ごとに異なる
    （`PhysicalIndexId` のみ `new -> Option<Self>` + serde 拒否）。
11. `router/src/policy_pushdown.rs:25-26` は「index scan にはガード付きフィルタを注入する」と書くが、
    その腕が無い（P1-01）。
12. `router/src/general_routing.rs:144` は「Endpoint type names are resolved to their first runtime label」と
    書くが、その解決に使う `node_labels` は未使用（D-03）。
13. `gql-planner/src/text_scan.rs:18-19` / `plan.rs:456-460` は「未融合は plan validation で拒否」と書くが、
    planner の検証に `text_score` は無く、Router が拒否している（C-02）。

---

## カバレッジと限界

| セクション | 精読の概算 | 未読・未断定の主な領域 |
| --- | --- | --- |
| A ストレージ基盤 | `record.rs` / `value_plane.rs` / `layout.rs` / `free_span.rs` / `spill_run_store.rs` / `paged-ordered-map` の edit/read / `vec-deque` / `variable-memory-manager` | `insert.rs`(2.3k), `tree_write.rs`(6.8k), `compact.rs`(10k), `linear-hash-map` 本体, `text-postings`, `vector-page-store` の kernel |
| B graph | executor の op dispatch / eval / expand / property / facade store の主要経路 / graph-view-core layout | `index/canonical_export.rs`(3.4k), `vertex_delete.rs` 後半, `path/*`, `paint.rs`(5.6k) |
| C GQL | type_check / parser / validate / planner / wire / output_schema | `match_plan.rs`(1.9k), `query_validation.rs`(1.4k), `constraint.rs`, `gql-ic*` |
| D Router 入口 | rbac / authz / policy_pushdown / gql 入口 / graph_context / prepared / routing | `gql_search.rs`(9.2k), `gql_text_scan.rs`(3.8k), `seed.rs`(3.0k), `bulk_load.rs` |
| E Router 状態 | recovery / effect_recovery / index_sync / retention / label_stats 投影 / reservation_catalog | `provisioning/graph.rs`(1.9k), `bulk_load.rs`(2.7k), `index_catalog.rs` の変更系 |
| F カニスター | provision types/store/canister の主要部 / account / text state / vector records | vector-canister の page_store/rebuild/search 本体（31k の大半） |
| G kernel/SDK/CLI/UI | shard_id / index / plan_exec の ID / cli network/load/decision / web/rune/console の接続 | `plan_exec.rs` 中央, `cli/ask.rs`(3.1k), `gpui-graph/view.rs`(4.9k)，graph-index worker |

- したがって本記録は「網羅的な保証」ではない。P1 はコード経路として成立することを確認したが、
  実行での再現（PocketIC / 単体）は未実施。特に P1-01 は索引付きグラフの統合テスト、
  P1-05 は Router 経由の DML 1 回で Graph journal が `Active` のまま残ることの確認が必要。
- 機械的スキャンはライブラリ/binary 対象で、テスト・bench の `_ =>` はほぼ除外している。
  テストだけが本番でない経路を検証している例（B-01）は、その逆の危険として記録した。
- 既知の良好例（修正の参考）:
  `LtbBlockReservation`（`ltb_raw_block_store.rs:294-354`）、`OwnedValueRoot`（`tree_plane.rs:287-337`）、
  `CanonicalSegmentGuard`（`facade/canonical_segment.rs:76`）、
  `PhysicalIndexId`（`graph-kernel/src/index.rs:50-116`）、
  vector-canister の tag decode（`records.rs:1405` ほか `_ => Err(Tag)`）。

## Reproduction notes

- clippy 生データ: `target/audit/clippy.json`（再生成:
  `cargo clippy --workspace --lib --bins --message-format=json -- -W clippy::wildcard_enum_match_arm
  -W clippy::cast_possible_truncation -W clippy::cast_sign_loss -W clippy::cast_possible_wrap
  -W clippy::struct_excessive_bools -W clippy::fn_params_excessive_bools -W clippy::mem_forget
  -W clippy::todo`）。
- 本記録の行番号は 2026-11-14 02:20 JST 時点の `main`（`28370ad1`）で確認した。

## Disposition (2026-11-14, same day remediation slice)

Scope decision: the project is pre-production, so this slice changes the canonical layout and
wire shapes directly. No migration, no compatibility branch, no dual decoder was added.

| Finding | Disposition | Where |
| --- | --- | --- |
| P1-01 conditional GRANT on index anchors | **fixed.** `decide_site` now lowers `IndexScan`, `IndexIntersection`, `ConditionalIndexScan` and `TextScan`; index anchors without a label fact receive `NOT IsLabeled(v,L) OR policy(L)`, and the chain probe lowering is shared with `NodeScan`. | `crates/router/src/policy_pushdown.rs`; regression `planner_emitted_index_anchor_receives_a_label_guarded_policy_filter` |
| P1-02 text-canister write/search authorization | **fixed.** `ingest_text`, `delete_docs`, `search` are controller-guarded; the decision is a native-testable pure function. | `crates/text-canister/src/lib.rs`, `guards.rs`; controller matrix test |
| P1-03 account overwrite on rejected create | **fixed.** Existence is checked before the write; last-owner demotion/removal is rejected. | `crates/account/src/stable/store.rs`; two regression tests in `canister.rs` |
| P1-04 provision deployment binding | **partially fixed.** The envelope must carry a non-anonymous `authorized_caller` and an `intent_key` naming the same deployment. The own-issuer/relay split remains the grant set's trust boundary: Provision cannot verify an Org account id without a relay scope, so that design decision is still open. | `crates/provision/src/canister/mod.rs`; two accept tests |
| P1-05 scalar retirement never fires | **fixed.** `ScalarRetirementOwed { graph_canister, shard_id, row_count }` is captured at the terminal transition before the dispatch envelope is compacted; pending/ack/receipt read that field only. Inline retirement also runs after the projection-advanced transition. | `facade/stable/label_stats.rs`, `facade/store/idempotency.rs`, `gql.rs`; regression `scalar_completion_captures_retirement_before_compacting_the_dispatch_envelope` |
| P1-06 recovery/retention timer latch | **fixed.** Both lanes create a deadline watchdog interval at install/upgrade; a latched `timer_armed` whose deadline passed without a tick is reset and re-armed. The lane no longer depends on the pass returning normally. | `crates/router/src/recovery.rs`, `retention.rs`, `lib.rs` |
| P1-07 bulk-load wire duplication | **fixed.** `router-wire` deletes its hand-written family and re-exports `gleaph-bulk-load-api`; the SDK test exercises the update chunk, `Updated` response and `updated_row_count` receipt. | `crates/router-wire/src/types.rs`, `Cargo.toml`, `sdk/canister/rust/src/lib.rs` |
| P1-08 spill-run ownership | **partially fixed.** `OwnedSpillRun` releases a repacked run on every failure path before publication; the delete path publishes the descriptor before releasing the values run. The publish-side liveness check is still a range check, not a free-list membership check. | `labeled/graph/layout.rs`, `value_plane.rs`, `remove.rs`; guard test |
| P1-09 multi-resource provision state | **fixed.** `Installed -> CreatePending` is the per-resource re-entry, and every `advance_state` result is propagated (`StateAdvanceFailed`); the multi-resource test asserts state, active index, effect count and both recorded resources. | `crates/provision/src/types.rs`, `canister/mod.rs`; `multi_resource_job_advances_each_resource_and_records_every_effect` |
| Duplicate `MAX_PREPARED_BATCH` | **fixed.** Owned by `gleaph-prepared-api` and re-exported by Router and CLI. | `prepared-api/src/lib.rs`, `router/src/prepared.rs`, `cli/src/prepared.rs` |
| Duplicate `TextCanisterInitArgs` | **fixed.** Text canister re-exports the graph-kernel type; the kernel type gained the serialization derives the install side needs. | `graph-kernel/src/provisioning/init_args.rs`, `text-canister/src/init.rs` |

Remaining from this audit (not in this slice): artifact-api/provision/ingress-client wire duplication,
the four `ShardId`-family newtypes, `MutationId`/`ShardEventSeq` newtypes, the bool-mode journal and
execution flags, the general wildcard sweep, the fail-open decode of an out-of-range spill run, and
the relay-scope decision for P1-04. The report above remains the record of those findings.

### Verification

- `cargo test -p gleaph-router -p gleaph-provision -p ic-stable-lara -p gleaph-account -p gleaph-cdk --lib`:
  1166 / 136 / 800 / 6 / 19 passed, 0 failed.
- `cargo test -p text-canister -p gleaph-router-wire --lib`: 96 / 7 passed, 0 failed.
- `cargo check -p gleaph-router -p text-canister -p gleaph-provision --target wasm32-unknown-unknown`:
  the wasm-only watchdog and inline-retirement code compiles.
- `cargo fmt --all -- --check` and `git diff --check`: clean.
- The stale `PreparedOperation` literals in `prepared-api` tests (missing `text_fusion`) were updated so the
  shared contract crate's tests build again.

### P1-04 closure (2026-11-14)

The deployment grant is no longer a bare membership set. Each issuer has a `DeploymentGrantScope`:
`OwnDeployment` issuers must set `deployment_id = caller.to_text()`, while a `Relay` issuer (the
Account canister) may name the account it issues for. Governance writes the scope through
`upsert_deployment_grant { issuer, scope }`; the Router-install auto-grant writes
`OwnDeployment`, and the CLI seeds the Account canister as `Relay` before starting the network.
`query_job` and `complete_graph_registration` apply the same own-deployment check, so an own
issuer can neither read nor acknowledge another deployment's job.

Tests: own issuer naming another deployment is rejected; a Relay issuer is accepted; the scope
survives a store reopen; the governance upsert records `Relay`; the Candid export and all wire
mirrors (CLI, operator, ingress-client, PocketIC tests) carry the new field.

Residual trust: Provision cannot verify an Org account id or that the Account is issuing a first
Router. A Relay grant therefore authorizes the Account to name any deployment, including one that
already has a Router. That first-Router rule stays in Account's `authorize_router_issuance` /
`resolve_router` path; the grant records the accepted trust boundary rather than re-deriving it.

### P1-08 closure (2026-11-14)

Spill layout version 3 gives every run **two header rows**. Word 0 is the used length while live and
`FREE_MARKER` (`u32::MAX`) while free; word 1 is the free-list link. A live length is at most
`MAX_ROWS` (1024), so the marker cannot alias one, and `SpillRunStore::is_live` no longer has to
trust a second field. `release` refuses an already-free run, and `read_rows` treats a freed run as
having no rows.

`publish_bucket_descriptor` now checks both planes' liveness: a descriptor that still names a freed
values or edge run traps instead of writing a free-link word into stable memory where the read path
would later interpret it as a used length. `LabelBucket::edge_run` / `values_run` also panic on an
id outside the arena instead of returning `None` in release builds (A-03).

The liveness assertion exposed a real stale-descriptor bug in non-forcing compaction: when
`rebuild_run` reclaimed only tail holes it returned an empty move batch, and the caller continued
with the pre-rebuild bucket, whose run had already been released. The step now refreshes the bucket
from stable memory after every rebuild, including the empty-move case.

Tests: 804 `ic-stable-lara` tests, 1166 `gleaph-router` tests, and the router wasm check pass. New
coverage: released run is not live, reuse clears the marker, double release traps, the largest class
does not collide with the marker, the marker/link survive reopen, and both publish paths reject a
freed run.

### P1-08 follow-up closure (2026-11-14)

`SpillRunStore::grow_to` no longer releases the relocated run. It returns a `GrownRun` whose
`retired` field stays live until the caller publishes the new run id; on publication failure the
graph releases the new run and keeps the descriptor's original run. The values plane carries the
retired chain through `BucketValuePlane` and releases it only after `publish` succeeds. ADR 0097's
`run + 1 + i` text was corrected to `run + 2 + i` (two header rows).

### B1 closure: artifact wire ownership (2026-11-14)

`gleaph-artifact-api` now owns the artifact/release/install/audit wire types and the catalog
bounds constants. Provision re-exports them, and `gleaph-ingress-client` re-exports the same
types for operator/CLI callers; the hand-maintained copies are deleted. `ArtifactStorageId` moved
with `ArtifactMetadata` (its Candid form stays `nat64`), and `Storable` impls live beside the
types behind the crate's optional `stable` feature so `ic-stable-structures` stays out of client
builds.

The canonical field/variant order is the did's order. That changes the Candid bytes used by
Provision's stable maps (catalog, upload, release manifest): this is a pre-production fresh-state
change, not a migration. Cross-crate tests encode with Provision's re-export and decode as
`gleaph_artifact_api::types`, and vice versa, covering metadata, install args/results, audit
entries and install errors.

### B2 closure: ordinal IDs (2026-11-14)

`ShardId`, `IndexClusterId`, `VectorIndexId` and `TextIndexId` now come from one `ordinal_id!`
macro, so `new`/`raw`/`to_le_bytes`/`from_le_bytes`/`checked_add`, the `u32`/`u64` conversions,
`Display` and the fixed-size `Storable` form are identical for all four. `ShardId` previously had
`checked_add` and `From<ShardId> for u64` while the other three did not; that asymmetry is gone.

`RegisterIndexBuildRequest.target_shard_ids` is `Vec<ShardId>` again. The old `Vec<u32>` field and
its "nested newtype serde is not self-roundtrippable" comment were stale: `#[serde(transparent)]`
already makes the container round-trip, and the existing typed vector test covers it. Router and
graph-index convert at their storage/wire boundaries instead of widening the public contract.

Coverage: graph-kernel 200, graph-index 157, router 1166 passed.

### C1 closure (2026-11-14)

`execute_plan_impl` no longer takes `drain_outbox_after`, `check_journal`,
`write_incomplete_journal` and `write_complete_journal` as four independent booleans. Each
endpoint names its shape: `ExecutionProfile::{Query, Update, BatchUpdate, BulkUpdate}`. The
profile owns both the synchronous outbox drain and the journal participation, so a call site
cannot select an impossible combination (for example, a complete journal without a replay
check).

`run_wire_plans_last_read_row_count` takes `JournalMode::{Disabled, CheckOnly, CompleteOnly,
CompleteAndIncomplete}` instead of `check_journal` + `write_journal`; the query wrapper and the
scalar helper select `CheckOnly` / `CompleteOnly` explicitly. Tests pin the four mode
capabilities and the endpoint-to-mode/drain mapping.

### C1 force follow-up (2026-11-14)

Every production construction of `GqlCall` / `PreparedRun` set `force: false`; the only `true`
callers were two unit tests asserting the bypass. The field, the `force` parameter threaded
through the DDL helpers and `check_*_execution_path`, and the bypass branch are deleted. A
write-on-query (`ExecutionPathMismatch`) is now unconditional, including for DDL, and the two
bypass tests are removed (router suite 1164).

### C2 closure (2026-11-14)

- **Session fail-open removed** (`crates/router/src/graph_context.rs`): `SESSION SET` and
  `SESSION RESET` match every AST variant. Only graph set/reset are implemented; schema, time
  zone, parameters, characteristics and binding-table forms return `InvalidArgument` instead of
  `Ok(current)`. Regression tests cover both set and reset.
- **Text dictionary state typed** (`crates/text-canister/src/state.rs`): `TextMeta.dict_state`
  is `DictState` (`Absent | Uploading | Finalized`), not `u8` with a wildcard that mapped every
  unknown value to `Finalized`. Candid decoding now rejects an unknown variant. The 96 text tests
  pass.
- **Planner no longer plans unsupported statements as empty** (`crates/gql-planner/src/planner.rs`):
  the `_ => Ok(PhysicalPlan::default())` arms are replaced with explicit `UnsupportedStatement`
  errors for graph/graph-type DDL, `EXPLAIN AUTHORIZATION` and `SESSION`. A session statement
  regression test fails if it ever returns an empty plan.
- **Edge index output kind corrected** (`crates/gql-planner/src/output_schema.rs`):
  `PlanOp::EdgeIndexScan` binds `OutputBindingKind::Edge`, not `Vertex`; a projection of the
  scanned edge variable is tested.

### C3 closure (2026-11-14)

Boundary modules now opt into fail-open lints with module-level
`#![deny(clippy::wildcard_enum_match_arm, clippy::let_underscore_must_use)]`:

- router `graph_context` (session command dispatch) and the guarded helpers in `policy_pushdown`
- provision `canister` (job-state and ingress dispatch)
- text `guards` and `state` (dictionary lifecycle)
- account `stable::store` (owner/grant decisions)
- ic-stable-lara `spill_run_store`, `value_plane`, `remove` (wildcard only; their best-effort
  `spill_release` calls are deliberate leak-on-failure contracts, so `let_underscore_must_use`
  stays off there)
- gql-planner `planner` statement dispatch functions (function-scoped so unrelated traversal
  matches are not caught by a module-wide deny)

The deny attributes exposed one remaining wildcard: `ProvisionResult` construction mapped every
non-terminal `JobState` through `_other`. It now lists every non-terminal variant explicitly.

`scripts/check-fail-open-lints.sh` runs clippy over exactly the packages with these attributes,
and `.github/workflows/rust-check.yml` invokes it as the "Fail-open lints" step. The gate is
intentionally scoped by the attributes rather than `-D` on the command line: a new wildcard in an
unrelated module does not fail CI, while a new arm in a guarded contract does.

### P2 text-wire closure (2026-11-14)

`FlushReport` was defined in the text canister and hand-mirrored in `router/src/text_sync.rs`;
`MAX_CANDIDATE_KEYS = 256` was repeated in the Router, the text canister and the canbench fixture.
Both now live in `gleaph_graph_kernel::text_wire`: the canister re-exports `FlushReport`, and the
Router, canister and bench import `MAX_CANDIDATE_KEYS`. A Candid round-trip test covers the shared
flush report. The Router's mirror struct is deleted.

### C-02 closure (2026-11-14)

`text_score` syntax analysis had two copies: planner `text_scan` and Router `gql_text_scan`.
The single owner is now `gleaph_gql_planner::text_scan`: `is_text_score_call`,
`expr_mentions_text_score`, `order_mentions_text_score`, `op_mentions_text_score`,
`contains_text_scan` and `plan_mentions_text_score` are public, and the Router imports them.
The merged walker keeps the planner's complete container coverage and adds the Router's
`SemiApply::terminal_predicates` and nested-`TextScan` checks.

Router tests that pin "a plan with a residual mention is not mistaken for a plain scan" now
exercise the shared walker (planner 130, router 1166).
