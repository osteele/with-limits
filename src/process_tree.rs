use std::collections::{HashMap, HashSet};
use sysinfo::{Pid, ProcessesToUpdate, System};

#[derive(Debug, Default)]
pub struct Usage {
    pub rss_bytes: u64,
    pub cpu_percent: f64,
    pub process_count: usize,
}

pub struct ProcessTree {
    root: Pid,
    known: HashMap<Pid, u64>,
}

impl ProcessTree {
    pub fn new(root: u32) -> Self {
        let root = Pid::from_u32(root);
        Self {
            root,
            known: HashMap::from([(root, 0)]),
        }
    }

    pub fn refresh(&mut self, system: &mut System) -> Usage {
        system.refresh_processes(ProcessesToUpdate::All, true);

        self.known.retain(|pid, start_time| {
            system
                .process(*pid)
                .is_some_and(|process| *start_time == 0 || process.start_time() == *start_time)
        });
        if let Some(process) = system.process(self.root) {
            self.known.insert(self.root, process.start_time());
        }

        loop {
            let parents: HashSet<Pid> = self.known.keys().copied().collect();
            let additions: Vec<(Pid, u64)> = system
                .processes()
                .iter()
                .filter_map(|(pid, process)| {
                    process
                        .parent()
                        .filter(|parent| parents.contains(parent) && !self.known.contains_key(pid))
                        .map(|_| (*pid, process.start_time()))
                })
                .collect();
            if additions.is_empty() {
                break;
            }
            self.known.extend(additions);
        }

        let mut usage = Usage::default();
        for pid in self.known.keys() {
            if let Some(process) = system.process(*pid) {
                usage.rss_bytes = usage.rss_bytes.saturating_add(process.memory());
                usage.cpu_percent += f64::from(process.cpu_usage());
                usage.process_count += 1;
            }
        }
        usage
    }

    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }
}
