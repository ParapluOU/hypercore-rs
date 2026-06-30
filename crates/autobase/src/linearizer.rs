use std::collections::{BTreeMap, BTreeSet};

use crate::*;
use crate::Confirm;

impl Linearizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of nodes currently in the DAG.
    pub fn len(&self) -> usize {
        self.deps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.deps.is_empty()
    }

    /// Whether a node at `id` has been delivered.
    pub fn contains(&self, id: &NodeId) -> bool {
        self.deps.contains_key(id)
    }

    /// Add `node`, referencing `heads` (the other writers' heads it saw). The
    /// same-writer predecessor `(key, seq - 1)` is added as a dependency
    /// automatically, so callers need only list cross-writer references.
    ///
    /// Enforces causal delivery: rejects a duplicate, a seq gap, or a head that has
    /// not been delivered. On success the DAG stays acyclic and causally closed.
    pub fn add(&mut self, node: NodeId, heads: &[NodeId]) -> Result<(), AddError> {
        if self.deps.contains_key(&node) {
            return Err(AddError::Duplicate(node));
        }

        let expected = self.next_seq.get(&node.key).copied().unwrap_or(0);
        if node.seq != expected {
            return Err(AddError::Gap {
                writer: node.key,
                expected,
                got: node.seq,
            });
        }

        // Every referenced head must already be delivered (the predecessor is
        // guaranteed present by the gap check above, so it is not re-checked here).
        for h in heads {
            if !self.deps.contains_key(h) {
                return Err(AddError::MissingHead(*h));
            }
        }

        let mut dep_set: BTreeSet<NodeId> = heads.iter().copied().collect();
        if node.seq > 0 {
            dep_set.insert(NodeId::new(node.key, node.seq - 1));
        }
        // A node never depends on itself; causal delivery already rules this out,
        // but be explicit so a self-referencing head can't smuggle in a cycle.
        dep_set.remove(&node);

        for d in &dep_set {
            self.dependents.entry(*d).or_default().insert(node);
        }
        self.deps.insert(node, dep_set);
        self.dependents.entry(node).or_default();
        self.next_seq.insert(node.key, node.seq + 1);

        Ok(())
    }

    /// The deterministic linearization: the **confirmed view first** (the precise
    /// consensus order — ADR-0043), then the remaining unconfirmed nodes in the same
    /// key tiebreak. So [`finalized`](Self::finalized) (the confirmed view) is always
    /// a true prefix of `order`. Causality is always respected and the result depends
    /// only on the node set, never on arrival order. When nothing is confirmed this is
    /// exactly [`plain_order`](Self::plain_order).
    pub fn order(&self) -> Vec<NodeId> {
        let confirmed = self.confirmed_view();
        if confirmed.is_empty() {
            return self.plain_order();
        }
        let emitted: BTreeSet<NodeId> = confirmed.iter().copied().collect();
        let remaining: BTreeSet<NodeId> = self
            .deps
            .keys()
            .filter(|n| !emitted.contains(n))
            .copied()
            .collect();
        let mut out = confirmed;
        out.extend(self.topo_key_order(&remaining, &emitted));
        out
    }

    /// Pure key-tiebreak topological linearization over *all* nodes (priority Kahn:
    /// at each step emit the causally-ready node with the smallest [`NodeId`]). The
    /// consensus-agnostic order; [`order`](Self::order) layers the confirmed view in
    /// front of it, and [`quorum_degree`](Self::quorum_degree) uses it as a plain
    /// topological order.
    fn plain_order(&self) -> Vec<NodeId> {
        // Remaining unsatisfied dependencies per node.
        let mut indegree: BTreeMap<NodeId, usize> =
            self.deps.iter().map(|(n, d)| (*n, d.len())).collect();

        // Ready frontier: causally-ready nodes, ordered by NodeId (key then seq).
        let mut frontier: BTreeSet<NodeId> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(n, _)| *n)
            .collect();

        let mut out = Vec::with_capacity(self.deps.len());
        while let Some(node) = frontier.pop_first() {
            out.push(node);
            if let Some(children) = self.dependents.get(&node) {
                for child in children {
                    let d = indegree
                        .get_mut(child)
                        .expect("dependent must be a known node");
                    *d -= 1;
                    if *d == 0 {
                        frontier.insert(*child);
                    }
                }
            }
        }

        out
    }

    /// Tails: nodes with no dependencies (the roots of the DAG). Mirrors upstream's
    /// `linearizer.tails` — useful for the quorum layer and as a structural check.
    pub fn tails(&self) -> BTreeSet<NodeId> {
        self.deps
            .iter()
            .filter(|(_, d)| d.is_empty())
            .map(|(n, _)| *n)
            .collect()
    }

    /// Designate the **indexers** — the writers whose references count as votes
    /// toward quorums (DESIGN.md "Indexing Writer"). Without this, [`finalized`]
    /// is always empty and [`quorum_degree`] is always 0: the linearizer still
    /// produces a deterministic order, but no prefix is ever *confirmed*.
    ///
    /// [`finalized`]: Self::finalized
    /// [`quorum_degree`]: Self::quorum_degree
    pub fn with_indexers(indexers: impl IntoIterator<Item = WriterKey>) -> Self {
        Self {
            indexers: Some(indexers.into_iter().collect()),
            ..Self::default()
        }
    }

    /// Majority of the indexer set (`⌊n/2⌋ + 1`), or `None` when quorum is disabled.
    fn majority(&self) -> Option<usize> {
        self.indexers.as_ref().map(|ix| ix.len() / 2 + 1)
    }

    /// Does node `a` causally see node `b` — i.e. is `b` in `a`'s causal history
    /// (inclusive of `a == b`)? This is the graph-reachability equivalent of
    /// upstream's `clock.includes`: votes and quorums are read from the DAG shape
    /// alone, never from a payload or a clock value a writer reports about itself.
    pub fn sees(&self, a: &NodeId, b: &NodeId) -> bool {
        if a == b {
            return true;
        }
        if !self.deps.contains_key(a) || !self.deps.contains_key(b) {
            return false;
        }
        let mut stack = vec![*a];
        let mut visited: BTreeSet<NodeId> = BTreeSet::new();
        while let Some(node) = stack.pop() {
            if let Some(ds) = self.deps.get(&node) {
                for d in ds {
                    if d == b {
                        return true;
                    }
                    if visited.insert(*d) {
                        stack.push(*d);
                    }
                }
            }
        }
        false
    }

    /// The **vector [`Clock`]** of `node`: for every writer, the count of its nodes
    /// in `node`'s causal history (inclusive of `node` itself). The clock form of
    /// [`sees`](Self::sees) — `self.clock(node).includes(k, len)` is exactly
    /// `self.sees(node, NodeId{k, len - 1})`. Returns an empty clock for an unknown
    /// node. This is the substrate the faithful `consensus.js` port reads from
    /// (ADR-0015); like `sees`, it is computed from DAG shape alone.
    pub fn clock(&self, node: &NodeId) -> Clock {
        let mut clock = Clock::default();
        if !self.deps.contains_key(node) {
            return clock;
        }
        // Walk the causal closure, recording the max seq (+1 = length) per writer.
        // Append-only deps guarantee the closure contains every lower seq of each
        // writer it reaches, so the max alone gives the correct length.
        let mut stack = vec![*node];
        let mut visited: BTreeSet<NodeId> = BTreeSet::new();
        visited.insert(*node);
        while let Some(v) = stack.pop() {
            clock.raise(v.key, v.seq + 1);
            if let Some(ds) = self.deps.get(&v) {
                for d in ds {
                    if visited.insert(*d) {
                        stack.push(*d);
                    }
                }
            }
        }
        clock
    }

    // ------------------------------------------------------------------------------
    // Faithful `consensus.js` port, stage 2 — the DAG predicates.
    //
    // Reimplemented over our DAG + [`Clock`] (not upstream's stateful BufferMap/Clock
    // machine), parameterised by `removed`: the per-writer length already confirmed
    // out of the prefix (upstream's `removed` clock + each writer's `indexed`/`offset`
    // watermark). An **empty** `removed` is the from-scratch base case. These feed the
    // confirmation predicates (stage 3). ADR-0042.
    // ------------------------------------------------------------------------------

    /// Whether indexer node `node` is a **merge**: it causally joins more than one
    /// independent indexer branch (upstream `_isMerge`). For each indexer we take the
    /// latest of its nodes that `node` saw (its own previous node for `node`'s own
    /// writer), skip any already-confirmed, and ask whether ≥ 2 of those heads are
    /// mutually independent (an antichain of size > 1).
    fn is_merge(&self, node: &NodeId, removed: &Clock) -> bool {
        let Some(indexers) = self.indexers.as_ref() else {
            return false;
        };
        if !indexers.contains(&node.key) {
            return false;
        }
        let clk = self.clock(node);
        let mut heads: Vec<NodeId> = Vec::new();
        for &idx in indexers {
            // seq = (latest length of idx seen) - 1, minus one more for node's own writer.
            let seq = clk.get(&idx) as i64 - 1 - if idx == node.key { 1 } else { 0 };
            if seq < 0 {
                continue;
            }
            let head = NodeId::new(idx, seq as u64);
            if !self.contains(&head) || removed.includes(&idx, seq as u64 + 1) {
                continue;
            }
            heads.push(head);
        }
        // Count the maximal heads — those no other head sees (older heads are
        // subsumed). > 1 mutually-independent maximal head ⇒ a merge.
        let maximal = heads
            .iter()
            .filter(|h| !heads.iter().any(|o| *o != **h && self.sees(o, h)))
            .count();
        maximal > 1
    }

    /// The **indexer tails**: the oldest not-yet-confirmed node of each indexer,
    /// reduced to the minimal antichain (upstream `_indexerTails`). These are the
    /// frontier the confirmation machine tries to yield next.
    fn indexer_tails(&self, removed: &Clock) -> BTreeSet<NodeId> {
        let Some(indexers) = self.indexers.as_ref() else {
            return BTreeSet::new();
        };
        let mut tails: Vec<NodeId> = Vec::new();
        for &idx in indexers {
            let length = removed.get(&idx); // first un-confirmed node of this indexer
            let head = NodeId::new(idx, length);
            if !self.contains(&head) || removed.includes(&head.key, head.seq + 1) {
                continue;
            }
            let mut is_tail = true;
            let mut remove: Vec<NodeId> = Vec::new();
            for &t in &tails {
                if self.sees(&head, &t) {
                    is_tail = false; // head is newer than an existing tail ⇒ not minimal
                    break;
                }
                if self.sees(&t, &head) {
                    remove.push(t); // existing tail is newer than head ⇒ head supersedes it
                }
            }
            tails.retain(|t| !remove.contains(t));
            if is_tail {
                tails.push(head);
            }
        }
        tails.into_iter().collect()
    }

    /// Whether `parent` is **strictly newer** than `object` (upstream
    /// `_strictlyNewer`): `parent` sees `object`, and every writer `parent` saw
    /// beyond `object` did so only through nodes that themselves acknowledge
    /// `object` — i.e. `parent`'s extra knowledge contains nothing concurrent to
    /// `object`. This is the ambiguity guard that keeps a vote from being
    /// double-counted across a fork.
    fn strictly_newer(&self, object: &NodeId, parent: &NodeId, removed: &Clock) -> bool {
        if !self.sees(parent, object) {
            return false;
        }
        let pclk = self.clock(parent);
        let oclk = self.clock(object);
        for (key, latest) in pclk.iter() {
            let oldest = removed.get(&key);
            if latest <= oldest {
                continue;
            }
            let length = oclk.get(&key).max(oldest);
            if latest < length {
                return false; // sanity — can't happen once parent sees object
            }
            if latest == length {
                continue; // both saw the same amount of this writer
            }
            // parent saw strictly more of `key` than object: the next node object
            // didn't see must itself already acknowledge object, else it's ambiguous.
            let next = NodeId::new(key, length);
            if !self.contains(&next) {
                continue; // confirmed away / doesn't exist
            }
            if self.sees(&next, object) {
                continue;
            }
            return false;
        }
        true
    }

    /// The **acks** of `target` (upstream `_acks`): the indexer nodes that vote for
    /// it — `target` itself if its writer is an indexer, plus, for each other
    /// indexer, the first of its nodes that did *not* see `target` already but whose
    /// node at `target`'s frontier sees `target` and is [`strictly_newer`]. The
    /// majority test for confirmation counts these.
    ///
    /// [`strictly_newer`]: Self::strictly_newer
    fn acks(&self, target: &NodeId, removed: &Clock) -> Vec<NodeId> {
        let Some(indexers) = self.indexers.as_ref() else {
            return Vec::new();
        };
        let mut acks: Vec<NodeId> = Vec::new();
        if indexers.contains(&target.key) {
            acks.push(*target);
        }
        let tclk = self.clock(target);
        for &idx in indexers {
            if idx == target.key {
                continue;
            }
            let next = tclk.get(&idx).max(removed.get(&idx));
            let node = NodeId::new(idx, next);
            if !self.contains(&node) {
                continue;
            }
            if !self.sees(&node, target) {
                continue;
            }
            if !self.strictly_newer(target, &node, removed) {
                continue;
            }
            acks.push(node);
        }
        acks
    }

    // ------------------------------------------------------------------------------
    // Faithful `consensus.js` port, stage 3 — the confirmation predicates.
    // ADR-0042. Still alongside the conservative `finalized()`; stage 4 wires the
    // `shift` driver and swaps the public confirmed prefix onto this machine.
    // ------------------------------------------------------------------------------

    /// Number of nodes writer `key` has (`indexer.length` upstream) — the next
    /// expected seq.
    fn indexer_length(&self, key: &WriterKey) -> u64 {
        self.next_seq.get(key).copied().unwrap_or(0)
    }

    /// How `indexer` (scanning its nodes below `length`) relates to confirming
    /// `target` given `acks` (upstream `confirms`). Walks the indexer's nodes
    /// newest-first for one that is [`strictly_newer`](Self::strictly_newer) than
    /// `target` and sees a majority of `acks`. The bisect *optimisation* upstream
    /// uses to skip the non-strictly-newer cluster is omitted — scanning every node
    /// is behaviour-identical.
    fn confirms(
        &self,
        indexer: WriterKey,
        target: &NodeId,
        acks: &[NodeId],
        length: u64,
        removed: &Clock,
    ) -> Confirm {
        let Some(majority) = self.majority() else {
            return Confirm::Unseen;
        };
        if length == 0 || removed.get(&indexer) >= length {
            return Confirm::Unseen;
        }
        let mut newer = true;
        let mut i = length as i64 - 1;
        while i >= 0 {
            let head = NodeId::new(indexer, i as u64);
            if !self.contains(&head) {
                return Confirm::Unseen;
            }
            let mut seen = 0usize;
            for a in acks {
                if self.sees(&head, a) {
                    seen += 1;
                    if seen >= majority {
                        break;
                    }
                }
            }
            if !newer && seen < majority {
                break;
            }
            if !self.strictly_newer(target, &head, removed) {
                newer = false;
                i -= 1;
                continue;
            } else if seen < majority {
                return Confirm::Newer;
            }
            return Confirm::Acked;
        }
        Confirm::Unseen
    }

    /// Whether a majority of `acks` are seen by `parent` (upstream `_ackedAt`).
    fn acked_at(&self, acks: &[NodeId], parent: &NodeId) -> bool {
        let Some(majority) = self.majority() else {
            return false;
        };
        let mut seen = 0usize;
        let mut missing = acks.len();
        for node in acks {
            missing -= 1;
            if !self.sees(parent, node) {
                if seen + missing < majority {
                    return false;
                }
                continue;
            }
            seen += 1;
            if seen >= majority {
                return true;
            }
        }
        false
    }

    /// Whether `target` is **confirmed** (upstream `_isConfirmed`): it has a
    /// majority of acks and either a majority of indexers `Acked`-confirm it, or —
    /// at the top level (`parent == None`) — every indexer is at least `Newer`
    /// (none `Unseen`). With a `parent`, defers to
    /// [`is_confirmable_at`](Self::is_confirmable_at) (used by the stage-4 merge
    /// walk). This is the precise rule the conservative `finalized()` approximates.
    fn is_confirmed(&self, target: &NodeId, parent: Option<&NodeId>, removed: &Clock) -> bool {
        let Some(majority) = self.majority() else {
            return false;
        };
        let indexers = self.indexers.as_ref().expect("majority ⇒ indexers");
        let acks = self.acks(target, removed);
        if acks.len() < majority {
            return false;
        }
        let mut confs: BTreeSet<WriterKey> = BTreeSet::new();
        let mut all_newer = true;
        for &indexer in indexers {
            let length = match parent {
                // parent.length - 1 for its own writer, else what parent saw of it
                Some(p) if p.key == indexer => p.seq,
                Some(p) => self.clock(p).get(&indexer),
                None => self.indexer_length(&indexer),
            };
            match self.confirms(indexer, target, &acks, length, removed) {
                Confirm::Acked => {
                    confs.insert(indexer);
                    if confs.len() >= majority {
                        return true;
                    }
                }
                Confirm::Unseen => all_newer = false,
                Confirm::Newer => {}
            }
        }
        match parent {
            Some(p) => self.is_confirmable_at(target, p, &acks, &confs, removed),
            None => all_newer,
        }
    }

    /// Whether `target` can still be confirmed *relative to a parent* given the
    /// indexers that already `confs` it (upstream `_isConfirmableAt`): the acks are
    /// acked at `parent`, and enough not-yet-confirming indexers remain that could
    /// still confirm it (those `target` already saw, or whose next node is
    /// strictly-newer).
    fn is_confirmable_at(
        &self,
        target: &NodeId,
        parent: &NodeId,
        acks: &[NodeId],
        confs: &BTreeSet<WriterKey>,
        removed: &Clock,
    ) -> bool {
        let Some(majority) = self.majority() else {
            return false;
        };
        let indexers = self.indexers.as_ref().expect("majority ⇒ indexers");
        if !self.acked_at(acks, parent) {
            return false;
        }
        let mut potential = confs.len();
        let pclk = self.clock(parent);
        let tclk = self.clock(target);
        for &indexer in indexers {
            if confs.contains(&indexer) {
                continue;
            }
            let length = pclk.get(&indexer);
            let is_seen = tclk.includes(&indexer, length);
            if !is_seen && length >= 1 {
                let head = NodeId::new(indexer, length - 1);
                if self.contains(&head)
                    && !removed.includes(&head.key, head.seq + 1)
                    && !self.strictly_newer(target, &head, removed)
                {
                    continue;
                }
            }
            potential += 1;
            if potential >= majority {
                return true;
            }
        }
        false
    }

    // ------------------------------------------------------------------------------
    // Faithful `consensus.js` port, stage 4 — the `shift` driver.
    //
    // `confirmed_prefix` drives `shift` from scratch (an empty `removed` clock that
    // grows as nodes are confirmed) until nothing more confirms, accumulating the
    // indexed sequence. This is the precise form of `finalized()`; the *swap* onto
    // it (reconciling with `order()` + re-validating the convergence sim) is the
    // closing step. ADR-0042.
    // ------------------------------------------------------------------------------

    /// The currently-unconfirmed indexer **merge** nodes, in deterministic
    /// ([`NodeId`]) order (upstream's `merges` set, recomputed).
    fn merge_nodes(&self, removed: &Clock) -> Vec<NodeId> {
        let Some(indexers) = self.indexers.as_ref() else {
            return Vec::new();
        };
        self.deps
            .keys()
            .filter(|n| indexers.contains(&n.key) && !removed.includes(&n.key, n.seq + 1))
            .filter(|n| self.is_merge(n, removed))
            .copied()
            .collect()
    }

    /// The tails (from `tails`) that `node` causally sees (upstream `_tails`).
    fn tails_seen(&self, node: &NodeId, tails: &BTreeSet<NodeId>) -> Vec<NodeId> {
        tails.iter().filter(|t| self.sees(node, t)).copied().collect()
    }

    /// `_tailsAndMerges`: the seen tails plus any other merge node `node` sees.
    fn tails_and_merges_seen(
        &self,
        node: &NodeId,
        tails: &BTreeSet<NodeId>,
        removed: &Clock,
    ) -> Vec<NodeId> {
        let mut all = self.tails_seen(node, tails);
        for m in self.merge_nodes(removed) {
            if m != *node && self.sees(node, &m) && !all.contains(&m) {
                all.push(m);
            }
        }
        all
    }

    /// Resolve a confirmed merge down to the next node(s) to yield (upstream
    /// `_yieldNext`): walk toward the tails, descending into whichever sub-arm is
    /// confirmed *relative to the current node*; when none is, yield every tail
    /// below it; on reaching a tail, yield it.
    fn yield_next(&self, mut node: NodeId, tails: &BTreeSet<NodeId>, removed: &mut Clock) -> Vec<NodeId> {
        while !tails.contains(&node) {
            let mut next = None;
            for t in self.tails_and_merges_seen(&node, tails, removed) {
                if self.is_confirmed(&t, Some(&node), removed) {
                    next = Some(t);
                    break;
                }
            }
            if let Some(t) = next {
                node = t;
                continue;
            }
            let arm = self.tails_seen(&node, tails);
            for t in &arm {
                removed.raise(t.key, t.seq + 1);
            }
            return arm;
        }
        removed.raise(node.key, node.seq + 1);
        vec![node]
    }

    /// One `shift`: yield the next confirmed tail, or resolve a confirmed merge, or
    /// nothing. Mutates `removed` for the nodes it yields.
    fn shift_once(&self, removed: &mut Clock) -> Vec<NodeId> {
        let tails = self.indexer_tails(removed);
        for tail in &tails {
            if self.is_confirmed(tail, None, removed) {
                removed.raise(tail.key, tail.seq + 1);
                return vec![*tail];
            }
        }
        for merge in self.merge_nodes(removed) {
            if self.is_confirmed(&merge, None, removed) {
                return self.yield_next(merge, &tails, removed);
            }
        }
        Vec::new()
    }

    /// The **confirmed indexer sequence** via the faithful `consensus.js` machine:
    /// drive [`shift`](Self::shift_once) from scratch until nothing more confirms.
    /// This is a complete port of `consensus.shift` — it confirms a merge-resolved
    /// fork arm the conservative [`finalized`](Self::finalized) defers, and it
    /// converges across delivery orders.
    ///
    /// It is **not yet the full indexed view**, and **not** the live finalization
    /// (ADR-0043): (1) `consensus.shift` yields only **indexer** nodes — the
    /// non-indexer nodes are woven in by upstream's `_yield`/`Topolist.add`, not yet
    /// ported, so this omits non-indexer deps; and (2) its consensus *yield* order
    /// differs from [`order`](Self::order)'s key tiebreak (ADR-0014), so it is not a
    /// prefix of `order()`. Both are the remaining work before swapping `finalized`
    /// onto the precise machine.
    pub fn confirmed_prefix(&self) -> Vec<NodeId> {
        if self.majority().is_none() {
            return Vec::new();
        }
        let mut removed = Clock::default();
        let mut out = Vec::new();
        loop {
            let batch = self.shift_once(&mut removed);
            if batch.is_empty() {
                break;
            }
            out.extend(batch);
        }
        out
    }

    /// The **full indexed view**: [`confirmed_prefix`](Self::confirmed_prefix) with the
    /// non-indexer nodes woven in — the faithful port of `linearizer.js`'s `_yield`.
    ///
    /// Each `shift` confirms a batch of indexer nodes; this expands every batch to its
    /// newly-covered causal closure (so a confirmed indexer node's non-indexer
    /// dependencies are included) and emits that closure in our key-tiebreak
    /// topological order (upstream's `Topolist` uses the *same* lowest-key tiebreak,
    /// `cmpUnlinked`). Batches only ever append, so the result is **monotone** under
    /// growth and is **causally closed over all deps** — the prefix that
    /// [`order`](Self::order) is being aligned to (ADR-0043 / the swap).
    pub fn confirmed_view(&self) -> Vec<NodeId> {
        if self.majority().is_none() {
            return Vec::new();
        }
        let mut removed = Clock::default();
        let mut emitted: BTreeSet<NodeId> = BTreeSet::new();
        let mut out: Vec<NodeId> = Vec::new();
        loop {
            let batch = self.shift_once(&mut removed);
            if batch.is_empty() {
                break;
            }
            // The newly-covered nodes: the causal closure of this batch not yet emitted.
            let mut fresh: BTreeSet<NodeId> = BTreeSet::new();
            let mut stack = batch;
            while let Some(n) = stack.pop() {
                if emitted.contains(&n) || !fresh.insert(n) {
                    continue;
                }
                if let Some(ds) = self.deps.get(&n) {
                    for d in ds {
                        if !emitted.contains(d) {
                            stack.push(*d);
                        }
                    }
                }
            }
            // Emit the batch in key-tiebreak topological order (deps already emitted
            // count as satisfied), then mark them emitted.
            for node in self.topo_key_order(&fresh, &emitted) {
                emitted.insert(node);
                out.push(node);
            }
        }
        out
    }

    /// Priority-Kahn (lowest [`NodeId`] first) over the node subset `nodes`, treating
    /// any dependency already in `emitted` as satisfied. The within-batch ordering for
    /// [`confirmed_view`](Self::confirmed_view) — the same tiebreak as
    /// [`order`](Self::order).
    fn topo_key_order(&self, nodes: &BTreeSet<NodeId>, emitted: &BTreeSet<NodeId>) -> Vec<NodeId> {
        let mut indeg: BTreeMap<NodeId, usize> = nodes
            .iter()
            .map(|n| {
                let unmet = self
                    .deps
                    .get(n)
                    .map(|ds| ds.iter().filter(|d| nodes.contains(d) && !emitted.contains(d)).count())
                    .unwrap_or(0);
                (*n, unmet)
            })
            .collect();
        let mut frontier: BTreeSet<NodeId> =
            indeg.iter().filter(|(_, d)| **d == 0).map(|(n, _)| *n).collect();
        let mut out = Vec::with_capacity(nodes.len());
        while let Some(node) = frontier.pop_first() {
            out.push(node);
            for dep in self.dependents.get(&node).into_iter().flatten() {
                if let Some(d) = indeg.get_mut(dep) {
                    *d -= 1;
                    if *d == 0 {
                        frontier.insert(*dep);
                    }
                }
            }
        }
        out
    }

    /// The **quorum degree** achieved over `target` (DESIGN.md "Quorums").
    ///
    /// A **vote** is a reference from an indexer to a node (at most one per
    /// indexer, counted by causal reachability). Degree 1 — a *single quorum* —
    /// means a majority of indexers reference `target`. Degree 2 — a *double
    /// quorum* — means a majority of indexers each reference a degree-1 quorum
    /// over `target`; and so on, recursively. The result is the highest degree
    /// any node in the DAG witnesses over `target`.
    ///
    /// Returns 0 when quorum is disabled, the target is unknown, or not even a
    /// single quorum has formed.
    ///
    /// Computed by a single bottom-up pass over a topological order: for every
    /// node `v` we track, per indexer `w`, the best degree reached by any node of
    /// `w` in `v`'s causal closure (`bestdeg`). `v` then witnesses degree `k` over
    /// `target` once a majority of indexers vouch the previous level — `v`'s own
    /// author vouching every level up to `v`'s degree. This recomputes from
    /// scratch rather than maintaining upstream's incremental `Consensus` machine
    /// (ADR-0015), so determinism is manifest.
    pub fn quorum_degree(&self, target: &NodeId) -> usize {
        let Some(m) = self.majority() else {
            return 0;
        };
        let indexers = self.indexers.as_ref().expect("majority ⇒ indexers set");
        if !self.deps.contains_key(target) {
            return 0;
        }

        // bestdeg[v][w] = max degree-over-target reached by any node of indexer
        // `w` within `v`'s causal closure (only nodes that see `target`).
        let mut bestdeg: BTreeMap<NodeId, BTreeMap<WriterKey, i64>> = BTreeMap::new();
        // seesx[v] = whether `v` causally sees `target`.
        let mut seesx: BTreeMap<NodeId, bool> = BTreeMap::new();
        let mut max_degree: i64 = 0;

        // plain_order() is a valid topological order: every dependency precedes its
        // node. (We use the consensus-agnostic order here to stay independent of the
        // confirmed view, which is itself built without quorum_degree.)
        for v in self.plain_order() {
            let mut pre: BTreeMap<WriterKey, i64> = BTreeMap::new();
            let mut sees_target = v == *target;
            if let Some(ds) = self.deps.get(&v) {
                for d in ds {
                    if seesx.get(d).copied().unwrap_or(false) {
                        sees_target = true;
                    }
                    if let Some(bd) = bestdeg.get(d) {
                        for (w, deg) in bd {
                            let e = pre.entry(*w).or_insert(*deg);
                            if *deg > *e {
                                *e = *deg;
                            }
                        }
                    }
                }
            }
            seesx.insert(v, sees_target);

            let author = v.key;
            let author_is_indexer = indexers.contains(&author);
            let mut deg_v: i64 = 0;
            if sees_target {
                // `v`'s author vouches every level up to `v`'s own degree, so it
                // contributes +1 at the level under test (the loop only reaches
                // level k-1 after confirming the degree is already ≥ k-1).
                let self_vote = if author_is_indexer { 1 } else { 0 };
                let mut d = 1i64;
                loop {
                    let level = d - 1;
                    let mut count = 0usize;
                    for w in indexers.iter() {
                        if *w == author {
                            continue;
                        }
                        if pre.get(w).map(|pd| *pd >= level).unwrap_or(false) {
                            count += 1;
                        }
                    }
                    if count + self_vote >= m {
                        deg_v = d;
                        d += 1;
                    } else {
                        break;
                    }
                }
            }

            // Record this node's own vote (only an indexer that sees the target).
            let mut bd = pre;
            if sees_target && author_is_indexer {
                let e = bd.entry(author).or_insert(deg_v);
                if deg_v > *e {
                    *e = deg_v;
                }
            }
            bestdeg.insert(v, bd);

            if sees_target && deg_v > max_degree {
                max_degree = deg_v;
            }
        }

        max_degree.max(0) as usize
    }

    /// The **finalized (indexed) prefix**: the precise confirmed view from the
    /// faithful `consensus.js` machine ([`confirmed_view`](Self::confirmed_view)) —
    /// every node a majority of indexers have confirmed (including merge-resolved fork
    /// arms the old conservative rule deferred), woven with the non-indexer nodes they
    /// cover, in the consensus order. A true prefix of [`order`](Self::order) by
    /// construction, and monotone under cooperative growth (the **finality-stability**
    /// property; ADR-0042/0043). Supersedes the earlier conservative no-active-fork
    /// rule (kept as a test oracle).
    pub fn finalized(&self) -> Vec<NodeId> {
        self.confirmed_view()
    }

    // ----------------------------------------------------------------------------------
    // View materialization (upstream autobase's `view`).
    //
    // Upstream linearizes the DAG and then *applies* each node to materialize a `view`
    // (a hypercore the consumer reads): `view.length` is the total materialized length,
    // `view.get(i)` reads entry `i`, and `getIndexedViewLength` (DESIGN.md "Indexing")
    // reports how much of that view is **confirmed** — the indexed prefix that can never
    // reorder. The apply step is where *domain* logic lives, so at L1 there is nothing to
    // apply: this layer is content-blind. The domain-agnostic materialization is therefore
    // the identity one — **each node contributes exactly one entry, its own [`NodeId`]** —
    // so the "view" is just the linearization and the "indexed view" is the finalized
    // prefix. A consuming application replays `view()` through its own apply function to
    // build the real, typed view; the ordering/confirmation it relies on lives here.
    // See ADR-0028.
    // ----------------------------------------------------------------------------------

    /// The **materialized view**: the linearization rendered as an indexable sequence of
    /// entries (≡ upstream `view`). At L1 each node is one entry (its [`NodeId`]), so this
    /// equals [`order`](Self::order); a domain consumer folds these through its own apply
    /// function to produce a typed view.
    pub fn view(&self) -> Vec<NodeId> {
        self.order()
    }

    /// Length of the materialized [`view`](Self::view) (≡ upstream `view.length`). One node
    /// = one entry at L1, so this is the node count.
    pub fn view_len(&self) -> usize {
        self.len()
    }

    /// Entry `i` of the materialized [`view`](Self::view), or `None` past the end
    /// (≡ upstream `view.get(i, { wait: false })` returning `null` beyond `view.length`).
    pub fn view_get(&self, i: usize) -> Option<NodeId> {
        self.order().into_iter().nth(i)
    }

    /// The **indexed view**: the confirmed prefix of the [`view`](Self::view) that can
    /// never reorder — the [`finalized`](Self::finalized) prefix, which is always a prefix
    /// of [`view`](Self::view).
    pub fn indexed_view(&self) -> Vec<NodeId> {
        self.finalized()
    }

    /// Length of the [`indexed_view`](Self::indexed_view) (≡ upstream
    /// `getIndexedViewLength` — `getIndexedInfo().views[].length`). Always
    /// `<= view_len()`.
    ///
    /// This is the **precise** confirmation depth from the faithful `consensus.js`
    /// machine ([`finalized`](Self::finalized) / [`confirmed_view`](Self::confirmed_view)):
    /// it confirms across resolved fork/merges, not just fork-free chains (ADR-0042/0043
    /// superseded the earlier conservative depth of ADR-0015).
    pub fn indexed_view_len(&self) -> usize {
        self.finalized().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer key built so that `wk(1) < wk(2) < wk(3)` byte-lexicographically —
    /// i.e. writers a < b < c, matching `reference/js/autobase/DESIGN.md`.
    fn wk(tag: u8) -> WriterKey {
        [tag; 32]
    }

    fn n(key: u8, seq: u64) -> NodeId {
        NodeId::new(wk(key), seq)
    }

    // Writers a=1, b=2, c=3.
    const A: u8 = 1;
    const B: u8 = 2;
    const C: u8 = 3;

    fn add(lin: &mut Linearizer, node: NodeId, heads: &[NodeId]) {
        lin.add(node, heads).expect("causal add must succeed");
    }

    // ---- vector clocks (consensus.js port, stage 1) -------------------------

    // The clock is the reachability form of `sees`: over a fork+merge DAG, for
    // every node, `clock.includes(k, len)` must agree with `sees(.., NodeId{k,
    // len-1})`, and `clock.get(k)` must equal 1 + the highest seq of `k` it sees.
    #[test]
    fn clock_includes_agrees_with_sees_over_a_fork_merge_dag() {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[]); // a0, b0 concurrent (a fork)
        add(&mut lin, n(C, 0), &[n(A, 0)]);
        add(&mut lin, n(A, 1), &[n(C, 0)]);
        add(&mut lin, n(C, 1), &[n(B, 0)]); // c1: pred c0 + cross-ref b0 → a merge
        add(&mut lin, n(B, 1), &[n(C, 1)]);

        let nodes = lin.order();
        for a in &nodes {
            let clk = lin.clock(a);
            for b in &nodes {
                assert_eq!(
                    clk.includes(&b.key, b.seq + 1),
                    lin.sees(a, b),
                    "clock/sees disagree: {a:?} sees {b:?}?"
                );
            }
            for &tag in &[A, B, C] {
                let key = wk(tag);
                let expected = nodes
                    .iter()
                    .filter(|m| m.key == key && lin.sees(a, m))
                    .map(|m| m.seq + 1)
                    .max()
                    .unwrap_or(0);
                assert_eq!(clk.get(&key), expected, "clock.get({tag}) at {a:?}");
            }
        }
    }

    #[test]
    fn clock_on_the_design_chain() {
        // a0 - b0 - c0 - a1, each seeing the previous.
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[n(A, 0)]);
        add(&mut lin, n(C, 0), &[n(B, 0)]);
        add(&mut lin, n(A, 1), &[n(C, 0)]);

        // a1 sees {a0,a1}, b0, c0.
        let c = lin.clock(&n(A, 1));
        assert_eq!(c.get(&wk(A)), 2);
        assert_eq!(c.get(&wk(B)), 1);
        assert_eq!(c.get(&wk(C)), 1);
        assert_eq!(c.writers(), 3);
        assert!(c.includes(&wk(A), 2) && c.includes(&wk(B), 1) && c.includes(&wk(C), 1));
        assert!(!c.includes(&wk(A), 3), "a1 hasn't seen a2");
        assert!(!c.includes(&wk(A), 0), "length 0 is vacuously not-included");

        // b0 sees only a0, b0 — never c.
        let cb = lin.clock(&n(B, 0));
        assert_eq!((cb.get(&wk(A)), cb.get(&wk(B)), cb.get(&wk(C))), (1, 1, 0));
        assert!(!cb.includes(&wk(C), 1));
    }

    #[test]
    fn clock_of_unknown_node_is_empty() {
        let lin = indexed(&[A, B, C]);
        let c = lin.clock(&n(A, 0));
        assert_eq!(c.writers(), 0);
        assert_eq!(c.get(&wk(A)), 0);
        assert!(!c.includes(&wk(A), 1));
    }

    // ---- consensus.js predicates (stage 2) ----------------------------------

    fn empty() -> Clock {
        Clock::default()
    }

    // a0,b0 concurrent (a fork); c0 sees a0; a1 sees c0; c1 merges c0 & b0; b1 sees c1.
    fn fork_merge() -> Linearizer {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[]);
        add(&mut lin, n(C, 0), &[n(A, 0)]);
        add(&mut lin, n(A, 1), &[n(C, 0)]);
        add(&mut lin, n(C, 1), &[n(B, 0)]); // pred c0 + cross-ref b0 → a merge
        add(&mut lin, n(B, 1), &[n(C, 1)]);
        lin
    }

    fn chain() -> Linearizer {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[n(A, 0)]);
        add(&mut lin, n(C, 0), &[n(B, 0)]);
        add(&mut lin, n(A, 1), &[n(C, 0)]);
        lin
    }

    /// The superseded conservative no-active-fork rule, kept as an oracle: the maximal
    /// `plain_order()` prefix of comparable-to-all, double-quorum'd nodes. The live
    /// `finalized()` (the precise consensus machine) must always be at least this eager.
    fn conservative_finalized(lin: &Linearizer) -> Vec<NodeId> {
        let order = lin.plain_order();
        let mut out = Vec::new();
        for node in &order {
            let comparable = order
                .iter()
                .all(|o| o == node || lin.sees(node, o) || lin.sees(o, node));
            if comparable && lin.quorum_degree(node) >= 2 {
                out.push(*node);
            } else {
                break;
            }
        }
        out
    }

    #[test]
    fn is_merge_identifies_the_fork_join_only() {
        let lin = fork_merge();
        assert!(lin.is_merge(&n(C, 1), &empty()), "c1 joins the b-branch and the a/c-branch");
        for node in [n(A, 0), n(B, 0), n(C, 0), n(A, 1), n(B, 1)] {
            assert!(!lin.is_merge(&node, &empty()), "{node:?} is not a merge");
        }
        let c = chain();
        for node in [n(A, 0), n(B, 0), n(C, 0), n(A, 1)] {
            assert!(!c.is_merge(&node, &empty()), "no merges in a linear chain");
        }
    }

    #[test]
    fn indexer_tails_are_the_minimal_frontier() {
        assert_eq!(
            chain().indexer_tails(&empty()),
            BTreeSet::from([n(A, 0)]),
            "a single tail in a chain"
        );
        assert_eq!(
            fork_merge().indexer_tails(&empty()),
            BTreeSet::from([n(A, 0), n(B, 0)]),
            "two tails across a fork"
        );
    }

    #[test]
    fn acks_of_a_chain_tail_are_all_indexers() {
        let lin = chain();
        let mut acks = lin.acks(&n(A, 0), &empty());
        acks.sort();
        assert_eq!(
            acks,
            vec![n(A, 0), n(B, 0), n(C, 0)],
            "a0 is acked by itself (a) and the strictly-newer b0, c0"
        );
    }

    #[test]
    fn strictly_newer_basics_and_invariant() {
        let lin = chain();
        assert!(
            lin.strictly_newer(&n(A, 0), &n(C, 0), &empty()),
            "c0 is strictly newer than a0"
        );
        assert!(
            lin.strictly_newer(&n(A, 0), &n(A, 0), &empty()),
            "reflexive: a node is strictly newer than itself"
        );

        let fm = fork_merge();
        assert!(
            !fm.strictly_newer(&n(B, 0), &n(A, 0), &empty()),
            "a0 doesn't even see b0, so it can't be strictly newer"
        );
        // invariant: strictly_newer(object, parent) ⇒ sees(parent, object).
        let nodes = fm.order();
        for o in &nodes {
            for p in &nodes {
                if fm.strictly_newer(o, p, &empty()) {
                    assert!(fm.sees(p, o), "strictly_newer({o:?}, {p:?}) but !sees");
                }
            }
        }
    }

    // ---- consensus.js confirmation (stage 3) --------------------------------

    // On a fork-free DAG the precise `is_confirmed` set must coincide exactly with
    // the conservative `finalized()` prefix — the strongest available cross-check.
    #[test]
    fn is_confirmed_matches_double_quorum_on_a_chain() {
        let lin = chain();
        let fin: BTreeSet<NodeId> = lin.finalized().into_iter().collect();
        for node in lin.order() {
            assert_eq!(
                lin.is_confirmed(&node, None, &empty()),
                fin.contains(&node),
                "is_confirmed vs finalized disagree for {node:?} (fork-free ⇒ equal)"
            );
        }
        assert!(lin.is_confirmed(&n(A, 0), None, &empty()));
        assert!(lin.is_confirmed(&n(B, 0), None, &empty()));
        assert!(!lin.is_confirmed(&n(C, 0), None, &empty()), "c0 only single-quorum");
        assert!(!lin.is_confirmed(&n(A, 1), None, &empty()));
    }

    // DESIGN "Condition for Consistency": competing single quorums never confirm.
    #[test]
    fn is_confirmed_rejects_competing_single_quorums() {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(C, 0), &[]);
        add(&mut lin, n(C, 1), &[n(A, 0)]);
        add(&mut lin, n(B, 0), &[n(C, 0)]);
        assert!(!lin.is_confirmed(&n(A, 0), None, &empty()));
        assert!(!lin.is_confirmed(&n(C, 0), None, &empty()));
    }

    // DESIGN "Higher Quorums": the double quorum over c0 confirms it; b0 (single) not.
    #[test]
    fn is_confirmed_on_the_higher_quorum_example() {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(C, 0), &[]);
        add(&mut lin, n(B, 0), &[n(C, 0)]);
        add(&mut lin, n(C, 1), &[n(B, 0)]);
        assert!(lin.is_confirmed(&n(C, 0), None, &empty()), "double quorum confirms c0");
        assert!(!lin.is_confirmed(&n(B, 0), None, &empty()), "b0 only single-quorum");
    }

    // The precise machine is never *less* eager than the conservative baseline:
    // everything `finalized()` commits, `is_confirmed` also confirms — even across a
    // fork.
    #[test]
    fn finalized_is_a_subset_of_confirmed_across_a_fork() {
        let lin = fork_merge();
        for node in lin.finalized() {
            assert!(
                lin.is_confirmed(&node, None, &empty()),
                "{node:?} finalized ⇒ confirmed"
            );
        }
    }

    // ---- consensus.js shift driver (stage 4) --------------------------------

    // A fork merged and double-quorum'd: a0,b0 → merged at c0; a1,b1,c1 ack c0;
    // a2 witnesses the double quorum. Exercises the merge-resolving `yield_next`.
    fn deep_fork_merge() -> Linearizer {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[]);
        add(&mut lin, n(C, 0), &[n(A, 0), n(B, 0)]); // merge of a0 & b0
        add(&mut lin, n(A, 1), &[n(C, 0)]);
        add(&mut lin, n(B, 1), &[n(C, 0)]);
        add(&mut lin, n(C, 1), &[n(C, 0)]);
        add(&mut lin, n(A, 2), &[n(A, 1), n(B, 1), n(C, 1)]);
        lin
    }

    #[test]
    fn confirmed_prefix_equals_finalized_on_fork_free() {
        let lin = chain();
        assert_eq!(lin.confirmed_prefix(), lin.finalized());
        assert_eq!(lin.confirmed_prefix(), vec![n(A, 0), n(B, 0)]);

        let mut sq = indexed(&[A, B, C]);
        add(&mut sq, n(A, 0), &[]);
        add(&mut sq, n(C, 0), &[]);
        add(&mut sq, n(C, 1), &[n(A, 0)]);
        add(&mut sq, n(B, 0), &[n(C, 0)]);
        assert!(sq.confirmed_prefix().is_empty(), "competing single quorums confirm nothing");
        assert_eq!(sq.confirmed_prefix(), sq.finalized());
    }

    #[test]
    fn confirmed_prefix_is_causally_closed() {
        for lin in [chain(), fork_merge(), deep_fork_merge()] {
            let prefix = lin.confirmed_prefix();
            let pos: BTreeMap<NodeId, usize> =
                prefix.iter().enumerate().map(|(i, nd)| (*nd, i)).collect();
            for (i, node) in prefix.iter().enumerate() {
                for dep in lin.deps.get(node).into_iter().flatten() {
                    let di = pos.get(dep).copied();
                    assert!(
                        di.is_some() && di.unwrap() < i,
                        "dep {dep:?} of {node:?} must be confirmed strictly earlier"
                    );
                }
            }
        }
    }

    #[test]
    fn confirmed_prefix_supersets_conservative_and_resolves_a_merge() {
        // The precise machine is never less eager than the old conservative rule.
        for lin in [chain(), fork_merge(), deep_fork_merge()] {
            let cp: BTreeSet<NodeId> = lin.confirmed_prefix().into_iter().collect();
            for node in conservative_finalized(&lin) {
                assert!(cp.contains(&node), "{node:?} conservatively-finalized ⇒ confirmed");
            }
        }
        // The merged fork: the conservative rule defers around the unresolved fork,
        // but the precise machine confirms the merged arms — strictly more eager.
        let dfm = deep_fork_merge();
        let cp = dfm.confirmed_prefix();
        assert!(cp.contains(&n(A, 0)) && cp.contains(&n(B, 0)) && cp.contains(&n(C, 0)),
            "the merged fork (a0,b0,c0) is confirmed: {cp:?}");
        assert!(
            cp.len() > conservative_finalized(&dfm).len(),
            "precise prefix ({}) outpaces conservative ({})",
            cp.len(),
            conservative_finalized(&dfm).len()
        );
    }

    /// Every dependency must appear before its dependent in `order`.
    fn assert_causal(lin: &Linearizer, order: &[NodeId]) {
        let pos: BTreeMap<NodeId, usize> =
            order.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        assert_eq!(pos.len(), lin.len(), "order must list every node exactly once");
        for (node, deps) in &lin.deps {
            for dep in deps {
                assert!(
                    pos[dep] < pos[node],
                    "causal violation: {dep:?} must precede {node:?}"
                );
            }
        }
    }

    // DESIGN.md: `a0 - b0 - c0 - a1 - b1` linearises to itself.
    #[test]
    fn linear_chain() {
        let mut lin = Linearizer::new();
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[n(A, 0)]);
        add(&mut lin, n(C, 0), &[n(B, 0)]);
        add(&mut lin, n(A, 1), &[n(C, 0)]);
        add(&mut lin, n(B, 1), &[n(A, 1)]);

        assert_eq!(
            lin.order(),
            vec![n(A, 0), n(B, 0), n(C, 0), n(A, 1), n(B, 1)]
        );
        assert_causal(&lin, &lin.order());
    }

    // DESIGN.md branch:
    //   a0 - c0 - a1
    //      /
    //   b0
    // c0 sees both tails {a0, b0}; with a < b the tails order [a0, b0].
    #[test]
    fn branch_tiebreak_by_key() {
        let mut lin = Linearizer::new();
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[]);
        add(&mut lin, n(C, 0), &[n(A, 0), n(B, 0)]);
        add(&mut lin, n(A, 1), &[n(C, 0)]);

        assert_eq!(lin.order(), vec![n(A, 0), n(B, 0), n(C, 0), n(A, 1)]);
        assert_causal(&lin, &lin.order());
    }

    // DESIGN.md recursive example (a < b < c):
    //
    //   a0   c0
    //    | X |
    //   b0   a1
    //    | \ |
    //   c1   b1
    //    | /
    //   b2
    //
    // canonical linearisation: [a0, c0, a1, b0, b1, c1, b2]
    fn recursive_dag() -> Vec<(NodeId, Vec<NodeId>)> {
        vec![
            (n(A, 0), vec![]),
            (n(C, 0), vec![]),
            (n(B, 0), vec![n(A, 0), n(C, 0)]),
            (n(A, 1), vec![n(C, 0)]), // + implicit a0
            (n(C, 1), vec![n(B, 0)]), // + implicit c0
            (n(B, 1), vec![n(A, 1)]), // + implicit b0
            (n(B, 2), vec![n(C, 1)]), // + implicit b1
        ]
    }

    #[test]
    fn recursive_dag_matches_design() {
        let mut lin = Linearizer::new();
        for (node, heads) in recursive_dag() {
            add(&mut lin, node, &heads);
        }

        assert_eq!(
            lin.order(),
            vec![
                n(A, 0),
                n(C, 0),
                n(A, 1),
                n(B, 0),
                n(B, 1),
                n(C, 1),
                n(B, 2)
            ]
        );
        assert_causal(&lin, &lin.order());
        assert_eq!(lin.tails(), [n(A, 0), n(C, 0)].into_iter().collect());
    }

    // Determinism: same DAG ⇒ same order, regardless of (causally-valid) arrival
    // order. This is the "replicas seeing the same set agree" property.
    #[test]
    fn deterministic_regardless_of_arrival_order() {
        let dag = recursive_dag();
        let by_index = |idxs: &[usize]| {
            let mut lin = Linearizer::new();
            for &i in idxs {
                let (node, heads) = &dag[i];
                add(&mut lin, *node, heads);
            }
            lin.order()
        };

        // Three distinct topologically-valid insertion orders.
        let canonical = by_index(&[0, 1, 2, 3, 4, 5, 6]);
        let perm_b = by_index(&[1, 0, 3, 2, 4, 5, 6]); // c0, a0, a1, b0, c1, b1, b2
        let perm_c = by_index(&[1, 0, 2, 4, 3, 5, 6]); // c0, a0, b0, c1, a1, b1, b2

        assert_eq!(canonical, perm_b);
        assert_eq!(canonical, perm_c);
    }

    #[test]
    fn rejects_causal_delivery_violations() {
        let mut lin = Linearizer::new();
        add(&mut lin, n(A, 0), &[]);

        // Duplicate id.
        assert_eq!(lin.add(n(A, 0), &[]), Err(AddError::Duplicate(n(A, 0))));

        // Seq gap: a2 before a1.
        assert_eq!(
            lin.add(n(A, 2), &[]),
            Err(AddError::Gap {
                writer: wk(A),
                expected: 1,
                got: 2,
            })
        );

        // Reference to an undelivered head.
        assert_eq!(
            lin.add(n(B, 0), &[n(C, 0)]),
            Err(AddError::MissingHead(n(C, 0)))
        );

        // The valid follow-up still works, and nothing partial was committed.
        add(&mut lin, n(A, 1), &[]);
        assert_eq!(lin.len(), 2);
        assert_eq!(lin.order(), vec![n(A, 0), n(A, 1)]);
    }

    #[test]
    fn empty_orders_to_nothing() {
        let lin = Linearizer::new();
        assert!(lin.order().is_empty());
        assert!(lin.tails().is_empty());
    }

    // A fourth writer key for the majority-threshold test (d, beyond a<b<c).
    const D: u8 = 4;

    fn indexed(keys: &[u8]) -> Linearizer {
        Linearizer::with_indexers(keys.iter().map(|&k| wk(k)))
    }

    // DESIGN.md "Quorums": in the chain `a0 - b0 - c0 - a1`, a0 accumulates a
    // single quorum at b0, a double quorum at c0, and a triple quorum at a1.
    #[test]
    fn quorum_degrees_match_design_chain() {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[n(A, 0)]);
        add(&mut lin, n(C, 0), &[n(B, 0)]);
        add(&mut lin, n(A, 1), &[n(C, 0)]);

        assert_eq!(lin.quorum_degree(&n(A, 0)), 3, "a1 triple-quorums a0");
        assert_eq!(lin.quorum_degree(&n(B, 0)), 2, "a1 double-quorums b0");
        assert_eq!(lin.quorum_degree(&n(C, 0)), 1, "a1 single-quorums c0");
        assert_eq!(lin.quorum_degree(&n(A, 1)), 0, "a1 itself unconfirmed");
    }

    // DESIGN.md "Higher Quorums": `c0 - b0 - c1` lifts c0 to a double quorum
    // because writers b & c reference the single quorum that formed at b0.
    #[test]
    fn double_quorum_design_higher_example() {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(C, 0), &[]);
        add(&mut lin, n(B, 0), &[n(C, 0)]);
        add(&mut lin, n(C, 1), &[n(B, 0)]);

        assert_eq!(lin.quorum_degree(&n(C, 0)), 2, "c1 double-quorums c0");
        assert_eq!(lin.quorum_degree(&n(B, 0)), 1, "b0 single-quorumed by c1");
    }

    // DESIGN.md "Condition for Consistency": two nodes can each reach a *single*
    // quorum yet conflict — so a single quorum must never finalize. Here c1 sees
    // {a0, c0} and b0 sees {c0}; a0 is voted by {a, c}, c0 by {b, c}. Both reach a
    // single quorum, but c is in both — writers b and c hold conflicting views.
    #[test]
    fn single_quorum_does_not_finalize() {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(C, 0), &[]);
        add(&mut lin, n(C, 1), &[n(A, 0)]); // c1: cross-ref a0, implicit pred c0
        add(&mut lin, n(B, 0), &[n(C, 0)]);

        assert_eq!(lin.quorum_degree(&n(A, 0)), 1);
        assert_eq!(lin.quorum_degree(&n(C, 0)), 1);
        assert!(
            lin.finalized().is_empty(),
            "competing single quorums must not finalize"
        );
    }

    // The finalized prefix is the double-quorum'd head of a chain, and it is
    // always a genuine prefix of the deterministic order.
    #[test]
    fn finalized_is_a_double_quorum_prefix() {
        let mut lin = indexed(&[A, B, C]);
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[n(A, 0)]);
        add(&mut lin, n(C, 0), &[n(B, 0)]);
        add(&mut lin, n(A, 1), &[n(C, 0)]);

        let f = lin.finalized();
        assert_eq!(f, vec![n(A, 0), n(B, 0)], "only a0,b0 are double-quorum'd");
        assert!(lin.order().starts_with(&f), "finalized ⊑ order");
    }

    // Finality-stability: as the DAG grows cooperatively (each append references
    // the head), the finalized prefix only ever extends — no node already
    // finalized is dropped or reordered.
    #[test]
    fn finalized_prefix_only_grows() {
        // round-robin chain a0,b0,c0,a1,b1,c1,a2,b2,c2
        let writers = [A, B, C];
        let steps: Vec<NodeId> = (0..9)
            .map(|i| n(writers[i % 3], (i / 3) as u64))
            .collect();

        let mut lin = indexed(&[A, B, C]);
        let mut prev: Vec<NodeId> = Vec::new();
        let mut last = String::new();
        for (i, node) in steps.iter().enumerate() {
            let heads = if i == 0 { vec![] } else { vec![steps[i - 1]] };
            add(&mut lin, *node, &heads);

            let cur = lin.finalized();
            assert!(cur.starts_with(&prev), "finalized must extend, never reorder");
            assert!(lin.order().starts_with(&cur), "finalized ⊑ order at every step");
            prev = cur;
            last = format!("{} nodes finalized", prev.len());
        }
        assert!(!prev.is_empty(), "a long chain must finalize something ({last})");
    }

    // Majority is ⌊n/2⌋+1: with four indexers it takes three distinct votes to
    // form even a single quorum.
    #[test]
    fn majority_threshold_scales_with_indexer_count() {
        let mut lin = indexed(&[A, B, C, D]); // majority = 3
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[n(A, 0)]); // a0 referenced by {a,b} = 2 < 3
        assert_eq!(lin.quorum_degree(&n(A, 0)), 0, "two of four is not a majority");

        add(&mut lin, n(C, 0), &[n(B, 0)]); // now {a,b,c} = 3 ≥ 3
        assert_eq!(lin.quorum_degree(&n(A, 0)), 1, "three of four is a single quorum");
    }

    // Without an indexer set the linearizer still orders, but confirms nothing.
    #[test]
    fn no_indexers_means_no_finalization() {
        let mut lin = Linearizer::new();
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(B, 0), &[n(A, 0)]);
        add(&mut lin, n(C, 0), &[n(B, 0)]);

        assert_eq!(lin.quorum_degree(&n(A, 0)), 0);
        assert!(lin.finalized().is_empty());
        // ordering is unaffected
        assert_eq!(lin.order(), vec![n(A, 0), n(B, 0), n(C, 0)]);
    }

    // A non-indexing writer is ordered but never casts a vote: its references do
    // not move a target toward quorum.
    #[test]
    fn non_indexer_references_do_not_vote() {
        // Only a & b are indexers; c is a non-indexing writer.
        let mut lin = indexed(&[A, B]); // majority = 2
        add(&mut lin, n(A, 0), &[]);
        add(&mut lin, n(C, 0), &[n(A, 0)]); // c (non-indexer) references a0

        // a0 has only its own indexer vote {a}; c's reference doesn't count.
        assert_eq!(lin.quorum_degree(&n(A, 0)), 0, "non-indexer vote ignored");

        add(&mut lin, n(B, 0), &[n(C, 0)]); // b (indexer) now references a0
        assert_eq!(lin.quorum_degree(&n(A, 0)), 1, "{{a,b}} is the quorum");
        // c is still part of the linearization.
        assert!(lin.order().contains(&n(C, 0)));
    }
}
