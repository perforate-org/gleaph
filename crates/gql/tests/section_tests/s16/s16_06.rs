//! §16.6 — Path pattern prefix.
//!
//! GQL rule: `pathPatternPrefix : pathModePrefix | pathSearchPrefix`

use crate::section_tests::{body, p};
use gleaph_gql::ast::*;

/// Helper to extract the first PathPattern from a MATCH query.
fn first_path(input: &str) -> PathPattern {
    let prog = p(input);
    let b = body(&prog);
    match &b.first {
        Statement::Query(cq) => {
            for part in &cq.left.parts {
                if let SimpleQueryStatement::Match(m) = part {
                    return m.pattern.paths[0].clone();
                }
            }
            panic!("no Match found in parts: {:?}", cq.left.parts);
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

// ── pathModePrefix ──────────────────────────────────────────────────────
//   : WALK | TRAIL | SIMPLE | ACYCLIC
//   ;
mod path_mode_prefix {
    use super::*;

    /// MATCH WALK (n)-[e]->(m) RETURN n → prefix is Mode(Walk)
    #[test]
    fn walk() {
        let pp = first_path("MATCH WALK (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Mode {
                mode: PathMode::Walk,
                path_keyword: None
            }),
            "expected WALK prefix"
        );
    }

    /// MATCH TRAIL (n)-[e]->(m) RETURN n → prefix is Mode(Trail)
    #[test]
    fn trail() {
        let pp = first_path("MATCH TRAIL (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Mode {
                mode: PathMode::Trail,
                path_keyword: None
            }),
            "expected TRAIL prefix"
        );
    }

    /// MATCH SIMPLE (n)-[e]->(m) RETURN n → prefix is Mode(Simple)
    #[test]
    fn simple() {
        let pp = first_path("MATCH SIMPLE (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Mode {
                mode: PathMode::Simple,
                path_keyword: None
            }),
            "expected SIMPLE prefix"
        );
    }

    /// MATCH ACYCLIC (n)-[e]->(m) RETURN n → prefix is Mode(Acyclic)
    #[test]
    fn acyclic() {
        let pp = first_path("MATCH ACYCLIC (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Mode {
                mode: PathMode::Acyclic,
                path_keyword: None
            }),
            "expected ACYCLIC prefix"
        );
    }
}

// ── pathSearchPrefix ────────────────────────────────────────────────────
//   : ANY | ANY SHORTEST | ALL SHORTEST | SHORTEST k | SHORTEST k GROUP
//   ;
mod path_search_prefix {
    use super::*;

