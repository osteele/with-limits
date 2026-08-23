use std::collections::{HashMap, HashSet};
use sysinfo::{ProcessesToUpdate, System};

#[derive(Debug, Default, PartialEq)]
pub struct Usage {
    pub rss_bytes: u64,
    pub cpu_percent: f64,
    pub process_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
}

pub struct ProcessTree {
    root: u32,
    count_root_usage: bool,
    root_identity: Option<u64>,
    root_retired: bool,
    known: HashMap<u32, u64>,
}

#[derive(Clone, Copy, Debug)]
struct ProcessSnapshot {
    pid: u32,
    parent: Option<u32>,
    start_time: u64,
    rss_bytes: u64,
    cpu_percent: f64,
}

impl ProcessTree {
    pub fn new(root: u32, count_root_usage: bool) -> Self {
        Self {
            root,
            count_root_usage,
            root_identity: None,
            root_retired: false,
            known: HashMap::new(),
        }
    }

    pub fn refresh(&mut self, system: &mut System) -> Usage {
        system.refresh_processes(ProcessesToUpdate::All, true);
        let snapshots: Vec<_> = system
            .processes()
            .iter()
            .map(|(pid, process)| ProcessSnapshot {
                pid: pid.as_u32(),
                parent: process.parent().map(|parent| parent.as_u32()),
                start_time: process.start_time(),
                rss_bytes: process.memory(),
                cpu_percent: f64::from(process.cpu_usage()),
            })
            .collect();
        self.update(&snapshots)
    }

