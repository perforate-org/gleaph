import { createContext, createMemo, createSignal, type Accessor, type ParentProps, useContext } from "solid-js";
import { resolveTemplate, translator } from "@solid-primitives/i18n";
import type { QueryAnnotation } from "~/data/queryAnnotations";

export const LOCALES = ["en", "ja"] as const;
export type Locale = (typeof LOCALES)[number];

export const LOCALE_NAMES: Record<Locale, string> = {
  en: "English",
  ja: "日本語",
};

const enDictionary = {
  "language.label": "Language",
  "language.menuLabel": "Select language",
  "brand.socialDemo": "Social Demo",
  "notice.anonymousReadOnly": "Anonymous read-only demo",
  "scenario.navigation": "Scenario navigation",
  "scenario.publicTimeline": "Public timeline",
  "scenario.aliceHomeFeed": "Alice home feed",
  "scenario.topicPath": "Topic path",
  "scenario.semanticDiscovery": "Semantic discovery",
  "scenario.aliceSemanticFeed": "Alice semantic feed",
  "scenario.publicTimeline.feedTitle": "Public posts",
  "scenario.publicTimeline.explanationTitle": "Relational baseline",
  "scenario.publicTimeline.rdbSummary":
    "A chronological list of public rows is exactly what an RDB does well: one index on created_at, a simple visibility predicate, and no joins.",
  "scenario.publicTimeline.graphSummary":
    "The graph materializes `IN_PUBLIC_FEED` edges from every public Post to a dedicated Feed vertex at seed time. Gleaph's labeled-edge insertion order is exposed explicitly through `GLEAPH.SEQUENCE(e)`, so a single fixed-label expansion with `LIMIT` returns newest-first without depending on undocumented default scan order. The value is not speed; it is that the same vertex model will later power relationship-aware feeds without a new schema.",
  "scenario.aliceHomeFeed.feedTitle": "Alice's home feed",
  "scenario.aliceHomeFeed.explanationTitle": "Graph traversal",
  "scenario.aliceHomeFeed.rdbSummary":
    "An RDB needs a follows join table and a two-hop join (follower → followee → posts). The query is still SQL-shaped, but the relationship is now a traversal, not a foreign-key lookup.",
  "scenario.aliceHomeFeed.graphSummary":
    "Gleaph materializes `IN_HOME_FEED` edges from every public Post to its author and each follower User at seed time, so Alice's feed is a single bounded expansion from her user vertex and includes her own posts. The feed is ordered explicitly with `GLEAPH.SEQUENCE(e)` on the labeled feed edge, returning newest posts first with a deterministic, declared sort key rather than an implicit scan assumption.",
  "scenario.topicPath.feedTitle": "Topic explanation path",
  "scenario.topicPath.explanationTitle": "Explainable recommendation",
  "scenario.topicPath.rdbSummary":
    "Proving why a post was recommended requires re-running and documenting the joins: follow, post, topic. The answer is scattered across tables.",
  "scenario.topicPath.graphSummary":
    "Gleaph returns the same result plus the edge identities that caused it: Alice follows Bob, Bob authored the post, and that post has the Graph databases topic. The path is part of the query result.",
  "scenario.semanticDiscovery.feedTitle": "Vector-only semantic discovery",
  "scenario.semanticDiscovery.explanationTitle": "Vector retrieval",
  "scenario.semanticDiscovery.rdbSummary":
    "Pure vector search has no join table: it scores every public Post against a fixed query vector and returns the nearest neighbors by L2-squared distance.",
  "scenario.semanticDiscovery.graphSummary":
    "Gleaph stores canonical Post embeddings on the Graph shard and routes vector SEARCH through the derived index. Dave's retrieval note is deliberately nearest even though Alice does not follow Dave.",
  "scenario.aliceSemanticFeed.feedTitle": "Alice's graph-constrained semantic feed",
  "scenario.aliceSemanticFeed.explanationTitle": "Hybrid retrieval",
  "scenario.aliceSemanticFeed.rdbSummary":
    "Combining vector similarity with a relational filter requires joining the vector result set back to followee authorship, usually in application code.",
  "scenario.aliceSemanticFeed.graphSummary":
    "Gleaph applies the same vector SEARCH inside the graph pattern `Alice → FOLLOWS → author → POSTED → Post`. Dave's nearer post is excluded because he is not a followee; posts from Alice's followed authors are returned in semantic order.",
  "feed.loading": "Loading scenario through Gateway…",
  "feed.empty": "No posts returned for this scenario.",
  "feed.errorTitle": "Scenario failed",
  "feed.gatewayNotConfigured":
    "Social demo Gateway canister id is not configured. The asset canister should inject PUBLIC_CANISTER_ID:gleaph-social-demo-gateway, or set VITE_SOCIAL_DEMO_GATEWAY_CANISTER_ID for local development.",
  "feed.noRows": "Gateway returned no rows. The scenario may not be seeded yet.",
  "feed.malformedRows": "Gateway row_count does not match decoded rows. The response may be malformed.",
  "feed.anonymousSubtitle": "Anonymous read-only demo",
  "feed.l2Distance": "L2-squared distance",
  "feed.vectorDistance": "Vector distance",
  "feed.l2DistanceValue": "L2-squared distance: {{value}}",
  "feed.relationshipTrail": "Relationship trail",
  "feed.followerEdge": "Follower edge",
  "feed.authorPostEdge": "Author-post edge",
  "feed.postTopicEdge": "Post-topic edge",
  "feed.edgeOn": "on",
  "feed.edgeToTopic": "to topic",
  "feed.seedLabelsNote": "Labels reflect the fixed social-graph seed. Update them if the seed subject changes.",
  "explanation.graphValueAdd": "Graph value add",
  "explanation.whyResultsDiffer": "Why the results differ",
  "explanation.whyResultsDifferBody":
    "The fixed query vector makes Dave's retrieval note the globally nearest public Post. In the vector-only scenario it appears first. In Alice's graph-constrained feed it is absent because Alice does not follow Dave, even though it is nearer than every followed-author result.",
  "query.title": "GQL query",
  "query.hoverHint": "Hover highlighted parts to see what they do.",
  "query.expand": "Expand query",
  "query.close": "Close",
  "query.formatting": "Formatting query…",
  "query.error.parse": "Query parsing failed: {{message}}",
  "query.error.unsupported": "This query shape is not supported by the formatter: {{message}}",
  "query.error.invalidOptions": "Formatter options are invalid: {{message}}",
  "query.error.adapter": "The query formatter could not run: {{message}}",
  "error.retry": "Retry",
  "date.justNow": "just now",
  "date.yesterday": "yesterday",
  "date.minutesAgo": "{{count}}m ago",
  "date.hoursAgo": "{{count}}h ago",
} as const;

