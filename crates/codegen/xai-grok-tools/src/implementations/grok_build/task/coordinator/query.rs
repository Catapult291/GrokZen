//! Session-scoped query, inspection, and progress delivery.

use std::sync::Arc;

use tokio::sync::oneshot;

use super::super::coordinator_state::{
    BlockingWaiter, CompletedChild, ListAgentsRequest, ListRequest, OUTPUT_UNAVAILABLE_PLACEHOLDER,
    ProgressFuture, ProgressTarget, RunningSeed, completed_inspection, completed_snapshot,
    pending_inspection, pending_snapshot, queued_inspection, queued_snapshot, running_agent_entry,
    running_inspection, running_seed,
};
use super::super::types::{
    SubagentAgentEntry, SubagentAgentStatus, SubagentInspection, SubagentRequest, SubagentSnapshot,
    SubagentSnapshotStatus,
};
use super::{ChildControl, ChildRunner, SubagentCoordinator, SubagentProgress};

const DEFAULT_QUERY_BLOCK_TIMEOUT_MS: u64 = 30_000;

impl<R: ChildRunner> SubagentCoordinator<R> {
    fn push_blocking_waiter(
        &mut self,
        id: String,
        timeout_ms: Option<u64>,
        respond_to: oneshot::Sender<Option<SubagentSnapshot>>,
    ) {
        self.waiters.entry(id).or_default().push(BlockingWaiter {
            deadline: tokio::time::Instant::now()
                + std::time::Duration::from_millis(
                    timeout_ms.unwrap_or(DEFAULT_QUERY_BLOCK_TIMEOUT_MS),
                ),
            respond_to,
        });
    }

    pub(super) fn handle_query(
        &mut self,
        id: String,
        parent_session_id: Option<String>,
        block: bool,
        timeout_ms: Option<u64>,
        respond_to: oneshot::Sender<Option<SubagentSnapshot>>,
    ) {
        if !self.is_reachable_from_session(&id, parent_session_id.as_deref()) {
            let _ = respond_to.send(None);
            return;
        }
        if let Some(child) = self.completed.get(&id) {
            let snapshot = (!child.request.owner.is_workflow())
                .then(|| self.completed_snapshot_for_query(child));
            let _ = respond_to.send(snapshot);
            return;
        }
        if let Some(child) = self.active.get(&id) {
            if child.request.owner.is_workflow() {
                let _ = respond_to.send(None);
                return;
            }
            if block {
                self.push_blocking_waiter(id, timeout_ms, respond_to);
            } else {
                self.queue_active_progress(&id, ProgressTarget::Query(respond_to));
            }
            return;
        }
        if let Some(child) = self.pending.get(&id) {
            if child.request.owner.is_workflow() {
                let _ = respond_to.send(None);
                return;
            }
            if block {
                self.push_blocking_waiter(id, timeout_ms, respond_to);
            } else {
                let mut snapshot = pending_snapshot(child);
                self.stamp_lineage(&mut snapshot, &id, &child.request.parent_session_id);
                let _ = respond_to.send(Some(snapshot));
            }
            return;
        }
        if let Some(queued) = self.queued.iter().find(|queued| queued.request.id == id) {
            if block {
                self.push_blocking_waiter(id, timeout_ms, respond_to);
            } else {
                let queue_position = self
                    .queued
                    .iter()
                    .filter(|other| {
                        other.request.parent_session_id == queued.request.parent_session_id
                    })
                    .position(|other| other.request.id == *id)
                    .map(|index| index + 1);
                let _ = respond_to.send(Some(queued_snapshot(
                    &queued.request,
                    queued.queued_at.into_std(),
                    queue_position,
                )));
            }
            return;
        }
        let _ = respond_to.send(None);
    }

