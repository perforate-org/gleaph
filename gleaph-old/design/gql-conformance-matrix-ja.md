# GQL 仕様適合表と Gleaph 実装ノート

> 本文書は ISO/IEC 39075:2024 (GQL) と `reference/grammar/GQL.g4` を基準に、現行の Gleaph 実装 (`crates/gql`, `crates/graph`) とテスト (`tests/src`) を照合してまとめた棚卸しである。`design/gql-specification-ja.md` が標準仕様の章立て説明を担い、本書は「現状どこまで動くか」「どこが独自仕様か」を整理することを目的とする。

## 判定記号

| 記号 | 意味 |
| --- | --- |
| `PVE` | parser/AST, validator or type-check, executor or bridge, tests まで確認できた |
| `PV` | parser/AST と validator/type-check までは確認できたが、実行経路が限定されるか機能が一部のみ |
| `P` | parser/AST まではあるが、実行や検証の裏付けが弱い |
| `-` | 未実装、または該当する parser/AST/bridge が見当たらない |

## 要約

- Gleaph は **§13-16 と §19-20 のコア問い合わせ言語** にはかなり強く、`MATCH`/`OPTIONAL`/`RETURN`/集約/`NEXT`/`CALL {}`/shortest path まで実装している。
- 一方で **§6-11 と §17 の「プログラム・セッション・トランザクション・グラフ参照値」系は弱い**。標準GQLの `gqlProgram`, `SESSION`, `START TRANSACTION`, `BINDING TABLE`, `CURRENT_GRAPH` などは未実装である。
- **型システム (§18.9) は PVE レベルだがスコープは部分的** で、実際に扱えるのは `INT/FLOAT/TEXT/BOOL/TIMESTAMP/LIST/BYTES/DATE/TIME/DATETIME/DURATION/NULL` を中心としたサブセットである。型推論・型検査は 78 テストでカバーされている。
- **独自仕様はかなり多い**。`MERGE`, Cypher 互換の `WITH`, `SHOW ...`, `CREATE INDEX`, `CREATE CONSTRAINT`, prepared statement API, 組み込みアルゴリズム `CALL bfs/sssp/pagerank/recommend` などがある。
- 最大の標準差分は **`~` 系の標準無向エッジ構文を拒否** する点である。無向トラバーサル自体は `-[e]-` 構文 (Cypher 互換, `Direction::Either`) でサポートしている。

## 標準GQL 適合表

