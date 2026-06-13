import type { RouterKnowledgeMapResponse, RouterKnowledgeMapRow } from "~/api/viewModelAdapter";

const sharedRows = [
  {
    kind: "node",
    node_id: "alice",
    node_label: "Alice",
    node_kind: "person",
    node_x: -3.8,
    node_y: 1.4,
    node_z: 0.4,
  },
  {
    kind: "node",
    node_id: "bob",
    node_label: "Bob",
    node_kind: "person",
    node_x: -3.4,
    node_y: -1.5,
    node_z: -1.1,
  },
  {
    kind: "node",
    node_id: "post-storage",
    node_label: "Storage notes",
    node_kind: "post",
    node_x: -1.6,
    node_y: 0.8,
    node_z: 0.1,
  },
  {
    kind: "node",
    node_id: "post-routing",
    node_label: "Routing note",
    node_kind: "post",
    node_x: -1.7,
    node_y: -1.3,
    node_z: 1.2,
  },
  {
    kind: "node",
    node_id: "topic-storage",
    node_label: "Storage",
    node_kind: "topic",
    node_x: 0.1,
    node_y: 1.1,
    node_z: -0.4,
  },
  {
    kind: "node",
    node_id: "topic-routing",
    node_label: "Routing",
    node_kind: "topic",
    node_x: 0.2,
    node_y: -1.1,
    node_z: 0.8,
  },
  {
    kind: "node",
    node_id: "project-gleaph",
    node_label: "Gleaph",
    node_kind: "project",
    node_x: 2.0,
    node_y: 0,
    node_z: 0,
  },
  {
    kind: "node",
    node_id: "doc-lara",
    node_label: "LARA overview",
    node_kind: "document",
    node_x: 3.9,
    node_y: 1.2,
    node_z: -0.3,
  },
  {
    kind: "node",
    node_id: "doc-federation",
    node_label: "Federation semantics",
    node_kind: "document",
    node_x: 4.1,
    node_y: -0.7,
    node_z: 0.9,
  },
  {
    kind: "node",
    node_id: "doc-rbac",
    node_label: "RBAC prepared queries",
    node_kind: "document",
    node_x: 3.5,
    node_y: -2.0,
    node_z: -0.8,
  },
  {
    kind: "edge",
    edge_id: "alice-storage",
    edge_source: "alice",
    edge_target: "post-storage",
    edge_label: "wrote",
  },
  {
    kind: "edge",
    edge_id: "bob-routing",
    edge_source: "bob",
    edge_target: "post-routing",
    edge_label: "wrote",
  },
  {
    kind: "edge",
    edge_id: "storage-topic",
    edge_source: "post-storage",
    edge_target: "topic-storage",
    edge_label: "tagged",
  },
  {
    kind: "edge",
    edge_id: "routing-topic",
    edge_source: "post-routing",
    edge_target: "topic-routing",
    edge_label: "tagged",
  },
  {
    kind: "edge",
    edge_id: "topic-storage-project",
    edge_source: "topic-storage",
    edge_target: "project-gleaph",
    edge_label: "used by",
  },
  {
    kind: "edge",
    edge_id: "topic-routing-project",
    edge_source: "topic-routing",
    edge_target: "project-gleaph",
    edge_label: "used by",
  },
  {
    kind: "edge",
    edge_id: "project-lara",
    edge_source: "project-gleaph",
    edge_target: "doc-lara",
    edge_label: "documented by",
  },
  {
    kind: "edge",
    edge_id: "project-federation",
    edge_source: "project-gleaph",
    edge_target: "doc-federation",
    edge_label: "documented by",
  },
  {
    kind: "edge",
    edge_id: "project-rbac",
    edge_source: "project-gleaph",
    edge_target: "doc-rbac",
    edge_label: "secured by",
  },
] satisfies RouterKnowledgeMapRow[];

