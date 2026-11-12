//! `EXPLAIN (FORMAT JSON)` to the core [`PlanNode`] tree.

use quarry_core::PlanNode;
use serde_json::Value as J;

/// Plan nesting kept; deeper nodes are dropped.
const MAX_DEPTH: u32 = 32;
/// Detail keys copied from a plan node, in this order.
const DETAILS: &[&str] = &[
    "Alias",
    "Index Name",
    "Join Type",
    "Strategy",
    "Scan Direction",
    "Parent Relationship",
    "Subplan Name",
    "CTE Name",
    "Function Name",
    "Index Cond",
    "Recheck Cond",
    "Hash Cond",
    "Merge Cond",
    "Join Filter",
    "Filter",
    "Rows Removed by Filter",
    "Sort Key",
    "Sort Method",
    "Group Key",
    "Workers Planned",
];
const DETAIL_MAX_BYTES: usize = 512;

fn short(v: &J) -> Option<String> {
    let s = match v {
        J::String(s) => s.clone(),
        J::Number(n) => n.to_string(),
        J::Bool(b) => b.to_string(),
        J::Array(a) => a.iter().filter_map(short).collect::<Vec<_>>().join(", "),
        _ => return None,
    };
    Some(crate::cells::clip_str(&s, DETAIL_MAX_BYTES).to_string())
}

fn node(v: &J, depth: u32) -> PlanNode {
    let f = |k: &str| v.get(k).and_then(J::as_f64);
    let loops = f("Actual Loops").unwrap_or(1.0).max(1.0);
    let relation = v.get("Relation Name").and_then(J::as_str).map(|r| match v.get("Schema").and_then(J::as_str) {
        Some(s) => format!("{s}.{r}"),
        None => r.to_string(),
    });
    PlanNode {
        op: v.get("Node Type").and_then(J::as_str).unwrap_or("?").to_string(),
        relation,
        startup_cost: f("Startup Cost"),
        total_cost: f("Total Cost"),
        est_rows: f("Plan Rows"),
        actual_rows: f("Actual Rows").map(|r| r * loops),
        actual_ms: f("Actual Total Time").map(|t| t * loops),
        details: DETAILS.iter().filter_map(|k| Some((k.to_string(), short(v.get(*k)?)?))).collect(),
        children: match v.get("Plans").and_then(J::as_array) {
            Some(plans) if depth < MAX_DEPTH => plans.iter().map(|p| node(p, depth + 1)).collect(),
            _ => Vec::new(),
        },
    }
}

/// The plan in an `EXPLAIN (FORMAT JSON)` result: `[{"Plan": {…}, "Planning Time": …, …}]`.
pub(crate) fn parse(json: &str) -> Option<PlanNode> {
    let v: J = serde_json::from_str(json).ok()?;
    let top = v.get(0)?;
    let mut root = node(top.get("Plan")?, 0);
    for k in ["Planning Time", "Execution Time"] {
        if let Some(t) = top.get(k).and_then(short) {
            root.details.push((k.to_string(), t));
        }
    }
    Some(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plan_with_children_and_analyze_fields() {
        let json = r#"[{"Plan": {"Node Type": "Hash Join", "Join Type": "Inner", "Startup Cost": 1.5, "Total Cost": 20.25,
            "Plan Rows": 100, "Actual Rows": 10, "Actual Loops": 2, "Actual Total Time": 0.5, "Hash Cond": "(a.id = b.a_id)",
            "Plans": [{"Node Type": "Seq Scan", "Relation Name": "a", "Schema": "public", "Alias": "a", "Plan Rows": 5,
                       "Filter": "(x > 1)", "Sort Key": ["x", "y"]}]}, "Planning Time": 0.1, "Execution Time": 1.2}]"#;
        let p = parse(json).unwrap();
        assert_eq!(p.op, "Hash Join");
        assert_eq!(p.total_cost, Some(20.25));
        assert_eq!(p.actual_rows, Some(20.0));
        assert_eq!(p.actual_ms, Some(1.0));
        assert!(p.details.contains(&("Hash Cond".into(), "(a.id = b.a_id)".into())));
        assert!(p.details.contains(&("Execution Time".into(), "1.2".into())));
        let c = &p.children[0];
        assert_eq!(c.relation.as_deref(), Some("public.a"));
        assert!(c.details.contains(&("Sort Key".into(), "x, y".into())));
        assert_eq!(parse("not json"), None);
        assert_eq!(parse("[]"), None);
    }

    #[test]
    fn deep_plans_are_cut() {
        let mut json = String::from(r#"{"Node Type": "Leaf"}"#);
        for _ in 0..50 {
            json = format!(r#"{{"Node Type": "N", "Plans": [{json}]}}"#);
        }
        let p = parse(&format!(r#"[{{"Plan": {json}}}]"#)).unwrap();
        let mut depth = 0;
        let mut n = &p;
        while let Some(c) = n.children.first() {
            n = c;
            depth += 1;
        }
        assert_eq!(depth, MAX_DEPTH);
    }
}