| 章 | 項目 | 判定 | 評価 | 実装メモ |
| --- | --- | --- | --- | --- |
| §6 | `gqlProgram`, `programActivity`, `sessionCloseCommand` | `-` | 未対応 | `parse_statement` は単一 statement/compound query を扱う実装で、標準の「プログラム」レベルは持たない。 |
| §7 | `SESSION SET/RESET/CLOSE`, session parameter | `-` | 未対応 | `crates/gql`/`crates/graph` に `SESSION` 系の parser/AST/bridge が存在しない。 |
| §8 | `START TRANSACTION`, `COMMIT`, `ROLLBACK` | `-` | 未対応 | 標準の transaction activity は未実装。Gleaph 側の query/mutate endpoint とは別物である。 |
| §9 | procedure body, nested procedure, `NEXT [YIELD]` | `PVE` | 部分対応 | `NEXT` は `SetOp::Next` として実装済み。`CALL { ... }` / `CALL (vars) { ... }` も動く。一方で `AT schema` と top-level binding variable definition block は未実装。 |
| §10 | `PROPERTY GRAPH ... =`, `BINDING TABLE ... =`, `VALUE ... =` | `-` | 未対応 | 標準の top-level 変数定義ブロックはない。代替として `LET`, value-level `LET ... IN ... END`, `NEXT`, `WITH` を使う形になっている。 |
| §11 | graph expression, binding table expression, `CURRENT_GRAPH` | `PV` | 部分対応 | `USE [GRAPH] name` はあるが standalone 解決用であり、標準の first-class graph/binding-table expression ではない。`CURRENT_GRAPH`/`CURRENT_PROPERTY_GRAPH` も未実装。 |
| §12 | `CREATE/DROP SCHEMA` | `PVE` | 部分対応 | `CREATE SCHEMA [IF NOT EXISTS] name` と `DROP SCHEMA [IF EXISTS] name` は bridge まで実装済み。catalog path は未対応。 |
| §12 | `CREATE/DROP GRAPH` | `PV` | 部分対応 | `CREATE [PROPERTY] GRAPH [IF NOT EXISTS] name` / `DROP [PROPERTY] GRAPH [IF EXISTS] name` は parse できるが、通常の `query`/`mutate` endpoint ではなく専用 endpoint が必要。`OR REPLACE` は未対応。 |
| §12 | `CREATE/DROP GRAPH TYPE`, `DESCRIBE GRAPH TYPE` | `PVE` | 対応 | `CREATE [OR REPLACE] [PROPERTY] GRAPH TYPE [IF NOT EXISTS] Name { ... | LIKE src | COPY OF src }`, `DROP [PROPERTY] GRAPH TYPE [IF EXISTS]`, `DESCRIBE GRAPH TYPE` を実装。 |
| §12 | catalog object path / schema reference / graph reference richness | `-` | 未対応 | `schemaReference`, `home graph`, `current schema`, reference parameter などの標準参照機構は持たない。識別子はほぼ単純名ベース。 |
| §13 | `INSERT` | `PVE` | 対応 | `INSERT (:Label {...})` と `INSERT (...)-[:REL]->(...)` は動く。カンマ区切りの `insertPathPatternList` (複数ノード/エッジの同時 INSERT) も対応済み。 |
| §13 | `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`, `NODETACH DELETE` | `PVE` | 対応 | property/label の `SET`, `SET n = { ... }` (all-properties 置換), `REMOVE` (頂点 property/label, edge property), `DELETE`, `DETACH DELETE`, `NODETACH DELETE`, カンマ区切りの `DELETE item1, item2` すべてテスト済み。 |
| §13 | 標準外 upsert | `PVE` | 独自 | `MERGE (n:Label {...}) ON CREATE SET ... ON MATCH SET ...` を実装。node/edge 両パターンのテストあり (作成・重複防止)。Cypher 互換の upsert であり、`GQL.g4` の data-modifying grammar にはない。 |
| §14 | `UNION`, `UNION ALL`, `EXCEPT`, `INTERSECT`, `NEXT`, `OTHERWISE` | `PVE` | 対応 | 集合演算と `NEXT` は実装済み。`UNION` dedup/ALL, `EXCEPT` 網羅/全除外, `INTERSECT`, `OTHERWISE` (左非空/左空) のテストあり。compound branch 数は 16 に制限される。 |
| §14 | `MATCH`, `OPTIONAL MATCH`, `OPTIONAL { ... }`, `OPTIONAL ( ... )` | `PVE` | 対応 | 3 形式とも parse/validate/execute まで確認できる。 |
| §14 | `FILTER`, `LET`, `FOR ... IN ...` | `PVE` | 対応 | `FILTER`, statement-level `LET`, `FOR ... RETURN` は parser/AST/executor/テストあり。`FOR ... WITH ORDINALITY` は対応済みだが、標準 grammar が許す `WITH OFFSET var` は未対応。 |
| §14 | `RETURN`, `GROUP BY`, `HAVING`, `ORDER BY`, `OFFSET`, `LIMIT`, `FINISH` | `PVE` | 対応 | 集約、`NULLS FIRST/LAST`, `OFFSET`, `LIMIT`, `RETURN *`, `DISTINCT`, `FINISH` はすべてテスト済み。ORDER BY + OFFSET + LIMIT の組み合わせ、DESC ソートも確認。`SKIP` は意図的に拒否し `OFFSET` のみ許容する。 |
| §14 | `SELECT` | `PVE` | 部分対応 | `SELECT ... MATCH ...` は `MATCH ... RETURN ...` へ脱糖される。`FROM <graph>` は single-graph モデルの都合で構文上は受けても実質無視される。 |
| §15 | inline `CALL { ... }`, `CALL (vars) { ... }`, `OPTIONAL CALL` | `PVE` | 対応 | inline subquery は動く。`OPTIONAL CALL` もサポート (エラー時は空結果を返す)。 |
| §15 | named procedure call | `PVE` | 部分対応 | `CALL proc(args) [YIELD ...]` はあり、実行できるのは組み込みの `bfs`/`sssp`/`pagerank`/`recommend`。引数はパラメータ・算術式・リスト・レコードリテラルに対応。YIELD 省略時は全カラム返却。 |
| §16 | `MATCH REPEATABLE ELEMENTS`, `MATCH DIFFERENT EDGES`, `KEEP` | `PVE` | 対応 | match mode と `KEEP *` / `KEEP a, b` を実装。`DIFFERENT EDGES` の実行テストもある。 |
| §16 | `WALK`/`TRAIL`/`SIMPLE`/`ACYCLIC`, `ANY n PATHS`, `SHORTEST`, `ALL SHORTEST`, `SHORTEST k`, `SHORTEST GROUP` | `PVE` | 対応 | `WALK`/`TRAIL`/`SIMPLE`/`ACYCLIC` 全モード実装・テスト済み。`SHORTEST`/`ALL SHORTEST`/`SHORTEST k`/`SHORTEST GROUP` も AST+executor。`ANY SHORTEST` は `SHORTEST` と同等に処理。`ALL PATHS` は shortest フィルタなしで全パス返却。可変長境界は `1 <= min <= max <= 6` に制限。 |
| §16 | edge syntax, simplified path pattern, parenthesized subpath | `PVE` | 対応 | `->`, `<-`, `-`, `-/L/->`, `<-/L/-`, `<-/L/->`, `-/L/-`, parenthesized subpath `((x)-[:E]->(y)){n,m}` は parser/executor/テストまで実装済み (固定・範囲量指定子、末端ノードラベルフィルタ、WALK/TRAIL/SIMPLE/ACYCLIC 各モード)。標準の `~` 系無向構文 (`~[...]~`, `~/L/~` など) はすべて拒否する。 |
| §16 | label expression, inline `WHERE`, pattern type annotation | `PVE` | 部分対応 | node/edge の inline `WHERE`, label expression, `n :: PersonType` などの型注釈は実装。実行テストは主要経路にあるが、標準文法の全組み合わせを網羅しているわけではない。 |
| §17 | schema/graph/graph type/binding table/procedure reference | `-` | 未対応 | 標準の catalog path, home/current schema, substituted parameter reference, binding table reference は実装されていない。 |
| §18 | graph type / node type / edge type 定義と enforcement | `PVE` | 部分対応 | `CREATE GRAPH TYPE` と bridge 側 enforcement により、node/edge type・property 定義・endpoint 制約を実装している。ただし文法は標準の nested graph type specification そのものではなく、Gleaph 独自の `DEFINE` 構文を含む。 |
| §18.9 | value type system | `PVE` | 部分対応 | 実型として強いのは `INT`, `FLOAT`, `TEXT`, `BOOL`, `TIMESTAMP`, `LIST`, `BYTES`, `DATE`, `TIME`, `DATETIME`, `DURATION`, `NULL`。型推論・型検査は 78 テストでカバー (BinaryOp, aggregate, CASE/COALESCE, パラメータ注釈, スキーマ連携, temporal 算術, NOT NULL 伝播, type union `\|`)。ただし標準の広い numeric family, length/precision 付き string/bytes, graph reference type, binding table type, record/array type annotation は未対応。 |
| §19 | search condition / predicate | `PVE` | 対応 | 比較, 論理演算, `IN`, `EXISTS`, `IS NULL`, `IS TRUE/FALSE/UNKNOWN`, `LIKE`, `IS :: type`, `IS DIRECTED` は実装・テスト済み。`IS LABELED` (正否・NOT・英綴り), `IS SOURCE OF` / `IS DESTINATION OF` (正否・NOT), `PROPERTY_EXISTS` (頂点・辺), `ALL_DIFFERENT`, `SAME` も parser+executor テスト完備。 |
| §20 | value expression | `PVE` | 対応 | 算術, aggregate, scalar function, `CASE` (searched/simple/ELSE なし), `COALESCE`, `NULLIF`, list/record literal, `CAST`, `IS TRUE/FALSE/UNKNOWN`, `VALUE { query }` (スカラー/NULL), `LET ... IN ... END` (単一/複数バインド), `PATH [...]` を実装・テスト済み。graph/binding-table object expression は未対応。 |
| §21 | names, parameters, literals | `PVE` | 部分対応 | single-quoted string, double/backtick quoted identifier, `$param`, bytes literal `X'..'`, temporal literal は対応。加えて hex/octal/binary integer literal は Gleaph 独自拡張である。 |

