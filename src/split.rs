//! 终端分屏布局树（轻量版）。
//!
//! 只负责「谁在哪儿」，不碰终端本身（`TermPane` 在 `tabs::TerminalTab` 里）。
//!
//! 设计取舍（与需求对齐）：
//! - 两个节点类型：叶子（一个终端窗格）与分割（横向左右 / 纵向上下 + 子节点）；
//! - 节点 id 全局单调自增：`ResizablePanelGroup` 内部用 `window.use_keyed_state`
//!   按 ElementId 缓存 `ResizableState`，id 稳定 → 用户拖出来的比例在每帧重绘、
//!   切标签、甚至再加一个分屏之后都还在（这就是「比例存 ResizableState」）；
//! - 不做拖拽换位、不做布局持久化（落盘留到以后再说）。
//!
//! 所有方法在找不到目标时都是**无操作**并返回 false，不 panic。

use gpui_kit::Axis;
use std::sync::atomic::{AtomicU64, Ordering};

/// 布局节点 id。
pub type NodeId = u64;
/// 窗格 id（在 `TerminalTab` 内稳定，与 `Vec` 下标解耦，删中间窗格不会串位）。
pub type PaneId = u64;

static NEXT_NODE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_PANE_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_node_id() -> NodeId {
    NEXT_NODE_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn next_pane_id() -> PaneId {
    NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed)
}

/// 布局节点。
#[derive(Clone, Debug)]
pub enum SplitNode {
    /// 一个终端窗格。
    Leaf { node: NodeId, pane: PaneId },
    /// 一列/一行子节点。
    Split {
        node: NodeId,
        axis: Axis,
        children: Vec<SplitNode>,
    },
}

impl SplitNode {
    pub fn leaf(pane: PaneId) -> Self {
        SplitNode::Leaf {
            node: next_node_id(),
            pane,
        }
    }

    /// 叶子返回它承载的窗格 id，分割节点返回 None。
    pub fn pane(&self) -> Option<PaneId> {
        match self {
            SplitNode::Leaf { pane, .. } => Some(*pane),
            SplitNode::Split { .. } => None,
        }
    }
}

/// 一棵分屏树。`root` 永远是叶子或至少两个子节点的分割。
#[derive(Clone, Debug)]
pub struct SplitTree {
    root: SplitNode,
}

impl SplitTree {
    /// 单窗格树。
    pub fn new(pane: PaneId) -> Self {
        Self {
            root: SplitNode::leaf(pane),
        }
    }

    pub fn root(&self) -> &SplitNode {
        &self.root
    }

    /// 按布局顺序（左→右 / 上→下）列出所有窗格。
    pub fn leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        collect_leaves(&self.root, &mut out);
        out
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.leaves().contains(&pane)
    }

    pub fn len(&self) -> usize {
        count_leaves(&self.root)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 在 `target` 窗格上分屏：`axis` 横向 = 左右并排，纵向 = 上下叠放。
    ///
    /// `before = true` 时新窗格放在左/上。返回 false 表示树里没有 `target`。
    ///
    /// 同轴向的兄弟会并入同一层（三格并排而不是两层嵌套），不同轴向才新开一层，
    /// 这样「右分屏 + 再右分屏」得到的是三等分而不是 1:1:2。
    pub fn split(&mut self, target: PaneId, axis: Axis, pane: PaneId, before: bool) -> bool {
        split_in(&mut self.root, target, axis, pane, before)
    }

    /// 移除一个窗格并塌掉多余的分割层。
    ///
    /// 返回移除后是否**还有**窗格；false 表示这棵树已经空了（调用方应关掉标签页）。
    pub fn remove(&mut self, pane: PaneId) -> bool {
        if self.root.pane() == Some(pane) {
            // 最后一个窗格：不在这里制造空树，交给调用方决定关标签
            return false;
        }
        remove_in(&mut self.root, pane);
        normalize(&mut self.root);
        !self.is_empty()
    }
}

fn collect_leaves(node: &SplitNode, out: &mut Vec<PaneId>) {
    match node {
        SplitNode::Leaf { pane, .. } => out.push(*pane),
        SplitNode::Split { children, .. } => {
            for c in children {
                collect_leaves(c, out);
            }
        }
    }
}

fn count_leaves(node: &SplitNode) -> usize {
    match node {
        SplitNode::Leaf { .. } => 1,
        SplitNode::Split { children, .. } => children.iter().map(count_leaves).sum(),
    }
}

