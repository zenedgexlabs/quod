//! The Skipcert footprint graph.
//!
//! A [`FootprintGraph`] records, for one repository at one base commit, which
//! entities (files, functions, external packages, environment variables, ...)
//! each test observed. It is indexed in both directions (ADR-002):
//!
//! - forward: test → observed nodes, which drives certificate evidence;
//! - inverse: node → observing tests, which drives selection, dependency bumps
//!   and the impact API.
//!
//! Graphs are canonical. [`GraphBuilder::build`] sorts nodes, tests and
//! adjacency, so the same observations always encode to the same bytes and the
//! same Merkle root, whatever order they were recorded in.
//!
//! # `.scmap` v1 layout
//!
//! ```text
//! magic "SCMAP" | format_version u32 | section_count u32
//! section table: [kind u32 | offset u64 | len u64 | blake3(map-section) 32B] × N
//! section data (CBOR tables, raw little-endian u32 arrays for the CSRs)
//! merkle_root 32B  (over section hashes, in table order)
//! ```
//!
//! The CSR arrays are stored raw so a later reader can map them without
//! copying. Loading validates every bound and every hash, and checks that the
//! inverse index is exactly the transpose of the forward one.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use sc_hash::{Digest, Domain, HashError, canonical_cbor, hash_domain, merkle_root};
use sc_model::version::{MAP_FORMAT_VERSION, MAP_MAGIC};
use sc_model::{NodeKind, StableTestId, TestFlags};

/// Errors from building, encoding or decoding a graph.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// The bytes are not a valid `.scmap` v1 file.
    #[error("invalid scmap: {0}")]
    Format(String),
    /// Encoding failed.
    #[error(transparent)]
    Hash(#[from] HashError),
}

fn bad(msg: impl Into<String>) -> GraphError {
    GraphError::Format(msg.into())
}

/// Index of a node in a built graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

/// Index of a test in a built graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TestIdx(pub u32);

/// Map-level metadata. It contains no timestamps, so the encoding stays
/// reproducible.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapMeta {
    /// Repository identifier.
    pub repo: String,
    /// Commit the observations were recorded at.
    pub base_commit: String,
    /// Engine version that wrote the map.
    pub engine: String,
    /// Probe identifiers and versions, e.g. `python-probe/0.1.0`.
    pub probes: Vec<String>,
}

/// A node as stored in the graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// Entity kind.
    pub kind: NodeKind,
    /// Index into the string table (canonical name, e.g. a repo-relative path).
    pub name: u32,
    /// Content hash observed at record time, if the entity has content.
    pub hash: Option<Digest>,
    /// Enclosing node (a function's file, an external file's package).
    pub parent: Option<u32>,
}

/// A test as stored in the graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestEntry {
    /// Stable identifier.
    pub id: StableTestId,
    /// Index into the string table (human-readable name).
    pub name: u32,
    /// Observed behaviour flags.
    pub flags: TestFlags,
    /// Median duration in milliseconds.
    pub cost_ms_p50: u32,
}

/// A set of tests, as a bitset over [`TestIdx`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestSet {
    words: Vec<u64>,
    len: usize,
}

impl TestSet {
    /// An empty set over `len` tests.
    pub fn empty(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
            len,
        }
    }

    /// The set of all `len` tests.
    pub fn full(len: usize) -> Self {
        let mut s = Self::empty(len);
        for i in 0..len {
            s.insert(TestIdx(i as u32));
        }
        s
    }

    /// Adds a test.
    pub fn insert(&mut self, t: TestIdx) {
        let i = t.0 as usize;
        debug_assert!(i < self.len);
        self.words[i / 64] |= 1 << (i % 64);
    }

    /// True if the set contains `t`.
    pub fn contains(&self, t: TestIdx) -> bool {
        let i = t.0 as usize;
        i < self.len && self.words[i / 64] & (1 << (i % 64)) != 0
    }

    /// Number of tests in the set.
    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Iterates members in index order.
    pub fn iter(&self) -> impl Iterator<Item = TestIdx> + '_ {
        (0..self.len as u32)
            .map(TestIdx)
            .filter(|t| self.contains(*t))
    }
}