## 独自仕様一覧

| 機能 | 標準との関係 | 実装メモ |
| --- | --- | --- |
| `MERGE ... ON CREATE SET ... ON MATCH SET ...` | 独自拡張 | Cypher 互換の upsert。node/edge 両パターン対応。 |
| `WITH` 句と `WITH ... MATCH` 継続 | 独自拡張 | 標準GQLの `NEXT` と並行して、Cypher 互換パイプラインも提供。 |
| `SHOW STATS`, `SHOW INDEXES`, `SHOW GRANTS`, `SHOW METRICS`, `SHOW SCHEMAS`, `SHOW GRAPH TYPES`, `SHOW QUOTA`, `SHOW ALIASES`, `SHOW SETTINGS`, `SHOW CONSTRAINTS`, `SHOW PREPARED` | 独自拡張 | `reference/grammar/GQL.g4` に対応 production がない運用系 introspection。 |
| `CREATE/DROP INDEX` | 独自拡張 | property equality index 用 DDL。 |
| `CREATE/DROP CONSTRAINT` | 独自拡張 | 現状は `UNIQUE` と `NOT NULL` のみ。 |
| `GRANT`, `REVOKE`, `ANALYZE`, `SET TYPE CHECK STRICT|WARNING` | 独自拡張 | ACL, planner stats 更新, strict type-check 切替を bridge が処理する。 |
| prepared statement API (`prepare_statement`, `execute_prepared_query`, `execute_prepared_mutation`, `drop_prepared`) | 実装拡張 | GQL文そのものではなく host API。`SHOW PREPARED` だけ GQL 表面に出る。 |
| `CALL bfs/sssp/pagerank/recommend ... YIELD ...` | 独自の組み込み procedure 群 | 標準の generic named procedure ではなく、Gleaph 固有アルゴリズムの公開面。 |
| `gleaph_weight(e)`, `gleaph_timestamp(e)` | 独自拡張 | エッジの内部 weight/timestamp を式や集約で参照できる。 |
| `ILIKE` | 独自拡張 | case-insensitive `LIKE`。 |
| hex/octal/binary integer literal (`0xFF`, `0o77`, `0b1010`) | 独自拡張 | 標準の数値リテラルより広い。 |
| `DEFINE` inside `CREATE GRAPH TYPE` | 独自拡張 | `DEFINE PersonType AS (:Person { name :: TEXT NOT NULL })` のように node/edge type を宣言する shorthand。 |

