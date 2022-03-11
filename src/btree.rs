use crate::{Memory64, WASM_PAGE_SIZE};
mod allocator;
mod node;
use crate::btree::allocator::Allocator;
use crate::btree::node::{InternalNode, LeafNode, Node};

const LAYOUT_VERSION: u8 = 1;
const NULL: u64 = 0;

const MAX_KEY_SIZE: u32 = 64;
const MAX_VALUE_SIZE: u32 = 64;

// Taken from `BTreeMap`.
const B: u64 = 6; // The minimum degree.
const CAPACITY: u64 = 2 * B - 1;

const LEAF_NODE_TYPE: u8 = 0;
const INTERNAL_NODE_TYPE: u8 = 1;

type Ptr = u64;

#[repr(packed)]
#[derive(Debug, PartialEq, Clone, Copy)]
struct BTreeHeader {
    magic: [u8; 3],
    version: u8,
    root_offset: u64,
    max_key_size: u32,
    max_value_size: u32,
}

#[derive(Debug)]
pub enum LoadError {
    MemoryEmpty,
    BadMagic([u8; 3]),
    UnsupportedVersion(u8),
}

#[derive(Debug, PartialEq, Eq)]
pub enum WriteError {
    GrowFailed { current: u64, delta: u64 },
    AddressSpaceOverflow,
}

pub struct StableBTreeMap<M: Memory64 + Clone> {
    memory: M,
    root_offset: Ptr,
    allocator: Allocator<M>,
    max_key_size: u32,
    max_value_size: u32,
}

type Key = Vec<u8>;
type Value = Vec<u8>;

pub struct Range;

impl<M: Memory64 + Clone> StableBTreeMap<M> {
    // TODO: make branching factor configurable.
    pub fn new(memory: M, max_key_size: u32, max_value_size: u32) -> Result<Self, WriteError> {
        let header_len = core::mem::size_of::<BTreeHeader>() as u64;
        let mut btree = Self {
            memory: memory.clone(),
            root_offset: NULL,
            allocator: Allocator::new(memory, 4096 /* TODO */, header_len)?,
            max_key_size,
            max_value_size,
        };

        btree.save()?;
        Ok(btree)
    }

    pub fn load(memory: M) -> Result<Self, LoadError> {
        let mut header: BTreeHeader = unsafe { core::mem::zeroed() };
        let header_slice = unsafe {
            core::slice::from_raw_parts_mut(
                &mut header as *mut _ as *mut u8,
                core::mem::size_of::<BTreeHeader>(),
            )
        };
        if memory.size() == 0 {
            return Err(LoadError::MemoryEmpty);
        }
        memory.read(0, header_slice);
        if &header.magic != b"BTR" {
            return Err(LoadError::BadMagic(header.magic));
        }
        if header.version != LAYOUT_VERSION {
            return Err(LoadError::UnsupportedVersion(header.version));
        }

        let header_len = core::mem::size_of::<BTreeHeader>() as u64;
        println!("Loading allocator from address: {}", header_len);
        Ok(Self {
            memory: memory.clone(),
            root_offset: header.root_offset,
            allocator: Allocator::load(memory, header_len).unwrap(),
            max_key_size: header.max_key_size,
            max_value_size: header.max_value_size,
        })
    }

    fn save(&self) -> Result<(), WriteError> {
        let header = BTreeHeader {
            magic: *b"BTR",
            version: LAYOUT_VERSION,
            root_offset: self.root_offset,
            max_key_size: self.max_key_size,
            max_value_size: self.max_value_size,
        };

        let header_slice = unsafe {
            core::slice::from_raw_parts(
                &header as *const _ as *const u8,
                core::mem::size_of::<BTreeHeader>(),
            )
        };

        write(&self.memory, 0, header_slice)?;

        self.allocator.save();
        Ok(())
    }

    pub fn insert(&mut self, key: Key, value: Value) -> Option<Value> {
        let root = if self.root_offset == NULL {
            let node_address = self.allocator.allocate();
            self.root_offset = node_address;

            Node::new_leaf(node_address)
        } else {
            Node::load(self.root_offset, &self.memory)
        };

        // if node is not full
        if !root.is_full() {
            self.insert_nonfull(root, key, value)
        } else {
            // The root is full. Allocate a new node that will be used as the new root.
            let mut new_root = self.allocate_internal_node();
            new_root.children.push(self.root_offset);
            println!(
                "Updating root from {:?} to {:?}",
                self.root_offset, new_root.address
            );
            self.root_offset = new_root.address;

            new_root.save(&self.memory).unwrap();

            self.split_child(&mut new_root, 0);
            println!("new root: {:?}", new_root);
            self.insert_nonfull(Node::Internal(new_root), key, value)
        }
    }

    pub fn get(&self, key: &Key) -> Option<Value> {
        if self.root_offset == NULL {
            return None;
        }

        Self::get_helper(self.root_offset, key, &self.memory)
    }

    fn get_helper(node_addr: Ptr, key: &Key, memory: &impl Memory64) -> Option<Value> {
        println!("get helper");
        let node = Node::load(node_addr, memory);
        println!("Loaded node: {:?}", node);
        match node {
            Node::Leaf(LeafNode { keys, values, .. }) => {
                println!("Leaf node");
                println!("keys: {:?}", keys);

                match keys.binary_search(key) {
                    Ok(idx) => Some(values[idx].clone()),
                    _ => None, // Key not found.
                }
            }
            Node::Internal(internal) => {
                println!("Internal node: {:?}", internal);
                match internal.keys.binary_search(key) {
                    Ok(idx) => Some(internal.values[idx].clone()),
                    Err(idx) => {
                        // The key isn't in the node. Look for the node in the child.
                        let child_address = internal.children[idx];
                        println!("Child address: {:?}", child_address);

                        // Recurse
                        Self::get_helper(child_address, key, memory)
                    }
                }
            }
        }
    }

    pub fn remove(&mut self, key: &Key) -> Option<Value> {
        if self.root_offset == NULL {
            return None;
        }

        let ret = self.remove_helper(self.root_offset, key);
        self.save();
        ret
    }

