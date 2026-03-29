# Prepared Statements V2 — Design

## Goals

1. **`Value::Principal`** — GQL で Principal を扱えるようにする
2. **`caller()` 組み込み関数** — GQL 式内で `ic_cdk::api::msg_caller()` を返す組み込み関数
3. **`AccessLevel::Execute`** — prepared statement のみ実行可能な権限レベル
4. **`PreparedStatementInfo`** — prepare 時にメタデータを返す + `list_prepared` を拡張
5. **CLI codegen** — Canister の prepared statements から型安全な SDK コードを生成

## 1. `Value::Principal`

### types/src/lib.rs

```rust
pub enum Value {
    // ... existing variants ...
    Principal(Principal),  // 新規
}
```

### 影響範囲

- GQL lexer: Principal リテラル構文は不要（パラメータ経由のみ）
- GQL executor: `Value::Principal` の比較演算（`=`, `<>`）をサポート
- Property store: Principal の serialize/deserialize
- JS SDK types.ts: `| { Principal: Principal }` を追加
- Candid (.did): `Principal: principal` を Value variant に追加

## 2. `caller()` 組み込み関数

### 概要

`caller()` は Gleaph 独自の GQL 組み込み関数。引数なしで呼び出すと、
現在のキャニスター呼び出し元の Principal を `Value::Principal` として返す。

既存の組み込み関数 `id()`, `source()`, `destination()` 等と同じパターン。

### 使用例

```gql
-- query: 自分のフォロー先を取得
MATCH (me:User {principal: caller()})-[:FOLLOWS]->(f)
RETURN f.name, f.avatar

-- mutation: フォロー
MATCH (me:User {principal: caller()}), (t:User {principal: $target})
CREATE (me)-[:FOLLOWS]->(t)

-- WHERE 句でも使える
MATCH (p:Post)-[:AUTHORED_BY]->(u:User)
WHERE u.principal = caller()
RETURN p.title
```

### 実装

#### executor — 関数評価

```rust
// executor/functions.rs (既存の組み込み関数と同じ場所)
"caller" => {
    if !args.is_empty() {
        return Err(ExecutionError::FunctionArgCount("caller", 0, args.len()));
    }
    Ok(resolve_caller())
}
```

#### caller 解決

```rust
/// キャニスター呼び出し元の Principal を返す。
fn resolve_caller() -> Value {
    #[cfg(target_arch = "wasm32")]
    { Value::Principal(ic_cdk::api::msg_caller()) }
    #[cfg(not(target_arch = "wasm32"))]
    { Value::Principal(candid::Principal::anonymous()) }
}
```

### メリット

- パラメータ名前空間を汚さない（`$CALLER` 予約が不要）
- GQL 式内のどこでも使える（WHERE, property map, RETURN 等）
- `id()` 等の既存組み込み関数と一貫したパターン
- params 注入ロジックが不要（executor が直接解決）

### PreparedStatementInfo との関連

`requires_caller` フィールドは AST を走査して `caller()` 関数呼び出しの
有無を判定する。codegen 時の注記・ドキュメント生成に使用。

## 3. `AccessLevel::Execute`

### types/src/lib.rs

```rust
pub enum AccessLevel {
    Execute,  // 新規: prepared statement のみ
    Read,     // query 直接 + prepared
    Write,    // mutate 直接 + prepared
    Admin,    // 全権 + ACL 管理 + prepare 登録
}
```

### 権限マトリクス

| エンドポイント              | Execute | Read | Write | Admin |
|---------------------------|---------|------|-------|-------|
| `execute_prepared`         | OK      | OK   | OK    | OK    |
| `execute_prepared_mutation`| OK      | -    | OK    | OK    |
| `query`                    | -       | OK   | OK    | OK    |
| `mutate`                   | -       | -    | OK    | OK    |
| `prepare` / `drop_prepared`| -       | -    | -     | OK    |
| `set_acl_entry`            | -       | -    | -     | OK    |

### lib.rs の変更

```rust
#[query(name = "execute_prepared")]
fn execute_prepared_gql(name: String, params: PropertyMap)
    -> Result<QueryResultWithContinuation, GleaphError>
{
    api::check_caller_permission(&AccessLevel::Execute)?;  // Execute で十分
    api::execute_prepared_gql(name, params)
}

#[update(name = "execute_prepared_mutation")]
fn execute_prepared_mutation_gql(name: String, params: PropertyMap)
    -> Result<MutationResult, GleaphError>
{
    // Execute + 書き込み権限の複合チェック
    // → Execute レベルでも prepared mutation は許可（prepared = Admin が承認済み）
    api::check_caller_permission(&AccessLevel::Execute)?;
    api::execute_prepared_mutation_gql(name, params)
}

#[update]
fn prepare(name: String, gql: String) -> Result<PreparedStatementInfo, GleaphError> {
    api::check_caller_permission(&AccessLevel::Admin)?;  // Admin のみ
    api::prepare_gql(name, gql)
}
```

