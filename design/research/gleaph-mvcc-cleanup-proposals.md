# Gleaph トランザクション境界 — クリーン化提案(2026-08-29)

Anchor timestamp: 2026-08-29 18:35:00 UTC +0000
Status: proposal(設計 SSOT を更新する形の提案。新規機能は追加しない)
Input: [gleaph-mvcc-and-ic-atomicity.md](./gleaph-mvcc-and-ic-atomicity.md) / [gleaph-mvcc-design-review.md](./gleaph-mvcc-design-review.md) / [ADR 0029](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md) / [ACID roadmap](../architecture/acid-roadmap.md) / [ADR 0030](../adr/0030-cross-shard-uniqueness-tcc-reservation.md)

## 結論(先に)

「現時点でクリーンな設計」とは、**設計 SSOT を今のうちに弱い点に向かって正す** こと。新機能は足さない。レビューで挙がった 6 弱点のうち、**コード変更を伴わずに SSOT を締めるだけで済むもの**(提案 A–C)をまず着手し、**コード変更が必要なもの**(提案 D–E)は順番を待つ。

優先順位は以下:

| 優先度 | 提案 | コスト | 効果 |
|--------|------|--------|------|
| A | ADR 0029 §8 enforcement note を **独立した Amendment** に切り出して path-independent guard の具体策を確定 | 文書(1日) | 構造的強制を P1 から下ろせる |
| B | `MutationToken` / `ReadMode` の doc comment を「watermark 集合」「graph-index のみ」を中心に **正す** | 文書(半日) | ユーザーの surprise を未然に防ぐ |
| C | acid-roadmap Phase 5 の Contract 3 を **「凍結」** として明示、ADR 0030 を代表例と呼ぶ | 文書(数時間) | multi-DML の表現力議論に終止符 |
| D | `SegmentGuard` RAII / 型状態による path-independent guard 実装 | 中(数日〜1週) | peer-shard client を入れる準備 |
| E | multi-mutation read / vector-text freshness barrier の **SDK 側ベストプラクティス文書** | 軽量(1日) | §2.3 の surprise 防止 |

A–C は **設計書のみ**、D は次の inter-canister path 追加が具体化したタイミングで、E は vector/text index の freshness 仕様が決まったタイミングで着手。

---

## 提案 A: ADR 0029 §8 enforcement note を Amendment に切り出す【P1 / 文書のみ】

### 動機

ADR 0029 §8 の最後:

> Today the boundary is enforced structurally but narrowly: the canonical mutation segment is constructed without a `PropertyIndexLookup` handle and runs `CALL` procedures synchronously, so it cannot reach the only existing inter-canister paths. **When a peer-shard client is introduced, that narrow construction is no longer sufficient on its own.** Enforcement must then generalize to a path-independent guard — assert "no canonical segment is active" at every inter-canister chokepoint (graph-index client, Router call, and any peer-shard client) — so a new call path added inside the segment fails loudly instead of silently extending the critical section across a commit point. This guard is expected when a second inter-canister path first appears (peer-shard client or the Phase 4/6 cross-shard coordination work), not before.

これは **「§8 は予定、今は何もしない」** というニュアンスで読めるが、後から入ってきた開発者が「なぜガードが無いのか」を誤読しやすい。**Amendment として切り出し、ガード導入の責務・担当境界・具体策の選択肢を明文化** する。

### 提案する Amendment の方向性(本体ではない、起票の叩き台)

タイトル案: **「ADR 0029-A: Cross-canister path-independent guard for the canonical mutation segment」**

含めるもの:

1. **現状の再記述**:`apply_canonical_mutation_segment` の構造的強制は `PropertyIndexLookup` ハンドルを取らない型に依拠。CALL プロシージャは同期実行で 1 つの inter-canister call path(graph-index)を経由しない。
2. **保証**:「`PropertyIndexLookup` を渡さない」と「`CALL` を同期で実行する」がコードレビューで守られている間は canonical segment は 1 message execution に閉じている。
3. **弱さ**:この保証は **経路独立ではない**。新しい inter-canister path(peer-shard client, 別 subgraph client, Router 経由の別系統 client)が `apply_canonical_mutation_segment` 内に到達する経路ができた瞬間、レビュー規律だけに依存する境界になる。
4. **採用する具体策**(3 案の中から 1 つ選ぶ。Pre-production simplicity に従い、最小コスト案 = `SegmentGuard` RAII を推す):
   - 案 1: **`SegmentGuard` RAII オブジェクト**(`CanonicalSegment::enter() -> SegmentGuard`)。`SegmentGuard` の生存中のみ inter-canister client 取得 API が動作する(あるいは逆に `SegmentGuard` を保持しないと取得できない)。`Drop` で検証。
   - 案 2: **型状態プログラミング**(`types::NotInSegment` / `types::InSegment` を trait 境界に仕込む)。inter-canister call 用の API シグネチャが `InSegment` を持つ型を取り、`apply_canonical_mutation_segment` の外では `NotInSegment` しか作れない。
   - 案 3: **`assert!(!in_canonical_segment())` を inter-canister chokepoint に配置**。軽量だが、レビュー規律には依存したまま。
   - 推奨: 案 1(`SegmentGuard` RAII)。既存 API への破壊変更が小さく、テストが書きやすく、エンフォースメントが「コード上必ず Drop が走る」性質を持つ。