    fn remove_helper(&mut self, node_addr: Ptr, key: &Key) -> Option<Value> {
        println!("REMOVING KEY: {:?}", key);
        let node = Node::load(node_addr, &self.memory);
        match node {
            Node::Leaf(mut leaf) => {
                match leaf.keys.binary_search(key) {
                    Ok(idx) => {
                        // NOTE: this is O(B). Is this acceptable?
                        let value = leaf.remove(idx);

                        leaf.save(&self.memory);

                        if leaf.address == self.root_offset && leaf.keys.is_empty() {
                            println!("DEALLOCATE1");
                            self.allocator.deallocate(leaf.address);
                            self.root_offset = NULL;
                            // TODO: try to make deallocation more strongly typed.
                        }

                        Some(value)
                    }
                    _ => None, // Key not found.
                }
            }
            Node::Internal(mut internal) => {
                println!("Internal node: {:?}", internal);
                match internal.keys.binary_search(key) {
                    Ok(idx) => {
                        // The key is in the node.
                        println!("Deleting key: {:?} from node {:?}", key, internal);
                        println!("index: {:?}", idx);
                        let value = internal.values[idx].clone(); // TODO: no clone

                        // Check if the child that precedes `key` has at least `B` keys.
                        let mut pre_child = Node::load(internal.children[idx], &self.memory);
                        if pre_child.keys().len() >= B as usize {
                            println!("CASE 2A");
                            // Case 2.a:

                            // Replace the `key` with its predecessor.
                            let predecessor = pre_child.get_max(&self.memory);
                            println!("PREDECESSOR: {:?}", predecessor);
                            internal.keys[idx] = predecessor.0.clone();
                            internal.values[idx] = predecessor.1;

                            println!("Parent: {:?}", internal);
                            //println!("left_child: {:?}", pre_child);
                            println!(
                                "left_child: {:?}",
                                Node::load(internal.children[idx], &self.memory)
                            );

                            // Recursively delete the predecessor.
                            self.remove_helper(internal.children[idx], &predecessor.0);

                            println!("Parent: {:?}", internal);
                            println!(
                                "left_child: {:?}",
                                Node::load(internal.children[idx], &self.memory)
                            );

                            // Save the internal node.
                            internal.save(&self.memory);
                            return Some(value);
                        }

                        // Case 2.b:
                        // Check if the child that succeeds `key` has at least `B` keys.
                        let mut post_child = Node::load(internal.children[idx + 1], &self.memory);
                        println!("post child: {:?}", post_child);
                        if post_child.keys().len() >= B as usize {
                            println!("CASE 2B");
                            // Replace the `key` with its successor.
                            let successor = post_child.get_min(&self.memory);
                            println!("SUCCESSOR: {:?}", successor);
                            internal.keys[idx] = successor.0.clone();
                            internal.values[idx] = successor.1;

                            // Recursively delete the successor.
                            self.remove_helper(internal.children[idx + 1], &successor.0);

                            // Save the internal node.
                            internal.save(&self.memory);
                            return Some(value);
                        }

                        // Case 2.c:
                        println!("CASE 2C");
                        println!("pre_child: {:?}", pre_child);
                        println!("post_child: {:?}", post_child);

                        println!("parent: {:?}", internal);
                        // Delete the key from the internal node.
                        //println!("DELETED: {:?}", internal.keys.remove(idx));
                        // Move the key into the prechild.
                        pre_child.keys_mut().push(internal.keys.remove(idx));
                        pre_child.values_mut().push(internal.values.remove(idx));

                        // Remove the post child from the internal node.
                        internal.children.remove(idx + 1);

                        // Migrate all keys and values from post_child into pre_child
                        pre_child.keys_mut().append(post_child.keys_mut());
                        pre_child.values_mut().append(post_child.values_mut());

                        // Migrate the children if any.
                        match (&mut pre_child, post_child) {
                            (
                                Node::Internal(ref mut pre_child),
                                Node::Internal(ref mut post_child),
                            ) => {
                                // Add the children.
                                pre_child.children.append(&mut post_child.children);
                            }
                            (Node::Leaf(_), Node::Leaf(_)) => { // do nothing
                            }
                            _ => unreachable!(),
                        }

                        // If the root node now has no keys, then delete it.
                        if internal.address == self.root_offset && internal.keys.is_empty() {
                            // Replace the root node with its (only) child.
                            assert_eq!(internal.children.len(), 1);
                            self.root_offset = internal.children[0];

                            // TODO: save btree?
                            // TODO: deallocate root
                        }
                        println!("pre_child after: {:?}", pre_child);

                        internal.save(&self.memory);
                        pre_child.save(&self.memory);
                        // TODO: deallocate postchild

                        // Recursively delete the key.
                        self.remove_helper(pre_child.address(), key)
                    }
                    Err(idx) => {
                        // The key isn't in the node. Look for the node in the child.
                        //let child_address = internal.children[idx];
                        //println!("Child address: {:?}", child_address);

                        let mut subtree = Node::load(internal.children[idx], &self.memory);

                        println!("IN REMOVING BRANCH");
                        println!("index: {:?}", idx);
                        println!("subtree: {:?}", subtree);
                        if subtree.keys().len() >= B as usize {
                            println!("CASE 3");
                            return self.remove_helper(internal.children[idx], key);
                        } else {
                            // Does the child have a sibling with >= `B` keys?
                            let mut left_sibling = if idx > 0 {
                                Some(Node::load(internal.children[idx - 1], &self.memory))
                            } else {
                                None
                            };

                            let mut right_sibling = if idx + 1 < internal.children.len() {
                                Some(Node::load(internal.children[idx + 1], &self.memory))
                            } else {
                                None
                            };

                            if let Some(ref mut left_sibling) = left_sibling {
                                if left_sibling.keys().len() >= B as usize {
                                    // Case 3.a left
                                    // Move one entry from the parent into subtree.
                                    subtree.keys_mut().insert(0, internal.keys[idx - 1].clone());
                                    subtree
                                        .values_mut()
                                        .insert(0, internal.values[idx - 1].clone());

                                    // Move one entry from left_sibling into parent.
                                    internal.keys[idx - 1] = left_sibling.keys_mut().pop().unwrap();
                                    internal.values[idx - 1] =
                                        left_sibling.values_mut().pop().unwrap();

                                    // Move the last child of left sibling into subtree.
                                    match (&mut subtree, left_sibling) {
                                        (Node::Internal(subtree), Node::Internal(left_sibling)) => {
                                            subtree
                                                .children
                                                .insert(0, left_sibling.children.pop().unwrap());
                                            left_sibling.save(&self.memory);
                                        }
                                        (Node::Leaf(_), Node::Leaf(left_sibling)) => {
                                            left_sibling.save(&self.memory);
                                        }
                                        _ => unreachable!(),
                                    }

                                    subtree.save(&self.memory);
                                    internal.save(&self.memory);
                                    return self.remove_helper(subtree.address(), key);
                                }
                            }

                            if let Some(ref mut right_sibling) = right_sibling {
                                if right_sibling.keys().len() >= B as usize {
                                    //todo!("case 3.a.right");
                                    // Move one entry from the parent into subtree.
                                    subtree.keys_mut().push(internal.keys[idx].clone());
                                    subtree.values_mut().push(internal.values[idx].clone());

                                    // Move one entry from right_sibling into parent.
                                    internal.keys[idx] = right_sibling.keys_mut().remove(0);
                                    internal.values[idx] = right_sibling.values_mut().remove(0);

                                    // Move the first child of right_sibling into subtree.
                                    match (&mut subtree, right_sibling) {
                                        (
                                            Node::Internal(subtree),
                                            Node::Internal(right_sibling),
                                        ) => {
                                            subtree.children.push(right_sibling.children.remove(0));
                                            right_sibling.save(&self.memory);
                                        }
                                        (Node::Leaf(_), Node::Leaf(right_sibling)) => {
                                            right_sibling.save(&self.memory);
                                        }
                                        _ => unreachable!(),
                                    }

                                    subtree.save(&self.memory);
                                    internal.save(&self.memory);
                                    return self.remove_helper(subtree.address(), key);
                                }
                            }

                            // Case 3.b: all the siblings have `B` - 1 keys.
                            println!("case 3b");

                            println!("subtree: {:?}", subtree);
                            println!("left sibling: {:?}", left_sibling);
                            println!("right sibling: {:?}", right_sibling);

                            // Merge
                            if let Some(mut left_sibling) = left_sibling {
                                println!("merging into left");
                                // Merge child into left sibling.

                                let left_sibling_address = left_sibling.address();
                                println!("MERGE LEFT");
                                let new_node = self.merge(
                                    subtree,
                                    left_sibling,
                                    (
                                        internal.keys.remove(idx - 1),
                                        internal.values.remove(idx - 1),
                                    ),
                                );
                                println!(
                                    "Removing child {} from parent",
                                    internal.children.remove(idx)
                                );

                                if internal.keys.is_empty() {
                                    println!("DEALLOCATE 2");
                                    self.allocator.deallocate(internal.address);

                                    if internal.address == self.root_offset {
                                        println!("updating root address");
                                        // Update the root.
                                        self.root_offset = left_sibling_address;
                                    }
                                } else {
                                    internal.save(&self.memory);
                                }

                                return self.remove_helper(left_sibling_address, key);
                            }

                            if let Some(mut right_sibling) = right_sibling {
                                println!("merging into right");
                                // Merge child into right sibling.

                                let right_sibling_address = right_sibling.address();
                                println!("MERGE RIGHT");
                                let new_node = self.merge(
                                    subtree,
                                    right_sibling,
                                    (internal.keys.remove(idx), internal.values.remove(idx)),
                                );
                                println!(
                                    "Removing child {} from parent",
                                    internal.children.remove(idx)
                                );

                                if internal.keys.is_empty() {
                                    println!("DEALLOCATE3");
                                    self.allocator.deallocate(internal.address);

                                    if internal.address == self.root_offset {
                                        println!("updating root address");
                                        // Update the root.
                                        self.root_offset = right_sibling_address;
                                    }
                                } else {
                                    internal.save(&self.memory);
                                }

                                return self.remove_helper(right_sibling_address, key);
                                // First add the median key in to the right sibling.
                                /*let median_key = internal.keys.remove(idx);
                                let median_value = internal.values.remove(idx);

                                let mut all_keys = vec![];
                                let mut all_values = vec![];

                                all_keys.append(&mut subtree.keys_mut());
                                all_values.append(&mut subtree.values_mut());

                                all_keys.push(median_key);
                                all_values.push(median_value);

                                all_keys.append(&mut right_sibling.keys_mut());
                                all_values.append(&mut right_sibling.values_mut());

                                // Remove the child from the children.
                                internal.children.remove(idx);

                                // Move all the keys/values/children into the right sibling.
                                match (subtree, right_sibling) {
                                    (
                                        Node::Leaf(mut subtree_leaf),
                                        Node::Leaf(mut right_sibling),
                                    ) => {
                                        right_sibling.keys = all_keys;
                                        right_sibling.values = all_values;

                                        println!("new right sibling: {:?}", right_sibling);
                                        right_sibling.save(&self.memory);

                                        self.allocator.deallocate(subtree_leaf.address);

                                        if internal.keys.is_empty() {
                                            self.allocator.deallocate(internal.address);

                                            if internal.address == self.root_offset {
                                                println!("updating root address");
                                                // Update the root.
                                                self.root_offset = right_sibling.address;
                                            }
                                        }

                                        return self.remove_helper(right_sibling.address, key);
                                    }
                                    (
                                        Node::Internal(mut internal),
                                        Node::Internal(mut right_sibling),
                                    ) => {
                                        right_sibling.keys = all_keys;
                                        right_sibling.values = all_values;

                                        let mut all_children = vec![];
                                        all_children.append(&mut internal.children);
                                        all_children.append(&mut right_sibling.children);

                                        right_sibling.children = all_children;

                                        // Add children as well.
                                        right_sibling.save(&self.memory);

                                        self.allocator.deallocate(internal.address);
                                        return self.remove_helper(right_sibling.address, key);
                                    }
                                    _ => unreachable!(),
                                }*/
                            }

                            println!("left sibling: {:?}", left_sibling);
                            println!("right sibling: {:?}", right_sibling);
                            todo!("3.b");
                        }
                    }
                }
            }
        }
    }

