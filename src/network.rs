use HashMap;
use log;
use message::{Request, Response};
use node::{self, Node};
use params::Params;
use prefix::{Name, Prefix};
use random;
use section::Section;
use stats::{Distribution, Stats};
use std::mem;
use std::ops::AddAssign;

pub struct Network {
    params: Params,
    stats: Stats,
    sections: HashMap<Prefix, Section>,
}

impl Network {
    /// Create new simulated network with the given parameters.
    pub fn new(params: Params) -> Self {
        let mut sections = HashMap::default();
        let _ = sections.insert(Prefix::EMPTY, Section::new(Prefix::EMPTY));

        Network {
            params,
            stats: Stats::new(),
            sections,
        }
    }

    /// Execute single iteration of the simulation. Returns `true` if the
    /// simulation is running successfuly so far, `false` if it failed and should
    /// be stopped.
    pub fn tick(&mut self, iterations: u64) -> bool {
        self.generate_random_messages();
        let stats = self.handle_messages();

        self.stats.record(
            iterations,
            self.sections
                .values()
                .map(|section| section.nodes().len() as u64)
                .sum(),
            self.sections.len() as u64,
            stats.merges,
            stats.splits,
            stats.relocations,
            stats.rejections,
        );

        let _ = self.check_section_sizes();

        true
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    #[allow(unused)]
    pub fn num_complete_sections(&self) -> u64 {
        self.sections
            .values()
            .filter(|section| section.is_complete(&self.params))
            .count() as u64
    }

    pub fn age_dist(&self) -> Distribution {
        Distribution::new(
            self.sections
                .values()
                .flat_map(|section| section.nodes().values())
                .map(|node| node.age() as u64),
        )
    }

    pub fn section_size_dist(&self) -> Distribution {
        Distribution::new(self.sections.values().map(
            |section| section.nodes().len() as u64,
        ))
    }

    pub fn prefix_len_dist(&self) -> Distribution {
        Distribution::new(self.sections.keys().map(|prefix| prefix.len() as u64))
    }

    fn generate_random_messages(&mut self) {
        let mut adds = 0;
        let mut drops = 0;

        for section in self.sections.values_mut() {
            if section.has_incoming_relocation() {
                panic!(
                    "section {:?} having non-empty incoming cache {:?}",
                    section.prefix(),
                    section.incoming_relocations
                );
            }

            if random::gen() {
                add_random_node(&self.params, section);
                adds += 1;

                if drop_random_node(&self.params, section) {
                    drops += 1;
                }
            } else {
                if drop_random_node(&self.params, section) {
                    drops += 1;
                }

                add_random_node(&self.params, section);
                adds += 1;
            }
        }

        info!(
            "Random Adds: {} Drops: {}",
            log::important(adds),
            log::important(drops)
        );
    }

    fn handle_messages(&mut self) -> TickStats {
        let mut responses = Vec::new();
        let mut stats = TickStats::new();

        loop {
            for section in self.sections.values_mut() {
                responses.extend(section.handle_requests(&self.params));
            }

            if responses.is_empty() {
                break;
            }

            stats += self.handle_responses(&mut responses)
        }

        stats
    }

    fn handle_responses(&mut self, responses: &mut Vec<Response>) -> TickStats {
        let mut stats = TickStats::new();

        let mut resps = mem::replace(responses, Vec::new());
        loop {
            let mut forwarded_requests: Vec<Response> = Vec::new();
            for response in resps.drain(..) {
                match response {
                    Response::Merge(section, old_prefix) => {
                        self.sections
                            .entry(section.prefix())
                            .or_insert_with(|| {
                                stats.merges += 1;
                                Section::new(section.prefix())
                            })
                            .merge(&self.params, section);
                        let _ = self.sections.remove(&old_prefix);
                    }
                    Response::Split(section0, section1, old_prefix) => {
                        stats.splits += 1;

                        let prefix0 = section0.prefix();
                        let prefix1 = section1.prefix();

                        assert!(
                            self.sections.insert(prefix0, section0).is_none(),
                            "section with prefix [{}] already exists",
                            prefix0
                        );
                        assert!(
                            self.sections.insert(prefix1, section1).is_none(),
                            "section with prefix [{}] already exists",
                            prefix1
                        );

                        let _ = self.sections.remove(&old_prefix);
                    }
                    Response::Reject(_) => {
                        stats.rejections += 1;
                    }
                    Response::RelocateRequest {
                        src,
                        dst,
                        node_name,
                    } => {
                        let section = self.find_matching_section(dst);
                        forwarded_requests.extend(section.receive(Request::RelocateRequest {
                            src,
                            dst,
                            node_name,
                        }));
                    }
                    Response::Relocate { dst, node } => {
                        stats.relocations += 1;
                        let section = self.find_matching_section(dst);
                        forwarded_requests.extend(section.receive(Request::Relocate { dst, node }));
                    }
                    Response::Send(prefix, request) => {
                        match request {
                            Request::Merge(target_prefix) => {
                                // The receiver of `Merge` might not exists, because
                                // it might have already split. So send the request
                                // to every section with matching prefix.
                                for section in self.sections.values_mut().filter(|section| {
                                    prefix.is_ancestor(&section.prefix())
                                })
                                {
                                    forwarded_requests.extend(section.receive(
                                        Request::Merge(target_prefix),
                                    ));
                                }
                            }
                            Request::Relocate { dst, node } => {
                                stats.relocations += 1;
                                forwarded_requests.extend(self.send(
                                    prefix,
                                    Request::Relocate { dst, node },
                                ));
                            }
                            _ => forwarded_requests.extend(self.send(prefix, request)),
                        }
                    }
                }
            }

            if forwarded_requests.is_empty() {
                break;
            }
            resps = mem::replace(&mut forwarded_requests, Vec::new());
        }

        stats
    }

    fn find_matching_section(&mut self, name: Name) -> &mut Section {
        if let Some(section) = self.sections.values_mut().find(|section| {
            section.prefix().matches(name)
        })
        {
            section
        } else {
            unreachable!()
        }
    }

    fn send(&mut self, prefix: Prefix, request: Request) -> Vec<Response> {
        if let Some(section) = self.sections.get_mut(&prefix) {
            return section.receive(request);
        }

        debug!(
                "{} {} {} {}",
                log::error("Section with prefix"),
                log::prefix(&prefix),
                log::error("not found for request"),
                log::message(&request),
            );
        if let Some(section) = self.sections.values_mut().find(|section| {
            section.prefix().is_ancestor(&prefix)
        })
        {
            return section.receive(request);
        }

        let mut section = match request {
            Request::RelocateRequest { dst, .. } |
            Request::Relocate { dst, .. } => self.find_matching_section(dst),
            Request::RelocateAccept { node_name, .. } |
            Request::RelocateReject { node_name, .. } => self.find_matching_section(node_name),
            _ => unreachable!(),
        };
        section.receive(request)
    }

    fn check_section_sizes(&self) -> bool {
        if let Some(section) = self.sections.values().find(|section| {
            section.nodes().len() > self.params.max_section_size
        })
        {
            let prefixes = section.prefix().split();
            let count0 =
                node::count_matching_adults(&self.params, prefixes[0], section.nodes().values());
            let count1 =
                node::count_matching_adults(&self.params, prefixes[1], section.nodes().values());

            error!(
                "{}: {}: {} (adults per subsections: [..0]: {}, [..1]: {})",
                log::prefix(&section.prefix()),
                log::error("too many nodes"),
                section.nodes().len(),
                count0,
                count1,
            );
            false
        } else {
            true
        }
    }
}

// Generate random `Live` request in the given section.
fn add_random_node(params: &Params, section: &mut Section) {
    let name = section.prefix().substituted_in(random::gen());
    section.receive(Request::Live(Node::new(name, params.init_age)));
}

// Generate random `Dead` request in the given section.
fn drop_random_node(_params: &Params, section: &mut Section) -> bool {
    let name = node::by_age(section.nodes().values())
        .into_iter()
        .find(|node| {
            random::gen_bool_with_probability(node.drop_probability())
        })
        .map(|node| node.name());

    if let Some(name) = name {
        section.receive(Request::Dead(name));
        true
    } else {
        false
    }
}

struct TickStats {
    merges: u64,
    splits: u64,
    relocations: u64,
    rejections: u64,
}

impl TickStats {
    fn new() -> Self {
        TickStats {
            merges: 0,
            splits: 0,
            relocations: 0,
            rejections: 0,
        }
    }
}

impl AddAssign for TickStats {
    fn add_assign(&mut self, other: Self) {
        self.merges += other.merges;
        self.splits += other.splits;
        self.relocations += other.relocations;
        self.rejections += other.rejections;
    }
}