5. **導入トリガー**:Phase 4/6 の cross-shard coordination 導入、または peer-shard client のいずれか早い方。
6. **導入前のレビュー規律**(今守るべきこと):新しい inter-canister client を `apply_canonical_mutation_segment` の呼び出し元経路に追加する場合、PR 説明に「canonical segment をまたがないこと」を必須項目として書く(チェックリスト化)。

### 既存 ADR への変更

- ADR 0029 §8 を Amendment への参照に置き換え、内容は Amendment 側に移す。
- acid-roadmap の Phase 1 exit criteria に「Path-independent guard の Amendment 起票済」を追加。

### コストと効果

- 文書のみ(1 日)。
- 効果:「§8 の予定」を具体的な責務・トリガー・具体策に格上げ。**peer-shard client 導入の PR レビューで必ず参照される**状態を作る。

---

## 提案 B: `MutationToken` / `ReadMode` の意味を doc comment で正す【P2 / 文書のみ】

### 動機

`crates/graph-kernel/src/plan_exec.rs:919-948` の doc comment は既にかなり良いが、以下の点が**まだ未文書化**:

1. **`MutationToken` は snapshot ではなく watermark 集合**であることの再強調(ADR 0029 §5 には書かれているが、`plan_exec.rs` の doc にも欲しい)
2. **複数 mutation をまたぐ read**(mutation A の後で B を発行、両方の効果を観測したい場合)の合成レシピ
3. **`AtLeast(token)` は ordinary graph-index の freshness だけを測る**。vector / text index の freshness は測らない(明示されていない)

### 提案する doc comment の方向性

`plan_exec.rs:919` の `MutationToken` doc に以下を追加:

```text
/// ## Composing tokens for multi-mutation reads
///
/// When a caller issues mutation A and then mutation B, the read-your-writes
/// barrier for "both A and B" is not a single built-in token. The caller must
/// construct it:
///
/// - `mutation_id`: `max(a.mutation_id, b.mutation_id)`
/// - per-shard `label_stats_seq`: `max(a.label_stats_seq, b.label_stats_seq)`
///
/// For graph-index freshness, the read is satisfied when each shard's
/// `index_pending_min_mutation_id` is `None` or `mutation_id < max(...)`. The
/// SDK is expected to expose this composition; the kernel does not own it.
///
/// ## Scope of the barrier
///
/// `ReadMode::AtLeast(token)` measures the freshness of the **ordinary
/// graph-index** projection (ADR 0029 Phase 2). It does not measure vector
/// index or text index freshness. A read served under `AtLeast(token)` may
/// therefore observe stale vector / text projections for the same `mutation_id`.
/// Vector / text freshness barriers are out of scope until a per-index
/// `index_pending_*_mutation_id` lands (planned in the vector / text index
/// ADRs; see `gleaph-mvcc-design-review.md` §3.2).
```

`ReadMode::AtLeast` の doc にも「これは ordinary graph-index だけである」旨の 1 文を追加。

### コストと効果

- doc comment のみ(半日)。
- 効果:ユーザーが multiple-mutation read や vector/text freshness を求めたときに surprise しない。後の PR で doc を直すよりも、**今書くのがゼロコスト**。

---

## 提案 C: acid-roadmap Phase 5 の Contract 3 を「凍結」として明示【P2 / 文書のみ】

### 動機

ADR 0029 §6 と acid-roadmap の Phase 5 は:

> Staged distributed commit (contract 3) still reserved — its first named invariant (cross-shard uniqueness) is specified in ADR 0030 (accepted, implementation pending).

という状態。「Contract 3 は将来やる/やらない両方の可能性を等しく残している」と読めるが、これだと:

