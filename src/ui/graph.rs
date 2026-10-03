use gitbuddy::git::Commit;
use std::collections::HashSet;

#[derive(Clone, Debug, Default)]
pub(super) struct GraphRow {
    pub lane: usize,
    pub incoming: bool,
    pub through: Vec<usize>,
    pub outgoing: Vec<usize>,
    pub links: Vec<(usize, usize)>,
}

/// Lay out parent edges in stable columns. Vacated columns are reused rather
/// than shifting existing branches, so adjacent rows join at the same x value.
pub(super) fn rows(commits: &[&Commit], filtered: bool) -> Vec<GraphRow> {
    let visible: HashSet<_> = commits.iter().map(|commit| commit.id.as_str()).collect();
    let mut lanes: Vec<Option<&str>> = Vec::new();
    let mut result = Vec::with_capacity(commits.len());
    for commit in commits {
        let lane = lanes
            .iter()
            .position(|id| *id == Some(commit.id.as_str()))
            .unwrap_or_else(|| vacant(&mut lanes, 0));
        let incoming = lanes[lane].is_some();
        let through = lanes
            .iter()
            .enumerate()
            .filter_map(|(i, id)| (i != lane && id.is_some()).then_some(i))
            .collect();
        lanes[lane] = None;
        let mut outgoing = Vec::new();
        let mut links = Vec::new();
        for (parent_index, parent) in commit.parents.iter().enumerate() {
            if filtered && !visible.contains(parent.as_str()) {
                continue;
            }
            let target =
                if let Some(existing) = lanes.iter().position(|id| *id == Some(parent.as_str())) {
                    existing
                } else {
                    let free = if parent_index == 0 { lane } else { lane + 1 };
                    let target = vacant(&mut lanes, free);
                    lanes[target] = Some(parent);
                    target
                };
            if !outgoing.contains(&target) {
                outgoing.push(target);
            }
            if lane != target {
                links.push((lane, target));
            }
        }
        result.push(GraphRow {
            lane,
            incoming,
            through,
            outgoing,
            links,
        });
    }
    result
}

fn vacant(lanes: &mut Vec<Option<&str>>, start: usize) -> usize {
    if let Some(index) = (start..lanes.len()).find(|i| lanes[*i].is_none()) {
        return index;
    }
    if start > 0
        && let Some(index) = (0..start.min(lanes.len())).find(|i| lanes[*i].is_none())
    {
        return index;
    }
    let index = lanes.len();
    lanes.push(None);
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    fn commit(id: &str, parents: &[&str]) -> Commit {
        Commit {
            id: id.into(),
            parents: parents.iter().map(|p| (*p).into()).collect(),
            subject: String::new(),
            author: String::new(),
            date: String::new(),
            refs: String::new(),
        }
    }
    #[test]
    fn merge_edge_joins_feature_lane_back_into_main() {
        let data = [
            commit("merge", &["main", "feature"]),
            commit("main", &["base"]),
            commit("feature", &["base"]),
            commit("base", &[]),
        ];
        let refs: Vec<_> = data.iter().collect();
        let rows = rows(&refs, false);
        assert_eq!(rows[0].links, vec![(0, 1)]);
        assert!(rows[1].through.contains(&1));
        assert_eq!(rows[2].links, vec![(1, 0)]);
        assert_eq!(rows[3].lane, 0);
    }
    #[test]
    fn filtered_history_does_not_connect_to_hidden_parents() {
        let data = [commit("head", &["hidden"]), commit("other", &[])];
        let refs: Vec<_> = data.iter().collect();
        let rows = rows(&refs, true);
        assert!(rows[0].outgoing.is_empty());
        assert!(!rows[1].incoming);
    }
}