/// The result of intersecting a change with the graph.
#[derive(Clone, Debug)]
pub struct Affected {
    /// Tests that observed at least one changed node.
    pub tests: TestSet,
    /// True if a changed node is in the root scope (conftest, plugins,
    /// startup). That affects every test.
    pub root_hit: bool,
}

/// Accumulates observations, then builds a canonical graph.
#[derive(Debug, Default)]
pub struct GraphBuilder {
    meta: MapMeta,
    nodes: Vec<(NodeKind, String, Option<Digest>, Option<u32>)>,
    node_ix: HashMap<(NodeKind, String), u32>,
    tests: Vec<(StableTestId, String, TestFlags, u32)>,
    test_ix: HashMap<StableTestId, u32>,
    obs: Vec<BTreeSet<u32>>,
    root: BTreeSet<u32>,
}

impl GraphBuilder {
    /// Starts a builder for the given metadata.
    pub fn new(meta: MapMeta) -> Self {
        Self {
            meta,
            ..Self::default()
        }
    }

    /// Adds (or finds) a node. Builder IDs are only valid in this builder.
    /// The first non-`None` hash and parent win.
    pub fn node(
        &mut self,
        kind: NodeKind,
        name: &str,
        hash: Option<Digest>,
        parent: Option<NodeId>,
    ) -> NodeId {
        let key = (kind, name.to_owned());
        if let Some(&i) = self.node_ix.get(&key) {
            let entry = &mut self.nodes[i as usize];
            if entry.2.is_none() {
                entry.2 = hash;
            }
            if entry.3.is_none() {
                entry.3 = parent.map(|p| p.0);
            }
            return NodeId(i);
        }
        let i = self.nodes.len() as u32;
        self.nodes
            .push((kind, name.to_owned(), hash, parent.map(|p| p.0)));
        self.node_ix.insert(key, i);
        NodeId(i)
    }

    /// Adds (or finds) a test. Flags accumulate, and the latest cost wins.
    pub fn test(
        &mut self,
        id: StableTestId,
        name: &str,
        flags: TestFlags,
        cost_ms_p50: u32,
    ) -> TestIdx {
        if let Some(&i) = self.test_ix.get(&id) {
            let entry = &mut self.tests[i as usize];
            entry.2.insert(flags);
            entry.3 = cost_ms_p50;
            return TestIdx(i);
        }
        let i = self.tests.len() as u32;
        self.tests.push((id, name.to_owned(), flags, cost_ms_p50));
        self.test_ix.insert(id, i);
        self.obs.push(BTreeSet::new());
        TestIdx(i)
    }

    /// Records that test `t` observed node `n`.
    pub fn observe(&mut self, t: TestIdx, n: NodeId) {
        self.obs[t.0 as usize].insert(n.0);
    }

    /// Records a root-scope observation (outside any test).
    pub fn observe_root(&mut self, n: NodeId) {
        self.root.insert(n.0);
    }