    pub(super) fn handle_inspect(
        &mut self,
        id: String,
        parent_session_id: Option<String>,
        respond_to: oneshot::Sender<Option<SubagentInspection>>,
    ) {
        if !self.is_reachable_from_session(&id, parent_session_id.as_deref()) {
            let _ = respond_to.send(None);
        } else if let Some(child) = self.completed.get(&id) {
            let _ = respond_to.send(Some(self.completed_inspection_for_query(child)));
        } else if let Some(child) = self.pending.get(&id) {
            let mut inspection = pending_inspection(child);
            self.stamp_lineage(
                &mut inspection.snapshot,
                &id,
                &child.request.parent_session_id,
            );
            let _ = respond_to.send(Some(inspection));
        } else if self.active.contains_key(&id) {
            self.queue_active_progress(&id, ProgressTarget::Inspect(respond_to));
        } else if let Some(queued) = self.queued.iter().find(|queued| queued.request.id == id) {
            let queue_position = self
                .queued
                .iter()
                .filter(|other| other.request.parent_session_id == queued.request.parent_session_id)
                .position(|other| other.request.id == *id)
                .map(|index| index + 1);
            let _ = respond_to.send(Some(queued_inspection(
                &queued.request,
                queued.queued_at.into_std(),
                queue_position,
            )));
        } else {
            let _ = respond_to.send(None);
        }
    }

    fn persisted_output(&self, child: &CompletedChild) -> Option<Arc<str>> {
        child.persisted_output_ref.as_deref().map(|reference| {
            self.runner
                .load_persisted_output(reference)
                .unwrap_or_else(|| Arc::from(OUTPUT_UNAVAILABLE_PLACEHOLDER))
        })
    }

    /// Fill the flat lineage columns (`parent_id` / `depth`) on a snapshot from
    /// the spawn graph, falling back to the request's parent session. Pending
    /// children have no `RunningSeed`, so their snapshots are stamped here.
    fn stamp_lineage(&self, snapshot: &mut SubagentSnapshot, id: &str, fallback_parent: &str) {
        snapshot.parent_id = Some(
            self.graph
                .direct_spawner(id)
                .unwrap_or(fallback_parent)
                .to_owned(),
        );
        snapshot.depth = Some(self.graph.depth_of(id).unwrap_or(1));
    }

    pub(super) fn completed_snapshot_for_query(&self, child: &CompletedChild) -> SubagentSnapshot {
        let output = self.persisted_output(child);
        completed_snapshot(child, output.as_deref())
    }

    fn completed_inspection_for_query(&self, child: &CompletedChild) -> SubagentInspection {
        let output = self.persisted_output(child);
        completed_inspection(child, output.as_deref())
    }

    pub(super) fn ready_snapshot(&self, id: &str) -> Option<SubagentSnapshot> {
        self.completed
            .get(id)
            .filter(|child| !child.request.owner.is_workflow())
            .map(|child| self.completed_snapshot_for_query(child))
            .or_else(|| {
                self.pending
                    .get(id)
                    .filter(|child| !child.request.owner.is_workflow())
                    .map(pending_snapshot)
            })
            .or_else(|| {
                self.queued
                    .iter()
                    // Workflow spawns never queue; the filter matches the
                    // completed/pending arms above should that ever bend.
                    .find(|queued| queued.request.id == id && !queued.request.owner.is_workflow())
                    .map(|queued| {
                        let queue_position = self
                            .queued
                            .iter()
                            .filter(|other| {
                                other.request.parent_session_id == queued.request.parent_session_id
                            })
                            .position(|other| other.request.id == *id)
                            .map(|index| index + 1);
                        queued_snapshot(
                            &queued.request,
                            queued.queued_at.into_std(),
                            queue_position,
                        )
                    })
            })
    }

    pub(super) fn handle_list_running(
        &mut self,
        parent_session_id: String,
        respond_to: oneshot::Sender<Vec<SubagentInspection>>,
    ) {
        let ids: Vec<_> = self
            .active
            .values()
            .filter(|child| {
                self.graph
                    .is_reachable_from(&child.request.id, &parent_session_id)
                    && !child.request.owner.is_workflow()
            })
            .map(|child| child.request.id.clone())
            .collect();
        if ids.is_empty() {
            let _ = respond_to.send(Vec::new());
            return;
        }

        let request_id = self.next_list_request_id;
        self.next_list_request_id = self.next_list_request_id.wrapping_add(1);
        self.list_requests.insert(
            request_id,
            ListRequest {
                slots: vec![None; ids.len()],
                remaining: ids.len(),
                respond_to,
            },
        );
        for (index, id) in ids.into_iter().enumerate() {
            self.queue_active_progress(&id, ProgressTarget::List { request_id, index });
        }
    }