fn split_in(node: &mut SplitNode, target: PaneId, axis: Axis, pane: PaneId, before: bool) -> bool {
    let SplitNode::Split {
        axis: cur,
        children,
        ..
    } = node
    else {
        return false;
    };

    // 1) 同轴向：直接插到这一层，避免嵌套出 1:1:2 这种比例
    if *cur == axis {
        if let Some(ix) = children.iter().position(|c| c.pane() == Some(target)) {
            let at = if before { ix } else { ix + 1 };
            children.insert(at, SplitNode::leaf(pane));
            return true;
        }
    }

    // 2) 目标就在本层的直接子节点上：换成一个新的分割层，原节点沿用（比例不乱）
    for child in children.iter_mut() {
        if child.pane() == Some(target) {
            let old = child.clone();
            let fresh = SplitNode::leaf(pane);
            let kids = if before {
                vec![fresh, old]
            } else {
                vec![old, fresh]
            };
            *child = SplitNode::Split {
                node: next_node_id(),
                axis,
                children: kids,
            };
            return true;
        }
    }

    // 3) 往下找
    for child in children.iter_mut() {
        if split_in(child, target, axis, pane, before) {
            return true;
        }
    }
    false
}

fn remove_in(node: &mut SplitNode, pane: PaneId) -> bool {
    let SplitNode::Split { children, .. } = node else {
        return false;
    };
    if let Some(ix) = children.iter().position(|c| c.pane() == Some(pane)) {
        children.remove(ix);
        return true;
    }
    for child in children.iter_mut() {
        if remove_in(child, pane) {
            return true;
        }
    }
    false
}

/// 塌掉只有一个子节点的分割层（关掉一个窗格后留下的空壳）。
fn normalize(node: &mut SplitNode) {
    if let SplitNode::Split { children, .. } = node {
        for c in children.iter_mut() {
            normalize(c);
        }
    }
    loop {
        let only = match node {
            SplitNode::Split { children, .. } if children.len() == 1 => Some(children.remove(0)),
            _ => None,
        };
        match only {
            Some(inner) => *node = inner,
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves_of(tree: &SplitTree) -> Vec<PaneId> {
        tree.leaves()
    }

    #[test]
    fn split_right_then_right_gives_three_equal_siblings() {
        let mut tree = SplitTree::new(1);
        assert!(tree.split(1, Axis::Horizontal, 2, false));
        // 同轴向并入同一层：根还是两层子节点，不是嵌套
        assert_eq!(leaves_of(&tree), vec![1, 2]);
        assert!(tree.split(1, Axis::Horizontal, 3, true));
        assert_eq!(leaves_of(&tree), vec![3, 1, 2]);
        match tree.root() {
            SplitNode::Split { children, .. } => assert_eq!(children.len(), 3),
            _ => panic!("根应当是分割节点"),
        }
    }

    #[test]
    fn split_down_after_right_nests_a_new_level() {
        let mut tree = SplitTree::new(1);
        assert!(tree.split(1, Axis::Horizontal, 2, false)); // 1 | 2
        assert!(tree.split(2, Axis::Vertical, 3, false)); // 1 | (2 / 3)
        assert_eq!(leaves_of(&tree), vec![1, 2, 3]);
        match tree.root() {
            SplitNode::Split { children, axis, .. } => {
                assert_eq!(*axis, Axis::Horizontal);
                assert_eq!(children.len(), 2);
                match &children[1] {
                    SplitNode::Split { axis, children, .. } => {
                        assert_eq!(*axis, Axis::Vertical);
                        assert_eq!(children.len(), 2);
                    }
                    _ => panic!("右半边应当是纵向分割"),
                }
            }
            _ => panic!("根应当是横向分割"),
        }
    }

    #[test]
    fn removing_a_middle_pane_collapses_the_empty_level() {
        let mut tree = SplitTree::new(1);
        tree.split(1, Axis::Horizontal, 2, false);
        tree.split(1, Axis::Vertical, 3, false); // 1 | 2，1 变成 (1 / 3)
        assert_eq!(leaves_of(&tree), vec![1, 3, 2]);

        assert!(tree.remove(3)); // (1 / 3) 塌回单叶子
        assert_eq!(leaves_of(&tree), vec![1, 2]);
        match tree.root() {
            SplitNode::Split { axis, children, .. } => {
                assert_eq!(*axis, Axis::Horizontal);
                assert_eq!(children.len(), 2);
            }
            _ => panic!("根应当是横向分割"),
        }

        assert!(tree.remove(2));
        assert_eq!(leaves_of(&tree), vec![1]);
    }

    #[test]
    fn removing_the_last_pane_reports_an_empty_tree() {
        let mut tree = SplitTree::new(7);
        assert!(!tree.remove(7), "最后一个窗格不该在树里制造空节点");
        assert_eq!(leaves_of(&tree), vec![7]);
    }

    #[test]
    fn unknown_target_is_a_no_op() {
        let mut tree = SplitTree::new(1);
        assert!(!tree.split(99, Axis::Horizontal, 2, false));
        assert_eq!(leaves_of(&tree), vec![1]);
        assert_eq!(tree.len(), 1);
        assert!(tree.contains(1));
        assert!(!tree.contains(99));
    }
}