export const routerKnowledgeMapResponses: RouterKnowledgeMapResponse[] = [
  {
    id: "alice-projects",
    title: "Alice's project trail",
    question: "Show Alice's related projects.",
    rows: [
      ...sharedRows,
      pathRow(0, "alice", undefined, "Alice is the starting point."),
      pathRow(1, "post-storage", "alice-storage", "Follow the post Alice wrote."),
      pathRow(2, "topic-storage", "storage-topic", "Read the topic attached to that post."),
      pathRow(
        3,
        "project-gleaph",
        "topic-storage-project",
        "Use the topic to reach the related project.",
      ),
      pathRow(
        4,
        "doc-lara",
        "project-lara",
        "Reveal the document that explains the project area.",
      ),
      resultRow(
        "LARA overview",
        "Document",
        "Found through Alice's storage post and the Gleaph project node.",
        "doc-lara",
      ),
      technicalRow(0, "Question selected", "The frontend asks the Router for the selected demo scenario."),
      technicalRow(1, "Router query", "Router remains the only public Gleaph entrypoint for the demo."),
      technicalRow(2, "Graph execution", "Graph shards execute local plans and return graph-shaped rows."),
      technicalRow(3, "View model", "Rows are adapted into nodes, edges, path, story, and result cards."),
    ],
  },
  {
    id: "routing-docs",
    title: "Routing explanation",
    question: "Find documents that explain query routing.",
    rows: [
      ...sharedRows,
      pathRow(0, "bob", undefined, "Bob is connected to a routing note."),
      pathRow(1, "post-routing", "bob-routing", "Follow the note that discusses routing."),
      pathRow(2, "topic-routing", "routing-topic", "Use the routing topic as the bridge."),
      pathRow(
        3,
        "project-gleaph",
        "topic-routing-project",
        "Connect that topic to the Gleaph project.",
      ),
      pathRow(
        4,
        "doc-federation",
        "project-federation",
        "Reveal the federation semantics document.",
      ),
      resultRow(
        "Federation semantics",
        "Document",
        "Found by following the routing topic through the Gleaph project.",
        "doc-federation",
      ),
      technicalRow(0, "Scenario request", "A future Router endpoint or prepared query supplies this path."),
      technicalRow(1, "Index-aware route", "Router can resolve candidate starts before shard dispatch."),
      technicalRow(2, "Shard-local execute", "Graph execution stays behind Router-owned orchestration."),
      technicalRow(3, "Merged story", "The frontend receives a presentation-safe graph view model."),
    ],
  },
  {
    id: "access-control",
    title: "Access control trail",
    question: "Show how access control relates to Gleaph.",
    rows: [
      ...sharedRows,
      pathRow(0, "project-gleaph", undefined, "Start at the Gleaph project."),
      pathRow(
        1,
        "doc-rbac",
        "project-rbac",
        "Follow the security relationship to prepared-query access control.",
      ),
      resultRow(
        "RBAC prepared queries",
        "Document",
        "Connected directly to the Gleaph project security model.",
        "doc-rbac",
      ),
      technicalRow(0, "Router boundary", "User-facing permissions are enforced at the Router boundary."),
      technicalRow(
        1,
        "Prepared execution",
        "Future demo copy can distinguish prepared calls from ad-hoc GQL.",
      ),
    ],
  },
];

function pathRow(
  path_index: number,
  path_node_id: string,
  path_edge_id: string | undefined,
  story_text: string,
): RouterKnowledgeMapRow {
  return {
    kind: "path",
    path_index,
    path_node_id,
    path_edge_id,
    story_text,
  };
}

function resultRow(
  result_title: string,
  result_kind: string,
  result_reason: string,
  result_node_id: string,
): RouterKnowledgeMapRow {
  return {
    kind: "result",
    result_title,
    result_kind,
    result_reason,
    result_node_id,
  };
}

function technicalRow(
  technical_index: number,
  technical_title: string,
  technical_detail: string,
): RouterKnowledgeMapRow {
  return {
    kind: "technical",
    technical_index,
    technical_title,
    technical_detail,
  };
}