    /// Status rank used to group a `list_agents` result the way the TUI tree
    /// groups it: live work first, finished rows last.
    fn agent_status_rank(status: SubagentAgentStatus) -> u8 {
        match status {
            SubagentAgentStatus::Running | SubagentAgentStatus::Idle => 0,
            SubagentAgentStatus::Queued => 1,
            SubagentAgentStatus::Ready => 2,
            SubagentAgentStatus::Completed => 3,
            SubagentAgentStatus::Failed => 4,
            SubagentAgentStatus::Cancelled => 5,
        }
    }

    fn completed_agent_entry(&self, child: &CompletedChild) -> SubagentAgentEntry {
        let mut snapshot = self.completed_snapshot_for_query(child);
        let parent = child.request.parent_session_id.clone();
        self.stamp_lineage(&mut snapshot, &child.request.id, &parent);
        let mut entry = snapshot.agent_entry();
        // Lineage overrides the terminal snapshot's parent fallback so nested
        // children report their immediate spawner, matching the TUI tree.
        entry.parent_id = snapshot
            .parent_id
            .unwrap_or_else(|| child.request.id.clone());
        entry.depth = snapshot.depth.unwrap_or(1);
        // Resume eligibility is a coordinator decision (wake-eligible, success,
        // not workflow); the graph does not know it.
        let resumable =
            child.wake_eligible && child.result.success && !child.request.owner.is_workflow();
        entry.resumable = resumable;
        if resumable && entry.status == SubagentAgentStatus::Completed {
            entry.status = SubagentAgentStatus::Ready;
        }
        entry
    }

    fn request_agent_entry(
        &self,
        request: &SubagentRequest,
        _status: SubagentAgentStatus,
        elapsed_ms: u64,
        queue_position: Option<usize>,
    ) -> SubagentAgentEntry {
        let mut snapshot = SubagentSnapshot {
            subagent_id: request.id.clone(),
            label: request.label.clone(),
            description: request.description.clone(),
            subagent_type: request.subagent_type.clone(),
            status: SubagentSnapshotStatus::Initializing,
            started_at_epoch_ms: 0,
            duration_ms: elapsed_ms,
            persona: request.runtime_overrides.persona.clone(),
            model: request.runtime_overrides.model.clone(),
            parent_id: Some(request.parent_session_id.clone()),
            depth: Some(self.graph.depth_of(&request.id).unwrap_or(1)),
            queue_position,
            ..SubagentSnapshot::default()
        };
        if let Some(parent) = self.graph.direct_spawner(&request.id) {
            snapshot.parent_id = Some(parent.to_owned());
        }
        snapshot.agent_entry()
    }

    fn queue_agent_list_progress(&mut self, request_id: u64, index: usize, id: &str) {
        let Some(child) = self.active.get(id) else {
            self.finish_list_agents_slot(request_id, index, None);
            return;
        };
        let mut seed = running_seed(child);
        seed.parent_id = self
            .graph
            .direct_spawner(id)
            .unwrap_or(&child.request.parent_session_id)
            .to_owned();
        seed.depth = self.graph.depth_of(id).unwrap_or(1);
        self.progress.push(ProgressFuture {
            future: Box::pin(child.control.progress()),
            seed: Some(seed),
            target: Some(ProgressTarget::ListAgents { request_id, index }),
        });
    }