## ISO 本文と `GQL.g4` の読み分けで注意すべき点

- `reference/grammar/GQL.g4` は `offsetSynonym : OFFSET | SKIP_RESERVED_WORD` を持つが、Gleaph は `SKIP` をサポート対象にせず `OFFSET` のみ許容する。
- `GQL.g4` には `SELECT`, `FILTER`, `LET`, `FOR`, `FINISH`, `NEXT` が含まれているため、これらは Gleaph 独自ではなく「標準機能の部分実装」と見るのが妥当である。
- 一方で `SHOW`, `CREATE INDEX`, `CREATE CONSTRAINT`, `GRANT`, `REVOKE`, prepared statement API などは `GQL.g4` の production には現れず、Gleaph の運用拡張として扱うのが妥当である。
- `GQL.g4` が持つ `~` 系の無向エッジ構文は、Gleaph の directed-only 方針と明示的に衝突するため、互換ではなく意図的な非対応である。

## 標準準拠を進めるなら優先度が高い項目

1. §6-11 / §17 の未実装領域: `SESSION`, transaction, graph/binding table reference, catalog path。IC モデルと非互換のため優先度低。
2. ~~§12 の簡略化解消: `IF [NOT] EXISTS`, `OR REPLACE`, `LIKE`, `COPY OF`, `PROPERTY GRAPH`~~ → **全項目実装済み。**
3. ~~§13 の完全化: `insertPathPatternList`, `deleteItemList`, `NODETACH`, `SET n = { ... }`~~ → **全項目実装済み。**
4. ~~§15 の汎用 procedure model: `OPTIONAL CALL`, non-literal arguments, optional `YIELD`~~ → **全項目実装済み。**
5. §18.9 の完全化: 事前定義型の拡張、record/array/reference types、型注釈構文の拡充。

## 判定根拠

- parser/AST: `crates/gql/src/parser.rs`, `crates/gql/src/ast.rs`, `crates/gql/src/lexer.rs`
- validate/type-check: `crates/gql/src/validate.rs`, `crates/gql/src/type_check.rs`
- execute/bridge: `crates/gql/src/executor.rs`, `crates/graph/src/gql_bridge.rs`
- tests: `tests/src/gql_parser_tests.rs`, `tests/src/gql_executor_tests.rs`, `tests/src/gql_type_check.rs`, `tests/src/gql_category_d.rs`, `tests/src/gql_prepared.rs`

## 参照

- ISO/IEC 39075:2024 official catalog page: <https://www.iso.org/standard/76120.html>
- `design/gql-specification-ja.md`
- `design/gql-standard-deviations.md`
- `reference/grammar/GQL.g4`