- ユーザーが Contract 3 を要求する multi-DML を書いたときに「実装はまだ」と返される
- ADR 0030 は **uniqueness に特化**しているので、「Contract 3 が必要」な他のケース(quota / schema publication / atomic compare-and-set)が現れたときに、毎回 ADR を起こす必要がある
- その ADR が **「uniqueness の ADR 0030 と同じ template をもう一度書く」** 繰り返しになる可能性が高い

判断は次のどちらか:

- **案 C-1(凍結)**:「Contract 3 は ADR 0030 を唯一の例外とし、他に必要な invariant は出てこない限り導入しない」と acid-roadmap に明記。
- **案 C-2(一般化)**:「Contract 3 のテンプレート化」を別 ADR で起こし、ADR 0030 を最初の具体例として吸収。

### 推奨

**案 C-1(凍結)** を推す。理由:

1. Pre-production simplicity(AGENTS.md)。**今必要ない機能を一般化しない**。
2. ADR 0030 の template は既に ADR 0029 §7 が定めているので、新しい invariant 用に ADR を起こすたびに「同じ template を参照する」だけで十分。
3. **uniqueness は特殊性が高い**(Router の stable CAS がそのまま予約テーブルになる)ので、template 化しても他 invariant ではほぼ同じコードにはならない可能性が高い。
4. **本当に別 invariant が必要になったとき**に ADR を立てる方が、誤って一般化 template を作って後方互換に縛られるより低コスト。

### 提案する acid-roadmap への 1 行追加

```text
Contract 3 (staged distributed commit for cross-shard bundles) is **frozen**:
ADR 0030 (cross-shard uniqueness) is the only contract-3 instance and remains
uniqueness-specific. Other cross-shard all-or-nothing requirements must each
land via their own ADR under the ADR 0029 §7 template; the template is not
parameterized into a reusable engine. This decision freezes the Phase 5
multi-DML contract set at 1 and 2.
```

### コストと効果

- 文書のみ(数時間)。
- 効果:**multi-DML の表現力に関する「未決」が解消される**。ADR 0030 以外の Contract 3 が話題に出たときに「凍結」と即答できる。

---

## 提案 D: `SegmentGuard` RAII による path-independent guard 実装【P1 / コード】

### 動機(再掲)

提案 A で文書化はするが、ガード本体は **コード** で実装しないと「経路独立」にならない。Phase 6 / peer-shard client 導入の前段として必須。

### 提案する実装方針

1. `crates/graph/src/plan_exec/segment_guard.rs`(新ファイル)
   - `pub struct CanonicalSegmentGuard { _private: () }`
   - `pub fn enter(store: &GraphStore) -> CanonicalSegmentGuard`(`store.in_canonical_segment = true` を設定)
   - `Drop` で `store.in_canonical_segment = false`(失敗したら log + **trap**)
   - `pub fn in_canonical_segment() -> bool`(テスト・assert 用)
2. `apply_canonical_mutation_segment` の **最初の 1 行** で `let _guard = CanonicalSegmentGuard::enter(store);`
3. inter-canister client を取得する各 chokepoint(graph-index client 取得 / 将来の peer-shard client / Router call client)で `assert!(!in_canonical_segment(), "inter-canister call from canonical segment")` を `debug_assert!` または `ic_cdk::trap` で挿入。

### 既存テストへの影響

- `crates/pocket-ic-tests/tests/adr0029_canonical_segment_rollback.rs` 系のテストはそのまま通る(`_guard` を関数が持つので、trap 時に message 全体がロールバックされる性質は変わらない)。
- ホスト unit test は `CanonicalSegmentGuard::enter` を **直接呼べない** ため、新規テストは `#[cfg(test)]` 配下に隔離。Production ビルドでは assert は `ic_cdk::trap` で動く。

### 既存トレイト・API への影響

- `PropertyIndexLookup` ハンドルを `apply_canonical_mutation_segment` に渡さない、という今の構造的強制は **そのまま残す**(冗長だが defense-in-depth)。
- `assert!(!in_canonical_segment())` を inter-canister client 取得 API に足す場合、その取得 API のドキュメントに「canonical segment 内では取得できません」と書く。

### コストと効果

- 中規模(数日〜1 週)。
- 効果:**peer-shard client / Phase 6 cross-shard coordination 導入時に必ず効く静的ガード**。レビュー規律だけに頼る必要がなくなる。

### 着手時期

