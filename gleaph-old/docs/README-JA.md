# Gleaph — Internet Computer上のグラフデータベース

## 概要

Gleaphは、[Internet Computer](https://internetcomputer.org/)（IC）上で動作するマルチテナント型のグラフデータベース・サービスです。各テナントに独立したグラフキャニスターを提供し、クエリ言語としてGQL（ISO/IEC 39075ベース）をサポートします。

主なユースケースはEコマースのレコメンデーション（購入履歴・レビューに基づく協調フィルタリング）やソーシャルグラフですが、汎用的なプロパティグラフとして幅広い領域に対応します。

**ステータス**: プリプロダクション。本番デプロイされたテナントはまだありません。API・ストレージレイアウトに破壊的変更の可能性があります。

## 主な特徴

### GQL対応

ISO/IEC 39075:2024に基づくGQLエンジンを搭載しています。パイプラインは lexer (nom) → parser → AST → validator → planner → executor (Volcanoモデル) の6段構成です。

| カテゴリ       | サポート範囲                                                                                        |
| -------------- | --------------------------------------------------------------------------------------------------- |
| **読み取り**   | MATCH（マルチホップ）, WHERE, RETURN, ORDER BY, LIMIT, OFFSET, DISTINCT, OPTIONAL MATCH, WITH, NEXT |
| **書き込み**   | INSERT（ノード/エッジ作成）, DELETE / DETACH DELETE, SET, REMOVE, MERGE（upsert）                   |
| **集合演算**   | UNION, UNION ALL, EXCEPT, INTERSECT, OTHERWISE                                                      |
| **集約**       | COUNT, SUM, AVG, MIN, MAX, COLLECT, PERCENTILE_CONT/DISC, STRING_AGG; GROUP BY, HAVING              |
| **パス**       | 可変長パス `*min..max`, パス変数, SHORTEST / ALL SHORTEST / SHORTEST k                              |
| **式・関数**   | 算術, 比較, 論理演算, IS NULL, LIKE/ILIKE, CASE, CAST, 文字列関数, 数学関数, レコード構築 等        |
| **プランナー** | コストベースのアンカー選択, プロパティ等値インデックススキャン, フィルタ/LIMITプッシュダウン        |
| **DDL**        | CREATE/DROP GRAPH, CREATE/DROP GRAPH TYPE, CREATE/DROP SCHEMA, USE GRAPH                            |
| **パラメータ** | `$param` によるクエリパラメータ                                                                     |
| **継続実行**   | ContinuationTokenによる大規模クエリ/ミューテーションの自動ページネーション                          |

詳細は [design/gql-specification-ja.md](../design/gql-specification-ja.md) を参照してください。

### PMA-CSRストレージエンジン

ICのステーブルメモリ上で動作するPacked Memory Array（PMA）ベースのCSR（Compressed Sparse Row）エンジンです。VCSRの頂点中心型PMAとDGAPのログ構造化アップデート手法を統合し、効率的な挿入・リバランス・近傍スキャンを実現します。

プロパティとセカンダリインデックスには、ステーブルメモリ上に構築された `(a,b)+ tree`（B+treeベースのページ管理型木構造）を使用します。

### マルチテナント

単一のレジストリキャニスターが複数のグラフキャニスターのライフサイクルを管理します。各テナントは独立したグラフインスタンスを持ち、データは完全に分離されます。

### グラフアルゴリズム

IC命令バジェットを考慮した組み込みアルゴリズムを提供します。いずれもContinuationTokenによる中断・再開に対応しています。

- **BFS** — 幅優先探索（最短経路探索）
- **PageRank** — ノードの重要度スコアリング（IC認定クエリ対応）
- **SSSP** — 単一始点最短経路（重み付き）
- **Recommend** — マルチホップ協調フィルタリング

### ACL（アクセス制御）

Principalベースの3段階アクセス制御モデルです。

| レベル    | 権限                                         |
| --------- | -------------------------------------------- |
| **Read**  | GQLクエリ、グラフ統計、アルゴリズム実行      |
| **Write** | Read + INSERT/DELETE/SET等のミューテーション |
| **Admin** | Write + ACL管理、インデックス作成、設定変更  |

### 継続実行

ICの命令数制限に対応するため、大規模なクエリ・ミューテーション・アルゴリズムは自動的に中断・再開可能です。

- **クエリ**: `query` → 結果に `ContinuationToken` が含まれる場合 → `query_continue` で残りを取得
- **ミューテーション**: `mutate_resumable` → `mutate_continue` で中断したDELETEを再開
- **アルゴリズム**: `bfs_resumable` / `compute_pagerank_resumable` / `compute_sssp_resumable`

## アーキテクチャ

### システム構成

```
レジストリ・キャニスター（単一）
├── テナント管理・プロビジョニング
├── ACL管理
└── サイクル消費追跡
    │
    ├── グラフ・キャニスター（テナントA）── ステーブルメモリ
    ├── グラフ・キャニスター（テナントB）── ステーブルメモリ
    └── ...
```

### クレート依存グラフ

```
types  ──────────────────────────┐
  │                              │
  ├── algo (BFS, PageRank, SSSP) │
  │     │                        │
  ├── pma (PMA storage engine)   │
  │     │                        │
  │     └── gql (GQL engine)     │
  │           │                  │
  │           └── graph (canister)
  │                              │
  └── registry (canister) ───────┘
```

### IC非依存コア設計

`pma`, `algo`, `gql` の3クレートはICに依存しません。`Memory` トレイト（ステーブルメモリの抽象化）と `InstructionBudget` トレイト（命令数制限の抽象化）により、ネイティブ環境で `VecMemory` と `CountingBudget` を使ったテストが可能です。キャニスタークレート（`graph`, `registry`）のみがIC SDKに依存します。

詳細は [design/architecture-ja.md](../design/architecture-ja.md) を参照してください。

## GQLクエリ例

### パターンマッチとフィルタリング

```sql
MATCH (u:User)-[:Bought]->(p:Product)
WHERE u.name = 'Alice'
RETURN p.name, p.price
ORDER BY p.price DESC
LIMIT 10
```

### データ挿入

```sql
INSERT (:User {name: 'Bob', age: 30})
```

```sql
MATCH (u:User {name: 'Bob'}), (p:Product {name: 'Widget'})
INSERT (u)-[:Bought {quantity: 2}]->(p)
```

### 集約とパス探索

```sql
MATCH (u:User)-[:Bought]->(p:Product)
RETURN p.name, COUNT(u) AS buyers, AVG(u.age) AS avg_age
ORDER BY buyers DESC
```

```sql
MATCH SHORTEST (a:User {name: 'Alice'})-[:Follows*]->(b:User {name: 'Charlie'})
RETURN a, b
```

## プロジェクト構造

| クレート          | 説明                                                               |
| ----------------- | ------------------------------------------------------------------ |
| `crates/types`    | 共有型定義（`#[repr(C)]` ステーブルメモリ構造体、API型、エラー型） |
| `crates/algo`     | グラフアルゴリズム（IC非依存、`GraphView` トレイト）               |
| `crates/pma`      | PMA-CSRストレージエンジン（IC非依存、`Memory` トレイト）           |
| `crates/gql`      | GQLエンジン（lexer → parser → planner → executor）                 |
| `crates/graph`    | グラフキャニスター（IC `cdylib`）                                  |
| `crates/registry` | レジストリキャニスター（テナント管理）                             |
| `tests`           | 統合テスト（ユニットテスト + PocketIC）                            |
| `design`          | 設計ドキュメント                                                   |

## ビルドとテスト

### 必要環境

- Rust（edition 2024）
- `wasm32-unknown-unknown` ターゲット（`rustup target add wasm32-unknown-unknown`）
- [PocketIC](https://github.com/dfinity/pocketic) ランタイム（PocketICテスト用）

### 基本コマンド

```bash
make build                  # cargo build --workspace
make test                   # cargo test --workspace（ユニットテスト、IC不要）
cargo clippy --workspace    # lint
```

### 単一テスト実行

```bash
cargo test -p gleaph-tests test_name
```

### PocketICテスト（キャニスター統合テスト）

```bash
make wasm-e2e-fixtures      # 事前にwasmビルドが必要
make test-pocket-ic          # cargo test --workspace -- --ignored --test-threads=1
```

### ベンチマーク

```bash
make bench                  # canbenchベンチマーク実行
make bench-persist          # 結果をファイルに保存
```

## APIエンドポイント概要

### グラフキャニスター

**クエリ（query call — コンセンサス不要、高速）**

| エンドポイント             | 説明                             |
| -------------------------- | -------------------------------- |
| `query(gql)`               | GQLクエリ実行                    |
| `query_resumable(gql)`     | 継続可能なGQLクエリ実行          |
| `query_continue(token)`    | 継続トークンによる結果取得       |
| `get_neighbors(vertex_id)` | 隣接エッジ一覧                   |
| `get_stats()`              | グラフ統計                       |
| `get_planner_stats()`      | プランナー統計（選択率推定値等） |
| `bfs(start, config)`       | BFS実行                          |
| `recommend(config)`        | レコメンデーション実行           |
| `get_canister_info()`      | キャニスター診断情報             |
| `get_metrics()`            | 運用メトリクス                   |

**ミューテーション（update call — コンセンサス必要）**

| エンドポイント                                             | 説明                                 |
| ---------------------------------------------------------- | ------------------------------------ |
| `mutate(gql)`                                              | GQLミューテーション実行              |
| `batch_mutate(gqls)`                                       | バッチミューテーション               |
| `mutate_resumable(gql)`                                    | 継続可能なミューテーション           |
| `mutate_continue(token)`                                   | 中断したミューテーションの再開       |
| `add_vertex(data)` / `add_edge(data)`                      | プログラム用の頂点/エッジ追加        |
| `bulk_insert_vertices(data)` / `bulk_insert_edges(data)`   | 一括挿入                             |
| `create_index(entity, field, type)`                        | セカンダリインデックス作成           |
| `set_acl_entry(principal, level)`                          | ACLエントリ設定                      |
| `compute_graph_stats()`                                    | 選択率推定用のサンプリング統計を計算 |
| `compute_pagerank(config)` / `compute_sssp(start, config)` | アルゴリズム実行                     |

### レジストリキャニスター

| エンドポイント                             | 説明                 |
| ------------------------------------------ | -------------------- |
| `create_graph(config)`                     | テナントグラフの作成 |
| `delete_graph(id)`                         | テナントグラフの削除 |
| `list_graphs()`                            | グラフ一覧           |
| `grant_access(graph_id, principal, level)` | アクセス権付与       |

## 関連ドキュメント

| ドキュメント                                                                    | 内容                         |
| ------------------------------------------------------------------------------- | ---------------------------- |
| [design/architecture-ja.md](../design/architecture-ja.md)                       | アーキテクチャ詳細（日本語） |
| [design/gql-specification-ja.md](../design/gql-specification-ja.md)             | GQL仕様リファレンス（日本語）|
| [design/gleaph-extensions.md](../design/gleaph-extensions.md)                   | GQL標準外の拡張機能          |
| [design/gql-standard-deviations.md](../design/gql-standard-deviations.md)       | 標準との差異と解決状況       |
| [design/future-roadmap.md](../design/future-roadmap.md)                         | 今後のロードマップ           |
| [design/gql-conformance-matrix-ja.md](../design/gql-conformance-matrix-ja.md)   | GQL準拠マトリクス（日本語）  |
