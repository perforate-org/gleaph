# GQL 仕様書 (ISO/IEC 39075 ベース)

> **本文書について**: ISO/IEC 39075:2024 (GQL — Graph Query Language) の公式仕様と `reference/grammar/GQL.g4` ANTLR4 文法ファイルを基にした、Gleaph プロジェクト向けの詳細リファレンスである。標準仕様の全構造をセクション番号付きで整理し、各構文要素の意味と Gleaph 実装における対応状況を併記する。

---

## 目次

1. [概要](#1-概要)
2. [用語と概念](#2-用語と概念)
3. [型システム](#3-型システム)
4. [プログラム構造](#4-プログラム構造)
5. [セッション管理 (§7)](#5-セッション管理)
6. [トランザクション管理 (§8)](#6-トランザクション管理)
7. [プロシージャとステートメント (§9)](#7-プロシージャとステートメント)
8. [変数定義 (§10)](#8-変数定義)
9. [グラフ式・バインディングテーブル式 (§11)](#9-グラフ式バインディングテーブル式)
10. [カタログ変更文 (§12)](#10-カタログ変更文)
11. [データ変更文 (§13)](#11-データ変更文)
12. [クエリ文 (§14)](#12-クエリ文)
13. [プロシージャ呼び出し (§15)](#13-プロシージャ呼び出し)
14. [グラフパターンマッチング (§16)](#14-グラフパターンマッチング)
15. [カタログ参照 (§17)](#15-カタログ参照)
16. [グラフ型定義 (§18)](#16-グラフ型定義)
17. [検索条件と述語 (§19)](#17-検索条件と述語)
18. [値式 (§20)](#18-値式)
19. [名前・変数・リテラル (§21)](#19-名前変数リテラル)
20. [Gleaph 実装対応表](#20-gleaph-実装対応表)

---

## 1. 概要

GQL (Graph Query Language) は ISO/IEC 39075:2024 として標準化されたプロパティグラフデータベース向けの宣言型クエリ言語である。SQL とは独立した標準だが、設計思想の多くを共有する:

- **プロパティグラフモデル**: ノード (頂点) とエッジ (辺) がそれぞれラベル群とキー・バリュー型プロパティを持つ
- **パターンマッチング**: ASCII アート風の視覚的パターン構文でグラフ構造を記述する
- **宣言的セマンティクス**: 「何を取得するか」を記述し「どう取得するか」は処理系に委ねる
- **合成可能性**: クエリ結果 (バインディングテーブル) を次のクエリにパイプできる

### 1.1 GQL プログラム全体構造 (§6)

```
gqlProgram
    : programActivity sessionCloseCommand? EOF
    | sessionCloseCommand EOF
    ;

programActivity
    : sessionActivity
    | transactionActivity
    ;
```

GQL プログラムはセッション操作またはトランザクション内のプロシージャ仕様 (= クエリ群) から構成される。最上位は `gqlProgram` であり、オプションの `SESSION CLOSE` コマンドで終了できる。

---

## 2. 用語と概念

| 用語                                       | 定義                                                                                           |
| ------------------------------------------ | ---------------------------------------------------------------------------------------------- |
| **プロパティグラフ**                       | ノード (頂点) の集合とエッジ (辺) の集合から成り、各要素にラベルとプロパティを付与できるグラフ |
| **ノード / 頂点 (Node / Vertex)**          | グラフの頂点。ゼロ個以上のラベルとプロパティを持つ                                             |
| **エッジ / 辺 (Edge / Relationship)**      | 2 つのノード間の有向または無向の接続。ラベルとプロパティを持つ                                 |
| **ラベル (Label)**                         | ノードまたはエッジの分類タグ (複数付与可)                                                      |
| **プロパティ (Property)**                  | キーと値のペア。値は型付き (§18.9)                                                             |
| **バインディング変数 (Binding Variable)**  | パターン内でノードやエッジを束縛する変数名                                                     |
| **バインディングテーブル (Binding Table)** | パターンマッチの結果をリレーショナルな行・列形式で表現したもの                                 |
| **パスパターン (Path Pattern)**            | ノードとエッジの連鎖からなるグラフ構造記述                                                     |
| **パス変数 (Path Variable)**               | マッチしたパス全体を参照する変数                                                               |

---

## 3. 型システム (§18.9)

GQL は豊富な型システムを持つ。`valueType` 規則から展開される型階層:

### 3.1 事前定義型 (Predefined Types)

#### ブーリアン型

```
booleanType : (BOOL | BOOLEAN) notNull? ;
```

#### 文字列型

```
characterStringType
    : STRING (LEFT_PAREN (minLength COMMA)? maxLength RIGHT_PAREN)? notNull?
    | CHAR (LEFT_PAREN fixedLength RIGHT_PAREN)? notNull?
    | VARCHAR (LEFT_PAREN maxLength RIGHT_PAREN)? notNull?
    ;
```

#### バイト列型

```
byteStringType
    : BYTES (LEFT_PAREN (minLength COMMA)? maxLength RIGHT_PAREN)? notNull?
    | BINARY (LEFT_PAREN fixedLength RIGHT_PAREN)? notNull?
    | VARBINARY (LEFT_PAREN maxLength RIGHT_PAREN)? notNull?
    ;
```

#### 数値型

**正確数値型 (Exact Numeric)**:

- 符号付き: `INT8`, `INT16`, `INT32`, `INT64`, `INT128`, `INT256`, `SMALLINT`, `INT`, `BIGINT`
- 符号なし: `UINT8`, `UINT16`, `UINT32`, `UINT64`, `UINT128`, `UINT256`, `USMALLINT`, `UINT`, `UBIGINT`
- 十進型: `DECIMAL(p, s)`, `DEC(p, s)`
- 冗長形式: `INTEGER`, `SMALL INTEGER`, `BIG INTEGER` (それぞれ `SIGNED`/`UNSIGNED` 修飾可)

**近似数値型 (Approximate Numeric)**:

- `FLOAT16`, `FLOAT32`, `FLOAT64`, `FLOAT128`, `FLOAT256`
- `FLOAT(p, s)`, `REAL`, `DOUBLE PRECISION`

#### 時間型

**時間インスタント型**:
| 型名 | 構文 |
|------|------|
| 日付型 | `DATE` |
| タイムゾーン付き日時 | `ZONED DATETIME` / `TIMESTAMP WITH TIME ZONE` |
| ローカル日時 | `LOCAL DATETIME` / `TIMESTAMP (WITHOUT TIME ZONE)?` |
| タイムゾーン付き時刻 | `ZONED TIME` / `TIME WITH TIME ZONE` |
| ローカル時刻 | `LOCAL TIME` / `TIME WITHOUT TIME ZONE` |

**期間型**:

```
temporalDurationType : DURATION LEFT_PAREN temporalDurationQualifier RIGHT_PAREN notNull? ;
temporalDurationQualifier : YEAR TO MONTH | DAY TO SECOND ;
```

#### 参照値型

- グラフ参照: `ANY PROPERTY? GRAPH` (開放型) / `PROPERTY? GRAPH { ... }` (閉鎖型)
- バインディングテーブル参照: `BINDING? TABLE { ... }`
- ノード参照: `ANY? NODE` / ノード型仕様
- エッジ参照: `ANY? EDGE` / エッジ型仕様

#### 非物質値型

- `NULL` (null 型)
- `NULL NOT NULL` / `NOTHING` (空型 — 値を持たない型)

### 3.2 構築型 (Constructed Types)

#### パス型

```
pathValueType : PATH notNull? ;
```

#### リスト型 (配列型)

```
(LIST | ARRAY) <valueType> [maxLength]
valueType (LIST | ARRAY) [maxLength]
```

#### レコード型

```
recordType
    : ANY? RECORD notNull?
    | RECORD? LEFT_BRACE fieldTypeList? RIGHT_BRACE notNull?
    ;
```

### 3.3 動的共用体型 (Dynamic Union Types)

```
ANY VALUE? notNull?                                          -- 開放型
ANY? PROPERTY VALUE notNull?                                 -- プロパティ値型
ANY VALUE? <valueType (| valueType)*>                       -- 閉鎖型
valueType | valueType                                        -- 閉鎖型 (中置)
```

### 3.4 NOT NULL 修飾

全ての型に `NOT NULL` 修飾子を付与できる。`NOT NULL` が付与された型は null 値を許容しない。

### 3.5 型付け演算子

```
typed : DOUBLE_COLON | TYPED ;
```

型付けは `::` 演算子または `TYPED` キーワードで行う。

---

## 4. プログラム構造

### 4.1 トランザクション活動

```
transactionActivity
    : startTransactionCommand (procedureSpecification endTransactionCommand?)?
    | procedureSpecification endTransactionCommand?
    | endTransactionCommand
    ;
```

GQL プログラムは暗黙または明示のトランザクション内でプロシージャ仕様を実行する。プロシージャ仕様は複数のステートメントから構成され、`NEXT` キーワードでチェーン接続される。

### 4.2 プロシージャ仕様 (§9)

```
procedureSpecification : procedureBody ;

procedureBody
    : atSchemaClause? bindingVariableDefinitionBlock? statementBlock
    ;

statementBlock : statement nextStatement* ;

statement
    : compositeQueryStatement
    | linearCatalogModifyingStatement
    | linearDataModifyingStatement
    ;

nextStatement : NEXT yieldClause? statement ;
```

`NEXT` は前のステートメントの結果を後続のステートメントに渡すパイプライン演算子。`YIELD` 句でパイプラインの中間結果をフィルタリングできる。

---

## 5. セッション管理 (§7)

### 5.1 セッション設定 (SESSION SET)

```
sessionSetCommand
    : SESSION SET (sessionSetSchemaClause
                 | sessionSetGraphClause
                 | sessionSetTimeZoneClause
                 | sessionSetParameterClause)
    ;
```

設定可能な項目:

- **スキーマ**: `SESSION SET SCHEMA <schemaReference>`
- **グラフ**: `SESSION SET [PROPERTY] GRAPH <graphExpression>`
- **タイムゾーン**: `SESSION SET TIME ZONE <timeZoneString>`
- **パラメータ**: グラフ / バインディングテーブル / 値パラメータの設定

### 5.2 セッションリセット (SESSION RESET)

```
sessionResetCommand : SESSION RESET sessionResetArguments? ;

sessionResetArguments
    : ALL? (PARAMETERS | CHARACTERISTICS)
    | SCHEMA | PROPERTY? GRAPH | TIME ZONE
    | PARAMETER? sessionParameterSpecification
    ;
```

### 5.3 セッション終了

```
sessionCloseCommand : SESSION CLOSE ;
```

---

## 6. トランザクション管理 (§8)

```
startTransactionCommand : START TRANSACTION transactionCharacteristics? ;

transactionCharacteristics : transactionMode (COMMA transactionMode)* ;

transactionMode : transactionAccessMode ;

transactionAccessMode : READ ONLY | READ WRITE ;

rollbackCommand : ROLLBACK ;
commitCommand   : COMMIT ;
```

GQL はトランザクション分離レベルの指定をサポートしないが、アクセスモード (`READ ONLY` / `READ WRITE`) を指定できる。

---

## 7. プロシージャとステートメント (§9)

### 7.1 ネストされたプロシージャ

```
nestedProcedureSpecification
    : LEFT_BRACE procedureSpecification RIGHT_BRACE ;
```

`{ ... }` でプロシージャをネストし、サブクエリとして使用可能。

### 7.2 ステートメント分類

```
statement
    : compositeQueryStatement           -- 読み取り専用クエリ (集合演算含む)
    | linearCatalogModifyingStatement   -- DDL (スキーマ/グラフ/グラフ型の作成・削除)
    | linearDataModifyingStatement      -- DML (INSERT/SET/REMOVE/DELETE)
    ;
```

---

## 8. 変数定義 (§10)

パイプラインの先頭で束縛変数を宣言できる:

```
bindingVariableDefinition
    : graphVariableDefinition           -- PROPERTY GRAPH x = ...
    | bindingTableVariableDefinition    -- BINDING TABLE t = ...
    | valueVariableDefinition           -- VALUE v = ...
    ;
```

各定義は型注釈 (`:: type`) と初期化子 (`= expression`) を含む。

---

## 9. グラフ式・バインディングテーブル式 (§11)

### 9.1 グラフ式

```
graphExpression
    : graphReference                    -- 名前付きグラフ参照
    | objectExpressionPrimary           -- 式由来
    | objectNameOrBindingVariable       -- 束縛変数
    | currentGraph                      -- CURRENT_GRAPH / CURRENT_PROPERTY_GRAPH
    ;
```

### 9.2 バインディングテーブル式

```
bindingTableExpression
    : nestedBindingTableQuerySpecification
    | bindingTableReference
    | objectExpressionPrimary
    | objectNameOrBindingVariable
    ;
```

---

## 10. カタログ変更文 (§12)

### 10.1 スキーマ管理

```
createSchemaStatement : CREATE SCHEMA (IF NOT EXISTS)? catalogSchemaParentAndName ;
dropSchemaStatement   : DROP SCHEMA (IF EXISTS)? catalogSchemaParentAndName ;
```

### 10.2 グラフ管理

```
createGraphStatement
    : CREATE (PROPERTY? GRAPH (IF NOT EXISTS)? | OR REPLACE PROPERTY? GRAPH)
      catalogGraphParentAndName (openGraphType | ofGraphType) graphSource?
    ;

dropGraphStatement
    : DROP PROPERTY? GRAPH (IF EXISTS)? catalogGraphParentAndName ;
```

グラフ型の指定方法:

- `TYPED ANY [PROPERTY GRAPH]` — 型なし (開放型)
- `LIKE graphExpression` — 既存グラフのスキーマをコピー
- `TYPED graphTypeReference` — 名前付きグラフ型
- `TYPED { elementTypeList }` — インライングラフ型定義

### 10.3 グラフ型管理

```
createGraphTypeStatement
    : CREATE (PROPERTY? GRAPH TYPE (IF NOT EXISTS)? | OR REPLACE PROPERTY? GRAPH TYPE)
      catalogGraphTypeParentAndName graphTypeSource
    ;

dropGraphTypeStatement
    : DROP PROPERTY? GRAPH TYPE (IF EXISTS)? catalogGraphTypeParentAndName ;
```

---

## 11. データ変更文 (§13)

### 11.1 全体構造

```
linearDataModifyingStatement
    : focusedLinearDataModifyingStatement     -- USE グラフ付き
    | ambientLinearDataModifyingStatement      -- 現在のグラフ上
    ;

simpleDataModifyingStatement
    : primitiveDataModifyingStatement
    | callDataModifyingProcedureStatement
    ;

primitiveDataModifyingStatement
    : insertStatement
    | setStatement
    | removeStatement
    | deleteStatement
    ;
```

### 11.2 INSERT 文

```
insertStatement : INSERT insertGraphPattern ;

insertPathPattern
    : insertNodePattern (insertEdgePattern insertNodePattern)*
    ;

insertNodePattern : LEFT_PAREN insertElementPatternFiller? RIGHT_PAREN ;

insertEdgePointingLeft  : LEFT_ARROW_BRACKET  filler? RIGHT_BRACKET_MINUS  ;   -- <-[e:L]-
insertEdgePointingRight : MINUS_LEFT_BRACKET  filler? BRACKET_RIGHT_ARROW  ;   -- -[e:L]->
insertEdgeUndirected    : TILDE_LEFT_BRACKET  filler? RIGHT_BRACKET_TILDE  ;   -- ~[e:L]~
```

INSERT パターンはノードとエッジを含む ASCII アート形式で新しい要素を作成する:

```gql
INSERT (n:Person {name: "Alice", age: 30})
INSERT (a:Person {name: "Alice"})-[:KNOWS {since: 2020}]->(b:Person {name: "Bob"})
```

`insertElementPatternFiller` でラベルとプロパティを指定:

```
insertElementPatternFiller
    : elementVariableDeclaration labelAndPropertySetSpecification?
    | elementVariableDeclaration? labelAndPropertySetSpecification
    ;

labelAndPropertySetSpecification
    : isOrColon labelSetSpecification elementPropertySpecification?
    | (isOrColon labelSetSpecification)? elementPropertySpecification
    ;
```

### 11.3 SET 文

```
setStatement : SET setItemList ;

setItem
    : setPropertyItem          -- v.prop = expr
    | setAllPropertiesItem     -- v = { key: value, ... }
    | setLabelItem             -- v IS Label / v:Label
    ;
```

プロパティの更新:

```gql
SET n.age = 31, n.status = "active"
```

全プロパティの置換:

```gql
SET n = { name: "Alice", age: 31 }
```

ラベルの追加:

```gql
SET n IS Employee
SET n:Employee
```

### 11.4 REMOVE 文

```
removeStatement : REMOVE removeItemList ;

removeItem
    : removePropertyItem    -- v.prop
    | removeLabelItem       -- v IS Label / v:Label
    ;
```

```gql
REMOVE n.age, n:Temporary
```

### 11.5 DELETE 文

```
deleteStatement : (DETACH | NODETACH)? DELETE deleteItemList ;
```

- `DELETE v` — ノードまたはエッジを削除 (接続エッジがある場合はエラー)
- `DETACH DELETE v` — ノードとその接続エッジを一括削除
- `NODETACH DELETE v` — 明示的に接続エッジがある場合のエラーを強制

---

## 12. クエリ文 (§14)

### 12.1 複合クエリ (Composite Query)

```
compositeQueryExpression
    : compositeQueryExpression queryConjunction compositeQueryPrimary
    | compositeQueryPrimary
    ;

queryConjunction
    : setOperator     -- UNION / EXCEPT / INTERSECT
    | OTHERWISE        -- 左側が空の場合のフォールバック
    ;

setOperator
    : UNION setQuantifier?       -- UNION [ALL|DISTINCT]
    | EXCEPT setQuantifier?      -- EXCEPT [ALL|DISTINCT]
    | INTERSECT setQuantifier?   -- INTERSECT [ALL|DISTINCT]
    ;
```

集合演算子:

- `UNION` — 和集合 (デフォルトは `DISTINCT`)
- `UNION ALL` — 重複を保持した和集合
- `EXCEPT` — 差集合
- `INTERSECT` — 積集合
- `OTHERWISE` — 左側の結果が空の場合に右側を返す

### 12.2 線形クエリ文

```
linearQueryStatement
    : focusedLinearQueryStatement    -- USE グラフ + クエリ
    | ambientLinearQueryStatement    -- 現在のグラフ上のクエリ
    ;
```

### 12.3 MATCH 文 (§14.4)

```
matchStatement
    : simpleMatchStatement           -- MATCH pattern
    | optionalMatchStatement         -- OPTIONAL (MATCH pattern | { matches })
    ;

simpleMatchStatement : MATCH graphPatternBindingTable ;

optionalMatchStatement : OPTIONAL optionalOperand ;

optionalOperand
    : simpleMatchStatement
    | LEFT_BRACE matchStatementBlock RIGHT_BRACE
    | LEFT_PAREN matchStatementBlock RIGHT_PAREN
    ;
```

`OPTIONAL MATCH` は SQL の `LEFT OUTER JOIN` に類似。パターンに一致しない場合は null 値が束縛される。

### 12.4 FILTER 文 (§14.6)

```
filterStatement : FILTER (whereClause | searchCondition) ;
```

`WHERE` 句と同等の述語フィルタリングを独立した文として提供。

### 12.5 LET 文 (§14.7)

```
letStatement : LET letVariableDefinitionList ;

letVariableDefinition
    : valueVariableDefinition
    | bindingVariable EQUALS_OPERATOR valueExpression
    ;
```

バインディングテーブルに新しい計算列を追加:

```gql
LET x = a.salary * 1.1
```

### 12.6 FOR 文 (§14.8)

```
forStatement : FOR forItem forOrdinalityOrOffset? ;

forItem : forItemAlias forItemSource ;
forItemAlias : bindingVariable IN ;
forItemSource : valueExpression ;

forOrdinalityOrOffset : WITH (ORDINALITY | OFFSET) bindingVariable ;
```

リスト展開:

```gql
FOR item IN a.tags WITH ORDINALITY idx
```

### 12.7 ORDER BY / LIMIT / OFFSET (§14.9)

```
orderByAndPageStatement
    : orderByClause offsetClause? limitClause?
    | offsetClause limitClause?
    | limitClause
    ;

orderByClause : ORDER BY sortSpecificationList ;

sortSpecification : sortKey orderingSpecification? nullOrdering? ;

orderingSpecification : ASC | ASCENDING | DESC | DESCENDING ;

nullOrdering : NULLS FIRST | NULLS LAST ;

limitClause : LIMIT nonNegativeIntegerSpecification ;

offsetClause : (OFFSET | SKIP) nonNegativeIntegerSpecification ;
```

### 12.8 RETURN 文 (§14.10–14.11)

```
primitiveResultStatement
    : returnStatement orderByAndPageStatement?
    | FINISH
    ;

returnStatement : RETURN returnStatementBody ;

returnStatementBody
    : setQuantifier? (ASTERISK | returnItemList) groupByClause?
    ;

returnItem : aggregatingValueExpression returnItemAlias? ;

returnItemAlias : AS identifier ;
```

RETURN の形式:

- `RETURN *` — 全バインディング変数を返す
- `RETURN DISTINCT a.name, b.age` — 重複排除つき射影
- `RETURN a.name AS name, COUNT(*) AS cnt GROUP BY a.name` — 集約

`FINISH` はデータ変更文の結果ステートメントとして使用し、出力を返さずにパイプラインを終了する。

### 12.9 SELECT 文 (§14.12)

SQL 互換の `SELECT` 構文も提供される:

```
selectStatement
    : SELECT setQuantifier? (ASTERISK | selectItemList)
      (selectStatementBody whereClause? groupByClause?
       havingClause? orderByClause? offsetClause? limitClause?)?
    ;

selectStatementBody
    : FROM (selectGraphMatchList | selectQuerySpecification)
    ;

havingClause : HAVING searchCondition ;
```

```gql
SELECT a.name, COUNT(*) AS cnt
FROM myGraph MATCH (a:Person)-[:KNOWS]->(b)
WHERE a.age > 25
GROUP BY a.name
HAVING COUNT(*) > 3
ORDER BY cnt DESC
LIMIT 10
```

### 12.10 GROUP BY (§16.15)

```
groupByClause : GROUP BY groupingElementList ;

groupingElementList
    : groupingElement (COMMA groupingElement)*
    | emptyGroupingSet
    ;

emptyGroupingSet : LEFT_PAREN RIGHT_PAREN ;  -- () は全行を1グループに集約
```

### 12.11 YIELD 句 (§16.14)

```
yieldClause : YIELD yieldItemList ;
yieldItem : yieldItemName yieldItemAlias? ;
yieldItemAlias : AS bindingVariable ;
```

パイプライン間で受け渡すバインディングテーブルの列を制限する。

---

## 13. プロシージャ呼び出し (§15)

### 13.1 呼び出し文

```
callProcedureStatement : OPTIONAL? CALL procedureCall ;

procedureCall
    : inlineProcedureCall     -- 無名インラインプロシージャ
    | namedProcedureCall      -- 名前付きプロシージャ
    ;
```

### 13.2 インラインプロシージャ

```
inlineProcedureCall
    : variableScopeClause? nestedProcedureSpecification ;

variableScopeClause
    : LEFT_PAREN bindingVariableReferenceList? RIGHT_PAREN ;
```

スコープ句でサブクエリに渡すバインディング変数を制限:

```gql
CALL (a, b) { MATCH (a)-[:KNOWS]->(c) RETURN c }
```

### 13.3 名前付きプロシージャ

```
namedProcedureCall
    : procedureReference LEFT_PAREN procedureArgumentList? RIGHT_PAREN yieldClause?
    ;
```

```gql
CALL myProcedure(arg1, arg2) YIELD col1, col2
```

---

## 14. グラフパターンマッチング (§16)

GQL の中核機能。パターンは ASCII アート風の構文で頂点と辺の構造を記述する。

### 14.1 グラフパターン全体 (§16.4)

```
graphPattern
    : matchMode? pathPatternList keepClause? graphPatternWhereClause?
    ;
```

#### マッチモード

```
matchMode
    : repeatableElementsMatchMode    -- REPEATABLE ELEMENT[S] [BINDINGS]
    | differentEdgesMatchMode        -- DIFFERENT EDGE[S] [BINDINGS]
    ;
```

- `REPEATABLE ELEMENTS` — 同じノードやエッジが複数回出現可能
- `DIFFERENT EDGES` (デフォルト) — 同一エッジの再利用を禁止

#### KEEP 句

```
keepClause : KEEP pathPatternPrefix ;
```

パスの重複排除方法を指定。

#### WHERE 句

```
graphPatternWhereClause : WHERE searchCondition ;
```

パターン内の WHERE は、パターンマッチの結果に対するフィルタ述語。

### 14.2 パスパターン (§16.4)

```
pathPattern
    : pathVariableDeclaration? pathPatternPrefix? pathPatternExpression
    ;

pathVariableDeclaration : pathVariable EQUALS_OPERATOR ;
```

パス変数の宣言:

```gql
p = MATCH (a)-[e*1..5]->(b)  -- p にパス全体を束縛
```

### 14.3 パスパターンプレフィックス (§16.6)

#### パスモード

```
pathMode : WALK | TRAIL | SIMPLE | ACYCLIC ;
```

| モード    | 制約                                    |
| --------- | --------------------------------------- |
| `WALK`    | 制約なし (同一ノード・エッジの再訪可)   |
| `TRAIL`   | 同一エッジの再訪不可                    |
| `SIMPLE`  | 同一ノードの再訪不可 (始点・終点以外)   |
| `ACYCLIC` | 同一ノードの再訪不可 (始点・終点を含む) |

#### パス検索プレフィックス

```
pathSearchPrefix
    : allPathSearch
    | anyPathSearch
    | shortestPathSearch
    ;
```

| プレフィックス                         | 意味                                     |
| -------------------------------------- | ---------------------------------------- |
| `ALL [mode] PATH[S]`                   | 全パスを返す                             |
| `ANY [n] [mode] PATH[S]`               | 任意の n 本のパスを返す                  |
| `ANY SHORTEST [mode] PATH[S]`          | 任意の最短パスを 1 本返す                |
| `ALL SHORTEST [mode] PATH[S]`          | 全最短パスを返す                         |
| `SHORTEST n [mode] PATH[S]`            | 最短の n 本のパスを返す                  |
| `SHORTEST [n] [mode] PATH[S] GROUP[S]` | 始点・終点ペアごとに最短パスをグループ化 |

### 14.4 パスパターン式 (§16.7)

```
pathPatternExpression
    : pathTerm                                              -- 単一パスターム
    | pathTerm (MULTISET_ALTERNATION_OPERATOR pathTerm)+    -- マルチセット和
    | pathTerm (VERTICAL_BAR pathTerm)+                     -- パターン和
    ;

pathTerm : pathFactor+ ;

pathFactor
    : pathPrimary
    | pathPrimary graphPatternQuantifier    -- 量化パターン
    | pathPrimary QUESTION_MARK            -- オプショナルパターン
    ;

pathPrimary
    : elementPattern
    | parenthesizedPathPatternExpression
    | simplifiedPathPatternExpression
    ;
```

### 14.5 要素パターン

#### ノードパターン

```
nodePattern : LEFT_PAREN elementPatternFiller RIGHT_PAREN ;

elementPatternFiller
    : elementVariableDeclaration? isLabelExpression? elementPatternPredicate?
    ;
```

例:

```
()                  -- 任意のノード
(a)                 -- 変数 a に束縛
(a:Person)          -- ラベル Person を持つノード
(:Person {age: 30}) -- ラベル + プロパティフィルタ
(a:Person WHERE a.age > 25)  -- WHERE 述語付き
```

#### エッジパターン

```
edgePattern : fullEdgePattern | abbreviatedEdgePattern ;
```

**完全エッジパターン (方向付き)**:

| 構文            | 方向                |
| --------------- | ------------------- |
| `<-[e:Label]-`  | 左向き (入力エッジ) |
| `-[e:Label]->`  | 右向き (出力エッジ) |
| `~[e:Label]~`   | 無向                |
| `<~[e:Label]~`  | 左向きまたは無向    |
| `~[e:Label]~>`  | 無向または右向き    |
| `<-[e:Label]->` | 左向きまたは右向き  |
| `-[e:Label]-`   | 任意方向            |

**省略エッジパターン**:

| 構文  | 方向               |
| ----- | ------------------ |
| `<-`  | 左向き             |
| `->`  | 右向き             |
| `~`   | 無向               |
| `<~`  | 左向きまたは無向   |
| `~>`  | 無向または右向き   |
| `<->` | 左向きまたは右向き |
| `-`   | 任意方向           |

### 14.6 ラベル式 (§16.8)

```
labelExpression
    : EXCLAMATION_MARK labelExpression          -- 否定
    | labelExpression AMPERSAND labelExpression  -- 論理積
    | labelExpression VERTICAL_BAR labelExpression -- 論理和
    | labelName                                  -- ラベル名
    | PERCENT                                    -- ワイルドカード (任意のラベル)
    | LEFT_PAREN labelExpression RIGHT_PAREN     -- グループ化
    ;
```

例:

```
:Person                    -- Person ラベルを持つ
:Person&Employee           -- Person AND Employee
:Person|Company            -- Person OR Company
:!Deleted                  -- Deleted ラベルを持たない
:%                         -- 任意のラベル
```

### 14.7 グラフパターン量化子 (§16.11)

```
graphPatternQuantifier
    : ASTERISK                      -- 0 回以上 ({0,})
    | PLUS_SIGN                     -- 1 回以上 ({1,})
    | fixedQuantifier               -- 固定回数 {n}
    | generalQuantifier             -- 範囲 {min, max}
    ;

fixedQuantifier : LEFT_BRACE unsignedInteger RIGHT_BRACE ;
generalQuantifier : LEFT_BRACE lowerBound? COMMA upperBound? RIGHT_BRACE ;
```

例:

```
(a)-[:KNOWS]->{2}(b)          -- 正確に 2 ホップ
(a)-[:KNOWS]->{1,5}(b)        -- 1〜5 ホップ
(a)-[:KNOWS]->*(b)             -- 0 ホップ以上
(a)-[:KNOWS]->+(b)             -- 1 ホップ以上
(a)(-[:KNOWS]->()){1,3}(b)    -- 括弧付き量化
```

### 14.8 括弧付きパスパターン (§16.7)

```
parenthesizedPathPatternExpression
    : LEFT_PAREN subpathVariableDeclaration? pathModePrefix?
      pathPatternExpression parenthesizedPathPatternWhereClause? RIGHT_PAREN
    ;
```

部分パスに変数を束縛し、WHERE でフィルタ可能:

```gql
MATCH (a)(sub = -[:KNOWS]->(x) WHERE x.age > 20){1,3}(b)
```

### 14.9 簡略化パスパターン (§16.12)

エッジの方向とラベルのみを簡潔に記述する構文:

```
simplifiedDefaultingRight OPTIONAL: MINUS_SLASH simplifiedContents SLASH_MINUS_RIGHT ;
-- -/Label/->
```

| 構文         | 意味                      |
| ------------ | ------------------------- |
| `-/Label/->` | 右向きエッジ (ラベル付き) |
| `<-/Label/-` | 左向きエッジ              |
| `~/Label/~`  | 無向エッジ                |
| `-/Label/-`  | 任意方向                  |

簡略化パス内でもラベル式の論理演算と量化子が使用可能。

### 14.10 INSERT グラフパターン (§16.5)

```
insertGraphPattern : insertPathPatternList ;

insertPathPattern : insertNodePattern (insertEdgePattern insertNodePattern)* ;
```

INSERT 専用のパターン構文。MATCH パターンとは異なり、ラベル式ではなくラベルセット仕様を使用する。

---

## 15. カタログ参照 (§17)

### 15.1 スキーマ参照

```
schemaReference
    : absoluteCatalogSchemaReference    -- /catalog/schema
    | relativeCatalogSchemaReference    -- ../schema, HOME_SCHEMA, CURRENT_SCHEMA
    | referenceParameterSpecification   -- $param
    ;
```

### 15.2 グラフ参照

```
graphReference
    : catalogObjectParentReference graphName
    | delimitedGraphName
    | homeGraph                         -- HOME_GRAPH / HOME_PROPERTY_GRAPH
    | referenceParameterSpecification
    ;
```

### 15.3 USE グラフ句 (§16.2)

```
useGraphClause : USE graphExpression ;
```

クエリの対象グラフを指定:

```gql
USE socialGraph MATCH (a:Person)-[:KNOWS]->(b) RETURN a, b
```

---

## 16. グラフ型定義 (§18)

### 16.1 ノード型仕様 (§18.2)

```
nodeTypePattern
    : (nodeSynonym TYPE? nodeTypeName)?
      LEFT_PAREN localNodeTypeAlias? nodeTypeFiller? RIGHT_PAREN
    ;

nodeTypeFiller
    : nodeTypeKeyLabelSet nodeTypeImpliedContent?
    | nodeTypeImpliedContent
    ;
```

`NODE` と `VERTEX` は同義語。

### 16.2 エッジ型仕様 (§18.3)

```
edgeTypePattern
    : (edgeKind? edgeSynonym TYPE? edgeTypeName)?
      (edgeTypePatternDirected | edgeTypePatternUndirected)
    ;
```

エッジ型は接続するノード型を `sourceNodeTypeReference` と `destinationNodeTypeReference` で指定:

```gql
CREATE GRAPH TYPE socialSchema {
  (Person :Person {name :: STRING NOT NULL, age :: INT}),
  (:Person)-[:KNOWS {since :: INT}]->(:Person)
}
```

### 16.3 ラベルセット仕様 (§18.4)

```
labelSetSpecification : labelName (AMPERSAND labelName)* ;

labelSetPhrase
    : LABEL labelName
    | LABELS labelSetSpecification
    | isOrColon labelSetSpecification
    ;
```

### 16.4 プロパティ型仕様 (§18.5–18.6)

```
propertyTypesSpecification : LEFT_BRACE propertyTypeList? RIGHT_BRACE ;
propertyType : propertyName typed? propertyValueType ;
```

---

## 17. 検索条件と述語 (§19)

### 17.1 検索条件

```
searchCondition : booleanValueExpression ;
```

検索条件はブーリアン値式であり、以下の述語から構成される。

### 17.2 述語一覧

```
predicate
    : existsPredicate
    | nullPredicate
    | valueTypePredicate
    | directedPredicate
    | labeledPredicate
    | sourceDestinationPredicate
    | all_differentPredicate
    | samePredicate
    | property_existsPredicate
    ;
```

#### 比較述語 (§19.3)

値式の中置演算子として定義:

| 演算子 | 意味       |
| ------ | ---------- |
| `=`    | 等しい     |
| `<>`   | 等しくない |
| `<`    | より小さい |
| `>`    | より大きい |
| `<=`   | 以下       |
| `>=`   | 以上       |

#### EXISTS 述語 (§19.4)

```
existsPredicate
    : EXISTS (LEFT_BRACE graphPattern RIGHT_BRACE
            | LEFT_PAREN graphPattern RIGHT_PAREN
            | nestedQuerySpecification)
    ;
```

サブパターンまたはサブクエリが少なくとも 1 行返すかをテスト。

#### NULL 述語 (§19.5)

```
nullPredicatePart2 : IS NOT? NULL ;
```

```gql
WHERE a.email IS NOT NULL
```

#### 型述語 (§19.6)

```
valueTypePredicatePart2 : IS NOT? typed valueType ;
```

値の動的型をテスト:

```gql
WHERE a.data IS :: INT
```

#### 有向述語 (§19.8)

```
directedPredicatePart2 : IS NOT? DIRECTED ;
```

エッジが有向かどうかをテスト。

#### ラベル述語 (§19.9)

```
labeledPredicatePart2 : IS NOT? LABELED labelExpression ;
```

```gql
WHERE a IS LABELED Person & Employee
```

#### ソース・デスティネーション述語 (§19.10)

```
sourcePredicatePart2 : IS NOT? SOURCE OF edgeReference ;
destinationPredicatePart2 : IS NOT? DESTINATION OF edgeReference ;
```

#### ALL_DIFFERENT 述語 (§19.11)

```
all_differentPredicate
    : ALL_DIFFERENT LEFT_PAREN elem (COMMA elem)+ RIGHT_PAREN ;
```

指定した要素変数が全て異なるグラフ要素を参照するかをテスト。

#### SAME 述語 (§19.12)

```
samePredicate
    : SAME LEFT_PAREN elem (COMMA elem)+ RIGHT_PAREN ;
```

指定した要素変数が全て同一のグラフ要素を参照するかをテスト。

#### PROPERTY_EXISTS 述語 (§19.13)

```
property_existsPredicate
    : PROPERTY_EXISTS LEFT_PAREN elementRef COMMA propertyName RIGHT_PAREN ;
```

要素がそのプロパティを持つかをテスト。

---

## 18. 値式 (§20)

### 18.1 値式全体 (§20.1)

```
valueExpression
    : sign valueExpression                           -- 符号付き (+/-)
    | valueExpression (* | /) valueExpression        -- 乗除算
    | valueExpression (+ | -) valueExpression        -- 加減算
    | valueExpression || valueExpression              -- 連結
    | valueExpression compOp valueExpression          -- 比較
    | NOT valueExpression                             -- 論理否定
    | valueExpression IS NOT? truthValue              -- 真偽テスト
    | valueExpression AND valueExpression             -- 論理積
    | valueExpression (OR | XOR) valueExpression      -- 論理和 / 排他的論理和
    | predicate                                       -- 述語
    | valueFunction                                   -- 値関数
    | valueExpressionPrimary                          -- 基本値式
    ;
```

演算子の優先順位 (高 → 低):

1. 単項 `+`, `-`
2. `*`, `/`
3. `+`, `-`
4. `||` (連結)
5. 比較演算子 (`=`, `<>`, `<`, `>`, `<=`, `>=`)
6. `NOT`
7. `IS [NOT] truthValue`
8. `AND`
9. `OR`, `XOR`

### 18.2 値式プライマリ (§20.2)

```
valueExpressionPrimary
    : parenthesizedValueExpression       -- (expr)
    | aggregateFunction                  -- COUNT(*), SUM(x), ...
    | unsignedValueSpecification         -- リテラル, パラメータ
    | pathValueConstructor               -- PATH [...]
    | valueExpressionPrimary.propertyName  -- プロパティアクセス
    | valueQueryExpression               -- VALUE { query }
    | caseExpression                     -- CASE...END
    | castSpecification                  -- CAST(expr AS type)
    | element_idFunction                 -- ELEMENT_ID(var)
    | letValueExpression                 -- LET ... IN ... END
    | bindingVariableReference           -- 変数参照
    ;
```

### 18.3 CASE 式 (§20.7)

```
-- 簡略 CASE
CASE operand
    WHEN value1 THEN result1
    WHEN value2 THEN result2
    ELSE default_result
END

-- 検索 CASE
CASE
    WHEN condition1 THEN result1
    WHEN condition2 THEN result2
    ELSE default_result
END

-- 略記形式
NULLIF(expr1, expr2)     -- expr1 = expr2 なら NULL、そうでなければ expr1
COALESCE(expr1, expr2, ...)  -- 最初の非 NULL 値
```

### 18.4 CAST 式 (§20.8)

```
castSpecification : CAST LEFT_PAREN castOperand AS castTarget RIGHT_PAREN ;
```

### 18.5 集約関数 (§20.9)

```
aggregateFunction
    : COUNT LEFT_PAREN ASTERISK RIGHT_PAREN        -- COUNT(*)
    | generalSetFunction                             -- COUNT/SUM/AVG/MIN/MAX/COLLECT_LIST/STDDEV_SAMP/STDDEV_POP
    | binarySetFunction                              -- PERCENTILE_CONT/PERCENTILE_DISC
    ;

generalSetFunction
    : generalSetFunctionType LEFT_PAREN setQuantifier? valueExpression RIGHT_PAREN ;

setQuantifier : DISTINCT | ALL ;
```

| 関数                          | 説明                 |
| ----------------------------- | -------------------- |
| `COUNT(*)`                    | 行数カウント         |
| `COUNT([DISTINCT] expr)`      | 非 NULL 値のカウント |
| `SUM([DISTINCT] expr)`        | 合計                 |
| `AVG([DISTINCT] expr)`        | 平均                 |
| `MIN(expr)`                   | 最小値               |
| `MAX(expr)`                   | 最大値               |
| `COLLECT_LIST(expr)`          | リスト集約           |
| `STDDEV_SAMP(expr)`           | 標本標準偏差         |
| `STDDEV_POP(expr)`            | 母集団標準偏差       |
| `PERCENTILE_CONT(expr, rank)` | 連続百分位           |
| `PERCENTILE_DISC(expr, rank)` | 離散百分位           |

### 18.6 数値関数 (§20.22)

| 関数                                 | 説明          |
| ------------------------------------ | ------------- |
| `ABS(x)`                             | 絶対値        |
| `FLOOR(x)`                           | 床関数        |
| `CEIL(x)` / `CEILING(x)`             | 天井関数      |
| `MOD(x, y)`                          | 剰余          |
| `SQRT(x)`                            | 平方根        |
| `POWER(x, y)`                        | 累乗          |
| `EXP(x)`                             | 指数関数      |
| `LN(x)`                              | 自然対数      |
| `LOG(base, x)`                       | 一般対数      |
| `LOG10(x)`                           | 常用対数      |
| `SIN/COS/TAN/COT(x)`                 | 三角関数      |
| `SINH/COSH/TANH(x)`                  | 双曲線関数    |
| `ASIN/ACOS/ATAN(x)`                  | 逆三角関数    |
| `DEGREES(x)`                         | ラジアン → 度 |
| `RADIANS(x)`                         | 度 → ラジアン |
| `CHAR_LENGTH(s)`                     | 文字数        |
| `BYTE_LENGTH(s)` / `OCTET_LENGTH(s)` | バイト長      |
| `PATH_LENGTH(p)`                     | パスの辺数    |
| `CARDINALITY(x)` / `SIZE(list)`      | 要素数        |

### 18.7 文字列関数 (§20.24)

| 関数                             | 説明                               |
| -------------------------------- | ---------------------------------- |
| `LEFT(s, n)` / `RIGHT(s, n)`     | 先頭/末尾 n 文字                   |
| `TRIM(s)`                        | 空白除去                           |
| `BTRIM/LTRIM/RTRIM(s [, chars])` | 両端/左/右トリム                   |
| `UPPER(s)` / `LOWER(s)`          | 大文字/小文字変換                  |
| `NORMALIZE(s [, form])`          | Unicode 正規化 (NFC/NFD/NFKC/NFKD) |

### 18.8 日時関数 (§20.27)

| 関数/定数                            | 説明                                 |
| ------------------------------------ | ------------------------------------ |
| `CURRENT_DATE`                       | 現在の日付                           |
| `CURRENT_TIME`                       | 現在のタイムゾーン付き時刻           |
| `CURRENT_TIMESTAMP`                  | 現在のタイムゾーン付きタイムスタンプ |
| `LOCAL_TIME`                         | 現在のローカル時刻                   |
| `LOCAL_TIMESTAMP` / `LOCAL_DATETIME` | 現在のローカル日時                   |
| `DATE(string)`                       | 文字列から日付を構築                 |
| `ZONED_TIME(string)`                 | 文字列からタイムゾーン付き時刻を構築 |
| `ZONED_DATETIME(string)`             | 文字列からタイムゾーン付き日時を構築 |
| `DURATION_BETWEEN(dt1, dt2)`         | 2 つの日時間の期間                   |

### 18.9 リスト関数とコンストラクタ (§20.15–20.17)

```
-- リストリテラル
[1, 2, 3]
LIST[1, 2, 3]

-- ELEMENTS 関数
ELEMENTS(pathExpression)  -- パスを要素リストに変換

-- TRIM 関数 (リスト用)
TRIM(list, n)  -- リストを n 要素にトリム
```

### 18.10 レコードコンストラクタ (§20.18)

```
{key1: value1, key2: value2}
RECORD {key1: value1, key2: value2}
```

### 18.11 パスコンストラクタ (§20.14)

```
PATH [node1, edge1, node2, edge2, node3]
```

### 18.12 動的パラメータ (§20.4)

```
dynamicParameterSpecification : GENERAL_PARAMETER_REFERENCE ;  -- $param
```

### 18.13 ELEMENT_ID 関数 (§20.10)

```
element_idFunction : ELEMENT_ID LEFT_PAREN elementVariableReference RIGHT_PAREN ;
```

ノードまたはエッジの実装定義 ID を返す。

### 18.14 LET 値式 (§20.5)

```
letValueExpression : LET letVariableDefinitionList IN valueExpression END ;
```

ローカル変数を定義して式内で使用:

```gql
LET x = a.salary * 0.1 IN a.salary + x END
```

### 18.15 VALUE サブクエリ (§20.6)

```
valueQueryExpression : VALUE nestedQuerySpecification ;
```

サブクエリの結果をスカラー値として使用:

```gql
VALUE { MATCH (n:Config) RETURN n.maxRetries }
```

---

## 19. 名前・変数・リテラル (§21)

### 19.1 識別子

```
identifier
    : regularIdentifier                    -- ASCII 英数字 + アンダースコア
    | DOUBLE_QUOTED_CHARACTER_SEQUENCE      -- "delimited identifier"
    | ACCENT_QUOTED_CHARACTER_SEQUENCE      -- `accent quoted`
    ;

regularIdentifier : REGULAR_IDENTIFIER | nonReservedWords ;
```

- 正規識別子はアルファベットまたはアンダースコアで始まり、英数字・アンダースコアが続く
- 予約語と同名の識別子を使う場合はデリミタ付き引用が必要
- 大文字小文字は区別しない (case-insensitive)

### 19.2 リテラル

```
unsignedLiteral
    : unsignedNumericLiteral    -- 42, 3.14, 1E10
    | generalLiteral            -- TRUE, 'text', NULL, [1,2], {k:v}, DATE '...'
    ;
```

#### 数値リテラル

| 形式                 | 例               |
| -------------------- | ---------------- |
| 10 進整数            | `42`, `1_000`    |
| 16 進整数            | `0x2A`           |
| 8 進整数             | `0o52`           |
| 2 進整数             | `0b101010`       |
| 小数                 | `3.14`           |
| 科学表記             | `1.5E10`         |
| 正確数値サフィックス | `42M`            |
| 近似数値サフィックス | `3.14F`, `3.14D` |

#### ブーリアンリテラル

```
BOOLEAN_LITERAL : TRUE | FALSE | UNKNOWN ;
```

#### 文字列リテラル

```
'single quoted'     -- シングルクォート
"double quoted"     -- ダブルクォート
```

エスケープシーケンス: `\\`, `\'`, `\"`, `` \` ``, `\t`, `\b`, `\n`, `\r`, `\f`, `\uXXXX`, `\UXXXXXX`

`@` プレフィックスでエスケープ無効: `@'raw string'`

#### バイト列リテラル

```
X'48656C6C6F'    -- 16進バイト列
```

#### 時間リテラル

```
DATE '2024-01-15'
TIME '14:30:00+09:00'
DATETIME '2024-01-15T14:30:00'
TIMESTAMP '2024-01-15T14:30:00Z'
DURATION 'P1Y6M'
```

#### NULL リテラル

```
NULL
```

#### リストリテラル

```
[1, 2, 3]
LIST [1, 2, 3]
```

#### レコードリテラル

```
{name: "Alice", age: 30}
```

### 19.3 予約語

GQL の予約語は約 200 語に及ぶ。主要なものの分類:

**クエリ構文**: `MATCH`, `WHERE`, `RETURN`, `ORDER`, `BY`, `LIMIT`, `OFFSET`, `SKIP`, `WITH`, `UNION`, `EXCEPT`, `INTERSECT`, `SELECT`, `FROM`, `GROUP`, `HAVING`, `DISTINCT`, `ALL`, `AS`, `ASC`, `DESC`, `FILTER`, `FOR`, `IN`, `LET`, `OPTIONAL`, `OTHERWISE`, `FINISH`, `YIELD`, `NEXT`, `USE`

**DML**: `CREATE`, `INSERT`, `SET`, `REMOVE`, `DELETE`, `DETACH`, `NODETACH`

**DDL**: `DROP`, `SCHEMA`, `GRAPH`, `TYPE`, `REPLACE`, `COPY`, `LIKE`, `IF`, `EXISTS`, `NOT`

**論理**: `AND`, `OR`, `XOR`, `NOT`, `IS`, `TRUE`, `FALSE`, `NULL`, `UNKNOWN`

**集約**: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `COLLECT_LIST`, `STDDEV_SAMP`, `STDDEV_POP`, `PERCENTILE_CONT`, `PERCENTILE_DISC`

**型**: `BOOL`, `BOOLEAN`, `INT`, `INTEGER`, `FLOAT`, `DOUBLE`, `STRING`, `CHAR`, `VARCHAR`, `BYTES`, `BINARY`, `VARBINARY`, `DECIMAL`, `DEC`, `REAL`, `DATE`, `TIME`, `TIMESTAMP`, `DATETIME`, `DURATION`, `PATH`, `LIST`, `ARRAY`, `RECORD`, `ANY`, `VALUE`, `PROPERTY`, `NOTHING`, `TYPED`

**パス**: `WALK`, `TRAIL`, `SIMPLE`, `ACYCLIC`, `SHORTEST`, `PATHS`, `PATH`

**セッション/トランザクション**: `SESSION`, `CLOSE`, `RESET`, `START`, `TRANSACTION`, `COMMIT`, `ROLLBACK`, `READ`, `WRITE`

**述語**: `SAME`, `ALL_DIFFERENT`, `PROPERTY_EXISTS`, `LABELED`, `DIRECTED`, `SOURCE`, `DESTINATION`, `NORMALIZED`

#### Gleaph の予約語一覧

Gleaph のレキサー (`lexer.rs`) では以下の語を予約語として定義している。これらに一致する識別子はバッククォートで囲む必要がある:

`MATCH`, `WHERE`, `RETURN`, `ORDER`, `BY`, `LIMIT`, `CREATE`, `DELETE`, `SET`, `REMOVE`, `OPTIONAL`, `OR`, `NOT`, `XOR`, `IN`, `IS`, `DISTINCT`, `CASE`, `WHEN`, `THEN`, `ELSE`, `END`, `DETACH`, `EXISTS`, `WITH`, `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `COLLECT`, `COALESCE`, `NULLIF`, `GROUP`, `HAVING`, `OFFSET`, `SKIP`, `UNION`, `ALL`, `EXCEPT`, `INTERSECT`, `INSERT`, `LABELS`, `PROPERTIES`, `TYPE`, `ID`, `UPPER`, `LOWER`, `TRIM`, `SUBSTRING`, `SIZE`, `ABS`, `FLOOR`, `CEIL`, `TOSTRING`, `TOINTEGER`, `TOFLOAT`, `ANY`, `SHORTEST`, `PATH`, `AND`, `AS`, `ASC`, `DESC`, `TRUE`, `FALSE`, `NULL`

この中には実際のグラフデータで頻繁に使われるプロパティ名やエッジラベル (`min`, `max`, `group`, `order`, `end`, `skip`, `all`, `type`, `id`, `size`, `path` など) が含まれている。これらを識別子として使う場合はバッククォートが必要:

```gql
-- エラー: 予約語をプロパティ名として使用
MATCH (r:Record) WHERE r.min > 0 RETURN r.end

-- 正しい: バッククォートでエスケープ
MATCH (r:Record) WHERE r.`min` > 0 RETURN r.`end`
```

> **注記**: 予約語の多さと実用的な命名の自由度のトレードオフは、GQL 標準コミュニティでも未解決の問題として議論されている ([opengql/grammar#31](https://github.com/opengql/grammar/issues/31))。

### 19.4 非予約語

以下は識別子としても使用可能:

`ACYCLIC`, `BINDING`, `BINDINGS`, `CONNECTING`, `DESTINATION`, `DIFFERENT`, `DIRECTED`, `EDGE`, `EDGES`, `ELEMENT`, `ELEMENTS`, `FIRST`, `GRAPH`, `GROUPS`, `KEEP`, `LABEL`, `LABELED`, `LABELS`, `LAST`, `NFC`, `NFD`, `NFKC`, `NFKD`, `NO`, `NODE`, `NORMALIZED`, `ONLY`, `ORDINALITY`, `PROPERTY`, `READ`, `RELATIONSHIP`, `RELATIONSHIPS`, `REPEATABLE`, `SHORTEST`, `SIMPLE`, `SOURCE`, `TABLE`, `TO`, `TRAIL`, `TRANSACTION`, `TYPE`, `UNDIRECTED`, `VERTEX`, `WALK`, `WITHOUT`, `WRITE`, `ZONE`

### 19.5 同義語

| 概念          | 同義語                   |
| ------------- | ------------------------ |
| ノード        | `NODE`, `VERTEX`         |
| エッジ        | `EDGE`, `RELATIONSHIP`   |
| エッジ (複数) | `EDGES`, `RELATIONSHIPS` |

---

## 20. Gleaph 実装対応表

以下は `reference/grammar/GQL.g4` の仕様セクションと Gleaph の `crates/gql/` 実装の対応状況である。

### 20.1 実装済み機能

| GQL 機能                       | 仕様セクション  | Gleaph 実装                                                                                                                       |
| ------------------------------ | --------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| **MATCH 文**                   | §14.4           | `parser.rs` — ノード・エッジパターン、1〜3 ホップ                                                                                 |
| **OPTIONAL MATCH**             | §14.4           | `parser.rs`, `executor.rs` — LEFT JOIN セマンティクス                                                                             |
| **WHERE 句**                   | §16.13          | `parser.rs` — 全比較演算子、AND/OR/NOT/XOR                                                                                        |
| **RETURN 句**                  | §14.11          | `parser.rs` — 射影、DISTINCT、AS エイリアス                                                                                       |
| **ORDER BY**                   | §16.16–16.17    | `parser.rs` — ASC/DESC、NULLS FIRST/LAST なし                                                                                     |
| **LIMIT**                      | §16.18          | `parser.rs`, `executor.rs` — 定数のみ                                                                                             |
| **OFFSET / SKIP**              | §16.19          | `parser.rs` — 両キーワード対応                                                                                                    |
| **WITH 句**                    | — (Cypher 由来) | `parser.rs` — 中間射影 + 後続 MATCH                                                                                               |
| **CREATE (INSERT)**            | §13.2           | `parser.rs` — ノード作成、エッジ作成                                                                                              |
| **DELETE**                     | §13.5           | `parser.rs` — DETACH DELETE、WHERE 必須                                                                                           |
| **SET**                        | §13.3           | `parser.rs` — プロパティ設定、ラベル追加                                                                                          |
| **REMOVE**                     | §13.4           | `parser.rs` — プロパティ削除、ラベル削除                                                                                          |
| **UNION / EXCEPT / INTERSECT** | §14.2           | `parser.rs`, `executor.rs` — 集合演算                                                                                             |
| **集約関数**                   | §20.9           | `executor.rs` — COUNT, SUM, AVG, MIN, MAX, COLLECT                                                                                |
| **GROUP BY / HAVING**          | §16.15          | `parser.rs`, `executor.rs`                                                                                                        |
| **CASE 式**                    | §20.7           | `parser.rs` — 検索 CASE、NULLIF、COALESCE                                                                                         |
| **IS NULL / IS NOT NULL**      | §19.5           | `parser.rs`, `executor.rs`                                                                                                        |
| **IN リスト**                  | —               | `parser.rs` — `expr IN [v1, v2, ...]`                                                                                             |
| **EXISTS サブクエリ**          | §19.4           | `parser.rs` — EXISTS { subquery }                                                                                                 |
| **パス変数**                   | §16.4           | `parser.rs` — `p = (a)-[*]->(b)`                                                                                                  |
| **可変長パス**                 | §16.11          | `parser.rs` — `[*1..6]` (max 6)                                                                                                   |
| **ANY SHORTEST PATH**          | §16.6           | `parser.rs`, `executor.rs` — BFS ベース                                                                                           |
| **算術演算子**                 | §20.1           | `parser.rs` — `+`, `-`, `*`, `/`, `%`                                                                                             |
| **文字列連結**                 | §20.1           | `parser.rs` — `\|\|` 演算子                                                                                                       |
| **プロパティアクセス**         | §20.11          | `parser.rs` — `var.property`                                                                                                      |
| **パラメータ**                 | §20.4           | `parser.rs` — `$param`                                                                                                            |
| **組み込み関数**               | §20.22ff        | `executor.rs` — ID, LABELS, TYPE, PROPERTIES, UPPER, LOWER, TRIM, SUBSTRING, SIZE, ABS, FLOOR, CEIL, TOSTRING, TOINTEGER, TOFLOAT |
| **リストリテラル**             | §20.17          | `parser.rs` — `[1, 2, 3]`                                                                                                         |
| **リストインデックス**         | —               | `parser.rs` — `list[index]`                                                                                                       |
| **PATH_LENGTH**                | §20.22          | `parser.rs` — `PATH_LENGTH(p)`                                                                                                    |

### 20.2 未実装機能

| GQL 機能                                  | 仕様セクション | 備考                                                                    |
| ----------------------------------------- | -------------- | ----------------------------------------------------------------------- |
| セッション管理                            | §7             | GQL 標準のセッション/トランザクションモデルは IC アーキテクチャと非互換 |
| トランザクション管理                      | §8             | IC のコンセンサスモデルで代替                                           |
| NEXT パイプライン                         | §9.2           | WITH 句で部分的に代替                                                   |
| DDL (CREATE/DROP SCHEMA/GRAPH/GRAPH TYPE) | §12            | IC カニスターレベルで管理                                               |
| SELECT 文                                 | §14.12         | MATCH + RETURN で代替                                                   |
| FILTER 文                                 | §14.6          | WHERE 句で代替                                                          |
| LET 文                                    | §14.7          | WITH 句で代替可能                                                       |
| FOR 文                                    | §14.8          | リスト展開未サポート                                                    |
| CALL プロシージャ                         | §15            | ストアドプロシージャ未サポート                                          |
| パスモード (WALK/TRAIL/SIMPLE/ACYCLIC)    | §16.6          | デフォルト動作のみ                                                      |
| マッチモード (REPEATABLE/DIFFERENT)       | §16.4          | デフォルト動作のみ                                                      |
| ラベル式の論理演算                        | §16.8          | 単一ラベルのみ (AND/OR/NOT 未サポート)                                  |
| 簡略化パスパターン                        | §16.12         | `-/Label/->` 形式未サポート                                             |
| KEEP 句                                   | §16.4          | 未サポート                                                              |
| 無向エッジ                                | §16.6          | Gleaph は有向グラフのみ                                                 |
| CAST 式                                   | §20.8          | 型変換は組み込み関数で代替 (TOSTRING 等)                                |
| 時間リテラル                              | §21.2          | Timestamp は整数ベース                                                  |
| レコードコンストラクタ                    | §20.18         | 未サポート                                                              |
| バイト列型                                | §18.9          | 未サポート                                                              |
| VALUE サブクエリ                          | §20.6          | 未サポート                                                              |
| 二項集合関数                              | §20.9          | PERCENTILE_CONT/DISC 未サポート                                         |
| 三角関数・対数関数                        | §20.22         | 未サポート                                                              |
| Unicode 正規化関数                        | §20.24         | 未サポート                                                              |
| YIELD 句                                  | §16.14         | 未サポート                                                              |
| NULLS FIRST/LAST                          | §16.17         | ORDER BY のデフォルト動作のみ                                           |

### 20.3 Gleaph 固有の値型マッピング

| GQL 型         | Gleaph `Value` enum             |
| -------------- | ------------------------------- |
| BOOLEAN        | `Value::Bool(bool)`             |
| INT / INTEGER  | `Value::Int(i64)`               |
| FLOAT / DOUBLE | `Value::Float(f64)`             |
| STRING         | `Value::Text(String)`           |
| TIMESTAMP      | `Value::Timestamp(u64)`         |
| LIST           | `Value::List(Vec<Value>)`       |
| PATH           | `Value::Path(Vec<PathElement>)` |
| NULL           | `Value::Null`                   |

### 20.4 Gleaph GQL パイプライン

```
文字列 → lexer::tokenize → parser::parse_statement → AST
    → validate::validate_statement → planner::build_plan → PhysicalPlan
    → executor::execute_plan → QueryResult
```

| ステージ     | ファイル      | 役割                                                 |
| ------------ | ------------- | ---------------------------------------------------- |
| レキサー     | `lexer.rs`    | トークン化 (nom ベース)                              |
| パーサー     | `parser.rs`   | トークン列 → AST (再帰下降、nom コンビネータ)        |
| AST          | `ast.rs`      | Statement, QueryStmt, Expr 等のデータ構造            |
| バリデータ   | `validate.rs` | 意味検証 (変数スコープ、型チェック、機能ゲート)      |
| プランナー   | `planner.rs`  | コストベースアンカー選択、フィルタ押し下げ、結合順序 |
| 実行エンジン | `executor.rs` | Volcano モデル (RowIterator)、BFS 最短パス           |
| 計画表現     | `plan.rs`     | PlanOp 列、PlanAnnotations                           |
| 値比較       | `value.rs`    | 型間比較ロジック                                     |
| 統計         | `stats.rs`    | プランナー向けコスト推定                             |

### 20.5 実装上の制約

| 制約                   | 値           | 根拠                                                |
| ---------------------- | ------------ | --------------------------------------------------- |
| MATCH ホップ数         | 1〜3         | Phase 2 機能ゲート                                  |
| 可変長パス上限         | max 6        | バリデーション制約                                  |
| クエリ文字列長         | 16KB         | GQL ブリッジガードレール                            |
| RETURN 項目数          | 1 以上       | バリデーション必須                                  |
| DELETE の WHERE        | 必須         | 安全性のため無制限削除を禁止                        |
| 重複バインディング変数 | 禁止         | バリデーションで検出 (WITH 継続 MATCH では再利用可) |
| プロパティヒント       | リテラルのみ | CREATE のプロパティ値はリテラル必須                 |

---

## 付録 A: 構文図の凡例

| 記法       | 意味                                              |
| ---------- | ------------------------------------------------- |
| `KEYWORD`  | 予約語 (大文字小文字不問)                         |
| `ruleName` | 構文規則への参照                                  |
| `( ... )`  | パターンのグルーピング                            |
| `[ ... ]`  | 角括弧リテラル (`LEFT_BRACKET` / `RIGHT_BRACKET`) |
| `{ ... }`  | 波括弧リテラル (`LEFT_BRACE` / `RIGHT_BRACE`)     |
| `?`        | オプション (0 回または 1 回)                      |
| `*`        | 0 回以上の繰り返し                                |
| `+`        | 1 回以上の繰り返し                                |
| `\|`       | 選択 (alternatives)                               |

## 付録 B: 文法セクション対応表

| 文法コメント | GQL 仕様セクション   | 本仕様書のセクション |
| ------------ | -------------------- | -------------------- |
| §6           | GQL プログラム       | §4                   |
| §7           | セッション管理       | §5                   |
| §8           | トランザクション管理 | §6                   |
| §9           | プロシージャ仕様     | §7                   |
| §10          | 変数定義             | §8                   |
| §11          | グラフ式             | §9                   |
| §12          | カタログ変更文       | §10                  |
| §13          | データ変更文         | §11                  |
| §14          | クエリ文             | §12                  |
| §15          | プロシージャ呼び出し | §13                  |
| §16          | グラフパターン       | §14                  |
| §17          | カタログ参照         | §15                  |
| §18          | グラフ型定義         | §16                  |
| §19          | 検索条件と述語       | §17                  |
| §20          | 値式                 | §18                  |
| §21          | 名前・変数・リテラル | §19                  |
