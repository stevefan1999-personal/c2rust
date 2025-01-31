use crate::graph::{Func, Graph, GraphId, Graphs, Node, NodeId, NodeKind};
use c2rust_analysis_rt::events::{Event, EventKind, Pointer};
use c2rust_analysis_rt::mir_loc::{EventMetadata, Metadata, TransferKind};
use c2rust_analysis_rt::{mir_loc, MirLoc};
use color_eyre::eyre;
use fs_err::File;
use itertools::Itertools;
use rustc_data_structures::fingerprint::Fingerprint;
use rustc_hir::def_id::DefPathHash;
use std::collections::HashMap;
use std::io::{self, BufReader};
use std::iter;
use std::path::Path;

pub fn read_event_log(path: &Path) -> io::Result<Vec<Event>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let events = iter::from_fn(|| bincode::deserialize_from(&mut reader).ok()).collect::<Vec<_>>();
    Ok(events)
}

pub fn _read_metadata(path: &Path) -> eyre::Result<Metadata> {
    let bytes = fs_err::read(path)?;
    let metadata = bincode::deserialize(&bytes)?;
    Ok(metadata)
}

pub trait EventKindExt {
    fn ptr(&self, metadata: &EventMetadata) -> Option<Pointer>;
    fn has_parent(&self) -> bool;
    fn parent(&self, obj: (GraphId, NodeId)) -> Option<(GraphId, NodeId)>;
    fn to_node_kind(&self) -> Option<NodeKind>;
}

impl EventKindExt for EventKind {
    /// return the ptr of interest for a particular event
    fn ptr(&self, _metadata: &EventMetadata) -> Option<Pointer> {
        use EventKind::*;
        Some(match *self {
            CopyPtr(lhs) => lhs,
            Field(ptr, ..) => ptr,
            Free { ptr } => ptr,
            Ret(ptr) => ptr,
            LoadAddr(ptr) => ptr,
            StoreAddr(ptr) => ptr,
            LoadValue(ptr) => ptr,
            StoreValue(ptr) => ptr,
            CopyRef => return None, // FIXME
            ToInt(ptr) => ptr,
            Realloc { old_ptr, .. } => old_ptr,
            FromInt(lhs) => lhs,
            Alloc { ptr, .. } => ptr,
            AddrOfLocal(lhs, _) => lhs,
            Offset(ptr, _, _) => ptr,
            Done => return None,
        })
    }

    fn has_parent(&self) -> bool {
        use EventKind::*;
        !matches!(
            self,
            Realloc { new_ptr: _, .. } | Alloc { ptr: _, .. } | AddrOfLocal(_, _) | Done
        )
    }

    fn parent(&self, obj: (GraphId, NodeId)) -> Option<(GraphId, NodeId)> {
        self.has_parent().then(|| obj)
    }

    fn to_node_kind(&self) -> Option<NodeKind> {
        use EventKind::*;
        Some(match *self {
            Alloc { .. } => NodeKind::Malloc(1),
            Realloc { .. } => NodeKind::Malloc(1),
            Free { .. } => NodeKind::Free,
            CopyPtr(..) | CopyRef => NodeKind::Copy,
            Field(_, field) => NodeKind::Field(field.into()),
            LoadAddr(..) => NodeKind::LoadAddr,
            StoreAddr(..) => NodeKind::StoreAddr,
            LoadValue(..) => NodeKind::LoadValue,
            StoreValue(..) => NodeKind::StoreValue,
            AddrOfLocal(_, local) => NodeKind::AddrOfLocal(local.as_u32().into()),
            ToInt(_) => NodeKind::PtrToInt,
            FromInt(_) => NodeKind::IntToPtr,
            Ret(_) => return None,
            Offset(_, offset, _) => NodeKind::Offset(offset),
            Done => return None,
        })
    }
}

fn update_provenance(
    provenances: &mut HashMap<Pointer, (GraphId, NodeId)>,
    event_kind: &EventKind,
    metadata: &EventMetadata,
    mapping: (GraphId, NodeId),
) {
    use EventKind::*;
    match *event_kind {
        Alloc { ptr, .. } => {
            provenances.insert(ptr, mapping);
        }
        CopyPtr(ptr) => {
            // only insert if not already there
            if let Err(..) = provenances.try_insert(ptr, mapping) {
                log::warn!("{:x} doesn't have a source", ptr);
            }
        }
        Realloc { new_ptr, .. } => {
            provenances.insert(new_ptr, mapping);
        }
        Offset(_, _, new_ptr) => {
            provenances.insert(new_ptr, mapping);
        }
        CopyRef => {
            provenances.insert(metadata.destination.clone().unwrap().local.into(), mapping);
        }
        AddrOfLocal(ptr, _) => {
            provenances.insert(ptr, mapping);
        }
        _ => {}
    }
}

