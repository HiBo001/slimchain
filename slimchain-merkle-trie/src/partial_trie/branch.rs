use super::SubTree;
use crate::{hash::branch_node_hash, nibbles::Nibbles, u4::U4};
use alloc::{boxed::Box, sync::Arc};
use core::{cell::Cell, mem};
use serde::{Deserialize, Serialize};
use slimchain_common::{basic::H256, digest::Digestible};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(from = "crate::proof::BranchNode")]
#[serde(into = "crate::proof::BranchNode")]
pub(crate) struct BranchNode {
    pub(crate) children: [Option<Arc<SubTree>>; 16],
    node_hash: Cell<Option<H256>>,
}

impl Digestible for BranchNode {
    fn to_digest(&self) -> H256 {
        if let Some(h) = self.node_hash.get() {
            return h;
        }

        let children = self
            .children
            .iter()
            .map(|c| c.as_ref().map(|n| n.to_digest()));
        let h = branch_node_hash(children);
        self.node_hash.set(Some(h));
        h
    }
}

impl From<crate::proof::BranchNode> for BranchNode {
    fn from(mut input: crate::proof::BranchNode) -> Self {
        let mut node = BranchNode::default();
        for (i, child) in input.children.iter_mut().enumerate() {
            if let Some(c) = child {
                let c2 = mem::take(c.as_mut());
                unsafe {
                    *node.children.get_unchecked_mut(i) = Some(Arc::new(c2.into()));
                }
            }
        }
        node
    }
}

impl Into<crate::proof::BranchNode> for BranchNode {
    fn into(self) -> crate::proof::BranchNode {
        let mut node = crate::proof::BranchNode::default();
        for (i, child) in self.children.iter().enumerate() {
            if let Some(c) = child {
                unsafe {
                    *node.children.get_unchecked_mut(i) = Some(Box::new((**c).clone().into()));
                }
            }
        }
        node
    }
}

impl PartialEq for BranchNode {
    fn eq(&self, other: &Self) -> bool {
        self.children == other.children
    }
}

impl Eq for BranchNode {}

impl BranchNode {
    pub(crate) fn new(children: [Option<Arc<SubTree>>; 16]) -> Self {
        Self {
            children,
            node_hash: Cell::new(None),
        }
    }

    pub(crate) fn get_child(&self, index: U4) -> Option<&'_ Arc<SubTree>> {
        let index: usize = index.into();
        unsafe { self.children.get_unchecked(index) }.as_ref()
    }

    pub(crate) fn get_child_mut(&mut self, index: U4) -> &'_ mut Option<Arc<SubTree>> {
        let index: usize = index.into();
        unsafe { self.children.get_unchecked_mut(index) }
    }

    pub(crate) fn value_hash(&self, key: Nibbles<'_>) -> Option<H256> {
        let (child_idx, remaining) = match key.split_first() {
            Some(res) => res,
            None => {
                panic!("Invalid key. Branch node does not store value.");
            }
        };

        match self.get_child(child_idx) {
            Some(child) => child.value_hash(remaining),
            None => Some(H256::zero()),
        }
    }

    pub(crate) fn num_of_materialized_children(&self) -> usize {
        self.children
            .iter()
            .filter(|c| match c {
                Some(sub) => match sub.as_ref() {
                    SubTree::Hash(_) => false,
                    _ => true,
                },
                None => false,
            })
            .count()
    }
}