**Execute レベルのユーザーは prepared mutation も実行可能。**
理由: prepared statement は Admin が登録したもの = 許可された操作。
Execute ユーザーが任意の GQL を送れないことが安全の要。

## 4. `PreparedStatementInfo`

### types/src/lib.rs

```rust
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
pub enum PreparedKind {
    Query,
    Mutation,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
pub struct PreparedStatementInfo {
    /// Registered name.
    pub name: String,
    /// Query or Mutation.
    pub kind: PreparedKind,
    /// User-supplied parameter names.
    pub parameters: Vec<String>,
    /// RETURN clause column names (empty for mutations).
    pub columns: Vec<String>,
    /// Whether this statement uses caller() (resolved via ic_cdk::api::msg_caller()).
    pub requires_caller: bool,
    /// Original GQL source.
    pub source: String,
}
```

### API 変更

```rust
// prepare の戻り値を変更
fn prepare(name: String, gql: String) -> Result<PreparedStatementInfo, GleaphError>

// list_prepared の戻り値を変更
fn list_prepared() -> Result<Vec<PreparedStatementInfo>, GleaphError>
```

### カラム名の抽出（prepare 時）

query の場合、ReturnClause から抽出:

```rust
fn extract_columns(stmt: &Statement) -> Vec<String> {
    match stmt {
        Statement::Query(q) => {
            if q.return_clause.star {
                vec!["*".into()]
            } else {
                q.return_clause.items.iter()
                    .map(|item| column_name(item))
                    .collect()
            }
        }
        _ => vec![],
    }
}
```

## 5. CLI Codegen

### 使い方

```bash
gleaph codegen \
  --canister-id bkyz2-fmaaa-aaaaa-qaaaq-cai \
  --output src/gleaph.generated.ts
```

### 処理フロー

```
list_prepared() を呼ぶ
       │
       ▼
Vec<PreparedStatementInfo> を取得
       │
       ▼
各ステートメントについて:
  - kind → executePrepared / executePreparedMutation の選択
  - parameters → 引数の型定義
  - columns → 戻り値の型ヒント (JSDoc)
  - requires_caller → caller() 使用の注記
       │
       ▼
TypeScript コード生成
```

### 生成コード例

```ts
// src/gleaph.generated.ts — AUTO-GENERATED, DO NOT EDIT

import type { GraphClient, QueryResultWithContinuation, MutationResult } from "@gleaph/sdk";

export interface GleaphPrepared {
  /**
   * ```gql
   * MATCH (me:User {principal: caller()})-[:FOLLOWS]->(f)
   * RETURN f.name, f.avatar
   * ```
   * Columns: f.name, f.avatar
   * Uses caller() — requires authenticated identity.
   */
  get_my_follows(): Promise<QueryResultWithContinuation>;

  /**
   * ```gql
   * MATCH (me:User {principal: caller()}), (t:User {principal: $target})
   * CREATE (me)-[:FOLLOWS]->(t)
   * ```
   * Uses caller() — requires authenticated identity.
   */
  follow(params: { target: string }): Promise<MutationResult>;

  /**
   * ```gql
   * MATCH (p:Post {id: $post_id})
   * RETURN p.title, p.body, p.created_at
   * ```
   * Columns: p.title, p.body, p.created_at
   */
  get_post(params: { post_id: number }): Promise<QueryResultWithContinuation>;
}

export function createPreparedClient(graph: GraphClient): GleaphPrepared {
  return {
    get_my_follows: () =>
      graph.executePrepared("get_my_follows", {}),

    follow: (params) =>
      graph.executePreparedMutation("follow", params),

    get_post: (params) =>
      graph.executePrepared("get_post", params),
  };
}
```

## 実装順序

### Phase 1: 基盤 (Value::Principal + $CALLER + Execute)

1. `Value::Principal` を types に追加
2. executor で Principal の比較演算をサポート
3. property store で Principal の serialize/deserialize
4. `caller()` 組み込み関数を executor に追加
5. `AccessLevel::Execute` を追加 + 権限チェック更新
6. テスト

### Phase 2: PreparedStatementInfo

1. `PreparedStatementInfo` / `PreparedKind` を types に追加
2. `prepare()` の戻り値を変更
3. `list_prepared()` の戻り値を変更
4. カラム名抽出の実装
5. `requires_caller` の判定（AST 内の `caller()` 呼び出し検出）
6. JS SDK の types / idl / graph.ts を更新
7. .did ファイルを更新

### Phase 3: CLI Codegen

1. CLI ツール (`gleaph-cli` or standalone) の実装
2. `list_prepared()` の呼び出し (ic-agent 経由)
3. TypeScript コード生成テンプレート
4. テスト + ドキュメント

## 非対象（将来検討）

- パラメータの型推定（AST からの静的解析）
- RETURN カラムの型推定
- Motoko CDK 向け codegen
- watch モード