    pub(super) fn handle_list_agents(
        &mut self,
        parent_session_id: String,
        include_finished: bool,
        parent_id: Option<String>,
        limit: usize,
        respond_to: oneshot::Sender<Vec<SubagentAgentEntry>>,
    ) {
        let subtree_root = parent_id
            .as_deref()
            .filter(|id| *id != parent_session_id.as_str());
        if subtree_root.is_some_and(|root| !self.graph.is_reachable_from(root, &parent_session_id))
        {
            let _ = respond_to.send(Vec::new());
            return;
        }
        let in_scope = |coordinator: &Self, id: &str, workflow: bool| {
            !workflow
                && coordinator.graph.is_reachable_from(id, &parent_session_id)
                && subtree_root.is_none_or(|root| coordinator.graph.is_reachable_from(id, root))
        };

        struct Candidate {
            id: String,
            entry: Option<SubagentAgentEntry>,
            rank: u8,
            depth: u32,
            started_epoch_ms: u64,
        }

        let mut candidates: Vec<Candidate> = Vec::new();
        for child in self.active.values() {
            let id = &child.request.id;
            if !in_scope(self, id, child.request.owner.is_workflow()) {
                continue;
            }
            candidates.push(Candidate {
                id: id.clone(),
                entry: None,
                rank: Self::agent_status_rank(SubagentAgentStatus::Running),
                depth: self.graph.depth_of(id).unwrap_or(1),
                started_epoch_ms: 0,
            });
        }
        for child in self.pending.values() {
            let id = &child.request.id;
            if !in_scope(self, id, child.request.owner.is_workflow()) {
                continue;
            }
            let entry = self.request_agent_entry(
                &child.request,
                SubagentAgentStatus::Running,
                child.started_at.elapsed().as_millis() as u64,
                None,
            );
            candidates.push(Candidate {
                id: id.clone(),
                entry: Some(entry),
                rank: Self::agent_status_rank(SubagentAgentStatus::Running),
                depth: self.graph.depth_of(id).unwrap_or(1),
                started_epoch_ms: 0,
            });
        }
        for (position, queued) in self.queued.iter().enumerate() {
            let id = &queued.request.id;
            if !in_scope(self, id, queued.request.owner.is_workflow()) {
                continue;
            }
            let queue_position = self
                .queued
                .iter()
                .filter(|other| other.request.parent_session_id == queued.request.parent_session_id)
                .position(|other| other.request.id == *id)
                .map(|index| index + 1);
            let entry = self.request_agent_entry(
                &queued.request,
                SubagentAgentStatus::Queued,
                queued.queued_at.elapsed().as_millis() as u64,
                queue_position,
            );
            candidates.push(Candidate {
                id: id.clone(),
                entry: Some(entry),
                rank: Self::agent_status_rank(SubagentAgentStatus::Queued),
                depth: self.graph.depth_of(id).unwrap_or(1),
                started_epoch_ms: position as u64,
            });
        }
        if include_finished {
            for child in self.completed.values() {
                let id = &child.request.id;
                if !in_scope(self, id, child.request.owner.is_workflow()) {
                    continue;
                }
                let entry = self.completed_agent_entry(child);
                candidates.push(Candidate {
                    id: id.clone(),
                    rank: Self::agent_status_rank(entry.status),
                    depth: entry.depth,
                    started_epoch_ms: child.result.duration_ms,
                    entry: Some(entry),
                });
            }
        }

        candidates.sort_by(|a, b| {
            (a.rank, a.depth, a.started_epoch_ms, &a.id).cmp(&(
                b.rank,
                b.depth,
                b.started_epoch_ms,
                &b.id,
            ))
        });
        candidates.truncate(limit);

        let mut slots: Vec<Option<SubagentAgentEntry>> =
            candidates.iter().map(|c| c.entry.clone()).collect();
        let remaining = slots.iter().filter(|slot| slot.is_none()).count();
        if remaining == 0 {
            let _ = respond_to.send(slots.into_iter().flatten().collect());
            return;
        }

        let request_id = self.next_list_agents_request_id;
        self.next_list_agents_request_id = self.next_list_agents_request_id.wrapping_add(1);
        self.list_agents_requests.insert(
            request_id,
            ListAgentsRequest {
                slots: std::mem::take(&mut slots),
                remaining,
                respond_to,
            },
        );
        for (index, candidate) in candidates.into_iter().enumerate() {
            if candidate.entry.is_none() {
                self.queue_agent_list_progress(request_id, index, &candidate.id);
            }
        }
    }

