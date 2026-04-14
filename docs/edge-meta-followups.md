# EdgeMeta / Sidecar Follow-ups

`EdgeMeta` の `flags + kind + inline + payload` 再設計後、Gleaph で有効活用するための後続タスク記録。

## Pending Work

### 1. Sidecar 実体設計
- sidecar storage schema 定義
- handle 解決方式 定義
- encode / decode 実装
- flush / hydrate 統合

### 2. `kind` / `inline` の本番意味を投入
- 最初の対象は `weight tier` を第一候補とする
- `kind=2` weighted edge の正式運用を検討
- `inline` 4bit を coarse bucket / tier として利用

### 3. Planner / Executor 活用
- `kind` / `inline` を見た early prune 追加
- sidecar 読み込みが必要な edge だけ cold path へ分岐
- weight / temporal / visibility 系クエリで活用を検討

### 4. Mutation API の sidecar 対応
- edge insert / update 時に inline-only / sidecar-backed を選択可能にする
- sidecar handle の生成 / 更新フローを追加

### 5. Observability / Invariant 検査
- `kind` 別件数
- sidecar 使用率
- decode error
- reserved 値混入検知
- flush / hydrate 一貫性確認

## Recommended Next Slice

最小の次スコープは `weight tier` を 1 ユースケースとして end-to-end 接続すること。

- `kind=2` weighted edge を正式化
- `inline` を weight bucket として保存
- planner / executor で bucket による prefilter を追加
- precise weight が必要な場合だけ sidecar を読む