    fn merge(&mut self, source: Node, into: Node, median: (Key, Value)) -> Node {
        // TODO: assert that source and into are non-empty.
        // TODO: assert that both types are the same.
        let into_address = into.address();
        let source_address = source.address();

        // Figure out which node contains lower values than the other.
        let (mut lower, mut higher) = if source.keys()[0] < into.keys()[0] {
            (source, into)
        } else {
            (into, source)
        };

        lower.keys_mut().push(median.0);
        lower.values_mut().push(median.1);

        lower.keys_mut().append(higher.keys_mut());
        lower.values_mut().append(higher.values_mut());

        match &mut lower {
            Node::Leaf(ref mut lower_leaf) => {
                lower_leaf.address = into_address;
                lower_leaf.save(&self.memory);
            }
            Node::Internal(ref mut lower_internal) => {
                lower_internal.address = into_address;

                if let Node::Internal(mut higher_internal) = higher {
                    // Move the children.
                    lower_internal
                        .children
                        .append(&mut higher_internal.children);
                } else {
                    unreachable!();
                }

                lower_internal.save(&self.memory);
            }
        }

        println!("DEALLOCATE4");
        self.allocator.deallocate(source_address);
        lower
    }

    /*
    pub fn range<T, R>(&self, range: R) -> Range
    where
        R: RangeBounds<T>,
    {
        todo!();
    }*/