    /// Builds the canonical graph.
    pub fn build(self) -> FootprintGraph {
        // Canonical node order: (kind, name).
        let mut order: Vec<u32> = (0..self.nodes.len() as u32).collect();
        order.sort_by(|&a, &b| {
            let (ka, na, ..) = &self.nodes[a as usize];
            let (kb, nb, ..) = &self.nodes[b as usize];
            (ka, na).cmp(&(kb, nb))
        });
        let mut node_remap = vec![0u32; self.nodes.len()];
        for (new, &old) in order.iter().enumerate() {
            node_remap[old as usize] = new as u32;
        }

        // Canonical string table: sorted and deduplicated.
        let mut all: Vec<&str> = self.nodes.iter().map(|n| n.1.as_str()).collect();
        all.extend(self.tests.iter().map(|t| t.1.as_str()));
        all.sort_unstable();
        all.dedup();
        let strings: Vec<String> = all.iter().map(|s| (*s).to_owned()).collect();
        let sidx = |s: &str| {
            strings
                .binary_search_by(|x| x.as_str().cmp(s))
                .expect("interned") as u32
        };

        let nodes: Vec<Node> = order
            .iter()
            .map(|&old| {
                let (kind, name, hash, parent) = &self.nodes[old as usize];
                Node {
                    kind: *kind,
                    name: sidx(name),
                    hash: *hash,
                    parent: parent.map(|p| node_remap[p as usize]),
                }
            })
            .collect();

        // Canonical test order: stable ID.
        let mut torder: Vec<u32> = (0..self.tests.len() as u32).collect();
        torder.sort_by_key(|&i| self.tests[i as usize].0);
        let tests: Vec<TestEntry> = torder
            .iter()
            .map(|&old| {
                let (id, name, flags, cost) = &self.tests[old as usize];
                TestEntry {
                    id: *id,
                    name: sidx(name),
                    flags: *flags,
                    cost_ms_p50: *cost,
                }
            })
            .collect();

        let forward: Vec<Vec<u32>> = torder
            .iter()
            .map(|&old| {
                let mut v: Vec<u32> = self.obs[old as usize]
                    .iter()
                    .map(|&n| node_remap[n as usize])
                    .collect();
                v.sort_unstable();
                v
            })
            .collect();
        let (fwd_off, fwd_tgt) = to_csr(&forward);
        let (inv_off, inv_tgt) = transpose(&fwd_off, &fwd_tgt, nodes.len());

        let mut root: Vec<u32> = self.root.iter().map(|&n| node_remap[n as usize]).collect();
        root.sort_unstable();

        FootprintGraph::assemble(
            self.meta, strings, nodes, tests, fwd_off, fwd_tgt, inv_off, inv_tgt, root,
        )
    }
}

fn to_csr(lists: &[Vec<u32>]) -> (Vec<u32>, Vec<u32>) {
    let mut off = Vec::with_capacity(lists.len() + 1);
    let mut tgt = Vec::new();
    off.push(0);
    for l in lists {
        tgt.extend_from_slice(l);
        off.push(tgt.len() as u32);
    }
    (off, tgt)
}

fn transpose(off: &[u32], tgt: &[u32], n_nodes: usize) -> (Vec<u32>, Vec<u32>) {
    let mut lists = vec![Vec::new(); n_nodes];
    for t in 0..off.len().saturating_sub(1) {
        for &n in &tgt[off[t] as usize..off[t + 1] as usize] {
            lists[n as usize].push(t as u32);
        }
    }
    to_csr(&lists)
}

/// A built, immutable footprint graph.
#[derive(Debug, Clone)]
pub struct FootprintGraph {
    meta: MapMeta,
    strings: Vec<String>,
    nodes: Vec<Node>,
    tests: Vec<TestEntry>,
    fwd_off: Vec<u32>,
    fwd_tgt: Vec<u32>,
    inv_off: Vec<u32>,
    inv_tgt: Vec<u32>,
    root: Vec<u32>,
    node_lookup: HashMap<(NodeKind, String), u32>,
    test_lookup: HashMap<StableTestId, u32>,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Meta = 1,
    Strings = 2,
    Nodes = 3,
    Tests = 4,
    FwdOff = 5,
    FwdTgt = 6,
    InvOff = 7,
    InvTgt = 8,
    Root = 9,
}

const SECTIONS: [Section; 9] = [
    Section::Meta,
    Section::Strings,
    Section::Nodes,
    Section::Tests,
    Section::FwdOff,
    Section::FwdTgt,
    Section::InvOff,
    Section::InvTgt,
    Section::Root,
];

const HEADER_LEN: usize = 5 + 4 + 4;
const ENTRY_LEN: usize = 4 + 8 + 8 + 32;

