import type { ScenarioId } from "~/data/scenarios";

export type QueryAnnotation = {
  /**
   * Exact substring from the prepared query that should be highlighted.
   * Must appear in left-to-right order and must not overlap with other
   * annotations for the same scenario.
   */
  queryText: string;
  /** Short label shown in the hover box header. */
  label: string;
  /** Explanation of what this part represents and does. */
  description: string;
};

export const QUERY_ANNOTATIONS: Record<ScenarioId, QueryAnnotation[]> = {
  PublicTimeline: [
    {
      queryText: "MATCH",
      label: "Pattern match",
      description:
        "Starts a graph pattern: tell Gleaph to find vertices and edges that match the following shape.",
    },
    {
      queryText: "(feed:Feed {demo_id: 40})",
      label: "Anchor vertex",
      description:
        "Begin at the public Feed vertex (demo_id 40). This is the fixed starting point of the traversal.",
    },
    {
      queryText: "<-[e:IN_PUBLIC_FEED]-",
      label: "Feed edge",
      description:
        "Follow IN_PUBLIC_FEED edges backwards from posts to the feed. The variable e captures each edge so we can sort by its insertion order later.",
    },
    {
      queryText: "(p:Post)",
      label: "Post vertex",
      description: "Match public Post vertices that are linked to the feed by those edges.",
    },
    {
      queryText: "<-[:POSTED]-",
      label: "Authorship edge",
      description: "Follow POSTED edges backwards from each post to the user who authored it.",
    },
    {
      queryText: "(author:User)",
      label: "Author vertex",
      description: "Bind the author of each matched post as author.",
    },
    {
      queryText: "OPTIONAL MATCH",
      label: "Optional pattern",
      description:
        "Try to match another pattern, but keep the row even if the optional pattern does not exist.",
    },
    {
      queryText: "(p)-[:REPLY_TO]->(parent:Post)",
      label: "Reply relationship",
      description:
        "Optionally find the parent post when the current post is a reply. If there is no reply, the parent fields will be null.",
    },
    {
      queryText: "RETURN",
      label: "Projection",
      description: "Declare which values to send back as result columns.",
    },
    {
      queryText: "p.demo_id AS post_id",
      label: "Post id column",
      description: "Expose the post's demo_id under the friendly column name post_id.",
    },
    {
      queryText: "parent.demo_id AS parent_post_id",
      label: "Parent id column",
      description: "Expose the parent post's id, or null when the post is not a reply.",
    },
    {
      queryText: "author.name AS author_name",
      label: "Author name column",
      description: "Return the author's display name.",
    },
    {
      queryText: "p.body AS body",
      label: "Body column",
      description: "Return the post body text.",
    },
    {
      queryText: "p.created_at AS created_at",
      label: "Timestamp column",
      description: "Return the post creation timestamp.",
    },
    {
      queryText: "ORDER BY GLEAPH.SEQUENCE(e) DESC",
      label: "Newest-first ordering",
      description:
        "Sort by the insertion order of the feed edges in descending order, so the most recently inserted posts appear first.",
    },
    {
      queryText: "LIMIT 20",
      label: "Result cap",
      description: "Return at most 20 posts.",
    },
  ],

  AliceHomeFeed: [
    {
      queryText: "MATCH",
      label: "Pattern match",
      description:
        "Starts a graph pattern: tell Gleaph to find vertices and edges that match the following shape.",
    },
    {
      queryText: "(u:User {demo_id: 1})",
      label: "Alice's user vertex",
      description: "Begin at Alice's User vertex (demo_id 1), the viewer of this feed.",
    },
    {
      queryText: "<-[e:IN_HOME_FEED]-",
      label: "Home feed edge",
      description:
        "Follow IN_HOME_FEED edges backwards from posts to Alice. These edges are materialized at seed time for Alice and everyone she follows.",
    },
    {
      queryText: "(p:Post)",
      label: "Post vertex",
      description: "Match the posts that appear in Alice's home feed.",
    },
    {
      queryText: "<-[:POSTED]-",
      label: "Authorship edge",
      description: "Follow POSTED edges backwards from each post to its author.",
    },
    {
      queryText: "(author:User)",
      label: "Author vertex",
      description: "Bind the author of each matched post as author.",
    },
    {
      queryText: "WHERE p.is_public = TRUE",
      label: "Visibility filter",
      description: "Keep only public posts; private posts are excluded from the feed.",
    },
    {
      queryText: "OPTIONAL MATCH",
      label: "Optional pattern",
      description:
        "Try to match another pattern, but keep the row even if the optional pattern does not exist.",
    },
    {
      queryText: "(p)-[:REPLY_TO]->(parent:Post)",
      label: "Reply relationship",
      description:
        "Optionally find the parent post when the current post is a reply. If there is no reply, the parent fields will be null.",
    },
    {
      queryText: "RETURN",
      label: "Projection",
      description: "Declare which values to send back as result columns.",
    },
    {
      queryText: "p.demo_id AS post_id",
      label: "Post id column",
      description: "Expose the post's demo_id under the friendly column name post_id.",
    },
    {
      queryText: "parent.demo_id AS parent_post_id",
      label: "Parent id column",
      description: "Expose the parent post's id, or null when the post is not a reply.",
    },
    {
      queryText: "author.name AS author_name",
      label: "Author name column",
      description: "Return the author's display name.",
    },
    {
      queryText: "p.body AS body",
      label: "Body column",
      description: "Return the post body text.",
    },
    {
      queryText: "p.created_at AS created_at",
      label: "Timestamp column",
      description: "Return the post creation timestamp.",
    },
    {
      queryText: "ORDER BY GLEAPH.SEQUENCE(e) DESC",
      label: "Newest-first ordering",
      description:
        "Sort by the insertion order of the home feed edges in descending order, so newest posts appear first.",
    },
    {
      queryText: "LIMIT 20",
      label: "Result cap",
      description: "Return at most 20 posts.",
    },
  ],

  TopicPath: [
    {
      queryText: "MATCH (p:Post)-[has_topic:HAS_TOPIC]->(t:Topic)",
      label: "Topic match",
      description: "Find posts that have a HAS_TOPIC edge pointing to a Topic vertex.",
    },
    {
      queryText: "WHERE t.demo_id = 13",
      label: "Topic filter",
      description: "Restrict the result to the 'Graph databases' topic (demo_id 13).",
    },
    {
      queryText: "MATCH (u:User)-[follows:FOLLOWS]->(author:User)-[posted:POSTED]->(p)",
      label: "Follower-author path",
      description:
        "Find a User who follows the author, and the author who posted the matched post. The pattern binds the follows and posted edges as evidence.",
    },
    {
      queryText: "WHERE u.demo_id = 1",
      label: "Viewer filter",
      description: "Restrict the follower to Alice (demo_id 1), so the path is personalized.",
    },
    {
      queryText: "RETURN",
      label: "Projection",
      description: "Declare which values to send back as result columns.",
    },
    {
      queryText: "p.demo_id AS post_id",
      label: "Post id column",
      description: "Expose the post's demo_id under the friendly column name post_id.",
    },
    {
      queryText: "author.name AS author_name",
      label: "Author name column",
      description: "Return the author's display name.",
    },
    {
      queryText: "t.demo_id AS topic_id",
      label: "Topic id column",
      description: "Return the matched topic's demo_id.",
    },
    {
      queryText: "p.body AS body",
      label: "Body column",
      description: "Return the post body text.",
    },
    {
      queryText: "p.created_at AS created_at",
      label: "Timestamp column",
      description: "Return the post creation timestamp.",
    },
    {
      queryText: "ELEMENT_ID(follows) AS follows_edge_id",
      label: "Follows edge id",
      description:
        "Return the unique identifier of the FOLLOWS edge as explainable evidence for the recommendation.",
    },
    {
      queryText: "ELEMENT_ID(posted) AS posted_edge_id",
      label: "Posted edge id",
      description: "Return the unique identifier of the POSTED edge as evidence.",
    },
    {
      queryText: "ELEMENT_ID(has_topic) AS topic_edge_id",
      label: "Topic edge id",
      description: "Return the unique identifier of the HAS_TOPIC edge as evidence.",
    },
  ],

  SemanticDiscovery: [
    {
      queryText: "MATCH",
      label: "Pattern match",
      description:
        "Starts a graph pattern: tell Gleaph to find vertices and edges that match the following shape.",
    },
    {
      queryText: "(p:Post)<-[:POSTED]-(author:User)",
      label: "Post-author pattern",
      description: "Match posts together with the users who posted them.",
    },
    {
      queryText: "WHERE p.is_public = TRUE",
      label: "Visibility filter",
      description: "Keep only public posts in the search pool.",
    },
    {
      queryText: "SEARCH p IN (\n  VECTOR INDEX post_vec\n  FOR $query\n  LIMIT 10\n)",
      label: "Vector search",
      description:
        "Search the post_vec vector index using the bound query vector parameter and return the 10 nearest neighbors.",
    },
    {
      queryText: "DISTANCE AS distance",
      label: "Distance column",
      description:
        "Capture the L2-squared vector distance between the post embedding and the query vector.",
    },
    {
      queryText: "RETURN",
      label: "Projection",
      description: "Declare which values to send back as result columns.",
    },
    {
      queryText: "p.demo_id AS post_id",
      label: "Post id column",
      description: "Expose the post's demo_id under the friendly column name post_id.",
    },
    {
      queryText: "author.name AS author_name",
      label: "Author name column",
      description: "Return the author's display name.",
    },
    {
      queryText: "p.body AS body",
      label: "Body column",
      description: "Return the post body text.",
    },
    {
      queryText: "distance",
      label: "Distance result",
      description: "Return the computed vector distance for each result.",
    },
    {
      queryText: "ORDER BY distance ASC",
      label: "Nearest-first ordering",
      description: "Sort results so the nearest vector neighbors appear first.",
    },
  ],

  AliceSemanticFeed: [
    {
      queryText: "MATCH",
      label: "Pattern match",
      description:
        "Starts a graph pattern: tell Gleaph to find vertices and edges that match the following shape.",
    },
    {
      queryText: "(u:User)-[:FOLLOWS]->(author:User)-[:POSTED]->(p:Post)",
      label: "Social graph pattern",
      description:
        "Traverse from a User through FOLLOWS to authors and then through POSTED to their posts. This constrains vector search to Alice's follow graph.",
    },
    {
      queryText: "WHERE u.demo_id = 1 AND p.is_public = TRUE",
      label: "Viewer and visibility filter",
      description: "Restrict the starting user to Alice (demo_id 1) and keep only public posts.",
    },
    {
      queryText: "SEARCH p IN (\n  VECTOR INDEX post_vec\n  FOR $query\n  LIMIT 10\n)",
      label: "Vector search",
      description:
        "Within the graph-constrained posts, search the post_vec vector index for the 10 nearest neighbors to the query vector.",
    },
    {
      queryText: "DISTANCE AS distance",
      label: "Distance column",
      description: "Capture the L2-squared vector distance as a result column.",
    },
    {
      queryText: "RETURN",
      label: "Projection",
      description: "Declare which values to send back as result columns.",
    },
    {
      queryText: "p.demo_id AS post_id",
      label: "Post id column",
      description: "Expose the post's demo_id under the friendly column name post_id.",
    },
    {
      queryText: "author.name AS author_name",
      label: "Author name column",
      description: "Return the author's display name.",
    },
    {
      queryText: "p.body AS body",
      label: "Body column",
      description: "Return the post body text.",
    },
    {
      queryText: "distance",
      label: "Distance result",
      description: "Return the computed vector distance for each result.",
    },
    {
      queryText: "ORDER BY distance ASC",
      label: "Nearest-first ordering",
      description: "Sort results so the nearest vector neighbors appear first.",
    },
  ],
};