    fn allocate_leaf_node(&mut self) -> LeafNode {
        //let node_header_len = core::mem::size_of::<NodeHeader>() as u64;
        //let node_size = node_header_len + CAPACITY * ((MAX_KEY_SIZE + MAX_VALUE_SIZE) as u64);
        LeafNode::new(self.allocator.allocate())
    }

    fn allocate_internal_node(&mut self) -> InternalNode {
        //let node_header_len = core::mem::size_of::<NodeHeader>() as u64;
        //let node_size = node_header_len + CAPACITY * ((MAX_KEY_SIZE + MAX_VALUE_SIZE) as u64) + /* children pointers */ 8 * (CAPACITY + 1);

        let node_address = self.allocator.allocate();

        Node::new_internal(node_address)
    }

    fn split_child(&mut self, parent: &mut InternalNode, full_child_idx: usize) {
        println!("SPLIT CHILD");
        assert!(!parent.is_full());
        let full_child = Node::load(parent.children[full_child_idx], &self.memory);

        // The child must be already full.
        assert!(full_child.is_full());

        // Create a sibling to this full child.
        match full_child {
            Node::Leaf(mut full_child_leaf) => {
                let mut sibling = self.allocate_leaf_node();

                // Move the values above the median into the new sibling.
                let mut keys_to_move = full_child_leaf.keys.split_off(B as usize - 1);
                let mut values_to_move = full_child_leaf.values.split_off(B as usize - 1);

                let median_key = keys_to_move.remove(0);
                let median_value = values_to_move.remove(0);

                println!("sibling keys: {:?}", keys_to_move);
                sibling.keys = keys_to_move;
                sibling.values = values_to_move;

                // Add sibling as a new child in parent.
                parent.children.insert(full_child_idx + 1, sibling.address);
                parent.keys.insert(full_child_idx, median_key);
                parent.values.insert(full_child_idx, median_value);

                println!("parent keys: {:?}", parent.keys);
                println!("child keys: {:?}", full_child_leaf.keys);

                full_child_leaf.save(&self.memory);
                sibling.save(&self.memory);
                parent.save(&self.memory);
            }
            Node::Internal(mut full_child_internal) => {
                let mut sibling = self.allocate_internal_node();

                // Move the values above the median into the new sibling.
                let mut keys_to_move = full_child_internal.keys.split_off(B as usize - 1);
                let mut values_to_move = full_child_internal.values.split_off(B as usize - 1);
                let mut children_to_move = full_child_internal.children.split_off(B as usize);

                let median_key = keys_to_move.remove(0);
                let median_value = values_to_move.remove(0);

                println!("sibling keys: {:?}", keys_to_move);
                sibling.keys = keys_to_move;
                sibling.values = values_to_move;
                sibling.children = children_to_move;

                // Add sibling as a new child in parent.
                parent.children.insert(full_child_idx + 1, sibling.address);
                parent.keys.insert(full_child_idx, median_key);
                parent.values.insert(full_child_idx, median_value);

                println!("parent keys: {:?}", parent.keys);
                println!("child keys: {:?}", full_child_internal.keys);

                full_child_internal.save(&self.memory);
                sibling.save(&self.memory);
                parent.save(&self.memory);
            }
        };
    }

    fn insert_nonfull(&mut self, mut node: Node, key: Key, value: Value) -> Option<Value> {
        println!("INSERT NONFULL: key {:?}", key);
        match node {
            Node::Leaf(LeafNode {
                ref mut keys,
                ref mut values,
                ..
            }) => {
                println!("leaf node");
                println!("Keys: {:?}", keys);
                let ret = match keys.binary_search(&key) {
                    Ok(idx) => {
                        // The key was already in the map. Overwrite and return the previous value.
                        let old_value = values[idx].clone(); // TODO: remove this clone?
                        values[idx] = value;
                        Some(old_value)
                    }
                    Err(idx) => {
                        // Key not present.
                        keys.insert(idx, key);
                        values.insert(idx, value);
                        None
                    }
                };

                node.save(&self.memory).unwrap();
                self.save();
                ret
            }
            Node::Internal(ref mut internal) => {
                // Find the child that we should add to.
                // Load the child from memory.
                //
                // if child is full, split the child
                // insert_nonfull(child_after_split, key, value,
                println!("internal node: {:?}", internal);

                let idx = internal.keys.binary_search(&key).unwrap_or_else(|x| x);
                let child_offset = internal.children[idx];
                println!("loading child at offset: {}", child_offset);
                let child = Node::load(child_offset, &self.memory);

                println!("Child Node: {:?}", child);

                if child.is_full() {
                    println!("SPLIT CHILD FROM INSERT NONFULL");
                    self.split_child(internal, idx);
                }

                let idx = internal.keys.binary_search(&key).unwrap_or_else(|x| x);
                let child_offset = internal.children[idx];
                let child = Node::load(child_offset, &self.memory);

                self.insert_nonfull(child, key, value)
            }
        }
    }
}

