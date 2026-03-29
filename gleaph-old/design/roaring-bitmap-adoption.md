# RoaringBitmap Adoption Plan

## Overview

`BTreeSet<u32>` を頂点ID集合の標準表現として多用しているが、RoaringBitmap に置き換えることで集合演算の高速化とメモリ削減が見込める。本ドキュメントでは導入箇所・優先度・実装方針を整理する。

## Background

現状、頂点IDの集合管理に `BTreeSet<u32>` を使用している箇所が多数ある。BTreeSet は汎用的だが、以下の点で非効率：

- **メモリ**: 1要素あたり約64バイト（ノードポインタ+メタデータ）
- **membership test**: O(log n)
- **集合演算** (intersection/union): O(n) だが定数係数が大きい（ツリー走査のキャッシュミス）

RoaringBitmap は u32 の集合に特化した圧縮ビットマップで：

- **メモリ**: 密な範囲では約2bit/要素、疎でもコンテナ単位で効率的
- **membership test**: O(1)
- **集合演算**: ビットワイズ演算で高速（特に intersection/union）

## Adoption Targets

### Phase 1: Label Index (効果: 大, 難易度: 中)

**対象ファイル**: `crates/pma/src/label_index.rs`

**現状**:
```rust
vertex_postings: BTreeMap<u32, BTreeSet<u32>>  // label_id -> vertex_ids
```

**変更後**:
```rust
vertex_postings: BTreeMap<u32, RoaringBitmap>  // label_id -> vertex_ids
```

**影響範囲**:
- `add_vertex_label()` / `remove_vertex_label()`: `insert()`/`remove()` はそのまま互換
- `scan_vertices_by_label()`: `.iter().collect()` で `Vec<u32>` を返す（API互換）
- ラベル式 (`Person|Employee`) の union: `|=` 演算子で高速化
- `vertices_with_label_count()`: `.len()` で O(1) 取得

**期待効果**:
- ラベルスキャンが主要なクエリアンカー選択パスなので、プランナーのコスト見積もりとスキャン実行の両方が高速化
- 10万頂点のラベルで BTreeSet ~6.4MB → RoaringBitmap ~数十KB

### Phase 2: Property Index (効果: 大, 難易度: 中)

**対象ファイル**: `crates/pma/src/pma.rs`

**現状**:
```rust
vertex_prop_eq_index: RapidHashMap<(String, Vec<u8>), BTreeSet<u32>>
vertex_prop_range_index: BTreeMap<(String, Vec<u8>), BTreeSet<u32>>
```

**変更後**:
```rust
vertex_prop_eq_index: RapidHashMap<(String, Vec<u8>), RoaringBitmap>
vertex_prop_range_index: BTreeMap<(String, Vec<u8>), RoaringBitmap>
```

**影響範囲**:
- `scan_vertex_property_indexed()`: 返り値を `.iter().collect()` で `Vec<u32>` に変換
- `build_abp_secondary_index()`: インデックス構築時に `insert()` を使用
- Compound Range Intersection (Phase 6 planner): `BTreeSet::intersection()` → `&` 演算子
- executor.rs のエッジインデックスフィルタ交差 (L8709-8737): `intersection()` チェーン → `&=`

**期待効果**:
- マルチプレディケート AND フィルタが O(n) のビット演算に
- 特に selectivity が低い（マッチ数が多い）プロパティ値での改善が顕著

### Phase 3: BFS / Algorithm Visited Sets (効果: 中, 難易度: 低)

**対象ファイル**: `crates/algo/src/bfs.rs`, `crates/algo/src/recommend.rs`

**現状**:
```rust
visited: BTreeSet<u32>
fwd_visited: BTreeSet<u32>
bwd_visited: BTreeSet<u32>
```

**変更後**:
```rust
visited: RoaringBitmap
fwd_visited: RoaringBitmap
bwd_visited: RoaringBitmap
```

**影響範囲**:
- `contains()`: O(log n) → O(1)
- bidirectional BFS の `union()`: ビットワイズ OR で高速化
- `BfsCheckpoint` のシリアライズ: `RoaringBitmap::serialize_into()` / `deserialize_from()` を使用

**注意点**:
- `BfsCheckpoint` のフォーマットが変わるため、既存チェックポイントとの後方互換に注意
- `dist: BTreeMap<u32, u32>` と `pred: BTreeMap<u32, u32>` は距離/先行ノード情報を持つため RoaringBitmap には不向き（そのまま維持）

**期待効果**:
- 大規模グラフ (100K+ 頂点) の BFS で visited チェックが支配的なケースで改善
- recommend の `owned` セットの `contains()` も O(1) に

### Phase 4: Tombstoned Vertices (効果: 小, 難易度: 低)

**対象ファイル**: `crates/pma/src/pma.rs`

**現状**:
```rust
tombstoned_vertices: BTreeSet<u32>
```

**変更後**:
```rust
tombstoned_vertices: RoaringBitmap
```

**影響範囲**:
- `is_vertex_tombstoned()`: 頻繁に呼ばれるフィルタ条件、O(1) に
- `restore_tombstoned_vertices_from_set()`: `RoaringBitmap::from_iter()` で構築
- stable memory の `VertexTombstoneBitset` は既に効率的なカスタムビットセットなので変更不要

**期待効果**:
- DELETE 後のクエリで tombstone チェックが O(1) に
- 実際の改善幅は小さい（tombstone 数は通常少ない）