pub fn add_node(
    graphs: &mut Graphs,
    provenances: &mut HashMap<Pointer, (GraphId, NodeId)>,
    event: &Event,
) -> Option<NodeId> {
    let node_kind = event.kind.to_node_kind()?;

    let MirLoc {
        body_def,
        mut basic_block_idx,
        mut statement_idx,
        metadata,
    } = mir_loc::get(event.mir_loc).unwrap();

    let this_func_hash = DefPathHash(Fingerprint::new(body_def.0, body_def.1));
    let (src_fn, dest_fn) = match metadata.transfer_kind {
        TransferKind::None => (this_func_hash, this_func_hash),
        TransferKind::Arg(p) => (this_func_hash, DefPathHash(Fingerprint::new(p.0, p.1))),
        TransferKind::Ret(p) => (DefPathHash(Fingerprint::new(p.0, p.1)), this_func_hash),
    };

    if let TransferKind::Arg(_) = metadata.transfer_kind {
        // FIXME: this is a special case for arguments
        basic_block_idx = 0;
        statement_idx = 0;
    }

    let head = event
        .kind
        .ptr(metadata)
        .and_then(|ptr| provenances.get(&ptr).cloned());
    let ptr = head.and_then(|(gid, _last_nid_ref)| {
        graphs.graphs[gid]
            .nodes
            .iter()
            .rposition(|n| {
                if let (Some(d), Some(s)) = (&n.dest, &metadata.source) {
                    d == s
                } else {
                    false
                }
            })
            .map(|nid| (gid, NodeId::from(nid)))
    });

    let source = ptr.or_else(|| {
        metadata.source.as_ref().and_then(|src| {
            let latest_assignment = graphs.latest_assignment.get(&(src_fn, src.local)).cloned();
            if !src.projection.is_empty() {
                if let Some((gid, _)) = latest_assignment {
                    if let Some((nid, n)) = graphs.graphs[gid].nodes.iter_enumerated().rev().next()
                    {
                        if let NodeKind::Field(..) = n.kind {
                            return Some((gid, nid));
                        }
                    }
                }
            }

            if src.projection.is_empty() {
                latest_assignment
            } else if let EventKind::Field(..) = event.kind {
                latest_assignment
            } else {
                head
            }
        })
    });

    let node = Node {
        function: Func(dest_fn),
        block: basic_block_idx.into(),
        statement_idx,
        kind: node_kind,
        source: source
            .and_then(|p| event.kind.parent(p))
            .map(|(_, nid)| nid),
        dest: metadata.destination.clone(),
    };

    let graph_id = source
        .or(ptr)
        .or(head)
        .and_then(|p| event.kind.parent(p))
        .map(|(gid, _)| gid)
        .unwrap_or_else(|| graphs.graphs.push(Graph::new()));
    let node_id = graphs.graphs[graph_id].nodes.push(node);

    update_provenance(provenances, &event.kind, metadata, (graph_id, node_id));

    if let Some(dest) = &metadata.destination {
        let unique_place = (dest_fn, dest.local);
        let last_setting = (graph_id, node_id);

        if let Some(last @ (last_gid, last_nid)) =
            graphs.latest_assignment.insert(unique_place, last_setting)
        {
            if !dest.projection.is_empty()
                && graphs.graphs[last_gid].nodes[last_nid]
                    .dest
                    .as_ref()
                    .unwrap()
                    .projection
                    .is_empty()
            {
                graphs.latest_assignment.insert(unique_place, last);
            }
        }
    }

    Some(node_id)
}

pub fn construct_pdg(events: &[Event]) -> Graphs {
    let mut graphs = Graphs::new();
    let mut provenances = HashMap::new();
    for event in events {
        add_node(&mut graphs, &mut provenances, event);
    }
    // TODO(kkysen) check if I have to remove any `GraphId`s from `graphs.latest_assignment`
    graphs.graphs = graphs.graphs.into_iter().unique().collect();

    // for ((func, local), p) in &graphs.latest_assignment {
    //     let func = Func(*func);
    //     println!("({func:?}:{local:?}) => {p:?}");
    // }
    graphs
}