fn u32s_to_bytes(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn bytes_to_u32s(b: &[u8]) -> Result<Vec<u32>, GraphError> {
    if b.len() % 4 != 0 {
        return Err(bad("u32 section length not a multiple of 4"));
    }
    Ok(b.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

fn decode_cbor<T: for<'de> Deserialize<'de>>(b: &[u8], what: &str) -> Result<T, GraphError> {
    ciborium::from_reader(b).map_err(|e| bad(format!("{what}: {e}")))
}

impl FootprintGraph {
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        meta: MapMeta,
        strings: Vec<String>,
        nodes: Vec<Node>,
        tests: Vec<TestEntry>,
        fwd_off: Vec<u32>,
        fwd_tgt: Vec<u32>,
        inv_off: Vec<u32>,
        inv_tgt: Vec<u32>,
        root: Vec<u32>,
    ) -> Self {
        let node_lookup = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| ((n.kind, strings[n.name as usize].clone()), i as u32))
            .collect();
        let test_lookup = tests
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id, i as u32))
            .collect();
        Self {
            meta,
            strings,
            nodes,
            tests,
            fwd_off,
            fwd_tgt,
            inv_off,
            inv_tgt,
            root,
            node_lookup,
            test_lookup,
        }
    }

    /// Map metadata.
    pub fn meta(&self) -> &MapMeta {
        &self.meta
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of tests.
    pub fn test_count(&self) -> usize {
        self.tests.len()
    }

    /// Looks up a node by kind and canonical name.
    pub fn find_node(&self, kind: NodeKind, name: &str) -> Option<NodeId> {
        self.node_lookup
            .get(&(kind, name.to_owned()))
            .map(|&i| NodeId(i))
    }

    /// Looks up a test by stable ID.
    pub fn find_test(&self, id: &StableTestId) -> Option<TestIdx> {
        self.test_lookup.get(id).map(|&i| TestIdx(i))
    }

    /// The stored node.
    pub fn node(&self, n: NodeId) -> &Node {
        &self.nodes[n.0 as usize]
    }

    /// The node's canonical name.
    pub fn node_name(&self, n: NodeId) -> &str {
        &self.strings[self.node(n).name as usize]
    }

    /// Canonical node reference used in certificate evidence, e.g. `file:src/a.py`.
    pub fn node_ref(&self, n: NodeId) -> String {
        format!("{}:{}", kind_tag(self.node(n).kind), self.node_name(n))
    }

    /// The stored test.
    pub fn test(&self, t: TestIdx) -> &TestEntry {
        &self.tests[t.0 as usize]
    }

    /// The test's human-readable name.
    pub fn test_name(&self, t: TestIdx) -> &str {
        &self.strings[self.test(t).name as usize]
    }

    /// Nodes observed by a test (forward index), in canonical order.
    pub fn footprint(&self, t: TestIdx) -> impl Iterator<Item = NodeId> + '_ {
        let i = t.0 as usize;
        self.fwd_tgt[self.fwd_off[i] as usize..self.fwd_off[i + 1] as usize]
            .iter()
            .map(|&n| NodeId(n))
    }

    /// Tests that observed a node (inverse index), in canonical order.
    pub fn observers(&self, n: NodeId) -> impl Iterator<Item = TestIdx> + '_ {
        let i = n.0 as usize;
        self.inv_tgt[self.inv_off[i] as usize..self.inv_off[i + 1] as usize]
            .iter()
            .map(|&t| TestIdx(t))
    }

    /// True if the node was observed in the root scope.
    pub fn is_root(&self, n: NodeId) -> bool {
        self.root.binary_search(&n.0).is_ok()
    }

    /// Root-scope nodes.
    pub fn root_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.root.iter().map(|&n| NodeId(n))
    }

    /// Intersects a set of changed nodes with the graph using the inverse
    /// index. Changed entities absent from the graph must be handled by the
    /// caller's policy (unknown means fail closed); they can't appear here.
    pub fn affected(&self, changed: &[NodeId]) -> Affected {
        let mut tests = TestSet::empty(self.tests.len());
        let mut root_hit = false;
        for &n in changed {
            root_hit |= self.is_root(n);
            for t in self.observers(n) {
                tests.insert(t);
            }
        }
        Affected { tests, root_hit }
    }

    /// Encodes to `.scmap` v1 bytes. Returns the bytes and the Merkle root.
    pub fn encode(&self) -> Result<(Vec<u8>, Digest), GraphError> {
        let payloads: Vec<Vec<u8>> = SECTIONS
            .iter()
            .map(|s| -> Result<Vec<u8>, GraphError> {
                Ok(match s {
                    Section::Meta => canonical_cbor(&self.meta)?,
                    Section::Strings => canonical_cbor(&self.strings)?,
                    Section::Nodes => canonical_cbor(&self.nodes)?,
                    Section::Tests => canonical_cbor(&self.tests)?,
                    Section::FwdOff => u32s_to_bytes(&self.fwd_off),
                    Section::FwdTgt => u32s_to_bytes(&self.fwd_tgt),
                    Section::InvOff => u32s_to_bytes(&self.inv_off),
                    Section::InvTgt => u32s_to_bytes(&self.inv_tgt),
                    Section::Root => u32s_to_bytes(&self.root),
                })
            })
            .collect::<Result<_, _>>()?;

        let hashes: Vec<Digest> = payloads
            .iter()
            .map(|p| hash_domain(Domain::MapSection, p))
            .collect();
        let root = merkle_root(&hashes);

        let mut out = Vec::new();
        out.extend_from_slice(MAP_MAGIC);
        out.extend_from_slice(&MAP_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(SECTIONS.len() as u32).to_le_bytes());
        let mut offset = (HEADER_LEN + ENTRY_LEN * SECTIONS.len()) as u64;
        for ((s, p), h) in SECTIONS.iter().zip(&payloads).zip(&hashes) {
            out.extend_from_slice(&(*s as u32).to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&(p.len() as u64).to_le_bytes());
            out.extend_from_slice(h.as_bytes());
            offset += p.len() as u64;
        }
        for p in &payloads {
            out.extend_from_slice(p);
        }
        out.extend_from_slice(root.as_bytes());
        Ok((out, root))
    }

    /// Decodes and fully validates `.scmap` v1 bytes. Returns the graph and
    /// its Merkle root.
    pub fn decode(bytes: &[u8]) -> Result<(Self, Digest), GraphError> {
        if bytes.len() < HEADER_LEN + 32 || &bytes[..5] != MAP_MAGIC {
            return Err(bad("missing SCMAP magic"));
        }
        let rd_u32 = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        let rd_u64 = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        let version = rd_u32(5);
        if version != MAP_FORMAT_VERSION {
            return Err(bad(format!("unsupported format version {version}")));
        }
        let count = rd_u32(9) as usize;
        if count != SECTIONS.len() {
            return Err(bad(format!(
                "expected {} sections, found {count}",
                SECTIONS.len()
            )));
        }
        let table_end = HEADER_LEN + ENTRY_LEN * count;
        let data_end = bytes.len() - 32;
        if table_end > data_end {
            return Err(bad("truncated section table"));
        }

        let mut payloads: Vec<&[u8]> = Vec::with_capacity(count);
        let mut hashes = Vec::with_capacity(count);
        for (i, expected) in SECTIONS.iter().enumerate() {
            let e = HEADER_LEN + ENTRY_LEN * i;
            if rd_u32(e) != *expected as u32 {
                return Err(bad(format!("section {i} has wrong kind")));
            }
            let off = usize::try_from(rd_u64(e + 4)).map_err(|_| bad("offset overflow"))?;
            let len = usize::try_from(rd_u64(e + 12)).map_err(|_| bad("length overflow"))?;
            let end = off
                .checked_add(len)
                .ok_or_else(|| bad("section overflow"))?;
            if off < table_end || end > data_end {
                return Err(bad(format!("section {i} out of bounds")));
            }
            let payload = &bytes[off..end];
            let stored = Digest::from_bytes(bytes[e + 20..e + 52].try_into().expect("32 bytes"));
            if hash_domain(Domain::MapSection, payload) != stored {
                return Err(bad(format!("section {i} hash mismatch")));
            }
            payloads.push(payload);
            hashes.push(stored);
        }
        let root = merkle_root(&hashes);
        if root.as_bytes()[..] != bytes[data_end..] {
            return Err(bad("merkle root mismatch"));
        }

        let meta: MapMeta = decode_cbor(payloads[0], "meta")?;
        let strings: Vec<String> = decode_cbor(payloads[1], "strings")?;
        let nodes: Vec<Node> = decode_cbor(payloads[2], "nodes")?;
        let tests: Vec<TestEntry> = decode_cbor(payloads[3], "tests")?;
        let fwd_off = bytes_to_u32s(payloads[4])?;
        let fwd_tgt = bytes_to_u32s(payloads[5])?;
        let inv_off = bytes_to_u32s(payloads[6])?;
        let inv_tgt = bytes_to_u32s(payloads[7])?;
        let root_nodes = bytes_to_u32s(payloads[8])?;

        validate(
            &strings,
            &nodes,
            &tests,
            &fwd_off,
            &fwd_tgt,
            &inv_off,
            &inv_tgt,
            &root_nodes,
        )?;
        let graph = Self::assemble(
            meta, strings, nodes, tests, fwd_off, fwd_tgt, inv_off, inv_tgt, root_nodes,
        );
        Ok((graph, root))
    }
}