export type TranslationKey = keyof typeof enDictionary;

const jaDictionary: Record<TranslationKey, string> = {
  "language.label": "言語",
  "language.menuLabel": "言語を選択",
  "brand.socialDemo": "ソーシャルデモ",
  "notice.anonymousReadOnly": "匿名・読み取り専用デモ",
  "scenario.navigation": "シナリオナビゲーション",
  "scenario.publicTimeline": "公開タイムライン",
  "scenario.aliceHomeFeed": "アリスのホームフィード",
  "scenario.topicPath": "トピックへの経路",
  "scenario.semanticDiscovery": "意味検索",
  "scenario.aliceSemanticFeed": "アリスの意味フィード",
  "scenario.publicTimeline.feedTitle": "公開投稿",
  "scenario.publicTimeline.explanationTitle": "リレーショナル検索の基準例",
  "scenario.publicTimeline.rdbSummary":
    "公開行の時系列一覧は、created_at のインデックスと単純な公開条件だけで取得できる、RDB が得意とする処理です。結合は必要ありません。",
  "scenario.publicTimeline.graphSummary":
    "シード時に、グラフはすべての公開 Post から専用の Feed 頂点へ `IN_PUBLIC_FEED` エッジを作成します。ラベル付きエッジの挿入順は `GLEAPH.SEQUENCE(e)` で明示的に利用できるため、暗黙の走査順に頼らず、固定ラベルの展開と `LIMIT` だけで新しい投稿から返せます。価値は速度ではなく、同じ頂点モデルを後から関係性のあるフィードにも使える点です。",
  "scenario.aliceHomeFeed.feedTitle": "アリスのホームフィード",
  "scenario.aliceHomeFeed.explanationTitle": "グラフ走査",
  "scenario.aliceHomeFeed.rdbSummary":
    "RDB では follows の結合テーブルと、フォロワー・フォロー先・投稿をたどる2段階の結合が必要です。クエリは SQL に似ていますが、関係は外部キー検索ではなく走査になります。",
  "scenario.aliceHomeFeed.graphSummary":
    "シード時に、Gleaph はすべての公開 Post から作者と各フォロワー User へ `IN_HOME_FEED` エッジを作成します。そのためアリスのフィードはユーザー頂点からの範囲付き展開1回で取得でき、自分の投稿も含みます。ラベル付きフィードエッジの `GLEAPH.SEQUENCE(e)` で順序を明示し、暗黙の走査順に頼らず新しい投稿から返します。",
  "scenario.topicPath.feedTitle": "トピック説明の経路",
  "scenario.topicPath.explanationTitle": "説明可能な推薦",
  "scenario.topicPath.rdbSummary":
    "投稿が推薦された理由を示すには、フォロー・投稿・トピックの結合を再実行して記録する必要があります。答えは複数のテーブルに分散します。",
  "scenario.topicPath.graphSummary":
    "Gleaph は結果に加えて、その理由となったエッジの識別子も返します。アリスがボブをフォローし、ボブが投稿者で、その投稿に Graph databases トピックが付いているという経路自体がクエリ結果になります。",
  "scenario.semanticDiscovery.feedTitle": "ベクトルのみの意味検索",
  "scenario.semanticDiscovery.explanationTitle": "ベクトル検索",
  "scenario.semanticDiscovery.rdbSummary":
    "純粋なベクトル検索には結合テーブルがありません。固定されたクエリベクトルとすべての公開 Post の距離を計算し、L2二乗距離が近い順に返します。",
  "scenario.semanticDiscovery.graphSummary":
    "Gleaph は正規の Post embedding を Graph shard に保持し、ベクトル SEARCH を派生インデックスへルーティングします。アリスがフォローしていないデイブの投稿も、ベクトル距離だけなら最も近い結果として意図的に含まれます。",
  "scenario.aliceSemanticFeed.feedTitle": "アリスのグラフ制約付き意味フィード",
  "scenario.aliceSemanticFeed.explanationTitle": "ハイブリッド検索",
  "scenario.aliceSemanticFeed.rdbSummary":
    "ベクトル類似度とリレーショナルな条件を組み合わせるには、通常アプリケーションコードでベクトル結果をフォロー先の投稿者情報へ再結合する必要があります。",
  "scenario.aliceSemanticFeed.graphSummary":
    "Gleaph は `Alice → FOLLOWS → author → POSTED → Post` というグラフパターンの中で同じベクトル SEARCH を実行します。デイブはアリスのフォロー先ではないため、距離が近くても除外され、フォロー先の投稿だけが意味順に返ります。",
  "feed.loading": "Gateway からシナリオを読み込んでいます…",
  "feed.empty": "このシナリオでは投稿が返されませんでした。",
  "feed.errorTitle": "シナリオに失敗しました",
  "feed.gatewayNotConfigured":
    "ソーシャルデモの Gateway canister ID が設定されていません。アセット canister から PUBLIC_CANISTER_ID:gleaph-social-demo-gateway を注入するか、ローカル開発では VITE_SOCIAL_DEMO_GATEWAY_CANISTER_ID を設定してください。",
  "feed.noRows": "Gateway から行が返されませんでした。シナリオのシードがまだ完了していない可能性があります。",
  "feed.malformedRows": "Gateway の row_count とデコードした行数が一致しません。レスポンスが壊れている可能性があります。",
  "feed.anonymousSubtitle": "匿名・読み取り専用デモ",
  "feed.l2Distance": "L2二乗距離",
  "feed.vectorDistance": "ベクトル距離",
  "feed.l2DistanceValue": "L2二乗距離: {{value}}",
  "feed.relationshipTrail": "関係性の経路",
  "feed.followerEdge": "フォローエッジ",
  "feed.authorPostEdge": "作者・投稿エッジ",
  "feed.postTopicEdge": "投稿・トピックエッジ",
  "feed.edgeOn": "投稿",
  "feed.edgeToTopic": "トピック",
  "feed.seedLabelsNote": "ラベルは固定されたソーシャルグラフのシードを表します。シード対象を変更した場合は更新してください。",
  "explanation.graphValueAdd": "グラフによる価値",
  "explanation.whyResultsDiffer": "結果が異なる理由",
  "explanation.whyResultsDifferBody":
    "固定クエリベクトルでは、デイブの取得メモが公開 Post の中で最も近くなります。ベクトルのみのシナリオでは先頭に表示されますが、アリスのグラフ制約付きフィードではデイブをフォローしていないため、距離が近くても除外されます。",
  "query.title": "GQL クエリ",
  "query.hoverHint": "ハイライトされた部分にカーソルを合わせると説明が表示されます。",
  "query.expand": "クエリを拡大",
  "query.close": "閉じる",
  "query.formatting": "クエリを整形しています…",
  "query.error.parse": "クエリの解析に失敗しました: {{message}}",
  "query.error.unsupported": "このクエリ形式は formatter で未対応です: {{message}}",
  "query.error.invalidOptions": "formatter のオプションが不正です: {{message}}",
  "query.error.adapter": "クエリ formatter を実行できませんでした: {{message}}",
  "error.retry": "再試行",
  "date.justNow": "たった今",
  "date.yesterday": "昨日",
  "date.minutesAgo": "{{count}}分前",
  "date.hoursAgo": "{{count}}時間前",
};