    /// MATCH ANY (n)-[e]->(m) RETURN n → prefix is Search(Any(None))
    #[test]
    fn any() {
        let pp = first_path("MATCH ANY (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::Any {
                k: None,
                mode: None,
                path_keyword: None
            })),
            "expected ANY search prefix"
        );
    }

    /// MATCH ANY SHORTEST (n)-[e]->(m) RETURN n → Search(AnyShortest(None))
    #[test]
    fn any_shortest() {
        let pp = first_path("MATCH ANY SHORTEST (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::AnyShortest {
                mode: None,
                path_keyword: None
            })),
            "expected ANY SHORTEST search prefix"
        );
    }

    /// MATCH ALL SHORTEST (n)-[e]->(m) RETURN n → Search(AllShortest(None))
    #[test]
    fn all_shortest() {
        let pp = first_path("MATCH ALL SHORTEST (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::AllShortest {
                mode: None,
                path_keyword: None
            })),
            "expected ALL SHORTEST search prefix"
        );
    }

    /// MATCH SHORTEST 3 (n)-[e]->(m) RETURN n → Search(ShortestK { k: 3, mode: None })
    #[test]
    fn shortest_k() {
        let pp = first_path("MATCH SHORTEST 3 (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::ShortestK {
                k: 3,
                mode: None,
                path_keyword: None,
            })),
            "expected SHORTEST 3 search prefix"
        );
    }

    /// MATCH SHORTEST 2 PATHS GROUP (n)-[e]->(m) RETURN n
    /// → Search(ShortestKGroup { k: 2, mode: None })
    #[test]
    fn shortest_k_group() {
        let pp = first_path("MATCH SHORTEST 2 PATHS GROUP (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::ShortestKGroup {
                k: 2,
                mode: None,
                path_keyword: Some(gleaph_gql::ast::PathOrPaths::Paths),
                group_keyword: gleaph_gql::ast::GroupOrGroups::Group,
            })),
            "expected SHORTEST 2 PATHS GROUP search prefix"
        );
    }

    /// MATCH SHORTEST 3 GROUPS (n)-[e]->(m) RETURN n — GROUPS (plural)
    #[test]
    fn shortest_k_groups_plural() {
        let pp = first_path("MATCH SHORTEST 3 GROUPS (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::ShortestKGroup {
                k: 3,
                mode: None,
                path_keyword: None,
                group_keyword: gleaph_gql::ast::GroupOrGroups::Groups,
            })),
        );
    }

    /// MATCH ALL (n)-[e]->(m) RETURN n — ALL (without SHORTEST)
    #[test]
    fn all_prefix() {
        let pp = first_path("MATCH ALL (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::All {
                mode: None,
                path_keyword: None
            })),
        );
    }

    /// MATCH ALL TRAIL PATHS (n)-[e]->(m) RETURN n — ALL with mode and PATHS
    #[test]
    fn all_trail_paths() {
        let pp = first_path("MATCH ALL TRAIL PATHS (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::All {
                mode: Some(PathMode::Trail),
                path_keyword: Some(gleaph_gql::ast::PathOrPaths::Paths),
            })),
        );
    }

    /// MATCH ANY 5 (n)-[e]->(m) RETURN n — ANY with k
    #[test]
    fn any_k() {
        let pp = first_path("MATCH ANY 5 (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::Any {
                k: Some(5),
                mode: None,
                path_keyword: None,
            })),
        );
    }

    /// MATCH ANY SHORTEST TRAIL (n)-[e]->(m) RETURN n — ANY SHORTEST with mode
    #[test]
    fn any_shortest_trail() {
        let pp = first_path("MATCH ANY SHORTEST TRAIL (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::AnyShortest {
                mode: Some(PathMode::Trail),
                path_keyword: None,
            })),
        );
    }

    /// MATCH ALL SHORTEST WALK PATH (n)-[e]->(m) RETURN n
    #[test]
    fn all_shortest_walk_path() {
        let pp = first_path("MATCH ALL SHORTEST WALK PATH (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::AllShortest {
                mode: Some(PathMode::Walk),
                path_keyword: Some(gleaph_gql::ast::PathOrPaths::Path),
            })),
        );
    }

    /// MATCH WALK PATH (n)-[e]->(m) RETURN n — path mode with PATH keyword
    #[test]
    fn walk_path() {
        let pp = first_path("MATCH WALK PATH (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Mode {
                mode: PathMode::Walk,
                path_keyword: Some(PathOrPaths::Path),
            }),
        );
    }

    /// MATCH SIMPLE PATHS (n)-[e]->(m) RETURN n — path mode with PATHS keyword
    #[test]
    fn simple_paths() {
        let pp = first_path("MATCH SIMPLE PATHS (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Mode {
                mode: PathMode::Simple,
                path_keyword: Some(PathOrPaths::Paths),
            }),
        );
    }

    /// MATCH SHORTEST (n)-[e]->(m) RETURN n — SHORTEST without k (defaults to 1)
    #[test]
    fn shortest_default() {
        let pp = first_path("MATCH SHORTEST (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::ShortestK {
                k: 1,
                mode: None,
                path_keyword: None,
            })),
        );
    }

    /// MATCH COUNT PATHS (n)-[e]->(m) RETURN n — COUNT PATHS (cypher)
    #[cfg(feature = "cypher")]
    #[test]
    fn count_paths() {
        let pp = first_path("MATCH COUNT PATHS (n)-[e]->(m) RETURN n");
        assert_eq!(
            pp.prefix,
            Some(PathPatternPrefix::Search(SearchPrefix::CountPaths {
                mode: None,
                path_keyword: Some(gleaph_gql::ast::PathOrPaths::Paths),
            })),
        );
    }
}