#[allow(clippy::too_many_arguments)]
fn validate(
    strings: &[String],
    nodes: &[Node],
    tests: &[TestEntry],
    fwd_off: &[u32],
    fwd_tgt: &[u32],
    inv_off: &[u32],
    inv_tgt: &[u32],
    root: &[u32],
) -> Result<(), GraphError> {
    let ns = nodes.len() as u32;
    let ss = strings.len() as u32;
    if strings.windows(2).any(|w| w[0] >= w[1]) {
        return Err(bad("string table not sorted and unique"));
    }
    for n in nodes {
        if n.name >= ss || n.parent.is_some_and(|p| p >= ns) {
            return Err(bad("node references out of range"));
        }
    }
    if nodes.windows(2).any(|w| {
        (w[0].kind, &strings[w[0].name as usize]) >= (w[1].kind, &strings[w[1].name as usize])
    }) {
        return Err(bad("nodes not in canonical order"));
    }
    for t in tests {
        if t.name >= ss {
            return Err(bad("test name out of range"));
        }
    }
    if tests.windows(2).any(|w| w[0].id >= w[1].id) {
        return Err(bad("tests not in canonical order"));
    }
    check_csr(fwd_off, fwd_tgt, tests.len(), ns, "forward")?;
    check_csr(inv_off, inv_tgt, nodes.len(), tests.len() as u32, "inverse")?;
    let (exp_off, exp_tgt) = transpose(fwd_off, fwd_tgt, nodes.len());
    if exp_off != inv_off || exp_tgt != inv_tgt {
        return Err(bad(
            "inverse index is not the transpose of the forward index",
        ));
    }
    if root.iter().any(|&n| n >= ns) || root.windows(2).any(|w| w[0] >= w[1]) {
        return Err(bad("root set invalid"));
    }
    Ok(())
}