const dictionaries = { en: enDictionary, ja: jaDictionary };

type I18nContextValue = {
  locale: Accessor<Locale>;
  setLocale: (locale: Locale) => void;
  t: (key: TranslationKey, args?: Record<string, string | number | boolean>) => string;
};

export type Translate = I18nContextValue["t"];

const I18nContext = createContext<I18nContextValue>();

const initialLocale = (): Locale => {
  if (typeof window === "undefined") return "en";
  const saved = window.localStorage.getItem("gleaph-social-demo-locale");
  return saved === "ja" ? "ja" : "en";
};

export function I18nProvider(props: ParentProps) {
  const [locale, setLocaleSignal] = createSignal<Locale>(initialLocale());
  const dictionary = createMemo(() => dictionaries[locale()]);
  const translate = translator(dictionary, resolveTemplate);

  const setLocale = (next: Locale) => {
    setLocaleSignal(next);
    if (typeof window !== "undefined") {
      window.localStorage.setItem("gleaph-social-demo-locale", next);
    }
  };

  const t = (key: TranslationKey, args?: Record<string, string | number | boolean>) =>
    String(translate(key, args));

  return (
    <I18nContext.Provider value={{ locale, setLocale, t }}>
      {props.children}
    </I18nContext.Provider>
  );
}

