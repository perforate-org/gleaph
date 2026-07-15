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
  "scenario.yuiHomeFeed": "Yui home feed",
  "scenario.topicPath": "Topic path",
  "scenario.semanticDiscovery": "Semantic discovery",
  "scenario.aliceSemanticFeed": "Alice semantic feed",
  "scenario.publicTimeline.feedTitle": "Public posts",
  "scenario.publicTimeline.rdbSummary":
    "This is the straightforward RDB case: index public posts by created_at, apply the visibility condition, and read the newest rows. There is no meaningful relational advantage to demonstrate here; the important baseline is a simple, well-indexed table query.",
  "scenario.publicTimeline.graphSummary":
    "Gleaph materializes `IN_PUBLIC_FEED` edges from each public Post to a Feed vertex, then reads one fixed-label edge stream. This is not presented as faster than the RDB baseline; it shows that the same Post/User/edge model can extend from a public timeline to relationship-aware feeds without introducing a separate feed schema.",
  "scenario.aliceHomeFeed.feedTitle": "Alice's home feed",
  "scenario.aliceHomeFeed.rdbSummary":
    "At small scale, an RDB can build Alice's feed on read: first get the followee IDs from follows, then fetch their public posts from tweets (a join or subquery). Twitter-like systems often avoid repeating that work for every read by using fan-out on write: copy each new post into followers' home-feed inboxes. At larger scale, a hybrid is common—push ordinary accounts and pull or merge posts from celebrity accounts.",
  "scenario.aliceHomeFeed.graphSummary":
    "Gleaph's social-demo uses fan-out on write: when the seed is built, each public Post creates an `IN_HOME_FEED` edge to its author and every follower User. Alice then reads those pre-materialized edges with one bounded expansion, including her own posts. This shifts work from feed reads to post ingestion; a production system can combine it with pull/merge for accounts with very large audiences.",
  "scenario.yuiHomeFeed.feedTitle": "Yui's home feed",
  "scenario.yuiHomeFeed.rdbSummary":
    "This is the same home-feed problem as Alice's, but the follow graph is intentionally centered on Japanese users. An RDB can join Yui's followees to their public posts on read, or use fan-out on write to maintain her feed inbox. The result makes the cluster visible without changing the feed model.",
  "scenario.yuiHomeFeed.graphSummary":
    "Gleaph reads Yui's pre-materialized `IN_HOME_FEED` edges with one bounded expansion. Most results come from the Japanese cluster Yui follows, while occasional cross-cluster posts appear when the seed includes a bridge connection. The query and storage shape are identical to Alice's feed; only the viewer and seeded social neighborhood differ.",
  "scenario.topicPath.feedTitle": "Topic explanation path",
  "scenario.topicPath.rdbSummary":
    "An RDB can answer this, but this four-hop path is an awkward hot read. In a 1-million-user friends-of-friends benchmark, depth 2 took MySQL 0.016 s versus Neo4j 0.010 s; depth 3 took 30.267 s versus 0.168 s; depth 4 took 1,543.505 s versus 1.359 s; and depth 5 did not finish in one hour for MySQL while Neo4j took 2.132 s. This is an implementation-dependent reference, not a universal benchmark, but it makes the scaling problem concrete: normalized RDB queries must repeatedly build and carry intermediate candidate sets as joins fan out. Production systems often avoid doing this on every request with denormalized paths, materialized recommendations, or precomputed feeds, trading read latency for write and freshness costs.",
  "scenario.topicPath.graphSummary":
    "The graph pattern follows four relationships directly: Alice follows Bob, Bob follows George, George authored the post, and the post has the selected topic. Gleaph returns the matching edge identities alongside the post, so the multi-hop reason is part of the result rather than a chain reconstructed afterward.",
  "scenario.semanticDiscovery.feedTitle": "Vector-only semantic discovery",
  "scenario.semanticDiscovery.rdbSummary":
    "An RDB with a vector extension can also run this query: search the Post embedding index, filter public posts, and return the nearest neighbors. The key capability here is vector indexing, not a relational join; a separate vector service is another common implementation.",
  "scenario.semanticDiscovery.graphSummary":
    "Gleaph keeps the canonical Post embeddings with the graph data and routes `SEARCH` through its derived vector index. This scenario intentionally has no relationship constraint, so Dave's note appears because it is the nearest public Post—not because the graph has inferred a social connection.",
  "scenario.aliceSemanticFeed.feedTitle": "Alice's graph-constrained semantic feed",
  "scenario.aliceSemanticFeed.rdbSummary":
    "A relational implementation must combine two concerns: find posts by vector distance and restrict them to Alice's followees. It can join or pre-filter in the database, but if vector search returns only a small top-k candidate set, filtering afterward can miss eligible posts; over-fetching or filter-aware vector search is needed.",
  "scenario.aliceSemanticFeed.graphSummary":
    "Gleaph expresses eligibility and ranking in one query: `Alice → FOLLOWS → author → POSTED → Post` plus vector `SEARCH`. Dave's nearer post is excluded because he is not a followee, while eligible posts are ranked by semantic distance. The point is composable relationship filtering, not that graphs replace vector indexes.",
  "feed.loading": "Loading scenario through Gateway…",
  "feed.empty": "No posts returned for this scenario.",
  "feed.replyingTo": "Replying to {{author}}",
  "feed.unknownAuthor": "the original author",
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
  "feed.secondFollowerEdge": "Second follower edge",
  "feed.authorPostEdge": "Author-post edge",
  "feed.postTopicEdge": "Post-topic edge",
  "feed.edgeOn": "on",
  "feed.edgeToTopic": "to topic",
  "feed.seedLabelsNote": "Labels reflect the fixed social-graph seed. Update them if the seed subject changes.",
  "explanation.graphValueAdd": "Graph value add",
  "explanation.rdbBaseline": "RDB baseline",
  "explanation.topicPathChartTitle": "Execution time by traversal depth",
  "explanation.topicPathChartDescription":
    "Log-scale comparison of MySQL and Neo4j execution times as relationship depth increases.",
  "explanation.topicPathChartCaption":
    "The logarithmic axis keeps both series visible: the gap widens sharply from depth 3. MySQL depth 5 was not finished within one hour.",
  "explanation.topicPathChartMysql": "MySQL",
  "explanation.topicPathChartNeo4j": "Neo4j",
  "explanation.topicPathChartMysqlIncomplete": "1h+",
  "explanation.topicPathChartDepth": "Relationship depth",
  "explanation.topicPathChartLogScale": "Seconds (log scale)",
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
  "scenario.yuiHomeFeed": "ゆいのホームフィード",
  "scenario.topicPath": "トピックへの経路",
  "scenario.semanticDiscovery": "意味検索",
  "scenario.aliceSemanticFeed": "アリスの意味フィード",
  "scenario.publicTimeline.feedTitle": "公開投稿",
  "scenario.publicTimeline.rdbSummary":
    "これは RDB が素直に処理できるケースです。公開投稿に created_at のインデックスを張り、公開条件を適用して新しい順に取得します。ここで示す基準は、単純で適切にインデックスされたテーブル検索です。",
  "scenario.publicTimeline.graphSummary":
    "Gleaph は各公開 Post から Feed 頂点へ `IN_PUBLIC_FEED` エッジを事前作成し、固定ラベルのエッジ列を読みます。これは RDB より速いと主張する例ではなく、同じ Post／User／エッジのモデルを、別のフィード用スキーマを増やさず関係性のあるフィードへ拡張できることを示します。",
  "scenario.aliceHomeFeed.feedTitle": "アリスのホームフィード",
  "scenario.aliceHomeFeed.rdbSummary":
    "小規模なら、RDB は読むたびにホームフィードを組み立てられます。まず follows からフォロー先 ID を取得し、その ID の公開投稿を tweets から取得する、結合またはサブクエリです。一方、Twitter のような実サービスでは毎回この処理を繰り返さず、投稿時にフォロワーのホームフィード受信箱へ配る fan-out on write もよく使います。大規模では、通常のアカウントは push、著名アカウントは pull／merge するハイブリッドが一般的です。",
  "scenario.aliceHomeFeed.graphSummary":
    "Gleaph の social-demo は fan-out on write を使います。シード構築時に、各公開 Post から作者とその作者をフォローするすべての User へ `IN_HOME_FEED` エッジを作成します。アリスは事前に配られたエッジをユーザー頂点から1回の範囲付き展開で読み、自分の投稿も取得できます。読み取りから投稿取り込みへ処理を移す設計なので、実サービスでは大規模アカウントだけ pull／merge と組み合わせる余地があります。",
  "scenario.yuiHomeFeed.feedTitle": "ゆいのホームフィード",
  "scenario.yuiHomeFeed.rdbSummary":
    "アリスと同じホームフィードの問題ですが、フォロー関係は日本人ユーザーを中心に意図的に構成しています。RDB なら、ゆいのフォロー先と公開投稿を読むたびに結合するか、fan-out on write で受信箱を維持します。フィードのモデルを変えずにクラスタが見える例です。",
  "scenario.yuiHomeFeed.graphSummary":
    "Gleaph はゆいに事前作成された `IN_HOME_FEED` エッジを1回の範囲付き展開で読みます。結果の多くはゆいがフォローする日本語ユーザーのクラスタから来て、シードの橋渡し関係によって少数の英語圏ユーザーの投稿も混ざります。クエリと保存形式はアリスのフィードと同じで、違うのは閲覧者とソーシャル近傍です。",
  "scenario.topicPath.feedTitle": "トピック説明の経路",
  "scenario.topicPath.rdbSummary":
    "RDB でも答えは出せますが、この4-hop経路は読み取りのたびに実行するには不利です。100万人規模の friends-of-friends ベンチマークでは、深さ2は MySQL 0.016秒／Neo4j 0.010秒でしたが、深さ3は 30.267秒／0.168秒、深さ4は 1,543.505秒／1.359秒まで広がり、深さ5では MySQL が1時間以内に完了せず、Neo4jは2.132秒でした。実装や環境に依存する参考値ですが、深くなるほど差が急拡大する様子を具体的に示します。正規化されたRDBでは、follows や多対多の中間テーブルを何度も結合して途中の候補集合を作るためです。実運用では、非正規化した経路、推薦結果のマテリアライズ、事前生成フィードなどで毎回の探索を避けますが、書き込み負荷と鮮度のコストを支払います。",
  "scenario.topicPath.graphSummary":
    "グラフパターンは4つの関係をそのままたどります。アリスがボブをフォローし、ボブがジョージをフォローし、ジョージが投稿し、その投稿に選択したトピックが付いている経路です。Gleaph は投稿と一緒に一致したエッジ識別子も返すため、多段の理由自体が結果になります。",
  "scenario.semanticDiscovery.feedTitle": "ベクトルのみの意味検索",
  "scenario.semanticDiscovery.rdbSummary":
    "ベクトル拡張を持つ RDB でも実行できます。Post の embedding インデックスを検索し、公開投稿に絞って近い順に返します。ここでの主役はリレーショナル結合ではなくベクトルインデックスで、別のベクトルサービスを使う実装も一般的です。",
  "scenario.semanticDiscovery.graphSummary":
    "Gleaph は正規の Post embedding をグラフデータとともに保持し、`SEARCH` を派生ベクトルインデックスへルーティングします。このシナリオには関係性の条件がないため、デイブの投稿はグラフが関係を推測したからではなく、公開 Post の中で最も近い結果として表示されます。",
  "scenario.aliceSemanticFeed.feedTitle": "アリスのグラフ制約付き意味フィード",
  "scenario.aliceSemanticFeed.rdbSummary":
    "RDB でも、ベクトル距離で投稿を探す処理と、アリスのフォロー先に限定する処理を組み合わせられます。ただし少数の top-k だけを先に取得して後から絞ると、対象投稿を取りこぼします。多めに取得するか、条件を考慮できるベクトル検索が必要です。",
  "scenario.aliceSemanticFeed.graphSummary":
    "Gleaph は `Alice → FOLLOWS → author → POSTED → Post` という対象条件とベクトル `SEARCH` を1つのクエリで表します。デイブはフォロー先ではないため距離が近くても除外され、対象となる投稿だけが意味距離順に返ります。グラフがベクトルインデックスを置き換えるのではなく、関係性の条件と検索を組み合わせる例です。",
  "feed.loading": "Gateway からシナリオを読み込んでいます…",
  "feed.empty": "このシナリオでは投稿が返されませんでした。",
  "feed.replyingTo": "{{author}}への返信",
  "feed.unknownAuthor": "元の投稿者",
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
  "feed.secondFollowerEdge": "2つ目のフォローエッジ",
  "feed.authorPostEdge": "作者・投稿エッジ",
  "feed.postTopicEdge": "投稿・トピックエッジ",
  "feed.edgeOn": "投稿",
  "feed.edgeToTopic": "トピック",
  "feed.seedLabelsNote": "ラベルは固定されたソーシャルグラフのシードを表します。シード対象を変更した場合は更新してください。",
  "explanation.graphValueAdd": "グラフによる価値",
  "explanation.rdbBaseline": "RDBでの基準実装",
  "explanation.topicPathChartTitle": "経路の深さごとの実行時間",
  "explanation.topicPathChartDescription":
    "関係の深さが増えたときの MySQL と Neo4j の実行時間を対数軸で比較したグラフです。",
  "explanation.topicPathChartCaption":
    "両方の系列を見せるために対数軸を使っています。深さ3から差が急拡大し、MySQLの深さ5は1時間以内に完了しませんでした。",
  "explanation.topicPathChartMysql": "MySQL",
  "explanation.topicPathChartNeo4j": "Neo4j",
  "explanation.topicPathChartMysqlIncomplete": "1時間超",
  "explanation.topicPathChartDepth": "関係の深さ",
  "explanation.topicPathChartLogScale": "秒（対数軸）",
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
  YuiHomeFeed: "yuiHomeFeed",
  TopicPath: "topicPath",
  SemanticDiscovery: "semanticDiscovery",
  AliceSemanticFeed: "aliceSemanticFeed",
} as const;

export type LocalizedScenarioField = "feedTitle" | "rdbSummary" | "graphSummary";

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