## Out of Scope

以下は RoaringBitmap の対象外：

| 箇所 | 理由 |
|------|------|
| `VertexTombstoneBitset` (stable memory) | 既にビット単位のカスタム実装で最適。RoaringBitmap のコンテナ構造は stable memory の直接 read/write パターンと合わない |
| `edge_prop_eq_index` の `BTreeSet<(u32,u32)>` | キーが `(u32, u32)` タプル。RoaringBitmap は `u32` 単一キー専用。`Roaring64Bitmap` で `(src << 32) \| dst` にエンコードする手もあるが、既存の edge index 構造との整合性を優先 |
| `HashSet<(u32,u32)>` (bulk edge dedup) | 同上、タプルキー |
| `BTreeMap<u32, u32>` (BFS dist/pred) | 値付きマッピングなのでビットマップは不適 |

## Implementation Notes

### Dependency

```toml
# Cargo.toml (workspace)
[workspace.dependencies]
roaring = "0.10"
```

`roaring` crate は `no_std` 対応で、wasm32 ターゲットでもコンパイル可能。

### Wasm サイズへの影響

`roaring` crate の追加で Wasm バイナリが約 50-80KB 増加する見込み。IC の canister Wasm サイズ上限 (2MB gzip後) に対して十分余裕がある。実測で確認すること。

### IC Instruction Budget との相性

RoaringBitmap の内部演算はループとビット演算が主体で、IC の instruction metering と相性が良いと予想される。ただし以下を実測で確認：

- コンテナ分岐（ArrayContainer vs BitmapContainer）のオーバーヘッド
- 小さい集合 (< 100要素) では BTreeSet の方が速い可能性

### API 方針: RoaringBitmap を公開 API に露出する

中間変換コストを排除するため、`BTreeSet<u32>` / `Vec<u32>` を返していた API を `RoaringBitmap` に変更する。具体型への結合を1箇所に集約するため、`types` crate に型エイリアスを導入する。

```rust
// crates/types/src/lib.rs
pub type VertexIdSet = roaring::RoaringBitmap;
```

#### GraphView trait の変更

```rust
// Before
fn scan_vertices_by_label(&self, label: &str) -> Vec<u32>;
fn scan_vertex_property_indexed(&self, ...) -> Option<Vec<u32>>;

// After
fn scan_vertices_by_label(&self, label: &str) -> VertexIdSet;
fn scan_vertex_property_indexed(&self, ...) -> Option<VertexIdSet>;
```

#### Executor での効果

```rust
// Before: Vec<u32> → BTreeSet<u32> に変換し、intersection を繰り返す
let targets: BTreeSet<u32> = indexed.into_iter().collect();  // O(n log n)
existing = existing.intersection(&targets).copied().collect(); // O(n)

// After: RoaringBitmap をそのまま &= で交差
existing &= indexed;  // ビットワイズ AND、collect 不要
```

3つのプロパティ条件の AND で `collect()` が 6回 → 0回になる。

#### BFS 結果の後続利用

```rust
// Before: BFS結果を受け取って再度 BTreeSet に変換
let reachable: BTreeSet<u32> = bfs_result.visited.into_iter().collect();
let filtered = label_set.intersection(&reachable);

// After: そのまま集合演算
let filtered = &label_set & &bfs_result.visited;  // ゼロコピー intersection
```

#### 影響範囲

- `GraphView` trait の全実装 (`PmaGraph`, `MockGraph`, `BothWaysView`) を更新
- テストの `MockGraph` も `VertexIdSet` を返すよう変更
- `algo` crate の BFS/recommend も `VertexIdSet` を使用
- `roaring` の依存が `types` → `algo`, `pma`, `gql` 全てに伝播（型エイリアス経由）

#### 将来の差し替え

`VertexIdSet` 型エイリアスの定義を変更するだけで、全体の実装を別のビットマップライブラリに切り替え可能。

### テスト戦略

- 各 Phase で既存テストが全てパスすることを確認（API互換のため基本的にグリーン）
- canbench ベンチマークで Phase 1, 2 の前後比較を実施
- 小規模 (100要素) / 中規模 (10K) / 大規模 (100K) の集合サイズでの性能プロファイルを取得

## Decision Log

| Date | Decision |
|------|----------|
| 2026-03-14 | 初版作成。Phase 1-4 の優先度と実装方針を策定 |
| 2026-03-14 | API方針決定: `VertexIdSet` 型エイリアスで RoaringBitmap を公開APIに露出。中間 `Vec<u32>` 変換を排除 |
| 2026-03-14 | Phase 1-4 全実装完了。`label_index`, `vertex_prop_eq_index`, `vertex_prop_range_index`, `tombstoned_vertices`, BFS visited, recommend owned を `VertexIdSet` に変換。全1594テスト通過 |
| 2026-03-14 | API露出完了。`scan_vertices_by_label`, `scan_vertices_by_property_eq/_range/_between` 系、`_auto`/`_live`/`_abp` 全バリアントが `VertexIdSet` を返すよう変更。executor の `edge_index_filter`, `initial_candidates`, `try_indexed_props_hint_scan`, `conditional_scan_vertices` も `VertexIdSet` に統一。range scan の `sort_unstable()+dedup()` を `\|=` union に、compound range の `HashSet` intersection を `&` 演算子に置換 |