    pub(super) fn queue_active_progress(&mut self, id: &str, target: ProgressTarget) {
        let Some(child) = self.active.get(id) else {
            match target {
                ProgressTarget::Query(tx) => {
                    let _ = tx.send(self.ready_snapshot(id));
                }
                ProgressTarget::Inspect(tx) => {
                    let value = self
                        .completed
                        .get(id)
                        .map(|child| self.completed_inspection_for_query(child));
                    let _ = tx.send(value);
                }
                ProgressTarget::List { request_id, index } => {
                    self.finish_list_slot(request_id, index, None);
                }
                ProgressTarget::ListAgents { request_id, index } => {
                    self.finish_list_agents_slot(request_id, index, None);
                }
            }
            return;
        };
        self.progress.push(ProgressFuture {
            future: Box::pin(child.control.progress()),
            seed: Some(running_seed(child)),
            target: Some(target),
        });
    }

    pub(super) fn finish_progress(
        &mut self,
        seed: RunningSeed,
        target: ProgressTarget,
        progress: SubagentProgress,
    ) {
        let still_active = self.active.contains_key(&seed.subagent_id);
        if !still_active {
            match target {
                ProgressTarget::Query(respond_to) => {
                    let _ = respond_to.send(self.ready_snapshot(&seed.subagent_id));
                }
                ProgressTarget::Inspect(respond_to) => {
                    let value = self
                        .completed
                        .get(&seed.subagent_id)
                        .map(|child| self.completed_inspection_for_query(child));
                    let _ = respond_to.send(value);
                }
                ProgressTarget::List { request_id, index } => {
                    self.finish_list_slot(request_id, index, None);
                }
                ProgressTarget::ListAgents { request_id, index } => {
                    self.finish_list_agents_slot(request_id, index, None);
                }
            }
            return;
        }
        match target {
            ProgressTarget::Query(respond_to) => {
                let _ = respond_to.send(Some(running_inspection(seed, progress).snapshot));
            }
            ProgressTarget::Inspect(respond_to) => {
                let _ = respond_to.send(Some(running_inspection(seed, progress)));
            }
            ProgressTarget::List { request_id, index } => {
                self.finish_list_slot(request_id, index, Some(running_inspection(seed, progress)));
            }
            ProgressTarget::ListAgents { request_id, index } => {
                let entry = running_agent_entry(&seed, &progress);
                self.finish_list_agents_slot(request_id, index, Some(entry));
            }
        }
    }

    fn finish_list_slot(
        &mut self,
        request_id: u64,
        index: usize,
        inspection: Option<SubagentInspection>,
    ) {
        let Some(request) = self.list_requests.get_mut(&request_id) else {
            return;
        };
        if let Some(slot) = request.slots.get_mut(index) {
            *slot = inspection;
        }
        request.remaining = request.remaining.saturating_sub(1);
        if request.remaining != 0 {
            return;
        }
        let Some(request) = self.list_requests.remove(&request_id) else {
            return;
        };
        let values = request.slots.into_iter().flatten().collect();
        let _ = request.respond_to.send(values);
    }

    fn finish_list_agents_slot(
        &mut self,
        request_id: u64,
        index: usize,
        entry: Option<SubagentAgentEntry>,
    ) {
        let Some(request) = self.list_agents_requests.get_mut(&request_id) else {
            return;
        };
        if let Some(slot) = request.slots.get_mut(index) {
            *slot = entry;
        }
        request.remaining = request.remaining.saturating_sub(1);
        if request.remaining != 0 {
            return;
        }
        let Some(request) = self.list_agents_requests.remove(&request_id) else {
            return;
        };
        let values = request.slots.into_iter().flatten().collect();
        let _ = request.respond_to.send(values);
    }
}