fn check_csr(
    off: &[u32],
    tgt: &[u32],
    rows: usize,
    bound: u32,
    what: &str,
) -> Result<(), GraphError> {
    if off.len() != rows + 1 || off[0] != 0 || *off.last().expect("non-empty") as usize != tgt.len()
    {
        return Err(bad(format!("{what} offsets malformed")));
    }
    for r in 0..rows {
        let (a, b) = (off[r] as usize, off[r + 1] as usize);
        if a > b {
            return Err(bad(format!("{what} offsets not monotonic")));
        }
        let row = &tgt[a..b];
        if row.iter().any(|&x| x >= bound) || row.windows(2).any(|w| w[0] >= w[1]) {
            return Err(bad(format!("{what} row {r} invalid")));
        }
    }
    Ok(())
}

/// Stable textual tag for a node kind, used in node references.
pub fn kind_tag(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::File => "file",
        NodeKind::Function => "fn",
        NodeKind::Module => "mod",
        NodeKind::ExtPackage => "pkg",
        NodeKind::ExtFile => "pkgfile",
        NodeKind::EnvVar => "env",
        NodeKind::NetEndpoint => "net",
        NodeKind::Resource => "res",
        NodeKind::Test => "test",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Digest {
        hash_domain(Domain::Blob, s.as_bytes())
    }

    fn sample(reverse: bool) -> FootprintGraph {
        let mut b = GraphBuilder::new(MapMeta {
            repo: "acme/app".into(),
            base_commit: "abc".into(),
            engine: "0.1.0".into(),
            probes: vec!["python-probe/0.1.0".into()],
        });
        let t_a = StableTestId::derive("tests/test_a.py", "test_a", "");
        let t_b = StableTestId::derive("tests/test_b.py", "test_b", "");
        let mut specs = vec![
            (
                t_a,
                "tests/test_a.py::test_a",
                vec!["src/a.py", "src/util.py"],
            ),
            (
                t_b,
                "tests/test_b.py::test_b",
                vec!["src/b.py", "src/util.py"],
            ),
        ];
        if reverse {
            specs.reverse();
        }
        for (id, name, files) in specs {
            let t = b.test(id, name, TestFlags::default(), 10);
            let mut files = files.clone();
            if reverse {
                files.reverse();
            }
            for f in files {
                let n = b.node(NodeKind::File, f, Some(h(f)), None);
                b.observe(t, n);
            }
        }
        let conftest = b.node(NodeKind::File, "tests/conftest.py", Some(h("c")), None);
        b.observe_root(conftest);
        b.build()
    }

    #[test]
    fn build_is_canonical_regardless_of_insertion_order() {
        let (b1, r1) = sample(false).encode().unwrap();
        let (b2, r2) = sample(true).encode().unwrap();
        assert_eq!(b1, b2);
        assert_eq!(r1, r2);
    }

    #[test]
    fn round_trip() {
        let g = sample(false);
        let (bytes, root) = g.encode().unwrap();
        let (back, root2) = FootprintGraph::decode(&bytes).unwrap();
        assert_eq!(root, root2);
        assert_eq!(back.test_count(), 2);
        assert_eq!(back.node_count(), 4);
        assert_eq!(back.meta().repo, "acme/app");
    }

    #[test]
    fn inverse_index_drives_selection() {
        let g = sample(false);
        let util = g.find_node(NodeKind::File, "src/util.py").unwrap();
        let a = g.find_node(NodeKind::File, "src/a.py").unwrap();
        assert_eq!(g.affected(&[util]).tests.count(), 2);
        let only_a = g.affected(&[a]);
        assert_eq!(only_a.tests.count(), 1);
        assert!(!only_a.root_hit);
        let t = only_a.tests.iter().next().unwrap();
        assert_eq!(g.test_name(t), "tests/test_a.py::test_a");
        assert!(g.footprint(t).any(|n| g.node_ref(n) == "file:src/a.py"));
    }

    #[test]
    fn root_scope_change_is_flagged() {
        let g = sample(false);
        let conftest = g.find_node(NodeKind::File, "tests/conftest.py").unwrap();
        assert!(g.affected(&[conftest]).root_hit);
    }

    #[test]
    fn tampering_is_rejected() {
        let (mut bytes, _) = sample(false).encode().unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        assert!(FootprintGraph::decode(&bytes).is_err());
    }

    #[test]
    fn truncation_and_bad_magic_rejected() {
        let (bytes, _) = sample(false).encode().unwrap();
        assert!(FootprintGraph::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut wrong = bytes.clone();
        wrong[0] = b'X';
        assert!(FootprintGraph::decode(&wrong).is_err());
    }

    #[test]
    fn test_set_ops() {
        let mut s = TestSet::empty(130);
        s.insert(TestIdx(0));
        s.insert(TestIdx(129));
        assert!(s.contains(TestIdx(129)));
        assert!(!s.contains(TestIdx(64)));
        assert_eq!(s.count(), 2);
        assert_eq!(TestSet::full(130).count(), 130);
        assert_eq!(s.iter().map(|t| t.0).collect::<Vec<_>>(), vec![0, 129]);
    }
}