- **Phase 4/6 cross-shard coordination の最初の PR が来る前**(今は未着手なので、それまで待つ)。
- **peer-shard client の最初の PR が来る前**(もし shard 分割が具体化したら)。
- 単に「先にやっておく」が許されるなら今やるのも可(リスクは「`SegmentGuard::enter` を trap せずに握り潰すコードが追加される」程度で、Plan + PocketIC テストで防げる)。

---

## 提案 E: multi-mutation read / vector-text freshness barrier の SDK 側ベストプラクティス文書【P2 / 文書】

### 動機

提案 B で kernel doc に書く内容は「**何が出来るか**」まで。**「ユーザーはどう書くべきか」** は SDK / チュートリアルの領分。

### 提案する文書

- `sdk/client/<lang>/docs/read-consistency.md`(Rust / JS 両方)
  - 単一 mutation の `AtLeast(token)` パターン
  - 複数 mutation の合成レシピ(`max(mutation_id)`、`max(label_stats_seq)`)
  - **vector / text freshness は `AtLeast(token)` でカバーされない** ことと、別途 barrier を要求するなら ADR を要する旨
  - `Eventual` から `AtLeast(token)` に切り替えるときのテストパターン

### コストと効果

- 軽量(1 日)。
- 効果:提案 B と組み合わせて、ユーザーが誤って `AtLeast(token)` を「全 projection が fresh」と誤解するのを防ぐ。

---

## 提案 F(任意): Phase 4 recovery timer の per-tick budget / back-off schedule の文書化【P3 / 文書】

### 動機

`crates/router/src/recovery.rs` の per-tick budget と back-off schedule の **設計根拠** が plan/ADR に書かれているか不明。書かれていない場合、後から「なぜこの数字?」が答えられない。

### 提案する確認手順

1. `crates/router/src/recovery.rs` のコメントに budget / back-off の根拠があるか確認。
2. 無ければ、`design/adr/0029-shard-local-atomicity-and-cross-canister-consistency.md` の Phase 4 節を 1 段落追加して「per-tick budget = ?, back-off schedule = ?, 根拠 = ?」を明記。
3. canbench への影響があれば `crates/router/canbench_results.yml` に追記。

### コストと効果

- 軽い(数時間)。
- 効果:長期運用時に timer パラメータを変える理由が説明できる。

---

## 着手順序(クリーン化として今やるべき順)

```
[即時]    提案 A: ADR 0029 Amendment 起票 (1日)
[即時]    提案 B: doc comment 更新 (半日)
[即時]    提案 C: acid-roadmap に Contract 3 凍結を明記 (数時間)
[条件付き] 提案 D: SegmentGuard 実装 (数日) — peer-shard client か Phase 6 調整の PR が具体化する前
[条件付き] 提案 E: SDK 文書 (1日) — vector/text freshness の ADR が具体化した後
[確認]    提案 F: recovery timer の budget / back-off 文書化 (数時間)
```

**新機能は足さない**。すべて「現設計の弱点を先回りで締める」提案。

## なぜこの順序か

- **A–C は文書のみ**で、Pre-production simplicity に従い「コード変更を伴わないクリーン化」を最優先。
- **D はコストが中規模**で、現時点では peer-shard client も Phase 6 も具体化していない。今やっても insurance。ただし先に A を文書化していないと、D の責務範囲が確定しない。
- **E は B と対**で、kernel doc と SDK 文書の両輪。B が先。
- **F は独立**で、いつでもできる軽い確認。

## 推奨する最初の 1 アクション

**提案 A の Amendment を起票する**。

- 1 つの新 ADR(`0029-A`) を書くだけ。
- 中身は「現状 / 弱さ / 採用する具体策候補 / トリガー / 導入前レビュー規律」で、3-5 ページ。
- レビューは acid-roadmap の Maintainer(既存の reviewer と同じ層)で回せる。
- 起票後、acid-roadmap Phase 1 exit criteria に「Path-independent guard の Amendment 起票済」を追加する 1 行パッチ。

これで §2.1 の P1 が「文書化された責務」に変わる。実装(D)は peer-shard client か Phase 6 が具体化したときに着手すれば良い。

## 参考リンク

- ADR 0029 §8 enforcement note: `design/adr/0029-shard-local-atomicity-and-cross-canister-consistency.md`
- acid-roadmap Phase 1 exit criteria: `design/architecture/acid-roadmap.md`
- `MutationToken` / `ReadMode`: `crates/graph-kernel/src/plan_exec.rs:919-948`
- Phase 4 recovery driver: `crates/router/src/recovery.rs`
- ADR 0030 (Contract 3 の唯一の具体例): `design/adr/0030-cross-shard-uniqueness-tcc-reservation.md`