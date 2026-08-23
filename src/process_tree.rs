use std::collections::{HashMap, HashSet};
use sysinfo::{Pid, ProcessesToUpdate, System};

#[derive(Debug, Default)]
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
    root: Pid,
    count_root_usage: bool,
    root_identity: Option<u64>,
    root_retired: bool,
    known: HashMap<Pid, u64>,
}

impl ProcessTree {
    pub fn new(root: u32, count_root_usage: bool) -> Self {
        let root = Pid::from_u32(root);
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

        self.known.retain(|pid, start_time| {
            system
                .process(*pid)
                .is_some_and(|process| process.start_time() == *start_time)
        });
        if !self.root_retired {
            if let Some(process) = system.process(self.root) {
                let start_time = process.start_time();
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
                if self.count_root_usage || *pid != self.root {
                    usage.rss_bytes = usage.rss_bytes.saturating_add(process.memory());
                    usage.cpu_percent += f64::from(process.cpu_usage());
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
                pid: pid.as_u32(),
                start_time: *start_time,
            })
            .collect()
    }
}
