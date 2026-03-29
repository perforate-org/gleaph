# GQL Specification (Based on ISO/IEC 39075)

> **About this document**: This is a detailed reference for the Gleaph project, based on the official ISO/IEC 39075:2024 (GQL — Graph Query Language) specification and the `reference/grammar/GQL.g4` ANTLR4 grammar file. It organizes the entire structure of the standard specification with section numbers, while providing the meaning of each syntactic element and its implementation status in Gleaph.

---

## Table of Contents

1. [Overview](#1-overview)
2. [Terms and Concepts](#2-terms-and-concepts)
3. [Type System](#3-type-system)
4. [Program Structure](#4-program-structure)
5. [Session Management (§7)](#5-session-management)
6. [Transaction Management (§8)](#6-transaction-management)
7. [Procedures and Statements (§9)](#7-procedures-and-statements)
8. [Variable Definitions (§10)](#8-variable-definitions)
9. [Graph Expressions and Binding Table Expressions (§11)](#9-graph-expressions-binding-table-expressions)
10. [Catalog Modification Statements (§12)](#10-catalog-modification-statements)
11. [Data Modification Statements (§13)](#11-data-modification-statements)
12. [Query Statements (§14)](#12-query-statements)
13. [Procedure Calls (§15)](#13-procedure-calls)
14. [Graph Pattern Matching (§16)](#14-graph-pattern-matching)
15. [Catalog References (§17)](#15-catalog-references)
16. [Graph Type Definitions (§18)](#16-graph-type-definitions)
17. [Search Conditions and Predicates (§19)](#17-search-conditions-and-predicates)
18. [Value Expressions (§20)](#18-value-expressions)
19. [Names, Variables, and Literals (§21)](#19-names-variables-and-literals)
20. [Gleaph Implementation Compatibility Table](#20-gleaph-implementation-compatibility-table)

---

## 1. Overview

GQL (Graph Query Language) is a declarative query language for property graph databases, standardized as ISO/IEC 39075:2024. While it is a standard independent of SQL, it shares many of its design philosophies:

- **Property Graph Model**: Consists of nodes (vertices) and edges (relationships), each of which can have sets of labels and key-value properties.
- **Pattern Matching**: Describes graph structures using a visual, ASCII-art-style pattern syntax.
- **Declarative Semantics**: Describes "what to retrieve," leaving "how to retrieve it" to the processing engine.
- **Composability**: Query results (binding tables) can be piped into subsequent queries.

### 1.1 Overall GQL Program Structure (§6)

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

A GQL program consists of session operations or procedure specifications (= queries) within a transaction. The top-level element is `gqlProgram`, which can optionally end with a `SESSION CLOSE` command.

---

## 2. Terms and Concepts

| Term                    | Definition                                                                                                                                       |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Property Graph**      | A graph consisting of a set of nodes (vertices) and a set of edges (relationships), where labels and properties can be assigned to each element. |
| **Node / Vertex**       | A vertex in the graph. It can have zero or more labels and properties.                                                                           |
| **Edge / Relationship** | A directed or undirected connection between two nodes. It has labels and properties.                                                             |
| **Label**               | A classification tag for nodes or edges (multiple labels can be assigned).                                                                       |
| **Property**            | A key-value pair. Values are typed (§18.9).                                                                                                      |
| **Binding Variable**    | A variable name that binds nodes or edges within a pattern.                                                                                      |
| **Binding Table**       | A relational row/column representation of the results of pattern matching.                                                                       |
| **Path Pattern**        | A description of a graph structure consisting of a chain of nodes and edges.                                                                     |
| **Path Variable**       | A variable that references an entire matched path.                                                                                               |

---

## 3. Type System (§18.9)

GQL has a rich type system. The type hierarchy expanded from the `valueType` rule:

### 3.1 Predefined Types

#### Boolean Type

```
booleanType : (BOOL | BOOLEAN) notNull? ;
```

#### Character String Type

```
characterStringType
    : STRING (LEFT_PAREN (minLength COMMA)? maxLength RIGHT_PAREN)? notNull?
    | CHAR (LEFT_PAREN fixedLength RIGHT_PAREN)? notNull?
    | VARCHAR (LEFT_PAREN maxLength RIGHT_PAREN)? notNull?
    ;
```

#### Byte String Type

```
byteStringType
    : BYTES (LEFT_PAREN (minLength COMMA)? maxLength RIGHT_PAREN)? notNull?
    | BINARY (LEFT_PAREN fixedLength RIGHT_PAREN)? notNull?
    | VARBINARY (LEFT_PAREN maxLength RIGHT_PAREN)? notNull?
    ;
```

#### Numeric Types

**Exact Numeric**:

- Signed: `INT8`, `INT16`, `INT32`, `INT64`, `INT128`, `INT256`, `SMALLINT`, `INT`, `BIGINT`
- Unsigned: `UINT8`, `UINT16`, `UINT32`, `UINT64`, `UINT128`, `UINT256`, `USMALLINT`, `UINT`, `UBIGINT`
- Decimal: `DECIMAL(p, s)`, `DEC(p, s)`
- Verbose forms: `INTEGER`, `SMALL INTEGER`, `BIG INTEGER` (Each can be modified by `SIGNED`/`UNSIGNED`)

**Approximate Numeric**:

- `FLOAT16`, `FLOAT32`, `FLOAT64`, `FLOAT128`, `FLOAT256`
- `FLOAT(p, s)`, `REAL`, `DOUBLE PRECISION`

#### Temporal Types

**Time Instant Types**:
| Type Name | Syntax |
|-----------|--------|
| Date | `DATE` |
| Datetime with Time Zone | `ZONED DATETIME` / `TIMESTAMP WITH TIME ZONE` |
| Local Datetime | `LOCAL DATETIME` / `TIMESTAMP (WITHOUT TIME ZONE)?` |
| Time with Time Zone | `ZONED TIME` / `TIME WITH TIME ZONE` |
| Local Time | `LOCAL TIME` / `TIME WITHOUT TIME ZONE` |

**Duration Type**:

```
temporalDurationType : DURATION LEFT_PAREN temporalDurationQualifier RIGHT_PAREN notNull? ;
temporalDurationQualifier : YEAR TO MONTH | DAY TO SECOND ;
```

#### Reference Value Types

- Graph Reference: `ANY PROPERTY? GRAPH` (Open type) / `PROPERTY? GRAPH { ... }` (Closed type)
- Binding Table Reference: `BINDING? TABLE { ... }`
- Node Reference: `ANY? NODE` / Node type specification
- Edge Reference: `ANY? EDGE` / Edge type specification

#### Non-Material Value Types

- `NULL` (null type)
- `NULL NOT NULL` / `NOTHING` (Empty type — a type that holds no values)

### 3.2 Constructed Types

#### Path Type

```
pathValueType : PATH notNull? ;
```

#### List Type (Array Type)

```
(LIST | ARRAY) <valueType> [maxLength]
valueType (LIST | ARRAY) [maxLength]
```

#### Record Type

```
recordType
    : ANY? RECORD notNull?
    | RECORD? LEFT_BRACE fieldTypeList? RIGHT_BRACE notNull?
    ;
```

### 3.3 Dynamic Union Types

```
ANY VALUE? notNull?                                          -- Open type
ANY? PROPERTY VALUE notNull?                                 -- Property value type
ANY VALUE? <valueType (| valueType)*>                       -- Closed type
valueType | valueType                                        -- Closed type (infix)
```

### 3.4 NOT NULL Modifier

The `NOT NULL` modifier can be applied to any type. Types with `NOT NULL` do not allow null values.

### 3.5 Typing Operator

```
typed : DOUBLE_COLON | TYPED ;
```

Typing is performed using the `::` operator or the `TYPED` keyword.

---

## 4. Program Structure

### 4.1 Transaction Activity

```
transactionActivity
    : startTransactionCommand (procedureSpecification endTransactionCommand?)?
    | procedureSpecification endTransactionCommand?
    | endTransactionCommand
    ;
```

A GQL program executes procedure specifications within an implicit or explicit transaction. A procedure specification consists of multiple statements chained with the `NEXT` keyword.

### 4.2 Procedure Specification (§9)

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

`NEXT` is a pipeline operator that passes the results of the previous statement to the subsequent statement. The `YIELD` clause can be used to filter intermediate pipeline results.

---

## 5. Session Management (§7)

### 5.1 Setting Session (SESSION SET)

```
sessionSetCommand
    : SESSION SET (sessionSetSchemaClause
                 | sessionSetGraphClause
                 | sessionSetTimeZoneClause
                 | sessionSetParameterClause)
    ;
```

Configurable items:

- **Schema**: `SESSION SET SCHEMA <schemaReference>`
- **Graph**: `SESSION SET [PROPERTY] GRAPH <graphExpression>`
- **Time Zone**: `SESSION SET TIME ZONE <timeZoneString>`
- **Parameters**: Setting graph, binding table, or value parameters.

### 5.2 Resetting Session (SESSION RESET)

```
sessionResetCommand : SESSION RESET sessionResetArguments? ;

sessionResetArguments
    : ALL? (PARAMETERS | CHARACTERISTICS)
    | SCHEMA | PROPERTY? GRAPH | TIME ZONE
    | PARAMETER? sessionParameterSpecification
    ;
```

### 5.3 Closing Session

```
sessionCloseCommand : SESSION CLOSE ;
```

---

## 6. Transaction Management (§8)

```
startTransactionCommand : START TRANSACTION transactionCharacteristics? ;

transactionCharacteristics : transactionMode (COMMA transactionMode)* ;

transactionMode : transactionAccessMode ;

transactionAccessMode : READ ONLY | READ WRITE ;

rollbackCommand : ROLLBACK ;
commitCommand   : COMMIT ;
```

GQL does not support specifying transaction isolation levels but allows for specifying access modes (`READ ONLY` / `READ WRITE`).

---

## 7. Procedures and Statements (§9)

### 7.1 Nested Procedures

```
nestedProcedureSpecification
    : LEFT_BRACE procedureSpecification RIGHT_BRACE ;
```

Procedures can be nested using `{ ... }` and used as subqueries.

### 7.2 Statement Categories

```
statement
    : compositeQueryStatement           -- Read-only queries (including set operations)
    | linearCatalogModifyingStatement   -- DDL (Create/Drop schemas, graphs, graph types)
    | linearDataModifyingStatement      -- DML (INSERT/SET/REMOVE/DELETE)
    ;
```

---

## 8. Variable Definitions (§10)

Binding variables can be declared at the beginning of a pipeline:

```
bindingVariableDefinition
    : graphVariableDefinition           -- PROPERTY GRAPH x = ...
    | bindingTableVariableDefinition    -- BINDING TABLE t = ...
    | valueVariableDefinition           -- VALUE v = ...
    ;
```

Each definition includes a type annotation (`:: type`) and an initializer (`= expression`).

---

## 9. Graph Expressions and Binding Table Expressions (§11)

### 9.1 Graph Expressions

```
graphExpression
    : graphReference                    -- Named graph reference
    | objectExpressionPrimary           -- Derived from expression
    | objectNameOrBindingVariable       -- Binding variable
    | currentGraph                      -- CURRENT_GRAPH / CURRENT_PROPERTY_GRAPH
    ;
```

### 9.2 Binding Table Expressions

```
bindingTableExpression
    : nestedBindingTableQuerySpecification
    | bindingTableReference
    | objectExpressionPrimary
    | objectNameOrBindingVariable
    ;
```

---

## 10. Catalog Modification Statements (§12)

### 10.1 Schema Management

```
createSchemaStatement : CREATE SCHEMA (IF NOT EXISTS)? catalogSchemaParentAndName ;
dropSchemaStatement   : DROP SCHEMA (IF EXISTS)? catalogSchemaParentAndName ;
```

### 10.2 Graph Management

```
createGraphStatement
    : CREATE (PROPERTY? GRAPH (IF NOT EXISTS)? | OR REPLACE PROPERTY? GRAPH)
      catalogGraphParentAndName (openGraphType | ofGraphType) graphSource?
    ;

dropGraphStatement
    : DROP PROPERTY? GRAPH (IF EXISTS)? catalogGraphParentAndName ;
```

Methods for specifying graph types:

- `TYPED ANY [PROPERTY GRAPH]` — Untyped (Open type)
- `LIKE graphExpression` — Copy schema from an existing graph
- `TYPED graphTypeReference` — Named graph type
- `TYPED { elementTypeList }` — Inline graph type definition

### 10.3 Graph Type Management

```
createGraphTypeStatement
    : CREATE (PROPERTY? GRAPH TYPE (IF NOT EXISTS)? | OR REPLACE PROPERTY? GRAPH TYPE)
      catalogGraphTypeParentAndName graphTypeSource
    ;

dropGraphTypeStatement
    : DROP PROPERTY? GRAPH TYPE (IF EXISTS)? catalogGraphTypeParentAndName ;
```

---

## 11. Data Modification Statements (§13)

### 11.1 General Structure

```
linearDataModifyingStatement
    : focusedLinearDataModifyingStatement     -- With USE graph
    | ambientLinearDataModifyingStatement      -- On the current graph
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

### 11.2 INSERT Statement

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

The INSERT pattern creates new elements using ASCII-art format including nodes and edges:

```gql
INSERT (n:Person {name: "Alice", age: 30})
INSERT (a:Person {name: "Alice"})-[:KNOWS {since: 2020}]->(b:Person {name: "Bob"})
```

Specify labels and properties using `insertElementPatternFiller`:

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

### 11.3 SET Statement

```
setStatement : SET setItemList ;

setItem
    : setPropertyItem          -- v.prop = expr
    | setAllPropertiesItem     -- v = { key: value, ... }
    | setLabelItem             -- v IS Label / v:Label
    ;
```

Updating properties:

```gql
SET n.age = 31, n.status = "active"
```

Replacing all properties:

```gql
SET n = { name: "Alice", age: 31 }
```

Adding labels:

```gql
SET n IS Employee
SET n:Employee
```

### 11.4 REMOVE Statement

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

### 11.5 DELETE Statement

```
deleteStatement : (DETACH | NODETACH)? DELETE deleteItemList ;
```

- `DELETE v` — Delete a node or edge (Returns an error if connecting edges exist)
- `DETACH DELETE v` — Delete a node and all its connecting edges at once
- `NODETACH DELETE v` — Force an error if connecting edges exist explicitly

---

## 12. Query Statements (§14)

### 12.1 Composite Query

```
compositeQueryExpression
    : compositeQueryExpression queryConjunction compositeQueryPrimary
    | compositeQueryPrimary
    ;

queryConjunction
    : setOperator     -- UNION / EXCEPT / INTERSECT
    | OTHERWISE        -- Fallback if the left side is empty
    ;

setOperator
    : UNION setQuantifier?       -- UNION [ALL|DISTINCT]
    | EXCEPT setQuantifier?      -- EXCEPT [ALL|DISTINCT]
    | INTERSECT setQuantifier?   -- INTERSECT [ALL|DISTINCT]
    ;
```

Set operators:

- `UNION` — Union (defaults to `DISTINCT`)
- `UNION ALL` — Union preserving duplicates
- `EXCEPT` — Difference
- `INTERSECT` — Intersection
- `OTHERWISE` — Returns the right side if the left result is empty

### 12.2 Linear Query Statement

```
linearQueryStatement
    : focusedLinearQueryStatement    -- USE graph + query
    | ambientLinearQueryStatement    -- Query on the current graph
    ;
```

### 12.3 MATCH Statement (§14.4)

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

`OPTIONAL MATCH` is similar to `LEFT OUTER JOIN` in SQL. If the pattern does not match, null values are bound.

### 12.4 FILTER Statement (§14.6)

```
filterStatement : FILTER (whereClause | searchCondition) ;
```

Provides predicate filtering equivalent to a `WHERE` clause as an independent statement.

### 12.5 LET Statement (§14.7)

```
letStatement : LET letVariableDefinitionList ;

letVariableDefinition
    : valueVariableDefinition
    | bindingVariable EQUALS_OPERATOR valueExpression
    ;
```

Adds new computed columns to the binding table:

```gql
LET x = a.salary * 1.1
```

### 12.6 FOR Statement (§14.8)

```
forStatement : FOR forItem forOrdinalityOrOffset? ;

forItem : forItemAlias forItemSource ;
forItemAlias : bindingVariable IN ;
forItemSource : valueExpression ;

forOrdinalityOrOffset : WITH (ORDINALITY | OFFSET) bindingVariable ;
```

List expansion:

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

### 12.8 RETURN Statement (§14.10–14.11)

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

RETURN formats:

- `RETURN *` — Returns all binding variables
- `RETURN DISTINCT a.name, b.age` — Projection with deduplication
- `RETURN a.name AS name, COUNT(*) AS cnt GROUP BY a.name` — Aggregation

`FINISH` is used as a result statement for data modification, ending the pipeline without returning output.

### 12.9 SELECT Statement (§14.12)

SQL-compatible `SELECT` syntax is also provided:

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

emptyGroupingSet : LEFT_PAREN RIGHT_PAREN ;  -- () aggregates all rows into a single group
```

### 12.11 YIELD Clause (§16.14)

```
yieldClause : YIELD yieldItemList ;
yieldItem : yieldItemName yieldItemAlias? ;
yieldItemAlias : AS bindingVariable ;
```

Restricts the columns of the binding table passed between pipeline steps.

---

## 13. Procedure Call (§15)

### 13.1 Call Statement

```
callProcedureStatement : OPTIONAL? CALL procedureCall ;

procedureCall
    : inlineProcedureCall     -- Anonymous inline procedure
    | namedProcedureCall      -- Named procedure
    ;
```

### 13.2 Inline Procedure

```
inlineProcedureCall
    : variableScopeClause? nestedProcedureSpecification ;

variableScopeClause
    : LEFT_PAREN bindingVariableReferenceList? RIGHT_PAREN ;
```

Restricts binding variables passed to the subquery using a scope clause:

```gql
CALL (a, b) { MATCH (a)-[:KNOWS]->(c) RETURN c }
```

### 13.3 Named Procedure

```
namedProcedureCall
    : procedureReference LEFT_PAREN procedureArgumentList? RIGHT_PAREN yieldClause?
    ;
```

```gql
CALL myProcedure(arg1, arg2) YIELD col1, col2
```

---

## 14. Graph Pattern Matching (§16)

The core feature of GQL. Patterns describe node and edge structures using ASCII-art-style syntax.

### 14.1 Overall Graph Pattern (§16.4)

```
graphPattern
    : matchMode? pathPatternList keepClause? graphPatternWhereClause?
    ;
```

#### Match Mode

```
matchMode
    : repeatableElementsMatchMode    -- REPEATABLE ELEMENT[S] [BINDINGS]
    | differentEdgesMatchMode        -- DIFFERENT EDGE[S] [BINDINGS]
    ;
```

- `REPEATABLE ELEMENTS`: The same node or edge can appear multiple times.
- `DIFFERENT EDGES` (Default): Reusing the same edge is prohibited.

#### KEEP Clause

```
keepClause : KEEP pathPatternPrefix ;
```

Specifies how paths are deduplicated.

#### WHERE Clause

```
graphPatternWhereClause : WHERE searchCondition ;
```

A WHERE clause inside a pattern acts as a filter predicate on the results of the pattern match.

### 14.2 Path Pattern (§16.4)

```
pathPattern
    : pathVariableDeclaration? pathPatternPrefix? pathPatternExpression
    ;

pathVariableDeclaration : pathVariable EQUALS_OPERATOR ;
```

Declaring path variables:

```gql
p = MATCH (a)-[e*1..5]->(b)  -- Binds the entire path to p
```

### 14.3 Path Pattern Prefix (§16.6)

#### Path Mode

```
pathMode : WALK | TRAIL | SIMPLE | ACYCLIC ;
```

| Mode      | Constraint                                               |
| --------- | -------------------------------------------------------- |
| `WALK`    | No constraints (Nodes and edges can be revisited).       |
| `TRAIL`   | The same edge cannot be revisited.                       |
| `SIMPLE`  | The same node cannot be revisited (except start/end).    |
| `ACYCLIC` | The same node cannot be revisited (including start/end). |

#### Path Search Prefix

```
pathSearchPrefix
    : allPathSearch
    | anyPathSearch
    | shortestPathSearch
    ;
```

| Prefix                                 | Meaning                                             |
| -------------------------------------- | --------------------------------------------------- |
| `ALL [mode] PATH[S]`                   | Returns all paths.                                  |
| `ANY [n] [mode] PATH[S]`               | Returns any n paths.                                |
| `ANY SHORTEST [mode] PATH[S]`          | Returns any one shortest path.                      |
| `ALL SHORTEST [mode] PATH[S]`          | Returns all shortest paths.                         |
| `SHORTEST n [mode] PATH[S]`            | Returns the shortest n paths.                       |
| `SHORTEST [n] [mode] PATH[S] GROUP[S]` | Groups shortest paths for each start/end node pair. |

### 14.4 Path Pattern Expression (§16.7)

```
pathPatternExpression
    : pathTerm                                              -- Single path term
    | pathTerm (MULTISET_ALTERNATION_OPERATOR pathTerm)+    -- Multiset union
    | pathTerm (VERTICAL_BAR pathTerm)+                     -- Pattern union
    ;

pathTerm : pathFactor+ ;

pathFactor
    : pathPrimary
    | pathPrimary graphPatternQuantifier    -- Quantified pattern
    | pathPrimary QUESTION_MARK            -- Optional pattern
    ;

pathPrimary
    : elementPattern
    | parenthesizedPathPatternExpression
    | simplifiedPathPatternExpression
    ;
```

### 14.5 Element Pattern

#### Node Pattern

```
nodePattern : LEFT_PAREN elementPatternFiller RIGHT_PAREN ;

elementPatternFiller
    : elementVariableDeclaration? isLabelExpression? elementPatternPredicate?
    ;
```

Examples:

```
()                  -- Any node
(a)                 -- Bind to variable a
(a:Person)          -- Node with label Person
(:Person {age: 30}) -- Label + property filter
(a:Person WHERE a.age > 25)  -- With WHERE predicate
```

#### Edge Pattern

```
edgePattern : fullEdgePattern | abbreviatedEdgePattern ;
```

**Full Edge Pattern (Directed)**:

| Syntax          | Direction                       |
| --------------- | ------------------------------- |
| `<-[e:Label]-`  | Pointing left (Incoming edge)   |
| `-[e:Label]->`  | Pointing right (Outgoing edge)  |
| `~[e:Label]~`   | Undirected                      |
| `<~[e:Label]~`  | Pointing left or Undirected     |
| `~[e:Label]~>`  | Undirected or Pointing right    |
| `<-[e:Label]->` | Pointing left or Pointing right |
| `-[e:Label]-`   | Any direction                   |

**Abbreviated Edge Pattern**:

| Syntax | Direction                       |
| ------ | ------------------------------- |
| `<-`   | Pointing left                   |
| `->`   | Pointing right                  |
| `~`    | Undirected                      |
| `<~`   | Pointing left or Undirected     |
| `~>`   | Undirected or Pointing right    |
| `<->`  | Pointing left or Pointing right |
| `-`    | Any direction                   |

### 14.6 Label Expression (§16.8)

```
labelExpression
    : EXCLAMATION_MARK labelExpression          -- Negation
    | labelExpression AMPERSAND labelExpression  -- Logical AND
    | labelExpression VERTICAL_BAR labelExpression -- Logical OR
    | labelName                                  -- Label name
    | PERCENT                                    -- Wildcard (Any label)
    | LEFT_PAREN labelExpression RIGHT_PAREN     -- Grouping
    ;
```

Examples:

```
:Person                    -- Has label Person
:Person&Employee           -- Person AND Employee
:Person|Company            -- Person OR Company
:!Deleted                  -- Does not have label Deleted
:%                         -- Any label
```

### 14.7 Graph Pattern Quantifiers (§16.11)

```
graphPatternQuantifier
    : ASTERISK                      -- 0 or more times ({0,})
    | PLUS_SIGN                     -- 1 or more times ({1,})
    | fixedQuantifier               -- Fixed count {n}
    | generalQuantifier             -- Range {min, max}
    ;

fixedQuantifier : LEFT_BRACE unsignedInteger RIGHT_BRACE ;
generalQuantifier : LEFT_BRACE lowerBound? COMMA upperBound? RIGHT_BRACE ;
```

Examples:

```
(a)-[:KNOWS]->{2}(b)          -- Exactly 2 hops
(a)-[:KNOWS]->{1,5}(b)        -- 1 to 5 hops
(a)-[:KNOWS]->*(b)             -- 0 or more hops
(a)-[:KNOWS]->+(b)             -- 1 or more hops
(a)(-[:KNOWS]->()){1,3}(b)    -- Parenthesized quantification
```

### 14.8 Parenthesized Path Patterns (§16.7)

```
parenthesizedPathPatternExpression
    : LEFT_PAREN subpathVariableDeclaration? pathModePrefix?
      pathPatternExpression parenthesizedPathPatternWhereClause? RIGHT_PAREN
    ;
```

Binds variables to subpaths and allows filtering via WHERE:

```gql
MATCH (a)(sub = -[:KNOWS]->(x) WHERE x.age > 20){1,3}(b)
```

### 14.9 Simplified Path Patterns (§16.12)

A syntax for concisely describing edge direction and labels:

```
simplifiedDefaultingRight : MINUS_SLASH simplifiedContents SLASH_MINUS_RIGHT ;
-- -/Label/->
```

| Syntax       | Meaning                          |
| ------------ | -------------------------------- |
| `-/Label/->` | Right-pointing edge (with label) |
| `<-/Label/-` | Left-pointing edge               |
| `~/Label/~`  | Undirected edge                  |
| `-/Label/-`  | Any direction                    |

Logical operations for label expressions and quantifiers can also be used within simplified paths.

### 14.10 INSERT Graph Patterns (§16.5)

```
insertGraphPattern : insertPathPatternList ;

insertPathPattern : insertNodePattern (insertEdgePattern insertNodePattern)* ;
```

Pattern syntax specific to INSERT. Unlike MATCH patterns, this uses label set specifications instead of label expressions.

---

## 15. Catalog References (§17)

### 15.1 Schema References

```
schemaReference
    : absoluteCatalogSchemaReference    -- /catalog/schema
    | relativeCatalogSchemaReference    -- ../schema, HOME_SCHEMA, CURRENT_SCHEMA
    | referenceParameterSpecification   -- $param
    ;
```

### 15.2 Graph References

```
graphReference
    : catalogObjectParentReference graphName
    | delimitedGraphName
    | homeGraph                         -- HOME_GRAPH / HOME_PROPERTY_GRAPH
    | referenceParameterSpecification
    ;
```

### 15.3 USE GRAPH Clause (§16.2)

```
useGraphClause : USE graphExpression ;
```

Specifies the target graph for the query:

```gql
USE socialGraph MATCH (a:Person)-[:KNOWS]->(b) RETURN a, b
```

---

## 16. Graph Type Definitions (§18)

### 16.1 Node Type Specifications (§18.2)

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

`NODE` and `VERTEX` are synonyms.

### 16.2 Edge Type Specifications (§18.3)

```
edgeTypePattern
    : (edgeKind? edgeSynonym TYPE? edgeTypeName)?
      (edgeTypePatternDirected | edgeTypePatternUndirected)
    ;
```

Edge types specify connected node types via `sourceNodeTypeReference` and `destinationNodeTypeReference`:

```gql
CREATE GRAPH TYPE socialSchema {
  (Person :Person {name :: STRING NOT NULL, age :: INT}),
  (:Person)-[:KNOWS {since :: INT}]->(:Person)
}
```

### 16.3 Label Set Specifications (§18.4)

```
labelSetSpecification : labelName (AMPERSAND labelName)* ;

labelSetPhrase
    : LABEL labelName
    | LABELS labelSetSpecification
    | isOrColon labelSetSpecification
    ;
```

### 16.4 Property Type Specifications (§18.5–18.6)

```
propertyTypesSpecification : LEFT_BRACE propertyTypeList? RIGHT_BRACE ;
propertyType : propertyName typed? propertyValueType ;
```

---

## 17. Search Conditions and Predicates (§19)

### 17.1 Search Conditions

```
searchCondition : booleanValueExpression ;
```

A search condition is a boolean value expression composed of the following predicates.

### 17.2 List of Predicates

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

#### Comparison Predicates (§19.3)

Defined as infix operators for value expressions:

| Operator | Meaning                  |
| -------- | ------------------------ |
| `=`      | Equal                    |
| `<>`     | Not equal                |
| `<`      | Less than                |
| `>`      | Greater than             |
| `<=`     | Less than or equal to    |
| `>=`     | Greater than or equal to |

#### EXISTS Predicate (§19.4)

```
existsPredicate
    : EXISTS (LEFT_BRACE graphPattern RIGHT_BRACE
            | LEFT_PAREN graphPattern RIGHT_PAREN
            | nestedQuerySpecification)
    ;
```

Tests whether a subpattern or subquery returns at least one row.

#### NULL Predicate (§19.5)

```
nullPredicatePart2 : IS NOT? NULL ;
```

```gql
WHERE a.email IS NOT NULL
```

#### Type Predicate (§19.6)

```
valueTypePredicatePart2 : IS NOT? typed valueType ;
```

Tests the dynamic type of a value:

```gql
WHERE a.data IS :: INT
```

#### Directed Predicate (§19.8)

```
directedPredicatePart2 : IS NOT? DIRECTED ;
```

Tests whether an edge is directed.

#### Label Predicate (§19.9)

```
labeledPredicatePart2 : IS NOT? LABELED labelExpression ;
```

```gql
WHERE a IS LABELED Person & Employee
```

#### Source/Destination Predicates (§19.10)

```
sourcePredicatePart2 : IS NOT? SOURCE OF edgeReference ;
destinationPredicatePart2 : IS NOT? DESTINATION OF edgeReference ;
```

#### ALL_DIFFERENT Predicate (§19.11)

```
all_differentPredicate
    : ALL_DIFFERENT LEFT_PAREN elem (COMMA elem)+ RIGHT_PAREN ;
```

Tests whether all specified element variables refer to distinct graph elements.

#### SAME Predicate (§19.12)

```
samePredicate
    : SAME LEFT_PAREN elem (COMMA elem)+ RIGHT_PAREN ;
```

Tests whether all specified element variables refer to the same graph element.

#### PROPERTY_EXISTS Predicate (§19.13)

```
property_existsPredicate
    : PROPERTY_EXISTS LEFT_PAREN elementRef COMMA propertyName RIGHT_PAREN ;
```

Tests whether an element possesses a given property.

---

## 18. Value Expressions (§20)

### 18.1 Overview of Value Expressions (§20.1)

```
valueExpression
    : sign valueExpression                           -- Signed (+/-)
    | valueExpression (* | /) valueExpression        -- Multiplication/Division
    | valueExpression (+ | -) valueExpression        -- Addition/Subtraction
    | valueExpression || valueExpression              -- Concatenation
    | valueExpression compOp valueExpression          -- Comparison
    | NOT valueExpression                             -- Logical NOT
    | valueExpression IS NOT? truthValue              -- Truth Test
    | valueExpression AND valueExpression             -- Logical AND
    | valueExpression (OR | XOR) valueExpression      -- Logical OR / Exclusive OR
    | predicate                                       -- Predicate
    | valueFunction                                   -- Value Function
    | valueExpressionPrimary                          -- Primary Value Expression
    ;
```

Operator Precedence (High → Low):

1. Unary `+`, `-`
2. `*`, `/`
3. `+`, `-`
4. `||` (Concatenation)
5. Comparison operators (`=`, `<>`, `<`, `>`, `<=` , `>=`)
6. `NOT`
7. `IS [NOT] truthValue`
8. `AND`
9. `OR`, `XOR`

### 18.2 Primary Value Expressions (§20.2)

```
valueExpressionPrimary
    : parenthesizedValueExpression       -- (expr)
    | aggregateFunction                  -- COUNT(*), SUM(x), ...
    | unsignedValueSpecification         -- Literals, Parameters
    | pathValueConstructor               -- PATH [...]
    | valueExpressionPrimary.propertyName  -- Property Access
    | valueQueryExpression               -- VALUE { query }
    | caseExpression                     -- CASE...END
    | castSpecification                  -- CAST(expr AS type)
    | element_idFunction                 -- ELEMENT_ID(var)
    | letValueExpression                 -- LET ... IN ... END
    | bindingVariableReference           -- Variable Reference
    ;
```

### 18.3 CASE Expressions (§20.7)

```
-- Simple CASE
CASE operand
    WHEN value1 THEN result1
    WHEN value2 THEN result2
    ELSE default_result
END

-- Searched CASE
CASE
    WHEN condition1 THEN result1
    WHEN condition2 THEN result2
    ELSE default_result
END

-- Abbreviated Forms
NULLIF(expr1, expr2)         -- NULL if expr1 = expr2, else expr1
COALESCE(expr1, expr2, ...)  -- Returns the first non-NULL value
```

### 18.4 CAST Expressions (§20.8)

```
castSpecification : CAST LEFT_PAREN castOperand AS castTarget RIGHT_PAREN ;
```

### 18.5 Aggregate Functions (§20.9)

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

| Function                      | Description                   |
| ----------------------------- | ----------------------------- |
| `COUNT(*)`                    | Counts the number of rows     |
| `COUNT([DISTINCT] expr)`      | Counts non-NULL values        |
| `SUM([DISTINCT] expr)`        | Sum of values                 |
| `AVG([DISTINCT] expr)`        | Average of values             |
| `MIN(expr)`                   | Minimum value                 |
| `MAX(expr)`                   | Maximum value                 |
| `COLLECT_LIST(expr)`          | Aggregates into a list        |
| `STDDEV_SAMP(expr)`           | Sample standard deviation     |
| `STDDEV_POP(expr)`            | Population standard deviation |
| `PERCENTILE_CONT(expr, rank)` | Continuous percentile         |
| `PERCENTILE_DISC(expr, rank)` | Discrete percentile           |

### 18.6 Numeric Functions (§20.22)

| Function                             | Description                     |
| ------------------------------------ | ------------------------------- |
| `ABS(x)`                             | Absolute value                  |
| `FLOOR(x)`                           | Floor function                  |
| `CEIL(x)` / `CEILING(x)`             | Ceiling function                |
| `MOD(x, y)`                          | Modulo (remainder)              |
| `SQRT(x)`                            | Square root                     |
| `POWER(x, y)`                        | Exponentiation                  |
| `EXP(x)`                             | Exponential function            |
| `LN(x)`                              | Natural logarithm               |
| `LOG(base, x)`                       | General logarithm               |
| `LOG10(x)`                           | Common logarithm                |
| `SIN/COS/TAN/COT(x)`                 | Trigonometric functions         |
| `SINH/COSH/TANH(x)`                  | Hyperbolic functions            |
| `ASIN/ACOS/ATAN(x)`                  | Inverse trigonometric functions |
| `DEGREES(x)`                         | Radians to degrees              |
| `RADIANS(x)`                         | Degrees to radians              |
| `CHAR_LENGTH(s)`                     | Character count                 |
| `BYTE_LENGTH(s)` / `OCTET_LENGTH(s)` | Byte length                     |
| `PATH_LENGTH(p)`                     | Number of edges in a path       |
| `CARDINALITY(x)` / `SIZE(list)`      | Number of elements              |

### 18.7 String Functions (§20.24)

| Function                         | Description                               |
| -------------------------------- | ----------------------------------------- |
| `LEFT(s, n)` / `RIGHT(s, n)`     | First/last n characters                   |
| `TRIM(s)`                        | Removes whitespace                        |
| `BTRIM/LTRIM/RTRIM(s [, chars])` | Both/Left/Right trim                      |
| `UPPER(s)` / `LOWER(s)`          | Case conversion                           |
| `NORMALIZE(s [, form])`          | Unicode normalization (NFC/NFD/NFKC/NFKD) |

### 18.8 Date and Time Functions (§20.27)

| Function/Constant                    | Description                                        |
| ------------------------------------ | -------------------------------------------------- |
| `CURRENT_DATE`                       | Current date                                       |
| `CURRENT_TIME`                       | Current time with time zone                        |
| `CURRENT_TIMESTAMP`                  | Current timestamp with time zone                   |
| `LOCAL_TIME`                         | Current local time                                 |
| `LOCAL_TIMESTAMP` / `LOCAL_DATETIME` | Current local date and time                        |
| `DATE(string)`                       | Constructs a date from a string                    |
| `ZONED_TIME(string)`                 | Constructs a time with time zone from a string     |
| `ZONED_DATETIME(string)`             | Constructs a datetime with time zone from a string |
| `DURATION_BETWEEN(dt1, dt2)`         | Duration between two date/times                    |

### 18.9 List Functions and Constructors (§20.15–20.17)

```
-- List Literals
[1, 2, 3]
LIST[1, 2, 3]

-- ELEMENTS Function
ELEMENTS(pathExpression)  -- Converts a path into a list of elements

-- TRIM Function (for Lists)
TRIM(list, n)  -- Trims list to n elements
```

### 18.10 Record Constructors (§20.18)

```
{key1: value1, key2: value2}
RECORD {key1: value1, key2: value2}
```

### 18.11 Path Constructors (§20.14)

```
PATH [node1, edge1, node2, edge2, node3]
```

### 18.12 Dynamic Parameters (§20.4)

```
dynamicParameterSpecification : GENERAL_PARAMETER_REFERENCE ;  -- $param
```

### 18.13 ELEMENT_ID Function (§20.10)

```
element_idFunction : ELEMENT_ID LEFT_PAREN elementVariableReference RIGHT_PAREN ;
```

Returns an implementation-defined ID for a node or edge.

### 18.13a `caller()` Function (Gleaph Extension)

```
callerFunction : CALLER LEFT_PAREN RIGHT_PAREN ;
```

Returns the IC caller principal as a `Principal` value. No arguments. Returns `NULL` if no caller is injected (e.g. in native tests without `set_caller`).

See `design/gleaph-extensions.md` §3.2 for details.

### 18.14 LET Value Expressions (§20.5)

```
letValueExpression : LET letVariableDefinitionList IN valueExpression END ;
```

Defines local variables for use within an expression:

```gql
LET x = a.salary * 0.1 IN a.salary + x END
```

### 18.15 VALUE Subqueries (§20.6)

```
valueQueryExpression : VALUE nestedQuerySpecification ;
```

Uses the result of a subquery as a scalar value:

```gql
VALUE { MATCH (n:Config) RETURN n.maxRetries }
```

---

## 19. Names, Variables, and Literals (§21)

### 19.1 Identifiers

```
identifier
    : regularIdentifier                    -- ASCII alphanumeric + underscore
    | DOUBLE_QUOTED_CHARACTER_SEQUENCE      -- "delimited identifier"
    | ACCENT_QUOTED_CHARACTER_SEQUENCE      -- `accent quoted`
    ;

regularIdentifier : REGULAR_IDENTIFIER | nonReservedWords ;
```

- Regular identifiers start with a letter or underscore, followed by alphanumeric characters or underscores.
- Identifiers matching reserved words must be delimited with quotes.
- Identifiers are case-insensitive.

### 19.2 Literals

```
unsignedLiteral
    : unsignedNumericLiteral    -- 42, 3.14, 1E10
    | generalLiteral            -- TRUE, 'text', NULL, [1,2], {k:v}, DATE '...'
    ;
```

#### Numeric Literals

| Format                     | Example          |
| -------------------------- | ---------------- |
| Decimal Integer            | `42`, `1_000`    |
| Hexadecimal                | `0x2A`           |
| Octal                      | `0o52`           |
| Binary                     | `0b101010`       |
| Decimal                    | `3.14`           |
| Scientific Notation        | `1.5E10`         |
| Exact Numeric Suffix       | `42M`            |
| Approximate Numeric Suffix | `3.14F`, `3.14D` |

#### Boolean Literals

```
BOOLEAN_LITERAL : TRUE | FALSE | UNKNOWN ;
```

#### String Literals

```
'single quoted'     -- Single quotes
"double quoted"     -- Double quotes
```

Escape sequences: `\\`, `\'`, `\"`, `` \` ``, `\t`, `\b`, `\n`, `\r`, `\f`, `\uXXXX`, `\UXXXXXX`

The `@` prefix disables escaping: `@'raw string'`

#### Byte String Literals

```
X'48656C6C6F'    -- Hexadecimal byte string
```

#### Temporal Literals

```
DATE '2024-01-15'
TIME '14:30:00+09:00'
DATETIME '2024-01-15T14:30:00'
TIMESTAMP '2024-01-15T14:30:00Z'
DURATION 'P1Y6M'
```

#### NULL Literal

```
NULL
```

#### List Literals

```
[1, 2, 3]
LIST [1, 2, 3]
```

#### Record Literals

```
{name: "Alice", age: 30}
```

### 19.3 Reserved Words

GQL has approximately 200 reserved words. Key categories include:

**Query Syntax**: `MATCH`, `WHERE`, `RETURN`, `ORDER`, `BY`, `LIMIT`, `OFFSET`, `SKIP`, `WITH`, `UNION`, `EXCEPT`, `INTERSECT`, `SELECT`, `FROM`, `GROUP`, `HAVING`, `DISTINCT`, `ALL`, `AS`, `ASC`, `DESC`, `FILTER`, `FOR`, `IN`, `LET`, `OPTIONAL`, `OTHERWISE`, `FINISH`, `YIELD`, `NEXT`, `USE`

**DML**: `CREATE`, `INSERT`, `SET`, `REMOVE`, `DELETE`, `DETACH`, `NODETACH`

**DDL**: `DROP`, `SCHEMA`, `GRAPH`, `TYPE`, `REPLACE`, `COPY`, `LIKE`, `IF`, `EXISTS`, `NOT`

**Logic**: `AND`, `OR`, `XOR`, `NOT`, `IS`, `TRUE`, `FALSE`, `NULL`, `UNKNOWN`

**Aggregation**: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `COLLECT_LIST`, `STDDEV_SAMP`, `STDDEV_POP`, `PERCENTILE_CONT`, `PERCENTILE_DISC`

**Types**: `BOOL`, `BOOLEAN`, `INT`, `INTEGER`, `FLOAT`, `DOUBLE`, `STRING`, `CHAR`, `VARCHAR`, `BYTES`, `BINARY`, `VARBINARY`, `DECIMAL`, `DEC`, `REAL`, `DATE`, `TIME`, `TIMESTAMP`, `DATETIME`, `DURATION`, `PATH`, `LIST`, `ARRAY`, `RECORD`, `ANY`, `VALUE`, `PROPERTY`, `NOTHING`, `TYPED`

**Path**: `WALK`, `TRAIL`, `SIMPLE`, `ACYCLIC`, `SHORTEST`, `PATHS`, `PATH`

**Session/Transaction**: `SESSION`, `CLOSE`, `RESET`, `START`, `TRANSACTION`, `COMMIT`, `ROLLBACK`, `READ`, `WRITE`

**Predicates**: `SAME`, `ALL_DIFFERENT`, `PROPERTY_EXISTS`, `LABELED`, `DIRECTED`, `SOURCE`, `DESTINATION`, `NORMALIZED`

#### Gleaph Reserved Keywords

Gleaph's lexer (`lexer.rs`) defines the following set of reserved words. Any identifier matching these must be wrapped in backticks:

`MATCH`, `WHERE`, `RETURN`, `ORDER`, `BY`, `LIMIT`, `CREATE`, `DELETE`, `SET`, `REMOVE`, `OPTIONAL`, `OR`, `NOT`, `XOR`, `IN`, `IS`, `DISTINCT`, `CASE`, `WHEN`, `THEN`, `ELSE`, `END`, `DETACH`, `EXISTS`, `WITH`, `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `COLLECT`, `COALESCE`, `NULLIF`, `GROUP`, `HAVING`, `OFFSET`, `SKIP`, `UNION`, `ALL`, `EXCEPT`, `INTERSECT`, `INSERT`, `LABELS`, `PROPERTIES`, `TYPE`, `ID`, `UPPER`, `LOWER`, `TRIM`, `SUBSTRING`, `SIZE`, `ABS`, `FLOOR`, `CEIL`, `TOSTRING`, `TOINTEGER`, `TOFLOAT`, `ANY`, `SHORTEST`, `PATH`, `AND`, `AS`, `ASC`, `DESC`, `TRUE`, `FALSE`, `NULL`

Several of these are common choices for property names or edge labels in real-world graphs (e.g. `min`, `max`, `group`, `order`, `end`, `skip`, `all`, `type`, `id`, `size`, `path`). When used as identifiers, backtick escaping is required:

```gql
-- Error: reserved keywords used as property names
MATCH (r:Record) WHERE r.min > 0 RETURN r.end

-- Correct: backtick escaping
MATCH (r:Record) WHERE r.`min` > 0 RETURN r.`end`
```

> **Note**: This tension between reserved-word coverage and practical usability is an open issue in the GQL standard community ([opengql/grammar#31](https://github.com/opengql/grammar/issues/31)).

### 19.4 Non-Reserved Words

The following can also be used as identifiers:

`ACYCLIC`, `BINDING`, `BINDINGS`, `CONNECTING`, `DESTINATION`, `DIFFERENT`, `DIRECTED`, `EDGE`, `EDGES`, `ELEMENT`, `ELEMENTS`, `FIRST`, `GRAPH`, `GROUPS`, `KEEP`, `LABEL`, `LABELED`, `LABELS`, `LAST`, `NFC`, `NFD`, `NFKC`, `NFKD`, `NO`, `NODE`, `NORMALIZED`, `ONLY`, `ORDINALITY`, `PROPERTY`, `READ`, `RELATIONSHIP`, `RELATIONSHIPS`, `REPEATABLE`, `SHORTEST`, `SIMPLE`, `SOURCE`, `TABLE`, `TO`, `TRAIL`, `TRANSACTION`, `TYPE`, `UNDIRECTED`, `VERTEX`, `WALK`, `WITHOUT`, `WRITE`, `ZONE`

### 19.5 Synonyms

| Concept        | Synonyms                 |
| -------------- | ------------------------ |
| Node           | `NODE`, `VERTEX`         |
| Edge           | `EDGE`, `RELATIONSHIP`   |
| Edges (Plural) | `EDGES`, `RELATIONSHIPS` |

---

## 20. Gleaph Implementation Mapping

The following table shows the correspondence between GQL specification sections (`reference/grammar/GQL.g4`) and the Gleaph implementation in `crates/gql/`.

### 20.1 Implemented Features

| GQL Feature                    | Spec Section       | Gleaph Implementation                                                                                                             |
| ------------------------------ | ------------------ | --------------------------------------------------------------------------------------------------------------------------------- | --- | ---------- |
| **MATCH statement**            | §14.4              | `parser.rs` — Node/edge patterns, 1–3 hops                                                                                        |
| **OPTIONAL MATCH**             | §14.4              | `parser.rs`, `executor.rs` — LEFT JOIN semantics                                                                                  |
| **WHERE clause**               | §16.13             | `parser.rs` — All comparison operators, AND/OR/NOT/XOR                                                                            |
| **RETURN clause**              | §14.11             | `parser.rs` — Projections, DISTINCT, AS aliases                                                                                   |
| **ORDER BY**                   | §16.16–16.17       | `parser.rs` — ASC/DESC; NULLS FIRST/LAST not supported                                                                            |
| **LIMIT**                      | §16.18             | `parser.rs`, `executor.rs` — Constants only                                                                                       |
| **OFFSET / SKIP**              | §16.19             | `parser.rs` — Both keywords supported                                                                                             |
| **WITH clause**                | — (Cypher-derived) | `parser.rs` — Intermediate projection + subsequent MATCH                                                                          |
| **CREATE (INSERT)**            | §13.2              | `parser.rs` — Node and edge creation                                                                                              |
| **DELETE**                     | §13.5              | `parser.rs` — DETACH DELETE; WHERE is mandatory                                                                                   |
| **SET**                        | §13.3              | `parser.rs` — Property updates, label addition                                                                                    |
| **REMOVE**                     | §13.4              | `parser.rs` — Property removal, label removal                                                                                     |
| **UNION / EXCEPT / INTERSECT** | §14.2              | `parser.rs`, `executor.rs` — Set operations                                                                                       |
| **Aggregate Functions**        | §20.9              | `executor.rs` — COUNT, SUM, AVG, MIN, MAX, COLLECT                                                                                |
| **GROUP BY / HAVING**          | §16.15             | `parser.rs`, `executor.rs`                                                                                                        |
| **CASE expressions**           | §20.7              | `parser.rs` — Searched CASE, NULLIF, COALESCE                                                                                     |
| **IS NULL / IS NOT NULL**      | §19.5              | `parser.rs`, `executor.rs`                                                                                                        |
| **IN lists**                   | —                  | `parser.rs` — `expr IN [v1, v2, ...]`                                                                                             |
| **EXISTS subqueries**          | §19.4              | `parser.rs` — EXISTS { subquery }                                                                                                 |
| **Path variables**             | §16.4              | `parser.rs` — `p = (a)-[*]->(b)`                                                                                                  |
| **Variable-length paths**      | §16.11             | `parser.rs` — `[*1..6]` (max 6)                                                                                                   |
| **ANY SHORTEST PATH**          | §16.6              | `parser.rs`, `executor.rs` — BFS-based                                                                                            |
| **Arithmetic Operators**       | §20.1              | `parser.rs` — `+`, `-`, `*`, `/`, `%`                                                                                             |
| **String Concatenation**       | §20.1              | `parser.rs` — `                                                                                                                   |     | ` operator |
| **Property Access**            | §20.11             | `parser.rs` — `var.property`                                                                                                      |
| **Parameters**                 | §20.4              | `parser.rs` — `$param`                                                                                                            |
| **Built-in Functions**         | §20.22ff           | `executor.rs` — ID, LABELS, TYPE, PROPERTIES, UPPER, LOWER, TRIM, SUBSTRING, SIZE, ABS, FLOOR, CEIL, TOSTRING, TOINTEGER, TOFLOAT |
| **List Literals**              | §20.17             | `parser.rs` — `[1, 2, 3]`                                                                                                         |
| **List Indexing**              | —                  | `parser.rs` — `list[index]`                                                                                                       |
| **PATH_LENGTH**                | §20.22             | `parser.rs` — `PATH_LENGTH(p)`                                                                                                    |

### 20.2 Unimplemented Features

| GQL Feature                               | Spec Section | Remarks                                                                     |
| ----------------------------------------- | ------------ | --------------------------------------------------------------------------- |
| Session Management                        | §7           | GQL standard session/transaction model is incompatible with IC architecture |
| Transaction Management                    | §8           | Replaced by IC consensus model                                              |
| NEXT Pipeline                             | §9.2         | Partially replaced by WITH clause                                           |
| DDL (CREATE/DROP SCHEMA/GRAPH/GRAPH TYPE) | §12          | Managed at IC canister level                                                |
| SELECT statement                          | §14.12       | Replaced by MATCH + RETURN                                                  |
| FILTER statement                          | §14.6        | Replaced by WHERE clause                                                    |
| LET statement                             | §14.7        | Replaceable by WITH clause                                                  |
| FOR statement                             | §14.8        | List unwinding not supported                                                |
| CALL procedure                            | §15          | Stored procedures not supported                                             |
| Path Modes (WALK/TRAIL/SIMPLE/ACYCLIC)    | §16.6        | Default behavior only                                                       |
| Match Modes (REPEATABLE/DIFFERENT)        | §16.4        | Default behavior only                                                       |
| Logic in Label Expressions                | §16.8        | Single label only (AND/OR/NOT not supported)                                |
| Simplified Path Patterns                  | §16.12       | `-/Label/->` format not supported                                           |
| KEEP clause                               | §16.4        | Not supported                                                               |
| Undirected Edges                          | §16.6        | Gleaph supports directed graphs only                                        |
| CAST expressions                          | §20.8        | Type conversion replaced by built-in functions (e.g., TOSTRING)             |
| Temporal Literals                         | §21.2        | Timestamps are integer-based                                                |
| Record Constructors                       | §20.18       | Not supported                                                               |
| Byte String Type                          | §18.9        | Not supported                                                               |
| VALUE Subqueries                          | §20.6        | Not supported                                                               |
| Binary Set Functions                      | §20.9        | PERCENTILE_CONT/DISC not supported                                          |
| Trig/Log Functions                        | §20.22       | Not supported                                                               |
| Unicode Normalization                     | §20.24       | Not supported                                                               |
| YIELD clause                              | §16.14       | Not supported                                                               |
| NULLS FIRST/LAST                          | §16.17       | Default ORDER BY behavior only                                              |

### 20.3 Gleaph-Specific Value Type Mapping

| GQL Type       | Gleaph `Value` enum             |
| -------------- | ------------------------------- |
| BOOLEAN        | `Value::Bool(bool)`             |
| INT / INTEGER  | `Value::Int(i64)`               |
| FLOAT / DOUBLE | `Value::Float(f64)`             |
| STRING         | `Value::Text(String)`           |
| TIMESTAMP      | `Value::Timestamp(u64)`         |
| LIST           | `Value::List(Vec<Value>)`       |
| PATH           | `Value::Path(Vec<PathElement>)` |
| NULL           | `Value::Null`                   |

### 20.4 Gleaph GQL Pipeline

```
String → lexer::tokenize → parser::parse_statement → AST
    → validate::validate_statement → planner::build_plan → PhysicalPlan
    → executor::execute_plan → QueryResult
```

| Stage               | File          | Role                                                        |
| ------------------- | ------------- | ----------------------------------------------------------- |
| Lexer               | `lexer.rs`    | Tokenization (nom-based)                                    |
| Parser              | `parser.rs`   | Token stream → AST (recursive descent, nom combinators)     |
| AST                 | `ast.rs`      | Data structures for Statement, QueryStmt, Expr, etc.        |
| Validator           | `validate.rs` | Semantic validation (scope, type checking, feature gates)   |
| Planner             | `planner.rs`  | Cost-based anchor selection, filter pushdown, join ordering |
| Execution Engine    | `executor.rs` | Volcano model (RowIterator), BFS shortest path              |
| Plan Representation | `plan.rs`     | PlanOp sequence, PlanAnnotations                            |
| Value Comparison    | `value.rs`    | Inter-type comparison logic                                 |
| Statistics          | `stats.rs`    | Cost estimation for the planner                             |

### 20.5 Implementation Constraints

| Constraint                  | Value         | Rationale                                                        |
| --------------------------- | ------------- | ---------------------------------------------------------------- |
| MATCH Hop Count             | 1–3           | Phase 2 feature gate                                             |
| Variable Path Limit         | max 6         | Validation constraint                                            |
| Query String Length         | 16KB          | GQL bridge guardrails                                            |
| RETURN Item Count           | 1 or more     | Required by validation                                           |
| DELETE WHERE                | Mandatory     | Safety measure to prevent unrestricted deletion                  |
| Duplicate Binding Variables | Prohibited    | Detected by validation (reusable in subsequent MATCH after WITH) |
| Property Hints              | Literals only | Property values in CREATE must be literals                       |

---

## Appendix A: Syntax Diagram Legend

| Notation   | Meaning                                             |
| ---------- | --------------------------------------------------- | ------------ |
| `KEYWORD`  | Reserved word (case-insensitive)                    |
| `ruleName` | Reference to a grammar rule                         |
| `( ... )`  | Grouping of patterns                                |
| `[ ... ]`  | Bracket literals (`LEFT_BRACKET` / `RIGHT_BRACKET`) |
| `{ ... }`  | Brace literals (`LEFT_BRACE` / `RIGHT_BRACE`)       |
| `?`        | Optional (0 or 1 time)                              |
| `*`        | Zero or more repetitions                            |
| `+`        | One or more repetitions                             |
| `          | `                                                   | Alternatives |

## Appendix B: Grammar Section Mapping Table

| Grammar Comment | GQL Specification Section        | Specification Section here |
| --------------- | -------------------------------- | -------------------------- |
| §6              | GQL Programs                     | §4                         |
| §7              | Session Management               | §5                         |
| §8              | Transaction Management           | §6                         |
| §9              | Procedure Specifications         | §7                         |
| §10             | Variable Definitions             | §8                         |
| §11             | Graph Expressions                | §9                         |
| §12             | Catalog Modification Statements  | §10                        |
| §13             | Data Modification Statements     | §11                        |
| §14             | Query Statements                 | §12                        |
| §15             | Procedure Calls                  | §13                        |
| §16             | Graph Patterns                   | §14                        |
| §17             | Catalog References               | §15                        |
| §18             | Graph Type Definitions           | §16                        |
| §19             | Search Conditions and Predicates | §17                        |
| §20             | Value Expressions                | §18                        |
| §21             | Names, Variables, and Literals   | §19                        |