export function useI18n(): I18nContextValue {
  const context = useContext(I18nContext);
  if (!context) {
    throw new Error("useI18n must be used inside I18nProvider");
  }
  return context;
}

const scenarioKey = {
  PublicTimeline: "publicTimeline",
  AliceHomeFeed: "aliceHomeFeed",
  TopicPath: "topicPath",
  SemanticDiscovery: "semanticDiscovery",
  AliceSemanticFeed: "aliceSemanticFeed",
} as const;

export type LocalizedScenarioField = "feedTitle" | "explanationTitle" | "rdbSummary" | "graphSummary";

export function scenarioTranslationKey(
  id: keyof typeof scenarioKey,
  field: LocalizedScenarioField,
): TranslationKey {
  return `scenario.${scenarioKey[id]}.${field}` as TranslationKey;
}

export function scenarioLabelKey(id: keyof typeof scenarioKey): TranslationKey {
  return `scenario.${scenarioKey[id]}` as TranslationKey;
}

const japaneseAnnotationCopy: Record<string, { label: string; description: string }> = {
  "Pattern match": {
    label: "パターンマッチ",
    description: "続く頂点とエッジの形に一致するグラフパターンを検索します。",
  },
  "Anchor vertex": {
    label: "起点の頂点",
    description: "フィードを表す固定の起点頂点から走査を開始します。",
  },
  "Feed edge": {
    label: "フィードエッジ",
    description: "フィードと投稿を結ぶエッジをたどります。変数 e で後続の並べ替えに利用できます。",
  },
  "Post vertex": {
    label: "投稿頂点",
    description: "フィードからエッジでつながった投稿頂点を検索します。",
  },
  "Authorship edge": {
    label: "投稿者エッジ",
    description: "投稿から、その投稿を書いたユーザーへ POSTED エッジをたどります。",
  },
  "Author vertex": {
    label: "投稿者頂点",
    description: "一致した投稿の作者を author として束縛します。",
  },
  "Optional pattern": {
    label: "任意パターン",
    description: "追加パターンを試します。存在しない場合も元の行は保持されます。",
  },
  "Reply relationship": {
    label: "返信関係",
    description: "現在の投稿が返信であれば親投稿を検索します。返信がなければ親の値は null になります。",
  },
  Projection: {
    label: "射影",
    description: "結果の列として返す値を宣言します。",
  },
  "Post id column": {
    label: "投稿 ID 列",
    description: "投稿の demo_id を post_id という名前で返します。",
  },
  "Parent id column": {
    label: "親投稿 ID 列",
    description: "親投稿の ID を返します。返信でなければ null です。",
  },
  "Author name column": {
    label: "投稿者名列",
    description: "投稿者の表示名を返します。",
  },
  "Body column": {
    label: "本文列",
    description: "投稿本文を返します。",
  },
  "Timestamp column": {
    label: "時刻列",
    description: "投稿の作成時刻を返します。",
  },
  "Newest-first ordering": {
    label: "新しい順",
    description: "フィードエッジの挿入順を降順にして、新しい投稿から表示します。",
  },
  "Result cap": {
    label: "結果数の上限",
    description: "返す投稿数を制限します。",
  },
  "Alice's user vertex": {
    label: "アリスのユーザー頂点",
    description: "このフィードの閲覧対象であるアリスのユーザー頂点を起点にします。",
  },
  "Home feed edge": {
    label: "ホームフィードエッジ",
    description: "アリスのホームフィードを構成するエッジをたどります。",
  },
  "Visibility filter": {
    label: "公開条件",
    description: "公開投稿だけを残し、非公開投稿を除外します。",
  },
  "Topic match": {
    label: "トピック検索",
    description: "HAS_TOPIC エッジでトピック頂点につながる投稿を検索します。",
  },
  "Topic filter": {
    label: "トピック条件",
    description: "Graph databases トピックに結果を絞り込みます。",
  },
  "Follower-author path": {
    label: "フォロワー・投稿者経路",
    description: "ユーザーから投稿者、投稿までのフォローと投稿の経路を検索します。",
  },
  "Viewer filter": {
    label: "閲覧者条件",
    description: "フォロワーをアリスに絞り込み、経路を個人向けにします。",
  },
  "Topic id column": {
    label: "トピック ID 列",
    description: "一致したトピックの demo_id を返します。",
  },
  "Follows edge id": {
    label: "フォローエッジ ID",
    description: "推薦理由の証拠として FOLLOWS エッジの一意な識別子を返します。",
  },
  "Posted edge id": {
    label: "投稿エッジ ID",
    description: "証拠として POSTED エッジの一意な識別子を返します。",
  },
  "Topic edge id": {
    label: "トピックエッジ ID",
    description: "証拠として HAS_TOPIC エッジの一意な識別子を返します。",
  },
  "Post-author pattern": {
    label: "投稿・投稿者パターン",
    description: "投稿と、その投稿を書いたユーザーを同時に検索します。",
  },
  "Vector search": {
    label: "ベクトル検索",
    description: "クエリベクトルを使って post_vec ベクトルインデックスを検索し、近い投稿を返します。",
  },
  "Distance column": {
    label: "距離列",
    description: "投稿 embedding とクエリベクトルの L2 二乗距離を取得します。",
  },
  "Distance result": {
    label: "距離の結果",
    description: "各結果のベクトル距離を返します。",
  },
  "Nearest-first ordering": {
    label: "近い順",
    description: "ベクトル距離が近い結果から並べます。",
  },
  "Social graph pattern": {
    label: "ソーシャルグラフパターン",
    description: "ユーザーから FOLLOWS、POSTED をたどって投稿を検索し、意味検索の対象を制約します。",
  },
  "Viewer and visibility filter": {
    label: "閲覧者・公開条件",
    description: "開始ユーザーをアリスに限定し、公開投稿だけを残します。",
  },
};

export function localizeAnnotation(annotation: QueryAnnotation, locale: Locale): QueryAnnotation {
  if (locale === "en") return annotation;
  const copy = japaneseAnnotationCopy[annotation.label];
  return copy ? { ...annotation, ...copy } : annotation;
}