/// A helper function that reads a single 32bit integer encoded as
/// little-endian from the specified memory at the specified offset.
fn read_u32<M: Memory64>(m: &M, offset: u64) -> u32 {
    let mut buf: [u8; 4] = [0; 4];
    m.read(offset, &mut buf);
    u32::from_le_bytes(buf)
}

/// A helper function that reads a single 32bit integer encoded as
/// little-endian from the specified memory at the specified offset.
fn read_u64<M: Memory64>(m: &M, offset: u64) -> u64 {
    let mut buf: [u8; 8] = [0; 8];
    m.read(offset, &mut buf);
    u64::from_le_bytes(buf)
}

fn write(memory: &impl Memory64, offset: u64, bytes: &[u8]) -> Result<(), WriteError> {
    let last_byte = offset
        .checked_add(bytes.len() as u64)
        .ok_or(WriteError::AddressSpaceOverflow)?;
    let size_pages = memory.size();
    let size_bytes = size_pages
        .checked_mul(WASM_PAGE_SIZE)
        .ok_or(WriteError::AddressSpaceOverflow)?;
    if size_bytes < last_byte {
        let diff_bytes = last_byte - size_bytes;
        let diff_pages = diff_bytes
            .checked_add(WASM_PAGE_SIZE - 1)
            .ok_or(WriteError::AddressSpaceOverflow)?
            / WASM_PAGE_SIZE;
        if memory.grow(diff_pages) == -1 {
            return Err(WriteError::GrowFailed {
                current: size_pages,
                delta: diff_pages,
            });
        }
    }
    memory.write(offset, bytes);
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::Memory64;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn make_memory() -> Rc<RefCell<Vec<u8>>> {
        Rc::new(RefCell::new(Vec::new()))
    }

    #[test]
    fn node_save_load_is_noop() {
        let mem = make_memory();
        let mut node = Node::new_leaf(0);

        // TODO: can we get rid of this if let?
        if let Node::Leaf(ref mut leaf) = node {
            leaf.keys.push(vec![1, 2, 3]);
            leaf.values.push(vec![4, 5, 6]);
        }

        node.save(&mem).unwrap();

        let node_2 = Node::load(0, &mem);

        assert_eq!(node, node_2);
    }

    #[test]
    fn insert_get() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1, 2, 3], vec![4, 5, 6]), None);
        assert_eq!(btree.get(&vec![1, 2, 3]), Some(vec![4, 5, 6]));
    }

    #[test]
    fn insert_overwrites_previous_value() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1, 2, 3], vec![4, 5, 6]), None);
        assert_eq!(
            btree.insert(vec![1, 2, 3], vec![7, 8, 9]),
            Some(vec![4, 5, 6])
        );
        assert_eq!(btree.get(&vec![1, 2, 3]), Some(vec![7, 8, 9]));
    }

    #[test]
    fn insert_get_multiple() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1, 2, 3], vec![4, 5, 6]), None);
        assert_eq!(btree.insert(vec![4, 5], vec![7, 8, 9, 10]), None);
        assert_eq!(btree.insert(vec![], vec![11]), None);
        assert_eq!(btree.get(&vec![1, 2, 3]), Some(vec![4, 5, 6]));
        assert_eq!(btree.get(&vec![4, 5]), Some(vec![7, 8, 9, 10]));
        assert_eq!(btree.get(&vec![]), Some(vec![11]));
    }

    #[test]
    fn insert_same_key_multiple() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1], vec![2]), None);

        for i in 2..100 {
            assert_eq!(btree.insert(vec![1], vec![i + 1]), Some(vec![i]));
        }
    }

    #[test]
    fn insert_split_node() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1], vec![2]), None);
        assert_eq!(btree.insert(vec![2], vec![2]), None);
        assert_eq!(btree.insert(vec![3], vec![2]), None);
        assert_eq!(btree.insert(vec![4], vec![2]), None);
        assert_eq!(btree.insert(vec![5], vec![2]), None);
        assert_eq!(btree.insert(vec![6], vec![2]), None);
        assert_eq!(btree.insert(vec![7], vec![2]), None);
        assert_eq!(btree.insert(vec![8], vec![2]), None);
        assert_eq!(btree.insert(vec![9], vec![2]), None);
        assert_eq!(btree.insert(vec![10], vec![2]), None);
        assert_eq!(btree.insert(vec![11], vec![2]), None);
        // Should now split a node.
        assert_eq!(btree.insert(vec![12], vec![2]), None);

        // The result should looks like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        for i in 1..=12 {
            println!("i: {:?}", i);
            assert_eq!(btree.get(&vec![i]), Some(vec![2]));
        }
    }

    #[test]
    fn insert_split_multiple_nodes() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1], vec![2]), None);
        assert_eq!(btree.insert(vec![2], vec![2]), None);
        assert_eq!(btree.insert(vec![3], vec![2]), None);
        assert_eq!(btree.insert(vec![4], vec![2]), None);
        assert_eq!(btree.insert(vec![5], vec![2]), None);
        assert_eq!(btree.insert(vec![6], vec![2]), None);
        assert_eq!(btree.insert(vec![7], vec![2]), None);
        assert_eq!(btree.insert(vec![8], vec![2]), None);
        assert_eq!(btree.insert(vec![9], vec![2]), None);
        assert_eq!(btree.insert(vec![10], vec![2]), None);
        assert_eq!(btree.insert(vec![11], vec![2]), None);
        // Should now split a node.
        assert_eq!(btree.insert(vec![12], vec![2]), None);

        // The result should looks like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        let root = Node::load(btree.root_offset, &mem);
        match root {
            Node::Internal(internal) => {
                assert_eq!(internal.keys, vec![vec![6]]);
                assert_eq!(internal.values, vec![vec![2]]);
                assert_eq!(internal.children.len(), 2);

                let child_0 = Node::load(internal.children[0], &mem);
                match child_0 {
                    Node::Leaf(leaf) => {
                        assert_eq!(leaf.keys, vec![vec![1], vec![2], vec![3], vec![4], vec![5]]);
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }

                let child_1 = Node::load(internal.children[1], &mem);
                match child_1 {
                    Node::Leaf(leaf) => {
                        assert_eq!(
                            leaf.keys,
                            vec![vec![7], vec![8], vec![9], vec![10], vec![11], vec![12]]
                        );
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }
            }
            _ => panic!("root should be internal"),
        }

        for i in 1..=12 {
            println!("i: {:?}", i);
            assert_eq!(btree.get(&vec![i]), Some(vec![2]));
        }

        // Insert more to cause more splitting.
        assert_eq!(btree.insert(vec![13], vec![2]), None);
        assert_eq!(btree.insert(vec![14], vec![2]), None);
        assert_eq!(btree.insert(vec![15], vec![2]), None);
        assert_eq!(btree.insert(vec![16], vec![2]), None);
        assert_eq!(btree.insert(vec![17], vec![2]), None);
        // Should cause another split
        assert_eq!(btree.insert(vec![18], vec![2]), None);

        for i in 1..=18 {
            println!("i: {:?}", i);
            assert_eq!(btree.get(&vec![i]), Some(vec![2]));
        }

        let root = Node::load(btree.root_offset, &mem);
        match root {
            Node::Internal(internal) => {
                assert_eq!(internal.keys, vec![vec![6], vec![12]]);
                assert_eq!(internal.values, vec![vec![2], vec![2]]);
                assert_eq!(internal.children.len(), 3);

                let child_0 = Node::load(internal.children[0], &mem);
                match child_0 {
                    Node::Leaf(leaf) => {
                        assert_eq!(leaf.keys, vec![vec![1], vec![2], vec![3], vec![4], vec![5]]);
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }

                let child_1 = Node::load(internal.children[1], &mem);
                match child_1 {
                    Node::Leaf(leaf) => {
                        assert_eq!(
                            leaf.keys,
                            vec![vec![7], vec![8], vec![9], vec![10], vec![11]]
                        );
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }

                let child_2 = Node::load(internal.children[2], &mem);
                match child_2 {
                    Node::Leaf(leaf) => {
                        assert_eq!(
                            leaf.keys,
                            vec![vec![13], vec![14], vec![15], vec![16], vec![17], vec![18]]
                        );
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }
            }
            _ => panic!("root should be internal"),
        }
    }

    #[test]
    fn remove_simple() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1, 2, 3], vec![4, 5, 6]), None);
        assert_eq!(btree.get(&vec![1, 2, 3]), Some(vec![4, 5, 6]));
        assert_eq!(btree.remove(&vec![1, 2, 3]), Some(vec![4, 5, 6]));
        assert_eq!(btree.get(&vec![1, 2, 3]), None);
    }

    #[test]
    fn remove_split_node() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1], vec![2]), None);
        assert_eq!(btree.insert(vec![2], vec![2]), None);
        assert_eq!(btree.insert(vec![3], vec![2]), None);
        assert_eq!(btree.insert(vec![4], vec![2]), None);
        assert_eq!(btree.insert(vec![5], vec![2]), None);
        assert_eq!(btree.insert(vec![6], vec![2]), None);
        assert_eq!(btree.insert(vec![7], vec![2]), None);
        assert_eq!(btree.insert(vec![8], vec![2]), None);
        assert_eq!(btree.insert(vec![9], vec![2]), None);
        assert_eq!(btree.insert(vec![10], vec![2]), None);
        assert_eq!(btree.insert(vec![11], vec![2]), None);
        // Should now split a node.
        assert_eq!(btree.insert(vec![12], vec![2]), None);

        // The result should looks like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        for i in 1..=12 {
            println!("i: {:?}", i);
            assert_eq!(btree.get(&vec![i]), Some(vec![2]));
        }

        // Remove node 6. Triggers case 2.b
        assert_eq!(btree.remove(&vec![6]), Some(vec![2]));

        // The result should looks like this:
        //                [7]
        //               /   \
        // [1, 2, 3, 4, 5]   [8, 9, 10, 11, 12]
        let root = Node::load(btree.root_offset, &mem);
        match root {
            Node::Internal(internal) => {
                assert_eq!(internal.keys, vec![vec![7]]);
                assert_eq!(internal.values, vec![vec![2]]);
                assert_eq!(internal.children.len(), 2);

                let child_0 = Node::load(internal.children[0], &mem);
                match child_0 {
                    Node::Leaf(leaf) => {
                        assert_eq!(leaf.keys, vec![vec![1], vec![2], vec![3], vec![4], vec![5]]);
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }

                let child_1 = Node::load(internal.children[1], &mem);
                match child_1 {
                    Node::Leaf(leaf) => {
                        assert_eq!(
                            leaf.keys,
                            vec![vec![8], vec![9], vec![10], vec![11], vec![12]]
                        );
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }
            }
            _ => panic!("root should be internal"),
        }

        // Remove node 7. Triggers case 2.c
        assert_eq!(btree.remove(&vec![7]), Some(vec![2]));
        // The result should looks like this:
        //
        // [1, 2, 3, 4, 5, 8, 9, 10, 11, 12]
        let root = Node::load(btree.root_offset, &mem);
        println!("root: {:?}", root);
        match root {
            Node::Leaf(leaf) => {
                assert_eq!(
                    leaf.keys,
                    vec![
                        vec![1],
                        vec![2],
                        vec![3],
                        vec![4],
                        vec![5],
                        vec![8],
                        vec![9],
                        vec![10],
                        vec![11],
                        vec![12]
                    ]
                );
                assert_eq!(leaf.values, vec![vec![2]; 10]);
            }
            _ => panic!("root should be leaf"),
        }
    }

    #[test]
    fn remove_split_node_2() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1], vec![2]), None);
        assert_eq!(btree.insert(vec![2], vec![2]), None);
        assert_eq!(btree.insert(vec![3], vec![2]), None);
        assert_eq!(btree.insert(vec![4], vec![2]), None);
        assert_eq!(btree.insert(vec![5], vec![2]), None);
        assert_eq!(btree.insert(vec![6], vec![2]), None);
        assert_eq!(btree.insert(vec![7], vec![2]), None);
        assert_eq!(btree.insert(vec![8], vec![2]), None);
        assert_eq!(btree.insert(vec![9], vec![2]), None);
        assert_eq!(btree.insert(vec![10], vec![2]), None);
        assert_eq!(btree.insert(vec![11], vec![2]), None);
        // Should now split a node.
        assert_eq!(btree.insert(vec![0], vec![2]), None);

        // The result should looks like this:
        //                    [6]
        //                   /   \
        // [0, 1, 2, 3, 4, 5]     [7, 8, 9, 10, 11]

        for i in 0..=11 {
            assert_eq!(btree.get(&vec![i]), Some(vec![2]));
        }

        // Remove node 6. Triggers case 2.a
        assert_eq!(btree.remove(&vec![6]), Some(vec![2]));

        /*
        // The result should looks like this:
        //                [5]
        //               /   \
        // [0, 1, 2, 3, 4]   [7, 8, 9, 10, 11]
        let root = Node::load(btree.root_offset, &mem);
        match root {
            Node::Internal(internal) => {
                assert_eq!(internal.keys, vec![vec![5]]);
                assert_eq!(internal.values, vec![vec![2]]);
                assert_eq!(internal.children.len(), 2);

                let child_0 = Node::load(internal.children[0], &mem);
                match child_0 {
                    Node::Leaf(leaf) => {
                        assert_eq!(leaf.keys, vec![vec![0], vec![1], vec![2], vec![3], vec![4]]);
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }

                let child_1 = Node::load(internal.children[1], &mem);
                match child_1 {
                    Node::Leaf(leaf) => {
                        assert_eq!(
                            leaf.keys,
                            vec![vec![7], vec![8], vec![9], vec![10], vec![11]]
                        );
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }
            }
            _ => panic!("root should be internal"),
        }*/
    }

    #[test]
    fn reloading() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1, 2, 3], vec![4, 5, 6]), None);

        let mut btree = StableBTreeMap::load(mem.clone()).unwrap();
        assert_eq!(btree.get(&vec![1, 2, 3]), Some(vec![4, 5, 6]));

        let mut btree = StableBTreeMap::load(mem.clone()).unwrap();
        assert_eq!(btree.remove(&vec![1, 2, 3]), Some(vec![4, 5, 6]));

        let mut btree = StableBTreeMap::load(mem.clone()).unwrap();
        assert_eq!(btree.get(&vec![1, 2, 3]), None);
    }

    #[test]
    fn remove_3a_right() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1], vec![2]), None);
        assert_eq!(btree.insert(vec![2], vec![2]), None);
        assert_eq!(btree.insert(vec![3], vec![2]), None);
        assert_eq!(btree.insert(vec![4], vec![2]), None);
        assert_eq!(btree.insert(vec![5], vec![2]), None);
        assert_eq!(btree.insert(vec![6], vec![2]), None);
        assert_eq!(btree.insert(vec![7], vec![2]), None);
        assert_eq!(btree.insert(vec![8], vec![2]), None);
        assert_eq!(btree.insert(vec![9], vec![2]), None);
        assert_eq!(btree.insert(vec![10], vec![2]), None);
        assert_eq!(btree.insert(vec![11], vec![2]), None);
        // Should now split a node.
        assert_eq!(btree.insert(vec![12], vec![2]), None);

        // The result should looks like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        // Remove node 3. Triggers case 3.a
        assert_eq!(btree.remove(&vec![3]), Some(vec![2]));

        // The result should looks like this:
        //                [7]
        //               /   \
        // [1, 2, 4, 5, 6]   [8, 9, 10, 11, 12]
        let root = Node::load(btree.root_offset, &mem);
        match root {
            Node::Internal(internal) => {
                assert_eq!(internal.keys, vec![vec![7]]);
                assert_eq!(internal.values, vec![vec![2]]);
                assert_eq!(internal.children.len(), 2);

                let child_0 = Node::load(internal.children[0], &mem);
                match child_0 {
                    Node::Leaf(leaf) => {
                        assert_eq!(leaf.keys, vec![vec![1], vec![2], vec![4], vec![5], vec![6]]);
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }

                let child_1 = Node::load(internal.children[1], &mem);
                match child_1 {
                    Node::Leaf(leaf) => {
                        assert_eq!(
                            leaf.keys,
                            vec![vec![8], vec![9], vec![10], vec![11], vec![12]]
                        );
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }
            }
            _ => panic!("root should be internal"),
        }
    }

    #[test]
    fn remove_3a_left() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1], vec![2]), None);
        assert_eq!(btree.insert(vec![2], vec![2]), None);
        assert_eq!(btree.insert(vec![3], vec![2]), None);
        assert_eq!(btree.insert(vec![4], vec![2]), None);
        assert_eq!(btree.insert(vec![5], vec![2]), None);
        assert_eq!(btree.insert(vec![6], vec![2]), None);
        assert_eq!(btree.insert(vec![7], vec![2]), None);
        assert_eq!(btree.insert(vec![8], vec![2]), None);
        assert_eq!(btree.insert(vec![9], vec![2]), None);
        assert_eq!(btree.insert(vec![10], vec![2]), None);
        assert_eq!(btree.insert(vec![11], vec![2]), None);
        // Should now split a node.
        assert_eq!(btree.insert(vec![0], vec![2]), None);

        // The result should looks like this:
        //                   [6]
        //                  /   \
        // [0, 1, 2, 3, 4, 5]   [7, 8, 9, 10, 11]

        // Remove node 8. Triggers case 3.a left
        assert_eq!(btree.remove(&vec![8]), Some(vec![2]));

        // The result should looks like this:
        //                [5]
        //               /   \
        // [0, 1, 2, 3, 4]   [6, 7, 9, 10, 11]
        let root = Node::load(btree.root_offset, &mem);
        match root {
            Node::Internal(internal) => {
                assert_eq!(internal.keys, vec![vec![5]]);
                assert_eq!(internal.values, vec![vec![2]]);
                assert_eq!(internal.children.len(), 2);

                let child_0 = Node::load(internal.children[0], &mem);
                match child_0 {
                    Node::Leaf(leaf) => {
                        assert_eq!(leaf.keys, vec![vec![0], vec![1], vec![2], vec![3], vec![4]]);
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }

                let child_1 = Node::load(internal.children[1], &mem);
                match child_1 {
                    Node::Leaf(leaf) => {
                        assert_eq!(
                            leaf.keys,
                            vec![vec![6], vec![7], vec![9], vec![10], vec![11]]
                        );
                        assert_eq!(
                            leaf.values,
                            vec![vec![2], vec![2], vec![2], vec![2], vec![2]]
                        );
                    }
                    _ => panic!("child should be leaf"),
                }
            }
            _ => panic!("root should be internal"),
        }
    }

    #[test]
    fn remove_3b_merge_into_right() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        assert_eq!(btree.insert(vec![1], vec![2]), None);
        assert_eq!(btree.insert(vec![2], vec![2]), None);
        assert_eq!(btree.insert(vec![3], vec![2]), None);
        assert_eq!(btree.insert(vec![4], vec![2]), None);
        assert_eq!(btree.insert(vec![5], vec![2]), None);
        assert_eq!(btree.insert(vec![6], vec![2]), None);
        assert_eq!(btree.insert(vec![7], vec![2]), None);
        assert_eq!(btree.insert(vec![8], vec![2]), None);
        assert_eq!(btree.insert(vec![9], vec![2]), None);
        assert_eq!(btree.insert(vec![10], vec![2]), None);
        assert_eq!(btree.insert(vec![11], vec![2]), None);
        // Should now split a node.
        assert_eq!(btree.insert(vec![12], vec![2]), None);

        // The result should looks like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        for i in 1..=12 {
            println!("i: {:?}", i);
            assert_eq!(btree.get(&vec![i]), Some(vec![2]));
        }

        // Remove node 6. Triggers case 2.b
        assert_eq!(btree.remove(&vec![6]), Some(vec![2]));
        // The result should looks like this:
        //                [7]
        //               /   \
        // [1, 2, 3, 4, 5]   [8, 9, 10, 11, 12]
        let root = Node::load(btree.root_offset, &mem);

        // Remove node 3. Triggers case 3.b
        assert_eq!(btree.remove(&vec![3]), Some(vec![2]));

        // The result should looks like this:
        //
        // [1, 2, 4, 5, 7, 8, 9, 10, 11, 12]
        let root = Node::load(btree.root_offset, &mem);
        assert_eq!(
            root.keys(),
            vec![
                vec![1],
                vec![2],
                vec![4],
                vec![5],
                vec![7],
                vec![8],
                vec![9],
                vec![10],
                vec![11],
                vec![12]
            ]
        );
        // TODO: assert node is a leaf node.
    }

    #[test]
    fn remove_3b_merge_into_left() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        for i in 1..=11 {
            assert_eq!(btree.insert(vec![i], vec![2]), None);
        }

        // Should now split a node.
        assert_eq!(btree.insert(vec![12], vec![2]), None);

        // The result should looks like this:
        //                [6]
        //               /   \
        // [1, 2, 3, 4, 5]   [7, 8, 9, 10, 11, 12]

        for i in 1..=12 {
            assert_eq!(btree.get(&vec![i]), Some(vec![2]));
        }

        // Remove node 6. Triggers case 2.b
        assert_eq!(btree.remove(&vec![6]), Some(vec![2]));

        // The result should looks like this:
        //                [7]
        //               /   \
        // [1, 2, 3, 4, 5]   [8, 9, 10, 11, 12]
        let root = Node::load(btree.root_offset, &mem);

        // Remove node 10. Triggers case 3.b where we merge the right into the left.
        assert_eq!(btree.remove(&vec![10]), Some(vec![2]));

        // The result should looks like this:
        //
        // [1, 2, 3, 4, 5, 7, 8, 9, 11, 12]
        let root = Node::load(btree.root_offset, &mem);
        assert_eq!(
            root.keys(),
            vec![
                vec![1],
                vec![2],
                vec![3],
                vec![4],
                vec![5],
                vec![7],
                vec![8],
                vec![9],
                vec![11],
                vec![12]
            ]
        );
        // TODO: assert node is a leaf node.
    }

    #[test]
    fn many_insertions() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        for j in 0..=10 {
            for i in 0..=255 {
                assert_eq!(btree.insert(vec![i, j], vec![i, j]), None);
            }
        }

        for j in 0..=10 {
            for i in 0..=255 {
                assert_eq!(btree.get(&vec![i, j]), Some(vec![i, j]));
            }
        }

        for j in 0..=10 {
            for i in 0..=255 {
                println!("i, j: {}, {}", i, j);
                assert_eq!(btree.remove(&vec![i, j]), Some(vec![i, j]));
            }
        }

        for j in 0..=10 {
            for i in 0..=255 {
                assert_eq!(btree.get(&vec![i, j]), None);
            }
        }
    }

    #[test]
    fn many_insertions_2() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0).unwrap();

        for j in (0..=10).rev() {
            for i in (0..=255).rev() {
                assert_eq!(btree.insert(vec![i, j], vec![i, j]), None);
            }
        }

        for j in 0..=10 {
            for i in 0..=255 {
                assert_eq!(btree.get(&vec![i, j]), Some(vec![i, j]));
            }
        }

        for j in (0..=10).rev() {
            for i in (0..=255).rev() {
                println!("i, j: {}, {}", i, j);
                assert_eq!(btree.remove(&vec![i, j]), Some(vec![i, j]));
            }
        }

        for j in 0..=10 {
            for i in 0..=255 {
                assert_eq!(btree.get(&vec![i, j]), None);
            }
        }
    }

    /*
    #[test]
    fn deallocating() {
        let mem = make_memory();
        let mut btree = StableBTreeMap::new(mem.clone(), 0, 0);

        let old_free_list = btree.free_list;

        assert_eq!(btree.insert(vec![1, 2, 3], vec![4, 5, 6]), None);
        assert_eq!(btree.remove(&vec![1, 2, 3]), Some(vec![4, 5, 6]));

        // Added an element and removed it. The free list should be unchanged.
        assert_eq!(old_free_list, btree.free_list);
    }*/
}