    fn update(&mut self, snapshots: &[ProcessSnapshot]) -> Usage {
        let by_pid: HashMap<_, _> = snapshots
            .iter()
            .map(|process| (process.pid, process))
            .collect();

        self.known.retain(|pid, start_time| {
            by_pid
                .get(pid)
                .is_some_and(|process| process.start_time == *start_time)
        });
        if !self.root_retired {
            if let Some(process) = by_pid.get(&self.root) {
                let start_time = process.start_time;
                match self.root_identity {
                    Some(expected) if expected != start_time => {
                        self.root_retired = true;
                        self.known.remove(&self.root);
                    }
                    Some(_) => {
                        self.known.insert(self.root, start_time);
                    }
                    None => {
                        self.root_identity = Some(start_time);
                        self.known.insert(self.root, start_time);
                    }
                }
            } else if self.root_identity.is_some() {
                self.root_retired = true;
            }
        }

        loop {
            let parents: HashSet<u32> = self.known.keys().copied().collect();
            let additions: Vec<(u32, u64)> = snapshots
                .iter()
                .filter_map(|process| {
                    process
                        .parent
                        .filter(|parent| {
                            parents.contains(parent) && !self.known.contains_key(&process.pid)
                        })
                        .map(|_| (process.pid, process.start_time))
                })
                .collect();
            if additions.is_empty() {
                break;
            }
            self.known.extend(additions);
        }

        let mut usage = Usage::default();
        for pid in self.known.keys() {
            if let Some(process) = by_pid.get(pid) {
                if self.count_root_usage || *pid != self.root {
                    usage.rss_bytes = usage.rss_bytes.saturating_add(process.rss_bytes);
                    usage.cpu_percent += process.cpu_percent;
                    usage.process_count += 1;
                }
            }
        }
        usage
    }

    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }

    pub fn observed_root(&self) -> bool {
        self.root_identity.is_some()
    }

    pub fn retire_root(&mut self) {
        self.root_retired = true;
        self.known.remove(&self.root);
    }

    pub fn identities(&self) -> Vec<ProcessIdentity> {
        self.known
            .iter()
            .map(|(pid, start_time)| ProcessIdentity {
                pid: *pid,
                start_time: *start_time,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(
        pid: u32,
        parent: Option<u32>,
        start_time: u64,
        rss_bytes: u64,
        cpu_percent: f64,
    ) -> ProcessSnapshot {
        ProcessSnapshot {
            pid,
            parent,
            start_time,
            rss_bytes,
            cpu_percent,
        }
    }

    fn identities(tree: &ProcessTree) -> HashSet<(u32, u64)> {
        tree.identities()
            .into_iter()
            .map(|identity| (identity.pid, identity.start_time))
            .collect()
    }

    #[test]
    fn discovers_a_transitive_tree_and_sums_its_usage() {
        let mut tree = ProcessTree::new(10, true);
        let usage = tree.update(&[
            process(30, Some(20), 3, 300, 30.0),
            process(20, Some(10), 2, 200, 20.0),
            process(10, Some(1), 1, 100, 10.0),
            process(99, Some(1), 9, 900, 90.0),
        ]);

        assert_eq!(
            usage,
            Usage {
                rss_bytes: 600,
                cpu_percent: 60.0,
                process_count: 3,
            }
        );
        assert_eq!(
            identities(&tree),
            HashSet::from([(10, 1), (20, 2), (30, 3)])
        );
    }

    #[test]
    fn retains_an_observed_descendant_after_it_reparents() {
        let mut tree = ProcessTree::new(10, true);
        tree.update(&[
            process(10, Some(1), 1, 100, 10.0),
            process(20, Some(10), 2, 200, 20.0),
        ]);

        let usage = tree.update(&[
            process(10, Some(1), 1, 100, 10.0),
            process(20, Some(1), 2, 200, 20.0),
        ]);

        assert_eq!(usage.process_count, 2);
        assert!(identities(&tree).contains(&(20, 2)));
    }

    #[test]
    fn does_not_adopt_a_reused_root_pid() {
        let mut tree = ProcessTree::new(10, true);
        tree.update(&[
            process(10, Some(1), 1, 100, 10.0),
            process(20, Some(10), 2, 200, 20.0),
        ]);

        let usage = tree.update(&[
            process(10, Some(1), 9, 900, 90.0),
            process(20, Some(1), 2, 200, 20.0),
        ]);

        assert_eq!(usage.process_count, 1);
        assert_eq!(identities(&tree), HashSet::from([(20, 2)]));
    }

    #[test]
    fn drops_a_reused_descendant_pid_unless_it_is_still_a_child() {
        let mut tree = ProcessTree::new(10, true);
        tree.update(&[
            process(10, Some(1), 1, 100, 10.0),
            process(20, Some(10), 2, 200, 20.0),
        ]);

        tree.update(&[
            process(10, Some(1), 1, 100, 10.0),
            process(20, Some(1), 3, 300, 30.0),
        ]);
        assert_eq!(identities(&tree), HashSet::from([(10, 1)]));

        tree.update(&[
            process(10, Some(1), 1, 100, 10.0),
            process(20, Some(10), 3, 300, 30.0),
        ]);
        assert_eq!(identities(&tree), HashSet::from([(10, 1), (20, 3)]));
    }

    #[test]
    fn retires_a_root_that_disappears_without_dropping_live_descendants() {
        let mut tree = ProcessTree::new(10, true);
        tree.update(&[
            process(10, Some(1), 1, 100, 10.0),
            process(20, Some(10), 2, 200, 20.0),
        ]);

        let usage = tree.update(&[process(20, Some(1), 2, 200, 20.0)]);

        assert_eq!(usage.process_count, 1);
        assert_eq!(identities(&tree), HashSet::from([(20, 2)]));
        assert!(tree.observed_root());
    }

    #[test]
    fn can_exclude_a_launch_helper_from_usage() {
        let mut tree = ProcessTree::new(10, false);
        let usage = tree.update(&[
            process(10, Some(1), 1, 1_000, 100.0),
            process(20, Some(10), 2, 200, 20.0),
        ]);

        assert_eq!(
            usage,
            Usage {
                rss_bytes: 200,
                cpu_percent: 20.0,
                process_count: 1,
            }
        );
    }

    #[test]
    fn saturates_aggregate_memory_instead_of_wrapping() {
        let mut tree = ProcessTree::new(10, true);
        let usage = tree.update(&[
            process(10, Some(1), 1, u64::MAX, 10.0),
            process(20, Some(10), 2, 1, 20.0),
        ]);

        assert_eq!(usage.rss_bytes, u64::MAX);
    }

    #[test]
    fn reports_observation_and_emptiness_transitions() {
        let mut tree = ProcessTree::new(10, true);
        assert!(tree.is_empty());
        assert!(!tree.observed_root());

        tree.update(&[process(10, Some(1), 1, 100, 10.0)]);
        assert!(!tree.is_empty());
        assert!(tree.observed_root());

        tree.retire_root();
        assert!(tree.is_empty());
        tree.update(&[process(10, Some(1), 1, 100, 10.0)]);
        assert!(tree.is_empty(), "a retired root must not be adopted again");
    }
}
